//! The typed query IR — what the parser produces and the executor consumes. Serde-shaped so
//! the MCP surface can accept a pre-built IR document instead of query text; every construct
//! is read-only by construction (there is no clause that names a mutation).
//!
//! v2 shape: `MATCH … (WHERE …)? (WITH … | UNWIND …)* RETURN … (UNION (ALL)? query)?` —
//! a clause pipeline over a table of cells, with a general expression language
//! (properties, literals, arithmetic, functions, CASE, aggregates, lists, EXISTS).

use serde::{Deserialize, Serialize};

/// One complete query. `union` chains a second query whose result columns must match.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Query {
  pub pattern: Pattern,
  /// The WHERE clause after MATCH, as a boolean expression tree.
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub predicate: Option<PredExpr>,
  /// Pipeline stages between MATCH and RETURN (`WITH`, `UNWIND`), in order.
  #[serde(default, skip_serializing_if = "Vec::is_empty")]
  pub stages: Vec<Stage>,
  pub returns: ReturnClause,
  #[serde(default, skip_serializing_if = "Vec::is_empty")]
  pub order_by: Vec<Ordering>,
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub skip: Option<u64>,
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub limit: Option<u64>,
  /// `UNION` / `UNION ALL` with the query that follows.
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub union: Option<Box<UnionTail>>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct UnionTail {
  /// `UNION ALL` keeps duplicates; plain `UNION` deduplicates the combined rows.
  pub all: bool,
  pub query: Query,
}

/// One linear pattern: a start node followed by zero or more relationship segments
/// (chains like `(a)-[:calls]->(b)-[:imports]->(c)`; no joins or cycles: each pattern
/// variable binds exactly once).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Pattern {
  pub left: NodePattern,
  #[serde(default, skip_serializing_if = "Vec::is_empty")]
  pub segments: Vec<PatternSegment>,
}

/// One `-[rel]-> (node)` link in a pattern chain.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PatternSegment {
  pub rel: RelPattern,
  pub node: NodePattern,
}

/// `(var:Kind|Kind2 {name: "x", path: "suffix", id: 7, eid: "hex"})` — every part optional.
/// The inline `path` property is a suffix match, mirroring the `--path` facet everywhere
/// else in vorpal; `name`, `id`, and `eid` are exact.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct NodePattern {
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub var: Option<String>,
  /// Symbol-kind labels, validated at plan time; several labels are alternatives.
  #[serde(default, skip_serializing_if = "Vec::is_empty")]
  pub kinds: Vec<String>,
  #[serde(default, skip_serializing_if = "Vec::is_empty")]
  pub props: Vec<(String, PropValue)>,
}

/// `-[:calls|data_flows*1..3 {grade: constrained}]->` and its In/Both spellings.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RelPattern {
  /// Relation names (`calls`, `data_flows`, …), unioned; empty = every relation.
  #[serde(default, skip_serializing_if = "Vec::is_empty")]
  pub types: Vec<String>,
  pub direction: RelDirection,
  /// Var-length bounds `(min, max)` when `*` was written; `None` = exactly one hop.
  /// A bare `*` is defined as `1..=10` (the language's documented default, not a
  /// truncation); an explicit bound above the depth ceiling is an error.
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub range: Option<(u32, u32)>,
  /// Resolution-grade floor (`exact` | `constrained` | `heuristic`).
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub grade: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RelDirection {
  /// `-…->`: the edge points left → right.
  Out,
  /// `<-…-`: the edge points right → left.
  In,
  /// `-…-`: either orientation (union of both).
  Both,
}

/// A literal in an inline property map or an expression.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum PropValue {
  Text(String),
  Int(u64),
  Float(f64),
  Bool(bool),
}

/// The expression language. Every variant is pure and read-only.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Expr {
  /// `var.prop`
  Prop { var: String, prop: String },
  /// A bare variable — a pattern node, or a column bound by WITH/UNWIND.
  Var { var: String },
  Lit(PropValue),
  Null,
  /// `[a, b, c]`
  List(Vec<Expr>),
  /// `name(args…)` — string/scalar functions.
  Call { name: String, args: Vec<Expr> },
  /// `CASE [subject] WHEN a THEN b … [ELSE c] END`
  Case {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    subject: Option<Box<Expr>>,
    whens: Vec<(Expr, Expr)>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    otherwise: Option<Box<Expr>>,
  },
  /// `count(*)`, `count(DISTINCT x)`, `sum/avg/min/max/collect(x)`.
  Agg {
    func: AggFn,
    #[serde(default)]
    distinct: bool,
    /// `None` only for `count(*)`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    arg: Option<Box<Expr>>,
  },
  /// `a + b`, `a - b`, `a * b`, `a / b`, `a % b`
  Binary {
    op: ArithOp,
    left: Box<Expr>,
    right: Box<Expr>,
  },
  /// `-a`
  Neg(Box<Expr>),
  /// A predicate in expression position (`CASE WHEN f.exported THEN … END`, or a boolean
  /// projection) — its value is the predicate's truth.
  Pred(Box<PredExpr>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AggFn {
  Count,
  Sum,
  Avg,
  Min,
  Max,
  Collect,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ArithOp {
  Add,
  Sub,
  Mul,
  Div,
  Mod,
}

/// A WHERE expression: comparisons combined with AND / OR / NOT (parentheses group).
/// Precedence NOT > AND > OR, exactly as parsed.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PredExpr {
  Cmp {
    left: Expr,
    op: CmpOp,
    right: Expr,
  },
  /// `x IN [a, b, c]`
  In { item: Expr, list: Expr },
  /// `x IS NULL` / `x IS NOT NULL`
  IsNull {
    expr: Expr,
    #[serde(default)]
    negated: bool,
  },
  /// `n:Label` — the node's kind is one of the labels.
  HasLabel { var: String, kinds: Vec<String> },
  /// `EXISTS { (var)-[…]->(…) }` — an existence probe from a bound pattern variable; the
  /// pattern's start node must name that variable.
  Exists { pattern: Pattern },
  And(Vec<PredExpr>),
  Or(Vec<PredExpr>),
  Not(Box<PredExpr>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CmpOp {
  Eq,
  Ne,
  StartsWith,
  EndsWith,
  Contains,
  Lt,
  Le,
  Gt,
  Ge,
  /// `=~` — regex match on text (compiled once at plan time, bounded).
  Matches,
}

/// A pipeline stage between MATCH and RETURN.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Stage {
  /// `WITH [DISTINCT] items… [WHERE …] [ORDER BY …] [SKIP n] [LIMIT n]` — projects (and
  /// aggregates, when any item aggregates) the current table into a new scope.
  With {
    #[serde(default)]
    distinct: bool,
    items: Vec<Projection>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    predicate: Option<PredExpr>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    order_by: Vec<Ordering>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    skip: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    limit: Option<u64>,
  },
  /// `UNWIND expr AS var` — one row per list element (a non-list unwinds as itself; null
  /// and empty lists produce no rows).
  Unwind { expr: Expr, alias: String },
}

/// `RETURN [DISTINCT] items…`. Any aggregate among the items turns the clause into an
/// implicit GROUP BY over the non-aggregate items (Cypher semantics).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ReturnClause {
  #[serde(default)]
  pub distinct: bool,
  pub items: Vec<Projection>,
}

/// One projected column: an expression with an optional alias. A bare pattern variable
/// projects to its four identity columns (`var.id`, `.name`, `.kind`, `.path`) — the
/// vorpal convention, kept from v1.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Projection {
  pub expr: Expr,
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub alias: Option<String>,
}

/// `ORDER BY expr [ASC|DESC]` — a returned column (by alias or spelling) or any expression
/// over the projected scope.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Ordering {
  pub key: Expr,
  #[serde(default)]
  pub descending: bool,
}
