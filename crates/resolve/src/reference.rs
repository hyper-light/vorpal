//! The reference model (use sites to be resolved).

use vorpal_kg::{EdgeType, NodeId};

use crate::intern::{self, NameId};

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
///
/// A plain-old-data record: names, paths, and qualifiers are process-interned ids
/// ([`crate::intern`]), so millions of references cost ~40 bytes each with zero per-reference
/// heap — at kernel scale this replaced ~700 MB of owned strings — and resolution compares
/// integers. Constructors still take `&str` and intern internally, so call sites read
/// unchanged; use [`intern::text_of`] when the text is needed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Reference {
  /// The node the reference occurs in (a function, method, …).
  pub from: NodeId,
  /// The file the reference occurs in — the intra-file scope key (interned path).
  pub from_path: NameId,
  /// The referenced name (simple, e.g. `parse`), interned.
  pub name: NameId,
  pub kind: RefKind,
  /// Byte span of the reference in `from_path`, kept as evidence.
  pub evidence: (u32, u32),
  /// The qualifying namespace/receiver the grammar provides, when any: `Kg::load` → `Kg`;
  /// `self.helper()` / `Self::helper()` → the enclosing item's name. `None` for bare names and
  /// for receivers whose type the syntax does not reveal.
  pub qualifier: Option<NameId>,
  /// The syntactic shape (see [`RefForm`]).
  pub form: RefForm,
  /// The local rebinding an aliased import introduces (`from x import y as z` → `z`;
  /// `use a::b as c` → `c`), interned. The reference's `name` stays the ORIGINAL (the edge
  /// targets the real definition); the alias is what the importing file's bare uses say, so
  /// the import-binding pre-pass keys on it.
  pub alias: Option<NameId>,
}

impl Reference {
  pub fn new(from: NodeId, from_path: &str, name: &str, kind: RefKind) -> Self {
    Self {
      from,
      from_path: intern::intern(from_path),
      name: intern::intern(name),
      kind,
      evidence: (0, 0),
      qualifier: None,
      form: RefForm::Bare,
      alias: None,
    }
  }

  /// [`Reference::new`] with an already-interned path — apply loops intern each file's path
  /// once instead of re-hashing it per reference.
  pub fn with_interned_path(from: NodeId, from_path: NameId, name: &str, kind: RefKind) -> Self {
    Self {
      from,
      from_path,
      name: intern::intern(name),
      kind,
      evidence: (0, 0),
      qualifier: None,
      form: RefForm::Bare,
      alias: None,
    }
  }

  pub fn with_evidence(mut self, start: u32, end: u32) -> Self {
    self.evidence = (start, end);
    self
  }

  pub fn with_qualifier(self, qualifier: Option<String>) -> Self {
    self.with_qualifier_ref(qualifier.as_deref())
  }

  /// [`Reference::with_qualifier`] over a borrowed qualifier — the view-replay path, which
  /// interns straight from mapped bytes without an owned intermediate.
  pub fn with_qualifier_ref(mut self, qualifier: Option<&str>) -> Self {
    self.qualifier = qualifier.map(intern::intern);
    self
  }

  pub fn with_form(mut self, form: RefForm) -> Self {
    self.form = form;
    self
  }

  /// [`Reference::with_qualifier_ref`]'s sibling for the aliased-import rebinding.
  pub fn with_alias_ref(mut self, alias: Option<&str>) -> Self {
    self.alias = alias.map(intern::intern);
    self
  }
}
