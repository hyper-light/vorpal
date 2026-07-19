//! Per-file extraction products — the incremental-rebuild cache unit (§3.4).
//!
//! A [`FileProduct`] is everything extraction learns from one file: its outline items and its
//! references, the latter keyed by the *entity path* of their enclosing definition (stable
//! across runs) rather than a `NodeId` (assigned per run). Re-indexing re-parses only changed
//! files and replays cached products for unchanged ones; the graph is always re-linked from the
//! complete product set, so removals and renames cannot leave stale nodes or edges behind.

use std::fs;
use std::io;
use std::path::Path;

use serde::{Deserialize, Serialize};
use vorpal_outline::model::{OutlineEntry, OutlineItem, OutlineMember};
use vorpal_resolve::RefKind;

/// One file's extraction output, serializable for the on-disk product cache.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileProduct {
  pub items: Vec<OutlineItem<'static>>,
  pub refs: Vec<ProductRef>,
}

/// A reference keyed by its enclosing definition's entity path (`""` = the file node).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProductRef {
  pub from_entity: String,
  pub name: String,
  pub kind: u8,
  pub start: u32,
  pub end: u32,
}

pub(crate) fn refkind_tag(kind: RefKind) -> u8 {
  match kind {
    RefKind::Call => 0,
    RefKind::Type => 1,
    RefKind::Import => 2,
    RefKind::Implements => 3,
    RefKind::Use => 4,
  }
}

pub(crate) fn tag_refkind(tag: u8) -> RefKind {
  match tag {
    1 => RefKind::Type,
    2 => RefKind::Import,
    3 => RefKind::Implements,
    4 => RefKind::Use,
    _ => RefKind::Call,
  }
}

/// The entity path of a member within its item — must match `KgWriter`'s identity convention.
pub(crate) fn member_entity_path(owner: &str, member: &str) -> String {
  format!("{owner}.{member}")
}

/// Detach an extracted item from its parse tree (owned strings) for caching.
pub(crate) fn own_item(item: OutlineItem<'_>) -> OutlineItem<'static> {
  OutlineItem {
    entry: own_entry(item.entry),
    is_import: item.is_import,
    is_exported: item.is_exported,
    members: item
      .members
      .into_iter()
      .map(|member| OutlineMember {
        entry: own_entry(member.entry),
        is_public: member.is_public,
      })
      .collect(),
  }
}

fn own_entry(entry: OutlineEntry<'_>) -> OutlineEntry<'static> {
  OutlineEntry {
    role: entry.role,
    symbol_type: entry.symbol_type,
    name: entry.name.into_owned().into(),
    range: entry.range,
    signature: entry.signature.into_owned().into(),
    ast_kind: entry.ast_kind.into_owned().into(),
  }
}

/// Filename-safe cache key for a source path.
pub fn cache_file_name(path: &str) -> String {
  format!("{}.json", blake3::hash(path.as_bytes()).to_hex())
}

pub fn save_product(path: &Path, product: &FileProduct) -> io::Result<()> {
  let bytes = serde_json::to_vec(product).map_err(io::Error::other)?;
  fs::write(path, bytes)
}

pub fn load_product(path: &Path) -> io::Result<FileProduct> {
  let bytes = fs::read(path)?;
  serde_json::from_slice(&bytes).map_err(io::Error::other)
}
