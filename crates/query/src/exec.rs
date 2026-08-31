//! Planner + executor: validated IR → rows, on the existing graph primitives.
//!
//! Shape: the MATCH engine picks the cheaper end of the pattern chain as the anchor
//! (id/eid, then name via `names.idx`, then a kind-column scan), expands each relationship
//! (one-hop adjacency walk or a per-seed bounded BFS), and filters with WHERE — producing a
//! table of pattern nodes. `WITH` / `UNWIND` stages then project, aggregate, filter, and
//! expand that table; `RETURN` projects the final columns; `UNION` concatenates. Every
//! stage runs under the crate ceilings — work counts, never wall time — and exceeding one
//! is a typed error naming it, never a silently truncated answer.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering as AtomicOrdering};

use vorpal_kg::{EdgeType, Kg, NodeId, NodeView, SymbolKind, SymbolSelector};

use crate::expr::{
  AggState, Cell, Env, ExprType, Position, RowAccess, Scope, check_expr, check_pred, eval,
  eval_pred, render_expr,
};
use crate::ir::*;
use crate::{MAX_DEPTH, MAX_EDGE_VISITS, MAX_ROWS, QueryError, QueryResult};

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

/// A node pattern compiled against the open graph: exact facets extracted, kinds parsed.
#[derive(Default, Clone)]
struct SidePlan {
  var: Option<String>,
  kinds: Vec<SymbolKind>,
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
    for kind in &node.kinds {
      plan.kinds.push(SymbolKind::parse(kind).ok_or_else(|| {
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
    } else if !self.kinds.is_empty() {
      2
    } else {
      3
    }
  }

  fn matches(&self, id: u32, view: &NodeView<'_>) -> bool {
    self.id.is_none_or(|want| want == id as u64)
      && self.eid.is_none_or(|want| view.external_id == Some(want))
      && (self.kinds.is_empty() || self.kinds.contains(&view.kind))
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
        kind: if self.kinds.len() == 1 { Some(self.kinds[0]) } else { None },
        external_id: None,
      };
      return kg
        .select(&selector)
        .into_iter()
        .map(|n| n.raw() as u32)
        .filter(|&id| kg.node(NodeId::new(id as u64)).is_some_and(|v| self.matches(id, &v)))
        .collect();
    }
    use rayon::prelude::*;
    let count = kg.node_count() as u32;
    // Kind-column prefilter when the slabs carry the dense tag column: contiguous stripes
    // in id order (one for flat graphs, one per bucket for bucketed generations).
    if let (false, Some(stripes)) = (self.kinds.is_empty(), kg.kind_tag_stripes()) {
      let wanted: Vec<u8> = self.kinds.iter().map(|k| k.tag()).collect();
      let wanted = &wanted;
      // Nested parallel iterators flatten without materializing candidates; indexed
      // combinators keep ascending-id order, same as the flat scan this replaces.
      return stripes
        .into_par_iter()
        .flat_map(|(base, tags)| {
          (0..tags.len())
            .into_par_iter()
            .filter(move |&row| wanted.contains(&tags[row]))
            .map(move |row| (base + row as u64) as u32)
        })
        .filter(|&id| kg.node(NodeId::new(id as u64)).is_some_and(|v| self.matches(id, &v)))
        .collect();
    }
    (0..count)
      .into_par_iter()
      .filter(|&id| kg.node(NodeId::new(id as u64)).is_some_and(|v| self.matches(id, &v)))
      .collect()
  }
}

/// One compiled relationship: allowed bases, direction, var-length range, grade floor.
struct CompiledRel {
  bases: Vec<u16>,
  direction: RelDirection,
  range: Option<(u32, u32)>,
  min_conf: u8,
}

impl CompiledRel {
  fn compile(rel: &RelPattern) -> Result<Self, QueryError> {
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
          return Err(QueryError::Plan(
            "a range minimum of 0 is not supported (paths have at least one hop)".into(),
          ));
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
    Ok(CompiledRel {
      bases,
      direction: rel.direction,
      range,
      min_conf: min_confidence_for_grade(rel.grade.as_deref())?,
    })
  }

  /// Which adjacency legs to walk when this segment is traversed from `from_slot` to
  /// `to_slot`: the authored direction reads left→right along the chain; a reversed
  /// walk flips it.
  fn legs(&self, forward: bool) -> (bool, bool) {
    match (self.direction, forward) {
      (RelDirection::Out, true) | (RelDirection::In, false) => (true, false),
      (RelDirection::Out, false) | (RelDirection::In, true) => (false, true),
      (RelDirection::Both, _) => (true, true),
    }
  }
}

/// A compiled `EXISTS { … }` probe: the start variable's column, then segments.
struct CompiledExists {
  pattern: Pattern,
  start_col: usize,
  rels: Vec<CompiledRel>,
  nodes: Vec<SidePlan>,
}

/// The shared traversal budget: every edge scanned by the engine or an EXISTS probe
/// costs one unit; exhaustion is a typed ceiling, never a partial answer.
struct Budget {
  left: AtomicU64,
  exhausted: AtomicBool,
}

impl Budget {
  fn new() -> Self {
    Self {
      left: AtomicU64::new(MAX_EDGE_VISITS),
      exhausted: AtomicBool::new(false),
    }
  }

  /// Spend one unit; `false` when the budget is gone (the flag is sticky).
  #[inline]
  fn spend(&self) -> bool {
    let prev = self.left.fetch_sub(1, AtomicOrdering::Relaxed);
    if prev == 0 {
      self.left.store(0, AtomicOrdering::Relaxed);
      self.exhausted.store(true, AtomicOrdering::Relaxed);
      false
    } else {
      true
    }
  }

  fn check(&self) -> Result<(), QueryError> {
    if self.exhausted.load(AtomicOrdering::Relaxed) {
      Err(QueryError::Ceiling {
        what: "edge visits",
        limit: MAX_EDGE_VISITS,
      })
    } else {
      Ok(())
    }
  }
}

/// A table of cells with a named, typed scope — what flows between clauses.
struct Table {
  scope: Scope,
  rows: Vec<Vec<Cell>>,
}

impl Table {
  fn check_rows(&self) -> Result<(), QueryError> {
    if self.rows.len() > MAX_ROWS {
      Err(QueryError::Ceiling {
        what: "rows",
        limit: MAX_ROWS as u64,
      })
    } else {
      Ok(())
    }
  }
}

pub(crate) fn execute(kg: &Kg, query: &Query) -> Result<QueryResult, QueryError> {
  let mut result = execute_one(kg, query)?;
  if let Some(tail) = &query.union {
    let more = execute_one(kg, &tail.query)?;
    if more.columns.len() != result.columns.len() {
      return Err(QueryError::Plan(format!(
        "UNION arms return {} and {} columns — they must match",
        result.columns.len(),
        more.columns.len()
      )));
    }
    result.rows.extend(more.rows);
    if !tail.all {
      let mut seen: HashSet<Vec<Cell>> = HashSet::new();
      result.rows.retain(|row| seen.insert(row.clone()));
    }
    result.total_rows = result.rows.len() as u64;
    if result.rows.len() > MAX_ROWS {
      return Err(QueryError::Ceiling {
        what: "rows",
        limit: MAX_ROWS as u64,
      });
    }
  }
  Ok(result)
}

fn execute_one(kg: &Kg, query: &Query) -> Result<QueryResult, QueryError> {
  let budget = Budget::new();
  let mut regexes: HashMap<String, regex::Regex> = HashMap::new();

  // ---- Plan the pattern: one slot per node, unnamed slots get unreferenceable names. ----
  let mut nodes: Vec<SidePlan> = vec![SidePlan::compile(&query.pattern.left)?];
  for segment in &query.pattern.segments {
    nodes.push(SidePlan::compile(&segment.node)?);
  }
  let mut slot_scope = Scope::default();
  for (index, node) in nodes.iter().enumerate() {
    let name = match &node.var {
      Some(var) => {
        if slot_scope.index_of(var).is_some() {
          return Err(QueryError::Plan(format!(
            "variable '{var}' is bound twice — cycle constraints are not supported"
          )));
        }
        var.clone()
      }
      None => format!("\u{0}slot{index}"),
    };
    slot_scope.cols.push((name, ExprType::Node));
  }
  let rels: Vec<CompiledRel> = query
    .pattern
    .segments
    .iter()
    .map(|s| CompiledRel::compile(&s.rel))
    .collect::<Result<_, _>>()?;

  // ---- Plan WHERE (incl. EXISTS probes) against the slot scope. ----
  let mut probes: Vec<CompiledExists> = Vec::new();
  if let Some(pred) = &query.predicate {
    check_pred(pred, &slot_scope, &mut regexes)?;
    collect_exists(pred, &slot_scope, &mut probes)?;
  }
  let stride = nodes.len();

  // A pure `RETURN count(*)` with no stages streams through a counter: the rows ceiling
  // protects materialized memory, and a scalar count materializes nothing.
  let pure_count = query.stages.is_empty()
    && !query.returns.distinct
    && query.order_by.is_empty()
    && query.returns.items.len() == 1
    && matches!(
      &query.returns.items[0].expr,
      Expr::Agg {
        func: AggFn::Count,
        arg: None,
        ..
      }
    );

  // ---- MATCH engine. ----
  let exists_probe = |row: &dyn RowAccess, pattern: &Pattern| -> bool {
    probes
      .iter()
      .find(|p| p.pattern == *pattern)
      .is_some_and(|probe| match row.cell(probe.start_col) {
        Cell::Node(seed) => exists_from(kg, seed, probe, &budget),
        _ => false,
      })
  };
  let env = Env {
    kg,
    scope: &slot_scope,
    aggs: &[],
    agg_values: None,
    regexes: &regexes,
    exists: &exists_probe,
  };
  let passes_where = |row: &[u32]| -> bool {
    query
      .predicate
      .as_ref()
      .is_none_or(|p| eval_pred(p, &env, &row))
  };

  let mut count_only: u64 = 0;
  let mut rows_flat: Vec<u32> = Vec::new();
  const NO_NODE: u32 = u32::MAX;

  if rels.is_empty() {
    // Single node: candidates + parallel WHERE (a full-scan predicate like
    // `in_degree >= 500` was ~1s serial at kernel scale; the order-preserving rayon
    // filter answers in ~10ms).
    let candidates = nodes[0].candidates(kg);
    let survivors: Vec<u32> = if query.predicate.is_none() {
      candidates
    } else {
      use rayon::prelude::*;
      candidates
        .into_par_iter()
        .filter(|&id| passes_where(&[id]))
        .collect()
    };
    budget.check()?;
    if pure_count {
      count_only = survivors.len() as u64;
    } else if survivors.len() > MAX_ROWS {
      return Err(QueryError::Ceiling {
        what: "rows",
        limit: MAX_ROWS as u64,
      });
    } else {
      rows_flat = survivors;
    }
  } else {
    // Anchor at the cheaper END of the chain; when anchored right, the chain is walked in
    // reverse with each segment's legs flipped. (Middle anchoring is a future planner.)
    let last = nodes.len() - 1;
    let anchor_left = nodes[0].cost() <= nodes[last].cost();
    let steps: Vec<(usize, usize, usize)> = if anchor_left {
      (0..rels.len()).map(|i| (i, i, i + 1)).collect()
    } else {
      (0..rels.len()).rev().map(|i| (i, i + 1, i)).collect()
    };
    let anchor_slot = if anchor_left { 0 } else { last };
    let graph = kg.graph();

    let mut partial: Vec<u32> = Vec::new();
    for id in nodes[anchor_slot].candidates(kg) {
      let base = partial.len();
      partial.resize(base + stride, NO_NODE);
      partial[base + anchor_slot] = id;
    }
    let mut reached: Vec<u32> = Vec::new();
    let total_steps = steps.len();
    for (step_index, &(rel_index, from_slot, to_slot)) in steps.iter().enumerate() {
      let rel = &rels[rel_index];
      let final_step = step_index + 1 == total_steps;
      let (walk_out, walk_in) = rel.legs(from_slot < to_slot);
      let target_plan = &nodes[to_slot];
      let mut next: Vec<u32> = Vec::new();
      for row in partial.chunks_exact(stride) {
        let seed = row[from_slot];
        reached.clear();
        expand(graph, seed, rel, walk_out, walk_in, &budget, &mut reached)?;
        reached.sort_unstable();
        reached.dedup();
        for &node in reached.iter() {
          let Some(view) = kg.node(NodeId::new(node as u64)) else {
            continue;
          };
          if !target_plan.matches(node, &view) {
            continue;
          }
          let base = next.len();
          next.extend_from_slice(row);
          next[base + to_slot] = node;
          if final_step {
            // The row is complete: WHERE, then stream (pure count) or keep.
            let complete = &next[base..base + stride];
            if !passes_where(complete) {
              next.truncate(base);
              continue;
            }
            budget.check()?;
            if pure_count {
              count_only += 1;
              next.truncate(base);
              continue;
            }
          }
          if next.len() / stride > MAX_ROWS {
            return Err(QueryError::Ceiling {
              what: "rows",
              limit: MAX_ROWS as u64,
            });
          }
        }
      }
      partial = next;
      if partial.is_empty() {
        break;
      }
    }
    rows_flat = partial;
    // Deterministic output order regardless of anchoring: rows sort lexicographically by
    // their slot ids (construction order already is a pure function of the graph; the sort
    // makes the contract independent of the planner's anchor choice).
    if !pure_count && stride > 0 && !rows_flat.is_empty() {
      let mut order: Vec<usize> = (0..rows_flat.len() / stride).collect();
      order.sort_by_key(|&row| &rows_flat[row * stride..(row + 1) * stride]);
      let mut sorted = Vec::with_capacity(rows_flat.len());
      for row in order {
        sorted.extend_from_slice(&rows_flat[row * stride..(row + 1) * stride]);
      }
      rows_flat = sorted;
    }
  }

  if pure_count {
    let title = query.returns.items[0]
      .alias
      .clone()
      .unwrap_or_else(|| "count".to_string());
    return Ok(QueryResult {
      columns: vec![title],
      rows: vec![vec![Cell::Int(count_only as i64)]],
      total_rows: 1,
    });
  }

  // ---- Table of pattern nodes: named slots only. ----
  let named: Vec<(usize, String)> = slot_scope
    .cols
    .iter()
    .enumerate()
    .filter(|(_, (name, _))| !name.starts_with('\u{0}'))
    .map(|(i, (name, _))| (i, name.clone()))
    .collect();
  let mut table = Table {
    scope: Scope {
      cols: named.iter().map(|(_, n)| (n.clone(), ExprType::Node)).collect(),
    },
    rows: rows_flat
      .chunks_exact(stride.max(1))
      .map(|row| {
        named
          .iter()
          .map(|(slot, _)| match row.get(*slot) {
            Some(&id) if id != NO_NODE => Cell::Node(id),
            _ => Cell::Null,
          })
          .collect()
      })
      .collect(),
  };

  // ---- Stages. ----
  for stage in &query.stages {
    table = match stage {
      Stage::With {
        distinct,
        items,
        predicate,
        order_by,
        skip,
        limit,
      } => {
        let mut projected =
          project(kg, &table, items, *distinct, order_by, false, &regexes, &probes, &budget)?;
        if let Some(pred) = predicate {
          check_pred(pred, &projected.scope, &mut regexes)?;
          let mut with_probes = Vec::new();
          collect_exists(pred, &projected.scope, &mut with_probes)?;
          let probe = |row: &dyn RowAccess, pattern: &Pattern| -> bool {
            with_probes
              .iter()
              .find(|p| p.pattern == *pattern)
              .is_some_and(|probe| match row.cell(probe.start_col) {
                Cell::Node(seed) => exists_from(kg, seed, probe, &budget),
                _ => false,
              })
          };
          let env = Env {
            kg,
            scope: &projected.scope,
            aggs: &[],
            agg_values: None,
            regexes: &regexes,
            exists: &probe,
          };
          projected.rows.retain(|row| eval_pred(pred, &env, &row.as_slice()));
          budget.check()?;
        }
        apply_skip_limit(&mut projected.rows, *skip, *limit);
        projected
      }
      Stage::Unwind { expr, alias } => {
        let mut aggs = Vec::new();
        check_expr(expr, &table.scope, Position::Predicate, &mut aggs, false)?;
        if table.scope.index_of(alias).is_some() {
          return Err(QueryError::Plan(format!(
            "UNWIND alias '{alias}' shadows an existing variable"
          )));
        }
        let never = |_: &dyn RowAccess, _: &Pattern| false;
        let env = Env {
          kg,
          scope: &table.scope,
          aggs: &[],
          agg_values: None,
          regexes: &regexes,
          exists: &never,
        };
        let mut rows = Vec::new();
        for row in &table.rows {
          let items = match eval(expr, &env, &row.as_slice()) {
            Cell::List(items) => items,
            Cell::Null => Vec::new(),
            single => vec![single],
          };
          for item in items {
            let mut out = row.clone();
            out.push(item);
            rows.push(out);
            if rows.len() > MAX_ROWS {
              return Err(QueryError::Ceiling {
                what: "rows",
                limit: MAX_ROWS as u64,
              });
            }
          }
        }
        let mut scope = table.scope.clone();
        scope.cols.push((alias.clone(), ExprType::Any));
        Table { scope, rows }
      }
    };
    table.check_rows()?;
  }

  // ---- RETURN. ----
  let projected = project(
    kg,
    &table,
    &query.returns.items,
    query.returns.distinct,
    &query.order_by,
    true,
    &regexes,
    &probes,
    &budget,
  )?;
  let total_rows = projected.rows.len() as u64;
  let mut rows = projected.rows;
  apply_skip_limit(&mut rows, query.skip, query.limit);
  Ok(QueryResult {
    columns: projected.scope.cols.into_iter().map(|(n, _)| n).collect(),
    rows,
    total_rows,
  })
}

fn apply_skip_limit(rows: &mut Vec<Vec<Cell>>, skip: Option<u64>, limit: Option<u64>) {
  let skip = skip.unwrap_or(0) as usize;
  if skip > 0 {
    if skip >= rows.len() {
      rows.clear();
    } else {
      rows.drain(..skip);
    }
  }
  if let Some(limit) = limit {
    rows.truncate(limit as usize);
  }
}

/// Project (and, when any item aggregates, group) `table` into a new table. `expand_nodes`
/// is the RETURN convention: a bare pattern node becomes its four identity columns; in
/// WITH the node stays a node column so later clauses can keep reading it.
#[allow(clippy::too_many_arguments)]
fn project(
  kg: &Kg,
  table: &Table,
  items: &[Projection],
  distinct: bool,
  order_by: &[Ordering],
  expand_nodes: bool,
  regexes: &HashMap<String, regex::Regex>,
  probes: &[CompiledExists],
  budget: &Budget,
) -> Result<Table, QueryError> {
  if items.is_empty() {
    return Err(QueryError::Plan("RETURN needs at least one projection".into()));
  }
  // Expand bare node variables (RETURN only) and plan every column.
  struct Column {
    title: String,
    expr: Expr,
    ty: ExprType,
  }
  let mut columns: Vec<Column> = Vec::new();
  let mut aggs: Vec<Expr> = Vec::new();
  for item in items {
    if let Expr::Var { var } = &item.expr {
      let is_node = matches!(
        table.scope.cols.iter().find(|(n, _)| n == var),
        Some((_, ExprType::Node))
      );
      if is_node && expand_nodes {
        if let Some(alias) = &item.alias {
          return Err(QueryError::Plan(format!(
            "a bare variable expands to {var}.id/name/kind/path and cannot take AS {alias}"
          )));
        }
        for prop in ["id", "name", "kind", "path"] {
          let expr = Expr::Prop {
            var: var.clone(),
            prop: prop.to_string(),
          };
          let ty = check_expr(&expr, &table.scope, Position::Projection, &mut aggs, false)?.ty;
          columns.push(Column {
            title: format!("{var}.{prop}"),
            expr,
            ty,
          });
        }
        continue;
      }
    }
    let ty = check_expr(&item.expr, &table.scope, Position::Projection, &mut aggs, false)?.ty;
    let title = item.alias.clone().unwrap_or_else(|| render_expr(&item.expr));
    if contains_agg(&item.expr) && refs_outside_aggs(&item.expr) {
      return Err(QueryError::Plan(format!(
        "'{title}' mixes an aggregate with non-aggregated values — every non-aggregated \
         value must be its own returned item (the grouping key)"
      )));
    }
    if columns.iter().any(|c| c.title == title) {
      return Err(QueryError::Plan(format!(
        "column '{title}' is projected twice — alias one of them"
      )));
    }
    columns.push(Column {
      title,
      expr: item.expr.clone(),
      ty,
    });
  }
  let out_scope = Scope {
    cols: columns.iter().map(|c| (c.title.clone(), c.ty)).collect(),
  };

  let probe = |row: &dyn RowAccess, pattern: &Pattern| -> bool {
    probes
      .iter()
      .find(|p| p.pattern == *pattern)
      .is_some_and(|probe| match row.cell(probe.start_col) {
        Cell::Node(seed) => exists_from(kg, seed, probe, budget),
        _ => false,
      })
  };

  // ORDER BY keys resolve against the output columns first, then the input scope (for
  // non-aggregating projections, Cypher allows ordering by an unreturned expression).
  let merged_scope = Scope {
    cols: out_scope
      .cols
      .iter()
      .cloned()
      .chain(table.scope.cols.iter().cloned())
      .collect(),
  };
  let mut order_keys: Vec<(Expr, bool)> = Vec::new();
  for ordering in order_by {
    // A key spelled exactly like a returned column (alias or `var.prop` title) names that
    // column — Cypher's rule, and the only way to order an aggregated result.
    let rendered = render_expr(&ordering.key);
    let key = match out_scope.cols.iter().find(|(title, _)| *title == rendered) {
      Some((title, _)) => Expr::Var { var: title.clone() },
      None => ordering.key.clone(),
    };
    let mut no_aggs = Vec::new();
    let scope_for_check = if aggs.is_empty() { &merged_scope } else { &out_scope };
    check_expr(&key, scope_for_check, Position::Predicate, &mut no_aggs, false)
      .map_err(|err| match err {
        QueryError::Plan(message) if aggs.is_empty() => QueryError::Plan(message),
        QueryError::Plan(_) => QueryError::Plan(format!(
          "ORDER BY '{}' does not name a returned column ({})",
          render_expr(&ordering.key),
          out_scope
            .cols
            .iter()
            .map(|(n, _)| n.as_str())
            .collect::<Vec<_>>()
            .join(", ")
        )),
        other => other,
      })?;
    order_keys.push((key, ordering.descending));
  }

  let rows: Vec<Vec<Cell>> = if aggs.is_empty() {
    let env = Env {
      kg,
      scope: &table.scope,
      aggs: &[],
      agg_values: None,
      regexes,
      exists: &probe,
    };
    // Each output row carries its ORDER BY keys until the sort is done.
    let mut keyed: Vec<(Vec<Cell>, Vec<Cell>)> = Vec::with_capacity(table.rows.len());
    let merged_env = Env {
      kg,
      scope: &merged_scope,
      aggs: &[],
      agg_values: None,
      regexes,
      exists: &probe,
    };
    for row in &table.rows {
      let out: Vec<Cell> = columns
        .iter()
        .map(|c| eval(&c.expr, &env, &row.as_slice()))
        .collect();
      let keys: Vec<Cell> = if order_keys.is_empty() {
        Vec::new()
      } else {
        let merged: Vec<Cell> = out.iter().cloned().chain(row.iter().cloned()).collect();
        order_keys
          .iter()
          .map(|(k, _)| eval(k, &merged_env, &merged.as_slice()))
          .collect()
      };
      keyed.push((out, keys));
    }
    budget.check()?;
    if distinct {
      let mut seen: HashSet<Vec<Cell>> = HashSet::new();
      keyed.retain(|(out, _)| seen.insert(out.clone()));
    }
    sort_keyed(&mut keyed, &order_keys);
    keyed.into_iter().map(|(out, _)| out).collect()
  } else {
    // Implicit GROUP BY over the non-aggregate columns (Cypher semantics). Rows within a
    // group share key values; other columns evaluate against the group's first row.
    let key_columns: Vec<usize> = columns
      .iter()
      .enumerate()
      .filter(|(_, c)| !contains_agg(&c.expr))
      .map(|(i, _)| i)
      .collect();
    let env = Env {
      kg,
      scope: &table.scope,
      aggs: &[],
      agg_values: None,
      regexes,
      exists: &probe,
    };
    let mut groups: HashMap<Vec<Cell>, (usize, Vec<AggState>)> = HashMap::new();
    let mut group_order: Vec<Vec<Cell>> = Vec::new();
    for (row_index, row) in table.rows.iter().enumerate() {
      let key: Vec<Cell> = key_columns
        .iter()
        .map(|&i| eval(&columns[i].expr, &env, &row.as_slice()))
        .collect();
      let entry = groups.entry(key.clone()).or_insert_with(|| {
        group_order.push(key);
        (row_index, aggs.iter().map(AggState::new).collect())
      });
      for (state, agg) in entry.1.iter_mut().zip(&aggs) {
        let value = match agg {
          Expr::Agg { arg: Some(arg), .. } => Some(eval(arg, &env, &row.as_slice())),
          _ => None,
        };
        state.feed(value)?;
      }
      if groups.len() > MAX_ROWS {
        return Err(QueryError::Ceiling {
          what: "rows",
          limit: MAX_ROWS as u64,
        });
      }
    }
    budget.check()?;
    let mut keyed: Vec<(Vec<Cell>, Vec<Cell>)> = Vec::with_capacity(groups.len());
    for key in group_order {
      let Some((first_row, states)) = groups.remove(&key) else {
        continue;
      };
      let values: Vec<Cell> = states.into_iter().map(AggState::finish).collect();
      let group_env = Env {
        kg,
        scope: &table.scope,
        aggs: &aggs,
        agg_values: Some(&values),
        regexes,
        exists: &probe,
      };
      let first = &table.rows[first_row];
      let out: Vec<Cell> = columns
        .iter()
        .map(|c| eval(&c.expr, &group_env, &first.as_slice()))
        .collect();
      let out_env = Env {
        kg,
        scope: &out_scope,
        aggs: &[],
        agg_values: None,
        regexes,
        exists: &probe,
      };
      let keys: Vec<Cell> = order_keys
        .iter()
        .map(|(k, _)| eval(k, &out_env, &out.as_slice()))
        .collect();
      keyed.push((out, keys));
    }
    // Groups come out in first-seen order; without ORDER BY, sort by the output row so the
    // result is a pure function of the row set (hash order never leaks).
    if order_keys.is_empty() {
      keyed.sort_by(|a, b| a.0.cmp(&b.0));
    } else {
      sort_keyed(&mut keyed, &order_keys);
    }
    if distinct {
      let mut seen: HashSet<Vec<Cell>> = HashSet::new();
      keyed.retain(|(out, _)| seen.insert(out.clone()));
    }
    keyed.into_iter().map(|(out, _)| out).collect()
  };
  if rows.len() > MAX_ROWS {
    return Err(QueryError::Ceiling {
      what: "rows",
      limit: MAX_ROWS as u64,
    });
  }
  Ok(Table {
    scope: out_scope,
    rows,
  })
}

/// Sort output rows by their ORDER BY keys (stable, then a total tie-break on the row).
fn sort_keyed(keyed: &mut [(Vec<Cell>, Vec<Cell>)], order_keys: &[(Expr, bool)]) {
  if order_keys.is_empty() {
    return;
  }
  keyed.sort_by(|a, b| {
    for (index, (_, descending)) in order_keys.iter().enumerate() {
      let ord = a.1[index].cmp(&b.1[index]);
      if ord != std::cmp::Ordering::Equal {
        return if *descending { ord.reverse() } else { ord };
      }
    }
    a.0.cmp(&b.0)
  });
}

/// Does the expression read the scope anywhere OUTSIDE an aggregate's argument? Such a
/// read has no single value per group, so Cypher (and we) refuse it at plan time.
fn refs_outside_aggs(expr: &Expr) -> bool {
  match expr {
    Expr::Agg { .. } | Expr::Lit(_) | Expr::Null => false,
    Expr::Prop { .. } | Expr::Var { .. } => true,
    Expr::Pred(_) => true,
    Expr::List(items) => items.iter().any(refs_outside_aggs),
    Expr::Call { args, .. } => args.iter().any(refs_outside_aggs),
    Expr::Case {
      subject,
      whens,
      otherwise,
    } => {
      subject.as_deref().is_some_and(refs_outside_aggs)
        || whens
          .iter()
          .any(|(w, t)| refs_outside_aggs(w) || refs_outside_aggs(t))
        || otherwise.as_deref().is_some_and(refs_outside_aggs)
    }
    Expr::Binary { left, right, .. } => refs_outside_aggs(left) || refs_outside_aggs(right),
    Expr::Neg(inner) => refs_outside_aggs(inner),
  }
}

fn contains_agg(expr: &Expr) -> bool {
  match expr {
    Expr::Agg { .. } => true,
    Expr::Prop { .. } | Expr::Var { .. } | Expr::Lit(_) | Expr::Null | Expr::Pred(_) => false,
    Expr::List(items) => items.iter().any(contains_agg),
    Expr::Call { args, .. } => args.iter().any(contains_agg),
    Expr::Case {
      subject,
      whens,
      otherwise,
    } => {
      subject.as_deref().is_some_and(contains_agg)
        || whens.iter().any(|(w, t)| contains_agg(w) || contains_agg(t))
        || otherwise.as_deref().is_some_and(contains_agg)
    }
    Expr::Binary { left, right, .. } => contains_agg(left) || contains_agg(right),
    Expr::Neg(inner) => contains_agg(inner),
  }
}

/// Compile every `EXISTS { … }` in a predicate tree against `scope`.
fn collect_exists(
  pred: &PredExpr,
  scope: &Scope,
  out: &mut Vec<CompiledExists>,
) -> Result<(), QueryError> {
  match pred {
    PredExpr::Exists { pattern } => {
      let start = pattern.left.var.as_deref().unwrap_or_default();
      let start_col = scope
        .index_of(start)
        .ok_or_else(|| QueryError::Plan(format!("variable '{start}' is not bound in MATCH")))?;
      let rels = pattern
        .segments
        .iter()
        .map(|s| CompiledRel::compile(&s.rel))
        .collect::<Result<Vec<_>, _>>()?;
      let nodes = pattern
        .segments
        .iter()
        .map(|s| SidePlan::compile(&s.node))
        .collect::<Result<Vec<_>, _>>()?;
      out.push(CompiledExists {
        pattern: pattern.clone(),
        start_col,
        rels,
        nodes,
      });
    }
    PredExpr::And(terms) | PredExpr::Or(terms) => {
      for term in terms {
        collect_exists(term, scope, out)?;
      }
    }
    PredExpr::Not(inner) => collect_exists(inner, scope, out)?,
    PredExpr::Cmp { .. } | PredExpr::In { .. } | PredExpr::IsNull { .. } | PredExpr::HasLabel { .. } => {}
  }
  Ok(())
}

/// Does a path matching `probe`'s segments exist from `seed`? Depth-first with early exit;
/// every scanned edge spends the shared budget (exhaustion reads as "no", and the engine
/// raises the ceiling afterwards).
fn exists_from(kg: &Kg, seed: u32, probe: &CompiledExists, budget: &Budget) -> bool {
  fn step(kg: &Kg, at: u32, probe: &CompiledExists, index: usize, budget: &Budget) -> bool {
    if index == probe.rels.len() {
      return true;
    }
    let rel = &probe.rels[index];
    let (walk_out, walk_in) = rel.legs(true);
    let mut reached = Vec::new();
    if expand(kg.graph(), at, rel, walk_out, walk_in, budget, &mut reached).is_err() {
      return false;
    }
    reached.sort_unstable();
    reached.dedup();
    let target = &probe.nodes[index];
    reached.into_iter().any(|node| {
      kg.node(NodeId::new(node as u64))
        .is_some_and(|view| target.matches(node, &view))
        && step(kg, node, probe, index + 1, budget)
    })
  }
  step(kg, seed, probe, 0, budget)
}

/// Expand one relationship from `seed` — one adjacency ring, or a bounded BFS for a
/// var-length segment — appending admitted neighbors.
fn expand(
  graph: &vorpal_graph::Graph,
  seed: u32,
  rel: &CompiledRel,
  walk_out: bool,
  walk_in: bool,
  budget: &Budget,
  reached: &mut Vec<u32>,
) -> Result<(), QueryError> {
  match rel.range {
    None => one_hop(graph, seed, &rel.bases, rel.min_conf, walk_out, walk_in, budget, reached),
    Some((min, max)) => bounded_bfs(
      graph, seed, &rel.bases, rel.min_conf, walk_out, walk_in, min, max, budget, reached,
    ),
  }
}

/// Walk one adjacency ring of `seed`, appending admitted neighbors. Every scanned edge
/// costs one unit of the budget.
#[allow(clippy::too_many_arguments)]
fn one_hop(
  graph: &vorpal_graph::Graph,
  seed: u32,
  bases: &[u16],
  min_conf: u8,
  walk_out: bool,
  walk_in: bool,
  budget: &Budget,
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
      if !budget.spend() {
        return Err(QueryError::Ceiling {
          what: "edge visits",
          limit: MAX_EDGE_VISITS,
        });
      }
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
/// seed are not rows).
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
  budget: &Budget,
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
          if !budget.spend() {
            return Err(QueryError::Ceiling {
              what: "edge visits",
              limit: MAX_EDGE_VISITS,
            });
          }
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
