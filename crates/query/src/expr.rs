//! Values, scopes, plan-time checking, and evaluation of the expression language.
//!
//! Evaluation is total: an ill-typed operation yields `Null` (Cypher's convention) rather
//! than an error, because every *static* mistake — an unbound variable, an unknown
//! property or function, an aggregate where none is allowed, a comparison whose operand
//! types can never meet — was refused at plan time by [`check_expr`] / [`check_pred`].

use std::collections::HashSet;

use vorpal_kg::{Kg, NodeId, SymbolKind};

use crate::ir::*;
use crate::{MAX_ROWS, QueryError};

/// One result cell. Ordering (for ORDER BY and grouping keys): `Null < Bool < numbers <
/// Text < List < Node`, numbers comparing numerically across Int/Float.
#[derive(Debug, Clone)]
pub enum Cell {
  Null,
  Bool(bool),
  Int(i64),
  Float(f64),
  Text(String),
  List(Vec<Cell>),
  /// A pattern node, by dense id — projects as its id, reads properties by `.prop`.
  Node(u32),
}

impl Cell {
  fn rank(&self) -> u8 {
    match self {
      Cell::Null => 0,
      Cell::Bool(_) => 1,
      Cell::Int(_) | Cell::Float(_) => 2,
      Cell::Text(_) => 3,
      Cell::List(_) => 4,
      Cell::Node(_) => 5,
    }
  }

  fn as_f64(&self) -> Option<f64> {
    match self {
      Cell::Int(n) => Some(*n as f64),
      Cell::Float(f) => Some(*f),
      _ => None,
    }
  }

  pub fn is_null(&self) -> bool {
    matches!(self, Cell::Null)
  }
}

impl PartialEq for Cell {
  fn eq(&self, other: &Self) -> bool {
    match (self, other) {
      (Cell::Null, Cell::Null) => true,
      (Cell::Bool(a), Cell::Bool(b)) => a == b,
      (Cell::Int(a), Cell::Int(b)) => a == b,
      (Cell::Float(a), Cell::Float(b)) => a.to_bits() == b.to_bits(),
      (Cell::Int(a), Cell::Float(b)) | (Cell::Float(b), Cell::Int(a)) => (*a as f64) == *b,
      (Cell::Text(a), Cell::Text(b)) => a == b,
      (Cell::List(a), Cell::List(b)) => a == b,
      (Cell::Node(a), Cell::Node(b)) => a == b,
      _ => false,
    }
  }
}

impl Eq for Cell {}

impl std::hash::Hash for Cell {
  fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
    match self {
      Cell::Null => 0u8.hash(state),
      Cell::Bool(b) => {
        1u8.hash(state);
        b.hash(state)
      }
      // Ints and whole floats compare equal, so they must hash alike.
      Cell::Int(n) => {
        2u8.hash(state);
        (*n as f64).to_bits().hash(state)
      }
      Cell::Float(f) => {
        2u8.hash(state);
        f.to_bits().hash(state)
      }
      Cell::Text(t) => {
        3u8.hash(state);
        t.hash(state)
      }
      Cell::List(items) => {
        4u8.hash(state);
        items.hash(state)
      }
      Cell::Node(id) => {
        5u8.hash(state);
        id.hash(state)
      }
    }
  }
}

impl PartialOrd for Cell {
  fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
    Some(self.cmp(other))
  }
}

impl Ord for Cell {
  fn cmp(&self, other: &Self) -> std::cmp::Ordering {
    use std::cmp::Ordering;
    match (self, other) {
      (Cell::Bool(a), Cell::Bool(b)) => a.cmp(b),
      (Cell::Int(a), Cell::Int(b)) => a.cmp(b),
      (Cell::Text(a), Cell::Text(b)) => a.cmp(b),
      (Cell::List(a), Cell::List(b)) => a.cmp(b),
      (Cell::Node(a), Cell::Node(b)) => a.cmp(b),
      _ => match (self.as_f64(), other.as_f64()) {
        (Some(a), Some(b)) => a.total_cmp(&b),
        _ => self.rank().cmp(&other.rank()).then(Ordering::Equal),
      },
    }
  }
}

impl serde::Serialize for Cell {
  fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
    match self {
      Cell::Null => serializer.serialize_unit(),
      Cell::Bool(b) => serializer.serialize_bool(*b),
      Cell::Int(n) => serializer.serialize_i64(*n),
      Cell::Float(f) => serializer.serialize_f64(*f),
      Cell::Text(t) => serializer.serialize_str(t),
      Cell::List(items) => items.serialize(serializer),
      Cell::Node(id) => serializer.serialize_u64(*id as u64),
    }
  }
}

impl std::fmt::Display for Cell {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    match self {
      Cell::Null => write!(f, "∅"),
      Cell::Bool(b) => write!(f, "{b}"),
      Cell::Int(n) => write!(f, "{n}"),
      Cell::Float(x) => write!(f, "{x}"),
      Cell::Text(t) => write!(f, "{t}"),
      Cell::List(items) => {
        write!(f, "[")?;
        for (i, item) in items.iter().enumerate() {
          if i > 0 {
            write!(f, ", ")?;
          }
          write!(f, "{item}")?;
        }
        write!(f, "]")
      }
      Cell::Node(id) => write!(f, "{id}"),
    }
  }
}

/// The static type of an expression, as far as planning can tell.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExprType {
  Node,
  Int,
  Float,
  Text,
  Bool,
  List,
  Null,
  Any,
}

impl ExprType {
  fn is_numeric(self) -> bool {
    matches!(self, ExprType::Int | ExprType::Float | ExprType::Any | ExprType::Null)
  }
  fn is_text(self) -> bool {
    matches!(self, ExprType::Text | ExprType::Any | ExprType::Null)
  }
}

pub const PROPS: &[&str] = &[
  "id", "eid", "name", "path", "kind", "exported", "signature", "in_degree", "out_degree",
  "scc_size",
];

pub fn prop_type(prop: &str) -> ExprType {
  match prop {
    "id" | "in_degree" | "out_degree" | "scc_size" => ExprType::Int,
    "exported" => ExprType::Bool,
    _ => ExprType::Text,
  }
}

pub fn check_prop(prop: &str) -> Result<(), QueryError> {
  if PROPS.contains(&prop) {
    Ok(())
  } else {
    Err(QueryError::Plan(format!(
      "unknown property '{prop}' (available: {})",
      PROPS.join(", ")
    )))
  }
}

pub fn prop_cell(kg: &Kg, id: u32, prop: &str) -> Cell {
  let Some(view) = kg.node(NodeId::new(id as u64)) else {
    return Cell::Null;
  };
  match prop {
    "id" => Cell::Int(id as i64),
    "in_degree" => Cell::Int(kg.in_degree(NodeId::new(id as u64)) as i64),
    "out_degree" => Cell::Int(kg.out_degree(NodeId::new(id as u64)) as i64),
    "scc_size" => match kg.scc_size(NodeId::new(id as u64)) {
      Some(size) => Cell::Int(size as i64),
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

/// Named columns with their static types — the scope an expression is checked and
/// evaluated against.
#[derive(Debug, Clone, Default)]
pub struct Scope {
  pub cols: Vec<(String, ExprType)>,
}

impl Scope {
  pub fn index_of(&self, name: &str) -> Option<usize> {
    self.cols.iter().position(|(n, _)| n == name)
  }
}

/// Row access for evaluation: pattern rows are `u32` slots (nodes), table rows are cells.
pub trait RowAccess {
  fn cell(&self, col: usize) -> Cell;
}

impl RowAccess for &[u32] {
  fn cell(&self, col: usize) -> Cell {
    match self.get(col) {
      Some(&id) if id != u32::MAX => Cell::Node(id),
      _ => Cell::Null,
    }
  }
}

impl RowAccess for &[Cell] {
  fn cell(&self, col: usize) -> Cell {
    self.get(col).cloned().unwrap_or(Cell::Null)
  }
}

/// What planning produces for one expression: the type, plus the aggregates it contains
/// (in encounter order, deduplicated structurally) so the aggregation pass can accumulate
/// them and evaluation can substitute their values.
pub struct Checked {
  pub ty: ExprType,
}

/// Where an expression may sit — projections admit aggregates, predicates do not.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Position {
  Projection,
  Predicate,
}

/// Validate an expression against `scope`; aggregates are collected into `aggs`.
pub fn check_expr(
  expr: &Expr,
  scope: &Scope,
  position: Position,
  aggs: &mut Vec<Expr>,
  in_agg: bool,
) -> Result<Checked, QueryError> {
  check_expr_with(expr, scope, position, aggs, in_agg, &mut std::collections::HashMap::new())
}

/// [`check_expr`] collecting `=~` patterns found inside predicate-valued expressions.
pub fn check_expr_with(
  expr: &Expr,
  scope: &Scope,
  position: Position,
  aggs: &mut Vec<Expr>,
  in_agg: bool,
  regexes: &mut std::collections::HashMap<String, regex::Regex>,
) -> Result<Checked, QueryError> {
  let ty = match expr {
    Expr::Prop { var, prop } => {
      let (_, var_ty) = scope
        .cols
        .iter()
        .find(|(n, _)| n == var)
        .ok_or_else(|| QueryError::Plan(format!("variable '{var}' is not bound in MATCH")))?;
      check_prop(prop)?;
      match var_ty {
        ExprType::Node | ExprType::Any => prop_type(prop),
        other => {
          return Err(QueryError::Plan(format!(
            "'{var}' is a {other:?} value, not a node — '.{prop}' cannot be read from it"
          )));
        }
      }
    }
    Expr::Var { var } => scope
      .cols
      .iter()
      .find(|(n, _)| n == var)
      .map(|(_, t)| *t)
      .ok_or_else(|| QueryError::Plan(format!("variable '{var}' is not bound in MATCH")))?,
    Expr::Lit(value) => match value {
      PropValue::Text(_) => ExprType::Text,
      PropValue::Int(_) => ExprType::Int,
      PropValue::Float(_) => ExprType::Float,
      PropValue::Bool(_) => ExprType::Bool,
    },
    Expr::Null => ExprType::Null,
    Expr::List(items) => {
      for item in items {
        check_expr(item, scope, position, aggs, in_agg)?;
      }
      ExprType::List
    }
    Expr::Call { name, args } => check_call(name, args, scope, position, aggs, in_agg)?,
    Expr::Case {
      subject,
      whens,
      otherwise,
    } => {
      if let Some(subject) = subject {
        check_expr(subject, scope, position, aggs, in_agg)?;
      }
      let mut result_ty: Option<ExprType> = None;
      for (when, then) in whens {
        check_expr(when, scope, position, aggs, in_agg)?;
        let t = check_expr(then, scope, position, aggs, in_agg)?.ty;
        result_ty = Some(match result_ty {
          Some(prev) if prev == t => prev,
          Some(_) => ExprType::Any,
          None => t,
        });
      }
      if let Some(otherwise) = otherwise {
        let t = check_expr(otherwise, scope, position, aggs, in_agg)?.ty;
        result_ty = Some(match result_ty {
          Some(prev) if prev == t => prev,
          _ => ExprType::Any,
        });
      }
      result_ty.unwrap_or(ExprType::Any)
    }
    Expr::Agg { func, arg, .. } => {
      if position != Position::Projection {
        return Err(QueryError::Plan(
          "aggregates (count, sum, avg, min, max, collect) belong in RETURN or WITH items, \
           not in WHERE"
            .into(),
        ));
      }
      if in_agg {
        return Err(QueryError::Plan("aggregates cannot nest".into()));
      }
      let arg_ty = match arg {
        Some(arg) => check_expr(arg, scope, position, aggs, true)?.ty,
        None => ExprType::Any,
      };
      if !aggs.contains(expr) {
        aggs.push(expr.clone());
      }
      match func {
        AggFn::Count => ExprType::Int,
        AggFn::Sum => {
          if !arg_ty.is_numeric() {
            return Err(QueryError::Plan("sum() takes a numeric argument".into()));
          }
          if arg_ty == ExprType::Float { ExprType::Float } else { ExprType::Int }
        }
        AggFn::Avg => {
          if !arg_ty.is_numeric() {
            return Err(QueryError::Plan("avg() takes a numeric argument".into()));
          }
          ExprType::Float
        }
        AggFn::Min | AggFn::Max => arg_ty,
        AggFn::Collect => ExprType::List,
      }
    }
    Expr::Binary { op, left, right } => {
      let l = check_expr(left, scope, position, aggs, in_agg)?.ty;
      let r = check_expr(right, scope, position, aggs, in_agg)?.ty;
      match op {
        ArithOp::Add if l.is_text() && r.is_text() && (l == ExprType::Text || r == ExprType::Text) => {
          ExprType::Text
        }
        ArithOp::Add if l == ExprType::List || r == ExprType::List => ExprType::List,
        _ => {
          if !l.is_numeric() || !r.is_numeric() {
            return Err(QueryError::Plan(format!(
              "arithmetic needs numeric operands ({l:?} {op:?} {r:?})"
            )));
          }
          if l == ExprType::Float || r == ExprType::Float || *op == ArithOp::Div {
            ExprType::Float
          } else if l == ExprType::Int && r == ExprType::Int {
            ExprType::Int
          } else {
            ExprType::Any
          }
        }
      }
    }
    Expr::Neg(inner) => {
      let t = check_expr(inner, scope, position, aggs, in_agg)?.ty;
      if !t.is_numeric() {
        return Err(QueryError::Plan("unary minus needs a numeric operand".into()));
      }
      t
    }
    Expr::Pred(pred) => {
      check_pred(pred, scope, regexes)?;
      ExprType::Bool
    }
  };
  Ok(Checked { ty })
}

fn check_call(
  name: &str,
  args: &[Expr],
  scope: &Scope,
  position: Position,
  aggs: &mut Vec<Expr>,
  in_agg: bool,
) -> Result<ExprType, QueryError> {
  let mut arg_types = Vec::with_capacity(args.len());
  for arg in args {
    arg_types.push(check_expr(arg, scope, position, aggs, in_agg)?.ty);
  }
  let arity = |want: std::ops::RangeInclusive<usize>| -> Result<(), QueryError> {
    if want.contains(&args.len()) {
      Ok(())
    } else {
      Err(QueryError::Plan(format!(
        "{name}() takes {} argument{}",
        if want.start() == want.end() {
          want.start().to_string()
        } else {
          format!("{} to {}", want.start(), want.end())
        },
        if *want.end() == 1 { "" } else { "s" }
      )))
    }
  };
  Ok(match name {
    "tolower" | "toupper" | "trim" | "ltrim" | "rtrim" | "reverse" | "tostring" => {
      arity(1..=1)?;
      ExprType::Text
    }
    "replace" => {
      arity(3..=3)?;
      ExprType::Text
    }
    "substring" => {
      arity(2..=3)?;
      ExprType::Text
    }
    "left" | "right" => {
      arity(2..=2)?;
      ExprType::Text
    }
    "split" => {
      arity(2..=2)?;
      ExprType::List
    }
    "size" | "length" | "tointeger" | "abs" | "sign" => {
      arity(1..=1)?;
      ExprType::Int
    }
    "tofloat" => {
      arity(1..=1)?;
      ExprType::Float
    }
    "round" | "floor" | "ceil" => {
      arity(1..=1)?;
      ExprType::Int
    }
    "toboolean" => {
      arity(1..=1)?;
      ExprType::Bool
    }
    "coalesce" => {
      arity(1..=16)?;
      arg_types
        .iter()
        .copied()
        .find(|t| *t != ExprType::Null)
        .unwrap_or(ExprType::Null)
    }
    "id" => {
      arity(1..=1)?;
      if !matches!(arg_types[0], ExprType::Node | ExprType::Any) {
        return Err(QueryError::Plan("id() takes a pattern node".into()));
      }
      ExprType::Int
    }
    "labels" | "keys" => {
      arity(1..=1)?;
      if !matches!(arg_types[0], ExprType::Node | ExprType::Any) {
        return Err(QueryError::Plan(format!("{name}() takes a pattern node")));
      }
      ExprType::List
    }
    "head" | "last" => {
      arity(1..=1)?;
      ExprType::Any
    }
    "range" => {
      arity(2..=3)?;
      ExprType::List
    }
    "type" => {
      return Err(QueryError::Plan(
        "type() needs a relationship variable, which patterns do not bind — the relation \
         is the pattern's `[:name]`"
          .into(),
      ));
    }
    "properties" => {
      return Err(QueryError::Plan(
        "properties() returns a map, which the result model does not carry — project the \
         properties you need"
          .into(),
      ));
    }
    other => {
      return Err(QueryError::Plan(format!(
        "unknown function '{other}' (available: toLower, toUpper, toString, toInteger, \
         toFloat, toBoolean, size, length, trim, ltrim, rtrim, reverse, replace, substring, \
         left, right, split, coalesce, id, labels, keys, head, last, range, abs, sign, round, \
         floor, ceil)"
      )));
    }
  })
}

/// Plan-time typing of a comparison, keeping the three historic messages.
fn check_cmp_types(op: CmpOp, l: ExprType, r: ExprType, describe: &str) -> Result<(), QueryError> {
  match op {
    CmpOp::Lt | CmpOp::Le | CmpOp::Gt | CmpOp::Ge => {
      let ordered = |t: ExprType| t.is_numeric() || t.is_text() || t == ExprType::Bool;
      if !ordered(l) || !ordered(r) || (l != r && !(l.is_numeric() && r.is_numeric()) && l != ExprType::Any && r != ExprType::Any && l != ExprType::Null && r != ExprType::Null) {
        return Err(QueryError::Plan(format!(
          "ordered comparison on {describe} — <, <=, >, >= compare two numbers (or two \
           strings) — got {l:?} vs {r:?}"
        )));
      }
    }
    CmpOp::StartsWith | CmpOp::EndsWith | CmpOp::Contains | CmpOp::Matches => {
      if !l.is_text() || !r.is_text() {
        return Err(QueryError::Plan(format!(
          "text comparison on {describe} — STARTS/ENDS WITH, CONTAINS, and =~ apply to text \
           values — got {l:?} vs {r:?}"
        )));
      }
    }
    CmpOp::Eq | CmpOp::Ne => {
      let compatible = l == r
        || l == ExprType::Any
        || r == ExprType::Any
        || l == ExprType::Null
        || r == ExprType::Null
        || (l.is_numeric() && r.is_numeric());
      if !compatible {
        return Err(QueryError::Plan(format!(
          "type mismatch: {describe} compares {l:?} with {r:?}"
        )));
      }
    }
  }
  Ok(())
}

fn describe(expr: &Expr) -> String {
  match expr {
    Expr::Prop { var, prop } => format!("'{var}.{prop}'"),
    Expr::Var { var } => format!("'{var}'"),
    other => format!("'{}'", render_expr(other)),
  }
}

/// Validate a predicate tree against `scope`, compiling every `=~` pattern into `regexes`.
pub fn check_pred(
  pred: &PredExpr,
  scope: &Scope,
  regexes: &mut std::collections::HashMap<String, regex::Regex>,
) -> Result<(), QueryError> {
  let mut no_aggs = Vec::new();
  match pred {
    PredExpr::Cmp { left, op, right } => {
      let l = check_expr(left, scope, Position::Predicate, &mut no_aggs, false)?.ty;
      let r = check_expr(right, scope, Position::Predicate, &mut no_aggs, false)?.ty;
      check_cmp_types(*op, l, r, &describe(left))?;
      if *op == CmpOp::Matches {
        match right {
          Expr::Lit(PropValue::Text(pattern)) => {
            if !regexes.contains_key(pattern) {
              regexes.insert(pattern.clone(), compile_match_regex(pattern)?);
            }
          }
          _ => {
            return Err(QueryError::Plan(
              "=~ takes a literal pattern on its right-hand side (patterns are compiled and \
               bounded at plan time)"
                .into(),
            ));
          }
        }
      }
    }
    PredExpr::In { item, list } => {
      check_expr(item, scope, Position::Predicate, &mut no_aggs, false)?;
      let t = check_expr(list, scope, Position::Predicate, &mut no_aggs, false)?.ty;
      if !matches!(t, ExprType::List | ExprType::Any | ExprType::Null) {
        return Err(QueryError::Plan("IN needs a list on its right-hand side".into()));
      }
    }
    PredExpr::IsNull { expr, .. } => {
      check_expr(expr, scope, Position::Predicate, &mut no_aggs, false)?;
    }
    PredExpr::HasLabel { var, kinds } => {
      let (_, ty) = scope
        .cols
        .iter()
        .find(|(n, _)| n == var)
        .ok_or_else(|| QueryError::Plan(format!("variable '{var}' is not bound in MATCH")))?;
      if !matches!(ty, ExprType::Node | ExprType::Any) {
        return Err(QueryError::Plan(format!("'{var}' is not a pattern node")));
      }
      for kind in kinds {
        SymbolKind::parse(kind).ok_or_else(|| {
          QueryError::Plan(format!("unknown kind '{kind}' (try: function, method, class, …)"))
        })?;
      }
    }
    PredExpr::Exists { pattern } => {
      let var = pattern.left.var.as_deref().ok_or_else(|| {
        QueryError::Plan("EXISTS { … } must start from a bound pattern variable".into())
      })?;
      match scope.cols.iter().find(|(n, _)| n == var) {
        Some((_, ExprType::Node | ExprType::Any)) => {}
        Some(_) => return Err(QueryError::Plan(format!("'{var}' is not a pattern node"))),
        None => {
          return Err(QueryError::Plan(format!("variable '{var}' is not bound in MATCH")));
        }
      }
      if !pattern.left.kinds.is_empty() || !pattern.left.props.is_empty() {
        return Err(QueryError::Plan(
          "the EXISTS start node is already bound — put its labels/properties on the outer \
           pattern"
            .into(),
        ));
      }
      if pattern.segments.is_empty() {
        return Err(QueryError::Plan("EXISTS { … } needs at least one relationship".into()));
      }
      for segment in &pattern.segments {
        if let Some(inner) = &segment.node.var {
          if scope.index_of(inner).is_some() {
            return Err(QueryError::Plan(format!(
              "EXISTS pattern variable '{inner}' shadows an outer variable"
            )));
          }
        }
      }
    }
    PredExpr::And(terms) | PredExpr::Or(terms) => {
      for term in terms {
        check_pred(term, scope, regexes)?;
      }
    }
    PredExpr::Not(inner) => check_pred(inner, scope, regexes)?,
  }
  Ok(())
}

/// Compile a `=~` pattern under the DoS bounds.
pub fn compile_match_regex(pattern: &str) -> Result<regex::Regex, QueryError> {
  const MAX_PATTERN: usize = 512;
  if pattern.len() > MAX_PATTERN {
    return Err(QueryError::Ceiling {
      what: "regex pattern bytes",
      limit: MAX_PATTERN as u64,
    });
  }
  regex::RegexBuilder::new(pattern)
    .size_limit(1 << 20)
    .dfa_size_limit(1 << 20)
    .build()
    .map_err(|err| QueryError::Plan(format!("invalid regex: {err}")))
}

/// The evaluation context: the graph, the scope, and — during aggregation output — the
/// accumulated aggregate values (parallel to the planner's `aggs` list).
pub struct Env<'a> {
  pub kg: &'a Kg,
  pub scope: &'a Scope,
  pub aggs: &'a [Expr],
  pub agg_values: Option<&'a [Cell]>,
  /// `=~` patterns compiled at plan time (pattern text → program). Patterns must be
  /// literals, so every regex is bounded and compiled exactly once, before any scan —
  /// a per-row cache behind a lock cost 7× on a parallel kernel-scale scan.
  pub regexes: &'a std::collections::HashMap<String, regex::Regex>,
  /// The existence-probe hook: `(row, pattern) → bool`, provided by the engine (it owns
  /// the traversal budget).
  pub exists: &'a (dyn Fn(&dyn RowAccess, &Pattern) -> bool + Sync),
}

pub fn eval(expr: &Expr, env: &Env<'_>, row: &dyn RowAccess) -> Cell {
  match expr {
    Expr::Prop { var, prop } => match env.scope.index_of(var).map(|i| row.cell(i)) {
      Some(Cell::Node(id)) => prop_cell(env.kg, id, prop),
      _ => Cell::Null,
    },
    Expr::Var { var } => env.scope.index_of(var).map(|i| row.cell(i)).unwrap_or(Cell::Null),
    Expr::Lit(value) => match value {
      PropValue::Text(t) => Cell::Text(t.clone()),
      PropValue::Int(n) => Cell::Int(*n as i64),
      PropValue::Float(f) => Cell::Float(*f),
      PropValue::Bool(b) => Cell::Bool(*b),
    },
    Expr::Null => Cell::Null,
    Expr::List(items) => Cell::List(items.iter().map(|e| eval(e, env, row)).collect()),
    Expr::Call { name, args } => eval_call(name, args, env, row),
    Expr::Case {
      subject,
      whens,
      otherwise,
    } => {
      let subject = subject.as_ref().map(|s| eval(s, env, row));
      for (when, then) in whens {
        let hit = match &subject {
          Some(subject) => eval(when, env, row) == *subject,
          None => truthy(&eval(when, env, row)),
        };
        if hit {
          return eval(then, env, row);
        }
      }
      otherwise.as_ref().map(|e| eval(e, env, row)).unwrap_or(Cell::Null)
    }
    Expr::Agg { .. } => match env.agg_values {
      Some(values) => env
        .aggs
        .iter()
        .position(|a| a == expr)
        .and_then(|i| values.get(i).cloned())
        .unwrap_or(Cell::Null),
      None => Cell::Null,
    },
    Expr::Binary { op, left, right } => {
      let l = eval(left, env, row);
      let r = eval(right, env, row);
      arith(*op, l, r)
    }
    Expr::Neg(inner) => match eval(inner, env, row) {
      Cell::Int(n) => Cell::Int(-n),
      Cell::Float(f) => Cell::Float(-f),
      _ => Cell::Null,
    },
    Expr::Pred(pred) => Cell::Bool(eval_pred(pred, env, row)),
  }
}

fn truthy(cell: &Cell) -> bool {
  matches!(cell, Cell::Bool(true))
}

fn arith(op: ArithOp, l: Cell, r: Cell) -> Cell {
  match (op, &l, &r) {
    (ArithOp::Add, Cell::Text(a), b) | (ArithOp::Add, b, Cell::Text(a)) if !b.is_null() => {
      if let Cell::Text(a2) = &l {
        Cell::Text(format!("{a2}{}", r))
      } else {
        let _ = a;
        Cell::Text(format!("{}{}", l, b))
      }
    }
    (ArithOp::Add, Cell::List(a), Cell::List(b)) => {
      let mut out = a.clone();
      out.extend(b.iter().cloned());
      Cell::List(out)
    }
    (ArithOp::Add, Cell::List(a), b) => {
      let mut out = a.clone();
      out.push(b.clone());
      Cell::List(out)
    }
    (_, Cell::Int(a), Cell::Int(b)) => match op {
      ArithOp::Add => a.checked_add(*b).map(Cell::Int).unwrap_or(Cell::Null),
      ArithOp::Sub => a.checked_sub(*b).map(Cell::Int).unwrap_or(Cell::Null),
      ArithOp::Mul => a.checked_mul(*b).map(Cell::Int).unwrap_or(Cell::Null),
      ArithOp::Div => {
        if *b == 0 { Cell::Null } else { Cell::Float(*a as f64 / *b as f64) }
      }
      ArithOp::Mod => {
        if *b == 0 { Cell::Null } else { Cell::Int(a.rem_euclid(*b)) }
      }
    },
    _ => match (l.as_f64(), r.as_f64()) {
      (Some(a), Some(b)) => match op {
        ArithOp::Add => Cell::Float(a + b),
        ArithOp::Sub => Cell::Float(a - b),
        ArithOp::Mul => Cell::Float(a * b),
        ArithOp::Div => {
          if b == 0.0 { Cell::Null } else { Cell::Float(a / b) }
        }
        ArithOp::Mod => {
          if b == 0.0 { Cell::Null } else { Cell::Float(a.rem_euclid(b)) }
        }
      },
      _ => Cell::Null,
    },
  }
}

fn as_text(cell: &Cell) -> Option<String> {
  match cell {
    Cell::Text(t) => Some(t.clone()),
    Cell::Null => None,
    other => Some(other.to_string()),
  }
}

fn eval_call(name: &str, args: &[Expr], env: &Env<'_>, row: &dyn RowAccess) -> Cell {
  let a = |i: usize| args.get(i).map(|e| eval(e, env, row)).unwrap_or(Cell::Null);
  match name {
    "tolower" => match a(0) {
      Cell::Text(t) => Cell::Text(t.to_lowercase()),
      _ => Cell::Null,
    },
    "toupper" => match a(0) {
      Cell::Text(t) => Cell::Text(t.to_uppercase()),
      _ => Cell::Null,
    },
    "trim" => match a(0) {
      Cell::Text(t) => Cell::Text(t.trim().to_string()),
      _ => Cell::Null,
    },
    "ltrim" => match a(0) {
      Cell::Text(t) => Cell::Text(t.trim_start().to_string()),
      _ => Cell::Null,
    },
    "rtrim" => match a(0) {
      Cell::Text(t) => Cell::Text(t.trim_end().to_string()),
      _ => Cell::Null,
    },
    "reverse" => match a(0) {
      Cell::Text(t) => Cell::Text(t.chars().rev().collect()),
      Cell::List(items) => Cell::List(items.into_iter().rev().collect()),
      _ => Cell::Null,
    },
    "tostring" => as_text(&a(0)).map(Cell::Text).unwrap_or(Cell::Null),
    "tointeger" => match a(0) {
      Cell::Int(n) => Cell::Int(n),
      Cell::Float(f) => Cell::Int(f.trunc() as i64),
      Cell::Text(t) => t.trim().parse::<i64>().map(Cell::Int).unwrap_or(Cell::Null),
      Cell::Bool(b) => Cell::Int(b as i64),
      _ => Cell::Null,
    },
    "tofloat" => match a(0) {
      Cell::Int(n) => Cell::Float(n as f64),
      Cell::Float(f) => Cell::Float(f),
      Cell::Text(t) => t.trim().parse::<f64>().map(Cell::Float).unwrap_or(Cell::Null),
      _ => Cell::Null,
    },
    "toboolean" => match a(0) {
      Cell::Bool(b) => Cell::Bool(b),
      Cell::Text(t) => match t.trim().to_ascii_lowercase().as_str() {
        "true" => Cell::Bool(true),
        "false" => Cell::Bool(false),
        _ => Cell::Null,
      },
      _ => Cell::Null,
    },
    "size" | "length" => match a(0) {
      Cell::Text(t) => Cell::Int(t.chars().count() as i64),
      Cell::List(items) => Cell::Int(items.len() as i64),
      _ => Cell::Null,
    },
    "replace" => match (a(0), a(1), a(2)) {
      (Cell::Text(t), Cell::Text(from), Cell::Text(to)) => Cell::Text(t.replace(&from, &to)),
      _ => Cell::Null,
    },
    "substring" => match (a(0), a(1), args.get(2).map(|_| a(2))) {
      (Cell::Text(t), Cell::Int(start), len) => {
        let start = start.max(0) as usize;
        let chars: Vec<char> = t.chars().collect();
        let end = match len {
          Some(Cell::Int(n)) => (start + n.max(0) as usize).min(chars.len()),
          Some(_) => return Cell::Null,
          None => chars.len(),
        };
        if start > chars.len() {
          Cell::Text(String::new())
        } else {
          Cell::Text(chars[start..end].iter().collect())
        }
      }
      _ => Cell::Null,
    },
    "left" => match (a(0), a(1)) {
      (Cell::Text(t), Cell::Int(n)) => Cell::Text(t.chars().take(n.max(0) as usize).collect()),
      _ => Cell::Null,
    },
    "right" => match (a(0), a(1)) {
      (Cell::Text(t), Cell::Int(n)) => {
        let chars: Vec<char> = t.chars().collect();
        let n = (n.max(0) as usize).min(chars.len());
        Cell::Text(chars[chars.len() - n..].iter().collect())
      }
      _ => Cell::Null,
    },
    "split" => match (a(0), a(1)) {
      (Cell::Text(t), Cell::Text(sep)) => {
        if sep.is_empty() {
          Cell::List(t.chars().map(|c| Cell::Text(c.to_string())).collect())
        } else {
          Cell::List(t.split(sep.as_str()).map(|s| Cell::Text(s.to_string())).collect())
        }
      }
      _ => Cell::Null,
    },
    "coalesce" => args
      .iter()
      .map(|e| eval(e, env, row))
      .find(|c| !c.is_null())
      .unwrap_or(Cell::Null),
    "id" => match a(0) {
      Cell::Node(id) => Cell::Int(id as i64),
      _ => Cell::Null,
    },
    "labels" => match a(0) {
      Cell::Node(id) => Cell::List(vec![prop_cell(env.kg, id, "kind")]),
      _ => Cell::Null,
    },
    "keys" => match a(0) {
      Cell::Node(_) => Cell::List(PROPS.iter().map(|p| Cell::Text(p.to_string())).collect()),
      _ => Cell::Null,
    },
    "head" => match a(0) {
      Cell::List(items) => items.into_iter().next().unwrap_or(Cell::Null),
      _ => Cell::Null,
    },
    "last" => match a(0) {
      Cell::List(items) => items.into_iter().last().unwrap_or(Cell::Null),
      _ => Cell::Null,
    },
    "range" => match (a(0), a(1), args.get(2).map(|_| a(2))) {
      (Cell::Int(start), Cell::Int(end), step) => {
        let step = match step {
          Some(Cell::Int(s)) if s != 0 => s,
          Some(_) => return Cell::Null,
          None => 1,
        };
        let mut out = Vec::new();
        let mut at = start;
        while (step > 0 && at <= end) || (step < 0 && at >= end) {
          if out.len() >= MAX_ROWS {
            break; // the list ceiling: a range can never outgrow a result
          }
          out.push(Cell::Int(at));
          at += step;
        }
        Cell::List(out)
      }
      _ => Cell::Null,
    },
    "abs" => match a(0) {
      Cell::Int(n) => Cell::Int(n.abs()),
      Cell::Float(f) => Cell::Float(f.abs()),
      _ => Cell::Null,
    },
    "sign" => match a(0) {
      Cell::Int(n) => Cell::Int(n.signum()),
      Cell::Float(f) => Cell::Int(if f > 0.0 { 1 } else if f < 0.0 { -1 } else { 0 }),
      _ => Cell::Null,
    },
    "round" => match a(0) {
      Cell::Int(n) => Cell::Int(n),
      Cell::Float(f) => Cell::Int(f.round() as i64),
      _ => Cell::Null,
    },
    "floor" => match a(0) {
      Cell::Int(n) => Cell::Int(n),
      Cell::Float(f) => Cell::Int(f.floor() as i64),
      _ => Cell::Null,
    },
    "ceil" => match a(0) {
      Cell::Int(n) => Cell::Int(n),
      Cell::Float(f) => Cell::Int(f.ceil() as i64),
      _ => Cell::Null,
    },
    _ => Cell::Null,
  }
}

pub fn eval_pred(pred: &PredExpr, env: &Env<'_>, row: &dyn RowAccess) -> bool {
  match pred {
    PredExpr::Cmp { left, op, right } => {
      let l = eval(left, env, row);
      let r = eval(right, env, row);
      compare(*op, &l, &r, env, right)
    }
    PredExpr::In { item, list } => {
      let item = eval(item, env, row);
      match eval(list, env, row) {
        Cell::List(items) => items.contains(&item),
        _ => false,
      }
    }
    PredExpr::IsNull { expr, negated } => eval(expr, env, row).is_null() != *negated,
    PredExpr::HasLabel { var, kinds } => match env.scope.index_of(var).map(|i| row.cell(i)) {
      Some(Cell::Node(id)) => {
        let Some(view) = env.kg.node(NodeId::new(id as u64)) else {
          return false;
        };
        kinds.iter().any(|k| SymbolKind::parse(k) == Some(view.kind))
      }
      _ => false,
    },
    PredExpr::Exists { pattern } => (env.exists)(row, pattern),
    PredExpr::And(terms) => terms.iter().all(|t| eval_pred(t, env, row)),
    PredExpr::Or(terms) => terms.iter().any(|t| eval_pred(t, env, row)),
    PredExpr::Not(inner) => !eval_pred(inner, env, row),
  }
}

fn compare(op: CmpOp, l: &Cell, r: &Cell, env: &Env<'_>, right_expr: &Expr) -> bool {
  if l.is_null() || r.is_null() {
    return false;
  }
  match op {
    CmpOp::Eq => {
      // Kind and eid text compare case-insensitively (hex / label spellings).
      match (l, r) {
        (Cell::Text(a), Cell::Text(b)) => a == b || a.eq_ignore_ascii_case(b) && looks_enum(a),
        _ => l == r,
      }
    }
    CmpOp::Ne => !compare(CmpOp::Eq, l, r, env, right_expr),
    CmpOp::Lt => l.rank() == r.rank() && l < r,
    CmpOp::Le => l.rank() == r.rank() && l <= r,
    CmpOp::Gt => l.rank() == r.rank() && l > r,
    CmpOp::Ge => l.rank() == r.rank() && l >= r,
    CmpOp::StartsWith => match (l, r) {
      (Cell::Text(a), Cell::Text(b)) => a.starts_with(b.as_str()),
      _ => false,
    },
    CmpOp::EndsWith => match (l, r) {
      (Cell::Text(a), Cell::Text(b)) => a.ends_with(b.as_str()),
      _ => false,
    },
    CmpOp::Contains => match (l, r) {
      (Cell::Text(a), Cell::Text(b)) => a.contains(b.as_str()),
      _ => false,
    },
    CmpOp::Matches => match (l, r) {
      (Cell::Text(a), Cell::Text(pattern)) => {
        let _ = right_expr;
        env.regexes.get(pattern).is_some_and(|re| re.is_match(a))
      }
      _ => false,
    },
  }
}

/// Kind labels and eids are the only text values compared case-insensitively — both are
/// spellings of an enum/hex, never user prose. Heuristic: all-lowercase ASCII alnum with no
/// spaces on the LEFT (property side) — kinds are `function`, eids are 32 hex chars.
fn looks_enum(text: &str) -> bool {
  !text.is_empty()
    && text
      .chars()
      .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
}

/// A stable rendering of an expression, used for column titles.
pub fn render_expr(expr: &Expr) -> String {
  match expr {
    Expr::Prop { var, prop } => format!("{var}.{prop}"),
    Expr::Var { var } => var.clone(),
    Expr::Lit(PropValue::Text(t)) => format!("\"{t}\""),
    Expr::Lit(PropValue::Int(n)) => n.to_string(),
    Expr::Lit(PropValue::Float(f)) => f.to_string(),
    Expr::Lit(PropValue::Bool(b)) => b.to_string(),
    Expr::Null => "null".to_string(),
    Expr::List(items) => format!(
      "[{}]",
      items.iter().map(render_expr).collect::<Vec<_>>().join(", ")
    ),
    Expr::Call { name, args } => format!(
      "{name}({})",
      args.iter().map(render_expr).collect::<Vec<_>>().join(", ")
    ),
    Expr::Case { .. } => "case".to_string(),
    Expr::Agg { func, distinct, arg } => {
      let name = match func {
        AggFn::Count => "count",
        AggFn::Sum => "sum",
        AggFn::Avg => "avg",
        AggFn::Min => "min",
        AggFn::Max => "max",
        AggFn::Collect => "collect",
      };
      match arg {
        None => name.to_string(),
        Some(arg) => format!(
          "{name}({}{})",
          if *distinct { "DISTINCT " } else { "" },
          render_expr(arg)
        ),
      }
    }
    Expr::Binary { op, left, right } => {
      let symbol = match op {
        ArithOp::Add => "+",
        ArithOp::Sub => "-",
        ArithOp::Mul => "*",
        ArithOp::Div => "/",
        ArithOp::Mod => "%",
      };
      format!("{} {symbol} {}", render_expr(left), render_expr(right))
    }
    Expr::Neg(inner) => format!("-{}", render_expr(inner)),
    Expr::Pred(_) => "predicate".to_string(),
  }
}

/// Accumulator for one aggregate over one group.
pub enum AggState {
  Count(u64),
  CountDistinct(HashSet<Cell>),
  Sum(Cell),
  Avg { sum: f64, n: u64 },
  Min(Cell),
  Max(Cell),
  Collect(Vec<Cell>),
  CollectDistinct(Vec<Cell>, HashSet<Cell>),
}

impl AggState {
  pub fn new(expr: &Expr) -> Self {
    match expr {
      Expr::Agg { func, distinct, .. } => match (func, distinct) {
        (AggFn::Count, false) => AggState::Count(0),
        (AggFn::Count, true) => AggState::CountDistinct(HashSet::new()),
        (AggFn::Sum, _) => AggState::Sum(Cell::Int(0)),
        (AggFn::Avg, _) => AggState::Avg { sum: 0.0, n: 0 },
        (AggFn::Min, _) => AggState::Min(Cell::Null),
        (AggFn::Max, _) => AggState::Max(Cell::Null),
        (AggFn::Collect, false) => AggState::Collect(Vec::new()),
        (AggFn::Collect, true) => AggState::CollectDistinct(Vec::new(), HashSet::new()),
      },
      _ => AggState::Count(0),
    }
  }

  /// Feed one row's argument value (`None` for `count(*)`). Nulls are skipped by every
  /// aggregate but `count(*)` — Cypher semantics.
  pub fn feed(&mut self, value: Option<Cell>) -> Result<(), QueryError> {
    match (self, value) {
      (AggState::Count(n), None) => *n += 1,
      (AggState::Count(n), Some(v)) => {
        if !v.is_null() {
          *n += 1;
        }
      }
      (AggState::CountDistinct(seen), Some(v)) => {
        if !v.is_null() {
          seen.insert(v);
        }
      }
      (AggState::Sum(acc), Some(v)) => {
        if !v.is_null() {
          *acc = arith(ArithOp::Add, acc.clone(), v);
        }
      }
      (AggState::Avg { sum, n }, Some(v)) => {
        if let Some(f) = v.as_f64() {
          *sum += f;
          *n += 1;
        }
      }
      (AggState::Min(acc), Some(v)) => {
        if !v.is_null() && (acc.is_null() || v < *acc) {
          *acc = v;
        }
      }
      (AggState::Max(acc), Some(v)) => {
        if !v.is_null() && (acc.is_null() || v > *acc) {
          *acc = v;
        }
      }
      (AggState::Collect(items), Some(v)) => {
        if !v.is_null() {
          if items.len() >= MAX_ROWS {
            return Err(QueryError::Ceiling {
              what: "collected list elements",
              limit: MAX_ROWS as u64,
            });
          }
          items.push(v);
        }
      }
      (AggState::CollectDistinct(items, seen), Some(v)) => {
        if !v.is_null() && seen.insert(v.clone()) {
          if items.len() >= MAX_ROWS {
            return Err(QueryError::Ceiling {
              what: "collected list elements",
              limit: MAX_ROWS as u64,
            });
          }
          items.push(v);
        }
      }
      _ => {}
    }
    Ok(())
  }

  pub fn finish(self) -> Cell {
    match self {
      AggState::Count(n) => Cell::Int(n as i64),
      AggState::CountDistinct(seen) => Cell::Int(seen.len() as i64),
      AggState::Sum(acc) => acc,
      AggState::Avg { sum, n } => {
        if n == 0 { Cell::Null } else { Cell::Float(sum / n as f64) }
      }
      AggState::Min(acc) | AggState::Max(acc) => acc,
      AggState::Collect(items) | AggState::CollectDistinct(items, _) => Cell::List(items),
    }
  }
}

/// Convert a predicate to an expression (CASE arms without a subject).
pub fn pred_to_expr(pred: PredExpr) -> Expr {
  Expr::Pred(Box::new(pred))
}
