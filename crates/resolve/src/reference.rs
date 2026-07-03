//! The reference model (use sites to be resolved).

use vorpal_kg::{EdgeType, NodeId};

/// What a reference is, which fixes the edge kind it resolves to (§3.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RefKind {
  /// A call site → `calls`.
  Call,
  /// A type mention → `of_type`.
  Type,
  /// An import → `imports`.
  Import,
  /// A generic use/reference → `references`.
  Use,
}

impl RefKind {
  pub fn edge(self) -> EdgeType {
    match self {
      RefKind::Call => EdgeType::CALLS,
      RefKind::Type => EdgeType::OF_TYPE,
      RefKind::Import => EdgeType::IMPORTS,
      RefKind::Use => EdgeType::REFERENCES,
    }
  }
}

/// One reference occurrence: node `from` (in file `from_path`) mentions `name` at `evidence`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reference {
  /// The node the reference occurs in (a function, method, …).
  pub from: NodeId,
  /// The file the reference occurs in — the intra-file scope key.
  pub from_path: String,
  /// The referenced name (simple, e.g. `parse`; qualified names carry a `.`).
  pub name: String,
  pub kind: RefKind,
  /// Byte span of the reference in `from_path`, kept as evidence.
  pub evidence: (u32, u32),
}

impl Reference {
  pub fn new(
    from: NodeId,
    from_path: impl Into<String>,
    name: impl Into<String>,
    kind: RefKind,
  ) -> Self {
    Self {
      from,
      from_path: from_path.into(),
      name: name.into(),
      kind,
      evidence: (0, 0),
    }
  }

  pub fn with_evidence(mut self, start: u32, end: u32) -> Self {
    self.evidence = (start, end);
    self
  }
}
