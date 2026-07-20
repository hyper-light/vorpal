//! The reference model (use sites to be resolved).

use std::sync::Arc;

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
  /// An implements/extends relation → `implements`.
  Implements,
  /// A generic use/reference → `references`.
  Use,
}

impl RefKind {
  pub fn edge(self) -> EdgeType {
    match self {
      RefKind::Call => EdgeType::CALLS,
      RefKind::Type => EdgeType::OF_TYPE,
      RefKind::Import => EdgeType::IMPORTS,
      RefKind::Implements => EdgeType::IMPLEMENTS,
      RefKind::Use => EdgeType::REFERENCES,
    }
  }
}

/// The syntactic shape of a reference — how much target evidence the grammar itself provides.
/// It governs how much ambiguity resolution may tolerate (§3.3 "approximate edges are labeled,
/// never faked"): a bare name is a lexical lookup where a labeled approximate pick is
/// acceptable; a member access on an untyped value proves nothing beyond the name, so only a
/// *unique* match may bind.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum RefForm {
  /// A bare identifier (`helper()`): plain lexical lookup.
  #[default]
  Bare,
  /// A static path (`Kg::load`, `module::helper`): the qualifier is a namespace by grammar.
  Static,
  /// A member access on a value (`x.helper()`): without receiver typing, the name must be
  /// unique among visible definitions to bind.
  Method,
}

/// One reference occurrence: node `from` (in file `from_path`) mentions `name` at `evidence`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reference {
  /// The node the reference occurs in (a function, method, …).
  pub from: NodeId,
  /// The file the reference occurs in — the intra-file scope key. Shared, not owned: at
  /// kernel scale a graph carries millions of references, and one `Arc<str>` per *file*
  /// (8-byte clones per reference) replaces what was a duplicated path `String` per
  /// reference — roughly a gigabyte of pure duplication on the Linux tree.
  pub from_path: Arc<str>,
  /// The referenced name (simple, e.g. `parse`; qualified names carry a `.`).
  pub name: String,
  pub kind: RefKind,
  /// Byte span of the reference in `from_path`, kept as evidence.
  pub evidence: (u32, u32),
  /// The qualifying namespace/receiver the grammar provides, when any: `Kg::load` → `Kg`;
  /// `self.helper()` / `Self::helper()` → the enclosing item's name. `None` for bare names and
  /// for receivers whose type the syntax does not reveal.
  pub qualifier: Option<String>,
  /// The syntactic shape (see [`RefForm`]).
  pub form: RefForm,
}

impl Reference {
  pub fn new(
    from: NodeId,
    from_path: impl Into<Arc<str>>,
    name: impl Into<String>,
    kind: RefKind,
  ) -> Self {
    Self {
      from,
      from_path: from_path.into(),
      name: name.into(),
      kind,
      evidence: (0, 0),
      qualifier: None,
      form: RefForm::Bare,
    }
  }

  pub fn with_evidence(mut self, start: u32, end: u32) -> Self {
    self.evidence = (start, end);
    self
  }

  pub fn with_qualifier(mut self, qualifier: Option<String>) -> Self {
    self.qualifier = qualifier;
    self
  }

  pub fn with_form(mut self, form: RefForm) -> Self {
    self.form = form;
    self
  }
}
