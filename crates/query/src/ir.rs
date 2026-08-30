//! The typed query IR — what the parser produces and the executor consumes. Serde-shaped so
//! the MCP surface can accept a pre-built IR document instead of query text; every construct
//! is read-only by construction (there is no clause that names a mutation).

use serde::{Deserialize, Serialize};

/// One complete query: `MATCH pattern (WHERE …)? RETURN … (ORDER BY …)? (SKIP n)? (LIMIT n)?`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Query {
  pub pattern: Pattern,
  #[serde(default, skip_serializing_if = "Vec::is_empty")]
  pub predicates: Vec<Predicate>,
  pub returns: Returns,
  #[serde(default, skip_serializing_if = "Vec::is_empty")]
  pub order_by: Vec<Ordering>,
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub skip: Option<u64>,
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub limit: Option<u64>,
}

/// A single linear pattern: one node, optionally connected through one relationship
/// segment to a second node (the v1 grammar — no multi-segment chains or joins).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Pattern {
  pub left: NodePattern,
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub rel: Option<RelPattern>,
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub right: Option<NodePattern>,
}

/// `(var:Kind {name: "x", path: "suffix", id: 7, eid: "hex"})` — every part optional.
/// The inline `path` property is a suffix match, mirroring the `--path` facet everywhere
/// else in vorpal; `name`, `id`, and `eid` are exact.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct NodePattern {
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub var: Option<String>,
  /// Symbol-kind label (`:Function`), validated at plan time.
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub kind: Option<String>,
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

/// A property literal in an inline map or a WHERE comparison.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum PropValue {
  Text(String),
  Int(u64),
  Bool(bool),
}

/// `var.prop` — the only addressable expression form.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PropRef {
  pub var: String,
  pub prop: String,
}

/// One WHERE conjunct (`AND`-combined; v1 has no OR/NOT).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Predicate {
  pub target: PropRef,
  pub op: CmpOp,
  pub value: PropValue,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CmpOp {
  Eq,
  Ne,
  StartsWith,
  EndsWith,
  Contains,
}

/// The RETURN clause: plain projections, or a count — optionally grouped by one key
/// (`RETURN f.name, COUNT(*)` groups implicitly on the single non-count projection).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Returns {
  Rows(Vec<Projection>),
  Count {
    /// `COUNT(DISTINCT var.prop)` counts distinct values; `COUNT(*)` counts rows.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    distinct: Option<PropRef>,
    /// The implicit-grouping key when a projection accompanies the count.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    group: Option<Projection>,
  },
}

/// One projected column: a `var.prop`, or a bare `var` (shorthand for the node's
/// `id`/`name`/`kind`/`path` — expanded at plan time).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Projection {
  #[serde(flatten)]
  pub expr: ProjExpr,
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub alias: Option<String>,
}

/// Untagged, most-specific first: `{"var": "f", "prop": "name"}` is a property read,
/// `{"var": "f"}` alone is the bare-variable shorthand.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ProjExpr {
  Prop { var: String, prop: String },
  Var { var: String },
}

/// `ORDER BY <returned column> [ASC|DESC]` — the key must name a returned column
/// (by alias or by its `var.prop` spelling).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Ordering {
  pub key: String,
  #[serde(default)]
  pub descending: bool,
}
