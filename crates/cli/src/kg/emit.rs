//! Machine emission for the KG verbs: `--format json` speaks the exact records/pagination
//! envelope the MCP server speaks (`{outcome, records, total, truncated, nextCursor?}`),
//! over stdout. The default text format stays on the library's rendered strings, byte-stable
//! for humans and existing scripts.

use anyhow::Result;
use serde::Serialize;
use vorpal_index::records::{Selected, paged_value, selected_value};

/// One page of a selector outcome, pretty-printed.
pub fn selected_json<T: Serialize>(
  selected: Selected<T>,
  cursor: Option<&str>,
  limit: Option<u64>,
) -> Result<String> {
  let value = selected_value(selected, cursor, limit).map_err(anyhow::Error::msg)?;
  Ok(serde_json::to_string_pretty(&value)?)
}

/// One page of a plain record vector (listings, search hits), pretty-printed.
pub fn records_json<T: Serialize>(
  records: &[T],
  cursor: Option<&str>,
  limit: Option<u64>,
) -> Result<String> {
  let value = paged_value(records, cursor, limit, "hits").map_err(anyhow::Error::msg)?;
  Ok(serde_json::to_string_pretty(&value)?)
}
