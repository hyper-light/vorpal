//! vorpal-query: a Cypher-shaped, read-only query language over the vorpal knowledge graph
//! (ADOPTION P4 item C / plan G-M4).
//!
//! v1 grammar: one `MATCH` with a single linear pattern (0–1 relationship segment, fixed or
//! var-length `*min..max`, direction `->` / `<-` / `--`, optional `{grade: …}` floor),
//! `WHERE` with AND-combined property predicates (`=`, `<>`, `STARTS WITH`, `ENDS WITH`,
//! `CONTAINS`), `RETURN` projections or `COUNT(*)` / `COUNT(DISTINCT var.prop)` with one
//! implicit grouping key, `ORDER BY` / `SKIP` / `LIMIT`. Structurally read-only: the IR has
//! no mutating construct.
//!
//! ```text
//! MATCH (f:Function)-[:data_flows*1..5 {grade: constrained}]->(g {name: "deserialize"})
//! RETURN f.name, f.path LIMIT 50
//! ```
//!
//! Everything runs under explicit ceilings — deterministic work counts, never wall time —
//! and exceeding one is a typed [`QueryError::Ceiling`] naming it, never a silently
//! truncated answer.

mod exec;
pub mod ir;
mod lexer;
mod parser;

pub use exec::Cell;
pub use ir::Query;

/// Query text longer than this is refused before lexing.
pub const MAX_TEXT_BYTES: usize = 16 * 1024;
/// Var-length ranges may not exceed this depth (a bare `*` means `1..=10` by definition).
pub const MAX_DEPTH: u32 = 10;
/// Total edges the executor may scan across the whole query.
pub const MAX_EDGE_VISITS: u64 = 5_000_000;
/// Materialized result rows before SKIP/LIMIT (ungrouped counts stream and are exempt).
pub const MAX_ROWS: usize = 100_000;
/// Relationship segments one pattern may chain.
pub const MAX_SEGMENTS: usize = 8;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum QueryError {
  /// The text failed to parse; `offset` is a byte position into the query.
  Parse { offset: usize, message: String },
  /// The query parsed but names something this index or grammar version doesn't have.
  Plan(String),
  /// A work ceiling was reached; the answer would be incomplete, so there is none.
  Ceiling { what: &'static str, limit: u64 },
}

impl QueryError {
  pub(crate) fn parse(offset: usize, message: impl Into<String>) -> Self {
    QueryError::Parse {
      offset,
      message: message.into(),
    }
  }
}

impl std::fmt::Display for QueryError {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    match self {
      QueryError::Parse { offset, message } => {
        write!(f, "parse error at byte {offset}: {message}")
      }
      QueryError::Plan(message) => write!(f, "{message}"),
      QueryError::Ceiling { what, limit } => {
        write!(f, "query ceiling reached: {what} > {limit} — narrow the pattern (anchor a name, lower the depth, add LIMIT-compatible filters)")
      }
    }
  }
}

impl std::error::Error for QueryError {}

/// One executed query's answer: named columns, rows of cells, and the pre-SKIP/LIMIT row
/// count (so surfaces can render "showing 50 of 1,234").
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct QueryResult {
  pub columns: Vec<String>,
  pub rows: Vec<Vec<Cell>>,
  pub total_rows: u64,
}

/// Parse query text into the typed IR.
pub fn parse(text: &str) -> Result<Query, QueryError> {
  if text.len() > MAX_TEXT_BYTES {
    return Err(QueryError::Ceiling {
      what: "query text bytes",
      limit: MAX_TEXT_BYTES as u64,
    });
  }
  parser::parse_text(text)
}

/// Parse a pre-built IR document (the MCP `query` tool's JSON form).
pub fn parse_ir_json(json: &str) -> Result<Query, QueryError> {
  if json.len() > MAX_TEXT_BYTES {
    return Err(QueryError::Ceiling {
      what: "query text bytes",
      limit: MAX_TEXT_BYTES as u64,
    });
  }
  serde_json::from_str(json).map_err(|err| QueryError::Plan(format!("IR document: {err}")))
}

/// Execute a parsed query against an open graph.
pub fn execute(kg: &vorpal_kg::Kg, query: &Query) -> Result<QueryResult, QueryError> {
  exec::execute(kg, query)
}

/// Parse + execute in one step.
pub fn run(kg: &vorpal_kg::Kg, text: &str) -> Result<QueryResult, QueryError> {
  execute(kg, &parse(text)?)
}
