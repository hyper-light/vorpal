//! The symbol table: name → candidate definitions, built from the KG's definition nodes.

use std::collections::HashMap;

use vorpal_kg::{Kg, NodeId, SymbolKind};

/// A definition candidate for resolution.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Symbol {
  pub id: NodeId,
  pub kind: SymbolKind,
  /// The file the definition lives in — used for intra-file scoping.
  pub path: String,
  /// Whether the definition is visible across files.
  pub exported: bool,
  /// The containing definition's name for members (`Kg` for `Kg.load`), `None` for top-level
  /// items — the target side of qualified-reference matching (§3.3).
  pub owner: Option<String>,
}

/// Maps a name to every definition with that name (§3.3 candidate set), plus an exact-path map
/// of file nodes for path-form import resolution.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct SymbolTable {
  by_name: HashMap<String, Vec<Symbol>>,
  files: HashMap<String, NodeId>,
}

impl SymbolTable {
  pub fn new() -> Self {
    Self::default()
  }

  pub fn insert(&mut self, name: &str, symbol: Symbol) {
    self
      .by_name
      .entry(name.to_owned())
      .or_default()
      .push(symbol);
  }

  /// Register a file node by its exact ingested path (the target of path-form imports).
  pub fn insert_file(&mut self, path: &str, id: NodeId) {
    self.files.insert(path.to_owned(), id);
  }

  /// The file node at exactly `path`, if indexed.
  pub fn file(&self, path: &str) -> Option<NodeId> {
    self.files.get(path).copied()
  }

  /// Merge another table's entries after this one's — the ordered-absorption step of a §7.5
  /// sharded table build. Same-named candidate lists concatenate in absorption order, so
  /// absorbing row-range shards in row order reproduces the serial insertion order exactly.
  /// (File paths and canonical identities are disjoint across shards by construction.)
  pub fn absorb(&mut self, other: SymbolTable) {
    for (name, symbols) in other.by_name {
      self.by_name.entry(name).or_default().extend(symbols);
    }
    self.files.extend(other.files);
  }

  /// Build a table from every node in a sealed [`Kg`]. `File` nodes go to the path map (targets
  /// of path-form imports); import/alias nodes are wiring, not definitions, and are never
  /// candidates; every other definition goes to the name candidate set, with its containment
  /// parent (when not the file) recorded as `owner`.
  pub fn from_kg(kg: &Kg) -> Self {
    let mut table = Self::new();
    for i in 0..kg.node_count() as u64 {
      let id = NodeId::new(i);
      if let Some(node) = kg.node(id) {
        if node.kind == SymbolKind::File {
          table.insert_file(node.path, id);
          continue;
        }
        if node.kind == SymbolKind::Import {
          continue;
        }
        let owner = kg.container_of(id).and_then(|cid| {
          let container = kg.node(cid)?;
          (container.kind != SymbolKind::File).then(|| container.name.to_owned())
        });
        table.insert(
          node.name,
          Symbol {
            id,
            kind: node.kind,
            path: node.path.to_owned(),
            exported: node.exported,
            owner,
          },
        );
      }
    }
    table
  }

  /// Every definition carrying `name` (the candidate set for resolution).
  pub fn candidates(&self, name: &str) -> &[Symbol] {
    self.by_name.get(name).map(Vec::as_slice).unwrap_or(&[])
  }

  /// Total distinct names in the table.
  pub fn names(&self) -> usize {
    self.by_name.len()
  }
}
