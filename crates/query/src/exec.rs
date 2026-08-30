//! Planner + executor: validated IR → rows, on the existing graph primitives. The shape is
//! fixed: pick the cheaper endpoint as the anchor (id/eid, then name via `names.idx`, then a
//! kind-column scan), expand the relationship (one-hop adjacency walk or a per-seed bounded
//! BFS), filter, then project/aggregate. Every stage runs under the crate ceilings — work
//! counts, never wall time — and exceeding one is a typed error naming it, never a silently
//! truncated answer.

use std::collections::{HashMap, HashSet};

use vorpal_kg::{EdgeType, Kg, NodeId, NodeView, SymbolKind, SymbolSelector};

use crate::ir::*;
use crate::{MAX_DEPTH, MAX_EDGE_VISITS, MAX_ROWS, QueryError, QueryResult};

/// One result cell. The order (`Null < Bool < Int < Text`) is the ORDER BY order.
#[derive(Debug, Clone, Hash, PartialEq, Eq, PartialOrd, Ord, serde::Serialize)]
#[serde(untagged)]
pub enum Cell {
  Null,
  Bool(bool),
  Int(u64),
  Text(String),
}

impl std::fmt::Display for Cell {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    match self {
      Cell::Null => write!(f, "∅"),
      Cell::Bool(b) => write!(f, "{b}"),
      Cell::Int(n) => write!(f, "{n}"),
      Cell::Text(t) => write!(f, "{t}"),
    }
  }
}

/// Mirrors `vorpal-index`'s grade floors (single behavioral source is the resolver's
/// confidence bands; both copies carry this cross-reference).
fn min_confidence_for_grade(grade: Option<&str>) -> Result<u8, QueryError> {
  Ok(match grade.map(str::to_ascii_lowercase).as_deref() {
    None | Some("") => 0,
    Some("heuristic") => 1,
    Some("constrained") => 85,
    Some("exact") => 100,
    Some(other) => {
      return Err(QueryError::Plan(format!(
        "unknown grade '{other}' (exact | constrained | heuristic)"
      )));
    }
  })
}

const PROPS: &[&str] = &[
  "id", "eid", "name", "path", "kind", "exported", "signature", "in_degree", "out_degree",
  "scc_size",
];

fn check_prop(prop: &str) -> Result<(), QueryError> {
  if PROPS.contains(&prop) {
    Ok(())
  } else {
    Err(QueryError::Plan(format!(
      "unknown property '{prop}' (available: {})",
      PROPS.join(", ")
    )))
  }
}

#[derive(PartialEq, Clone, Copy)]
enum PropType {
  Int,
  Text,
  Bool,
}

fn prop_type(prop: &str) -> PropType {
  match prop {
    "id" | "in_degree" | "out_degree" | "scc_size" => PropType::Int,
    "exported" => PropType::Bool,
    _ => PropType::Text,
  }
}

/// Plan-time predicate typing: ordered comparisons need integer properties and values;
/// the substring operators need text on both sides — mistakes fail before any scan.
fn check_predicate_types(prop: &str, op: CmpOp, value: &PropValue) -> Result<(), QueryError> {
  let ty = prop_type(prop);
  let value_ty = match value {
    PropValue::Int(_) => PropType::Int,
    PropValue::Text(_) => PropType::Text,
    PropValue::Bool(_) => PropType::Bool,
  };
  match op {
    CmpOp::Lt | CmpOp::Le | CmpOp::Gt | CmpOp::Ge => {
      if ty != PropType::Int || value_ty != PropType::Int {
        return Err(QueryError::Plan(format!(
          "ordered comparison on '{prop}' — <, <=, >, >= apply to integer properties \
           (id, in_degree, out_degree) with integer values"
        )));
      }
    }
    CmpOp::StartsWith | CmpOp::EndsWith | CmpOp::Contains => {
      if ty != PropType::Text || value_ty != PropType::Text {
        return Err(QueryError::Plan(format!(
          "substring comparison on '{prop}' — STARTS/ENDS WITH and CONTAINS apply to text \
           properties with string values"
        )));
      }
    }
    CmpOp::Eq | CmpOp::Ne => {
      if ty != value_ty {
        return Err(QueryError::Plan(format!(
          "type mismatch: property '{prop}' does not compare to that value's type"
        )));
      }
    }
  }
  Ok(())
}

fn prop_cell(kg: &Kg, id: u32, prop: &str) -> Cell {
  let Some(view) = kg.node(NodeId::new(id as u64)) else {
    return Cell::Null;
  };
  match prop {
    "id" => Cell::Int(id as u64),
    "in_degree" => Cell::Int(kg.in_degree(NodeId::new(id as u64)) as u64),
    "out_degree" => Cell::Int(kg.out_degree(NodeId::new(id as u64)) as u64),
    "scc_size" => match kg.scc_size(NodeId::new(id as u64)) {
      Some(size) => Cell::Int(size as u64),
      None => Cell::Null, // pre-column generation: unknown, never "acyclic"
    },
    "eid" => match view.external_id {
      Some(eid) => Cell::Text(format!("{eid:032x}")),
      None => Cell::Null,
    },
    "name" => Cell::Text(view.name.to_string()),
    "path" => Cell::Text(view.path.to_string()),
    "kind" => Cell::Text(format!("{:?}", view.kind).to_ascii_lowercase()),
    "exported" => Cell::Bool(view.exported),
    "signature" => Cell::Text(view.signature.to_string()),
    _ => Cell::Null,
  }
}

/// A node pattern compiled against the open graph: exact facets extracted, kind parsed.
#[derive(Default, Clone)]
struct SidePlan {
  var: Option<String>,
  kind: Option<SymbolKind>,
  name: Option<String>,
  path_suffix: Option<String>,
  id: Option<u64>,
  eid: Option<u128>,
  exported: Option<bool>,
}

impl SidePlan {
  fn compile(node: &NodePattern) -> Result<Self, QueryError> {
    let mut plan = SidePlan {
      var: node.var.clone(),
      ..Default::default()
    };
    if let Some(kind) = &node.kind {
      plan.kind = Some(SymbolKind::parse(kind).ok_or_else(|| {
        QueryError::Plan(format!("unknown kind '{kind}' (try: function, method, class, …)"))
      })?);
    }
    for (key, value) in &node.props {
      match (key.as_str(), value) {
        ("name", PropValue::Text(text)) => plan.name = Some(text.clone()),
        ("path", PropValue::Text(text)) => plan.path_suffix = Some(text.clone()),
        ("id", PropValue::Int(n)) => plan.id = Some(*n),
        ("eid", PropValue::Text(hex)) => {
          let eid = u128::from_str_radix(hex, 16)
            .map_err(|_| QueryError::Plan(format!("eid '{hex}' is not 128-bit hex")))?;
          plan.eid = Some(eid);
        }
        ("exported", PropValue::Bool(b)) => plan.exported = Some(*b),
        ("name" | "path" | "eid", _) => {
          return Err(QueryError::Plan(format!("property '{key}' takes a string")));
        }
        ("id", _) => return Err(QueryError::Plan("property 'id' takes an integer".into())),
        ("exported", _) => {
          return Err(QueryError::Plan("property 'exported' takes true/false".into()));
        }
        (other, _) => {
          return Err(QueryError::Plan(format!(
            "unknown inline property '{other}' (name, path, id, eid, exported; kind is a :Label)"
          )));
        }
      }
    }
    Ok(plan)
  }

  /// Anchor cost class: lower enumerates cheaper.
  fn cost(&self) -> u8 {
    if self.id.is_some() || self.eid.is_some() {
      0
    } else if self.name.is_some() {
      1
    } else if self.kind.is_some() {
      2
    } else {
      3
    }
  }

  fn matches(&self, id: u32, view: &NodeView<'_>) -> bool {
    self.id.is_none_or(|want| want == id as u64)
      && self.eid.is_none_or(|want| view.external_id == Some(want))
      && self.kind.is_none_or(|want| view.kind == want)
      && self.name.as_deref().is_none_or(|want| view.name == want)
      && self.path_suffix.as_deref().is_none_or(|want| view.path.ends_with(want))
      && self.exported.is_none_or(|want| view.exported == want)
  }

  /// Enumerate matching node ids, ascending. The scan arm parallelizes over the kind
  /// column and keeps ascending order by indexed collect.
  fn candidates(&self, kg: &Kg) -> Vec<u32> {
    if let Some(id) = self.id {
      return match kg.node(NodeId::new(id)) {
        Some(view) if self.matches(id as u32, &view) => vec![id as u32],
        _ => Vec::new(),
      };
    }
    if let Some(eid) = self.eid {
      return kg
        .nodes_with_external_id(eid)
        .into_iter()
        .map(|n| n.raw() as u32)
        .filter(|&id| kg.node(NodeId::new(id as u64)).is_some_and(|v| self.matches(id, &v)))
        .collect();
    }
    if self.name.is_some() {
      let selector = SymbolSelector {
        id: None,
        name: self.name.as_deref(),
        path_suffix: self.path_suffix.as_deref(),
        kind: self.kind,
        external_id: None,
      };
      return kg
        .select(&selector)
        .into_iter()
        .map(|n| n.raw() as u32)
        .filter(|&id| {
          self.exported.is_none()
            || kg.node(NodeId::new(id as u64)).is_some_and(|v| self.matches(id, &v))
        })
        .collect();
    }
    use rayon::prelude::*;
    let count = kg.node_count() as u32;
    // Kind-column prefilter when the segment carries the dense tag column.
    if let (Some(kind), Some(tags)) = (self.kind, kg.kind_tags()) {
      let tag = kind.tag();
      return (0..count)
        .into_par_iter()
        .filter(|&id| {
          tags.get(id as usize) == Some(&tag)
            && kg.node(NodeId::new(id as u64)).is_some_and(|v| self.matches(id, &v))
        })
        .collect();
    }
    (0..count)
      .into_par_iter()
      .filter(|&id| kg.node(NodeId::new(id as u64)).is_some_and(|v| self.matches(id, &v)))
      .collect()
  }
}

/// A WHERE conjunct bound to a side (0 = left, 1 = right).
struct BoundPredicate {
  side: usize,
  prop: String,
  op: CmpOp,
  value: PropValue,
}

/// The WHERE tree with every leaf's variable resolved to a side.
enum BoundPred {
  Cmp(BoundPredicate),
  And(Vec<BoundPred>),
  Or(Vec<BoundPred>),
  Not(Box<BoundPred>),
}

impl BoundPred {
  fn eval(&self, kg: &Kg, l: u32, r: u32) -> bool {
    match self {
      BoundPred::Cmp(leaf) => {
        let id = if leaf.side == 0 { l } else { r };
        id != u32::MAX && leaf.eval(kg, id)
      }
      BoundPred::And(terms) => terms.iter().all(|t| t.eval(kg, l, r)),
      BoundPred::Or(terms) => terms.iter().any(|t| t.eval(kg, l, r)),
      BoundPred::Not(inner) => !inner.eval(kg, l, r),
    }
  }
}

impl BoundPredicate {
  fn eval(&self, kg: &Kg, id: u32) -> bool {
    let cell = prop_cell(kg, id, &self.prop);
    match (&self.op, &self.value, &cell) {
      (CmpOp::Eq, PropValue::Text(want), Cell::Text(have)) => {
        if self.prop == "kind" || self.prop == "eid" {
          want.eq_ignore_ascii_case(have)
        } else {
          want == have
        }
      }
      (CmpOp::Eq, PropValue::Int(want), Cell::Int(have)) => want == have,
      (CmpOp::Eq, PropValue::Bool(want), Cell::Bool(have)) => want == have,
      (CmpOp::Ne, _, _) => !BoundPredicate {
        side: self.side,
        prop: self.prop.clone(),
        op: CmpOp::Eq,
        value: self.value.clone(),
      }
      .eval(kg, id),
      (CmpOp::StartsWith, PropValue::Text(want), Cell::Text(have)) => have.starts_with(want),
      (CmpOp::EndsWith, PropValue::Text(want), Cell::Text(have)) => have.ends_with(want),
      (CmpOp::Contains, PropValue::Text(want), Cell::Text(have)) => have.contains(want),
      (CmpOp::Lt, PropValue::Int(want), Cell::Int(have)) => have < want,
      (CmpOp::Le, PropValue::Int(want), Cell::Int(have)) => have <= want,
      (CmpOp::Gt, PropValue::Int(want), Cell::Int(have)) => have > want,
      (CmpOp::Ge, PropValue::Int(want), Cell::Int(have)) => have >= want,
      _ => false,
    }
  }
}

/// One projected column bound to a side.
struct BoundColumn {
  title: String,
  side: usize,
  prop: String,
}

pub(crate) fn execute(kg: &Kg, query: &Query) -> Result<QueryResult, QueryError> {
  // ---- Plan: sides, vars, relationship. ----
  let left = SidePlan::compile(&query.pattern.left)?;
  let right = match (&query.pattern.rel, &query.pattern.right) {
    (Some(_), Some(node)) => Some(SidePlan::compile(node)?),
    (None, None) => None,
    _ => {
      return Err(QueryError::Plan(
        "a relationship needs nodes on both sides (and vice versa)".into(),
      ));
    }
  };
  if let (Some(l), Some(r)) = (&left.var, right.as_ref().and_then(|r| r.var.as_ref())) {
    if l == r.as_str() {
      return Err(QueryError::Plan(format!("variable '{l}' is bound twice")));
    }
  }
  let side_of = |var: &str| -> Result<usize, QueryError> {
    if left.var.as_deref() == Some(var) {
      Ok(0)
    } else if right.as_ref().and_then(|r| r.var.as_deref()) == Some(var) {
      Ok(1)
    } else {
      Err(QueryError::Plan(format!("variable '{var}' is not bound in MATCH")))
    }
  };

  let rel = match &query.pattern.rel {
    Some(rel) => {
      let mut bases: Vec<u16> = Vec::new();
      for name in &rel.types {
        let edge = EdgeType::from_name(name).ok_or_else(|| {
          QueryError::Plan(format!(
            "unknown relation '{name}' — `vorpal graph schema` lists this index's relations"
          ))
        })?;
        if !bases.contains(&edge.base().0) {
          bases.push(edge.base().0);
        }
      }
      let range = match rel.range {
        None => None,
        Some((min, max)) => {
          if min == 0 {
            return Err(QueryError::Plan("a range minimum of 0 is not supported (paths have at least one hop)".into()));
          }
          if min > max {
            return Err(QueryError::Plan(format!("empty range *{min}..{max}")));
          }
          if max > MAX_DEPTH {
            return Err(QueryError::Ceiling {
              what: "depth",
              limit: MAX_DEPTH as u64,
            });
          }
          Some((min, max))
        }
      };
      Some((bases, rel.direction, range, min_confidence_for_grade(rel.grade.as_deref())?))
    }
    None => None,
  };

  // ---- Plan: predicates, projections, order, aggregation. ----
  fn bind_pred(
    expr: &PredExpr,
    side_of: &impl Fn(&str) -> Result<usize, QueryError>,
  ) -> Result<BoundPred, QueryError> {
    Ok(match expr {
      PredExpr::Cmp(pred) => {
        check_prop(&pred.target.prop)?;
        check_predicate_types(&pred.target.prop, pred.op, &pred.value)?;
        BoundPred::Cmp(BoundPredicate {
          side: side_of(&pred.target.var)?,
          prop: pred.target.prop.clone(),
          op: pred.op,
          value: pred.value.clone(),
        })
      }
      PredExpr::And(terms) => BoundPred::And(
        terms.iter().map(|t| bind_pred(t, side_of)).collect::<Result<_, _>>()?,
      ),
      PredExpr::Or(terms) => BoundPred::Or(
        terms.iter().map(|t| bind_pred(t, side_of)).collect::<Result<_, _>>()?,
      ),
      PredExpr::Not(inner) => BoundPred::Not(Box::new(bind_pred(inner, side_of)?)),
    })
  }
  let predicate: Option<BoundPred> =
    query.predicate.as_ref().map(|p| bind_pred(p, &side_of)).transpose()?;

  let expand_projection =
    |proj: &Projection, out: &mut Vec<BoundColumn>| -> Result<(), QueryError> {
      match &proj.expr {
        ProjExpr::Var { var } => {
          let side = side_of(var)?;
          if let Some(alias) = &proj.alias {
            return Err(QueryError::Plan(format!(
              "a bare variable expands to {var}.id/name/kind/path and cannot take AS {alias}"
            )));
          }
          for prop in ["id", "name", "kind", "path"] {
            out.push(BoundColumn {
              title: format!("{var}.{prop}"),
              side,
              prop: prop.to_string(),
            });
          }
        }
        ProjExpr::Prop { var, prop } => {
          check_prop(prop)?;
          out.push(BoundColumn {
            title: proj.alias.clone().unwrap_or_else(|| format!("{var}.{prop}")),
            side: side_of(var)?,
            prop: prop.clone(),
          });
        }
      }
      Ok(())
    };

  enum Consumer {
    Rows(Vec<BoundColumn>),
    Count {
      distinct: Option<(usize, String)>,
      group: Option<BoundColumn>,
    },
  }
  let consumer = match &query.returns {
    Returns::Rows(projections) => {
      if projections.is_empty() {
        return Err(QueryError::Plan("RETURN needs at least one projection".into()));
      }
      let mut columns = Vec::new();
      for proj in projections {
        expand_projection(proj, &mut columns)?;
      }
      Consumer::Rows(columns)
    }
    Returns::Count { distinct, group } => {
      let distinct = match distinct {
        Some(prop_ref) => {
          check_prop(&prop_ref.prop)?;
          Some((side_of(&prop_ref.var)?, prop_ref.prop.clone()))
        }
        None => None,
      };
      let group = match group {
        Some(proj) => {
          let mut cols = Vec::new();
          expand_projection(proj, &mut cols)?;
          if cols.len() != 1 {
            return Err(QueryError::Plan(
              "the grouping key must be a single var.prop (bare variables expand to four columns)"
                .into(),
            ));
          }
          cols.pop()
        }
        None => None,
      };
      Consumer::Count { distinct, group }
    }
  };

  // ---- Materialize bindings: (left id, right id) pairs, deterministic order. ----
  // A pure ungrouped COUNT(*) streams through a counter instead — the rows ceiling
  // protects materialized memory, and a scalar count materializes nothing (the edge-visit
  // budget still bounds its traversal work).
  let pure_count =
    matches!(&consumer, Consumer::Count { distinct: None, group: None });
  let mut count_only: u64 = 0;
  let mut bindings: Vec<(u32, u32)> = Vec::new(); // right == u32::MAX when unbound
  const NO_NODE: u32 = u32::MAX;
  fn emit(
    pure_count: bool,
    count_only: &mut u64,
    bindings: &mut Vec<(u32, u32)>,
    l: u32,
    r: u32,
  ) -> Result<(), QueryError> {
    if pure_count {
      *count_only += 1;
      return Ok(());
    }
    bindings.push((l, r));
    if bindings.len() > MAX_ROWS {
      return Err(QueryError::Ceiling {
        what: "rows",
        limit: MAX_ROWS as u64,
      });
    }
    Ok(())
  }
  let passes_where = |kg: &Kg, l: u32, r: u32| -> bool {
    predicate.as_ref().is_none_or(|p| p.eval(kg, l, r))
  };

  match (&rel, &right) {
    (None, _) => {
      // WHERE evaluates in parallel over the candidate set (a full-scan predicate like
      // `in_degree >= 500` touches every node's view — serial, that was ~1s at kernel
      // scale; the rayon filter preserves encounter order, so rows stay deterministic).
      let candidates = left.candidates(kg);
      let survivors: Vec<u32> = if predicate.is_none() {
        candidates
      } else {
        use rayon::prelude::*;
        candidates
          .into_par_iter()
          .filter(|&id| passes_where(kg, id, NO_NODE))
          .collect()
      };
      for id in survivors {
        emit(pure_count, &mut count_only, &mut bindings, id, NO_NODE)?;
      }
    }
    (Some((bases, direction, range, min_conf)), Some(right_plan)) => {
      // Anchor the cheaper side; expansion legs derive from direction and anchor side.
      let anchor_left = left.cost() <= right_plan.cost();
      let (anchor, other) = if anchor_left { (&left, right_plan) } else { (right_plan, &left) };
      let seeds = anchor.candidates(kg);
      // Which adjacency legs to walk from the anchor: Out means the edge points
      // left → right, so an anchor on the right follows In-edges to find lefts.
      let (walk_out, walk_in) = match (direction, anchor_left) {
        (RelDirection::Out, true) | (RelDirection::In, false) => (true, false),
        (RelDirection::Out, false) | (RelDirection::In, true) => (false, true),
        (RelDirection::Both, _) => (true, true),
      };
      let graph = kg.graph();
      let mut budget: u64 = MAX_EDGE_VISITS;
      let mut reached: Vec<u32> = Vec::new();
      for &seed in &seeds {
        reached.clear();
        match range {
          None => one_hop(graph, seed, bases, *min_conf, walk_out, walk_in, &mut budget, &mut reached)?,
          Some((min, max)) => bounded_bfs(
            graph, seed, bases, *min_conf, walk_out, walk_in, *min, *max, &mut budget,
            &mut reached,
          )?,
        }
        reached.sort_unstable();
        reached.dedup();
        for &node in reached.iter() {
          let Some(view) = kg.node(NodeId::new(node as u64)) else {
            continue;
          };
          if !other.matches(node, &view) {
            continue;
          }
          let (l, r) = if anchor_left { (seed, node) } else { (node, seed) };
          if !passes_where(kg, l, r) {
            continue;
          }
          // Pairs are unique by construction (distinct seeds × per-seed deduped reach),
          // so the streaming counter and the materialized path agree exactly.
          emit(pure_count, &mut count_only, &mut bindings, l, r)?;
        }
      }
      bindings.sort_unstable();
      bindings.dedup();
    }
    _ => {
      return Err(QueryError::Plan(
        "a relationship needs nodes on both sides (and vice versa)".into(),
      ));
    }
  }

  // ---- Consume: project or aggregate; then ORDER BY / SKIP / LIMIT. ----
  let (columns, mut rows): (Vec<String>, Vec<Vec<Cell>>) = match consumer {
    Consumer::Rows(cols) => {
      let titles: Vec<String> = cols.iter().map(|c| c.title.clone()).collect();
      let rows = bindings
        .iter()
        .map(|&(l, r)| {
          cols
            .iter()
            .map(|c| {
              let id = if c.side == 0 { l } else { r };
              if id == NO_NODE { Cell::Null } else { prop_cell(kg, id, &c.prop) }
            })
            .collect()
        })
        .collect();
      (titles, rows)
    }
    Consumer::Count { distinct, group } => match group {
      None => {
        let count = match distinct {
          None => count_only,
          Some((side, prop)) => {
            let mut seen: HashSet<Cell> = HashSet::new();
            for &(l, r) in &bindings {
              let id = if side == 0 { l } else { r };
              if id != NO_NODE {
                seen.insert(prop_cell(kg, id, &prop));
              }
            }
            seen.len() as u64
          }
        };
        (vec!["count".to_string()], vec![vec![Cell::Int(count)]])
      }
      Some(key) => {
        let mut groups: HashMap<Cell, (u64, HashSet<Cell>)> = HashMap::new();
        for &(l, r) in &bindings {
          let key_id = if key.side == 0 { l } else { r };
          let key_cell =
            if key_id == NO_NODE { Cell::Null } else { prop_cell(kg, key_id, &key.prop) };
          let entry = groups.entry(key_cell).or_default();
          entry.0 += 1;
          if let Some((side, prop)) = &distinct {
            let id = if *side == 0 { l } else { r };
            if id != NO_NODE {
              entry.1.insert(prop_cell(kg, id, prop));
            }
          }
        }
        let mut rows: Vec<Vec<Cell>> = groups
          .into_iter()
          .map(|(key_cell, (count, distinct_cells))| {
            let n = if distinct.is_some() { distinct_cells.len() as u64 } else { count };
            vec![key_cell, Cell::Int(n)]
          })
          .collect();
        rows.sort_unstable();
        (vec![key.title, "count".to_string()], rows)
      }
    },
  };

  // ORDER BY: keys must name returned columns.
  if !query.order_by.is_empty() {
    let mut key_indices = Vec::new();
    for ordering in &query.order_by {
      let index = columns.iter().position(|c| c == &ordering.key).ok_or_else(|| {
        QueryError::Plan(format!(
          "ORDER BY '{}' does not name a returned column ({})",
          ordering.key,
          columns.join(", ")
        ))
      })?;
      key_indices.push((index, ordering.descending));
    }
    rows.sort_by(|a, b| {
      for &(index, descending) in &key_indices {
        let ord = a[index].cmp(&b[index]);
        if ord != std::cmp::Ordering::Equal {
          return if descending { ord.reverse() } else { ord };
        }
      }
      a.cmp(b) // total tie-break keeps the order a pure function of the row set
    });
  }

  let total_rows = rows.len() as u64;
  let skip = query.skip.unwrap_or(0) as usize;
  let rows: Vec<Vec<Cell>> = rows
    .into_iter()
    .skip(skip)
    .take(query.limit.map(|l| l as usize).unwrap_or(usize::MAX))
    .collect();
  Ok(QueryResult {
    columns,
    rows,
    total_rows,
  })
}

/// Walk one adjacency ring of `seed`, appending admitted neighbors. Every scanned edge
/// costs one unit of `budget`.
#[allow(clippy::too_many_arguments)]
fn one_hop(
  graph: &vorpal_graph::Graph,
  seed: u32,
  bases: &[u16],
  min_conf: u8,
  walk_out: bool,
  walk_in: bool,
  budget: &mut u64,
  reached: &mut Vec<u32>,
) -> Result<(), QueryError> {
  if seed as usize >= graph.node_count() {
    return Ok(());
  }
  let legs = [
    walk_out.then(|| (graph.out_targets(seed), graph.out_edge_types(seed))),
    walk_in.then(|| (graph.in_targets(seed), graph.in_edge_types(seed))),
  ];
  for (targets, types) in legs.into_iter().flatten() {
    for (&v, &et) in targets.iter().zip(types) {
      if *budget == 0 {
        return Err(QueryError::Ceiling {
          what: "edge visits",
          limit: MAX_EDGE_VISITS,
        });
      }
      *budget -= 1;
      let edge = EdgeType(et);
      if (bases.is_empty() || bases.contains(&edge.base().0)) && edge.confidence() >= min_conf {
        reached.push(v);
      }
    }
  }
  Ok(())
}

/// Per-seed BFS bounded by depth and the shared edge budget; nodes first reached at depths
/// in `[min, max]` are appended (the seed itself is never reported — cycles back to the
/// seed are not rows in v1).
#[allow(clippy::too_many_arguments)]
fn bounded_bfs(
  graph: &vorpal_graph::Graph,
  seed: u32,
  bases: &[u16],
  min_conf: u8,
  walk_out: bool,
  walk_in: bool,
  min: u32,
  max: u32,
  budget: &mut u64,
  reached: &mut Vec<u32>,
) -> Result<(), QueryError> {
  if seed as usize >= graph.node_count() {
    return Ok(());
  }
  let mut visited: HashSet<u32> = HashSet::new();
  visited.insert(seed);
  let mut frontier = vec![seed];
  let mut depth = 0u32;
  while !frontier.is_empty() && depth < max {
    let mut next = Vec::new();
    for &u in &frontier {
      let legs = [
        walk_out.then(|| (graph.out_targets(u), graph.out_edge_types(u))),
        walk_in.then(|| (graph.in_targets(u), graph.in_edge_types(u))),
      ];
      for (targets, types) in legs.into_iter().flatten() {
        for (&v, &et) in targets.iter().zip(types) {
          if *budget == 0 {
            return Err(QueryError::Ceiling {
              what: "edge visits",
              limit: MAX_EDGE_VISITS,
            });
          }
          *budget -= 1;
          let edge = EdgeType(et);
          if !(bases.is_empty() || bases.contains(&edge.base().0)) || edge.confidence() < min_conf
          {
            continue;
          }
          if visited.insert(v) {
            if depth + 1 >= min {
              reached.push(v);
            }
            next.push(v);
          }
        }
      }
    }
    frontier = next;
    depth += 1;
  }
  Ok(())
}
