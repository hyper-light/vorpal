//! The reference model (use sites to be resolved).

use vorpal_kg::{EdgeType, NodeId};

use crate::intern::{Interner, NameId};

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
  /// A member access whose receiver is a plain name we cannot verify (`Foo.bar()`,
  /// `obj.helper()`): the receiver text rides along as a HINT. If it names an owner in the
  /// tree, the resolution is corroborated exactly like a static qualifier; if it names
  /// nothing, the reference falls back to [`RefForm::Method`] semantics — the hint can
  /// upgrade, never mask.
  MethodHinted,
}

/// One reference occurrence: node `from` (in file `from_path`) mentions `name` at `evidence`.
///
/// A plain-old-data record: names, paths, and qualifiers are process-interned ids
/// ([`crate::intern`]), so millions of references cost ~40 bytes each with zero per-reference
/// heap — at kernel scale this replaced ~700 MB of owned strings — and resolution compares
/// integers. Constructors still take `&str` and intern internally, so call sites read
/// unchanged; use [`intern::text_of`] when the text is needed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Reference<'i> {
  /// The node the reference occurs in (a function, method, …).
  pub from: NodeId,
  /// The file the reference occurs in — the intra-file scope key (interned path).
  pub from_path: NameId<'i>,
  /// The referenced name (simple, e.g. `parse`), interned.
  pub name: NameId<'i>,
  pub kind: RefKind,
  /// Byte span of the reference in `from_path`, kept as evidence.
  pub evidence: (u32, u32),
  /// The qualifying namespace/receiver the grammar provides, when any: `Kg::load` → `Kg`;
  /// `self.helper()` / `Self::helper()` → the enclosing item's name. `None` for bare names and
  /// for receivers whose type the syntax does not reveal.
  pub qualifier: Option<NameId<'i>>,
  /// The syntactic shape (see [`RefForm`]).
  pub form: RefForm,
  /// The receiver's file-locally bound type (G-M2), interned — capture's conservative
  /// verdict, never a resolver guess. `None` for untyped/complex/poisoned receivers.
  pub receiver_type: Option<NameId<'i>>,
  /// `vorpal-ingest` typefacts `BindOrigin` tag for `receiver_type` (`0xFF` = none): the
  /// resolver's confidence cap keys on how the type was learned.
  pub receiver_type_origin: u8,
  /// The local rebinding an aliased import introduces (`from x import y as z` → `z`;
  /// `use a::b as c` → `c`), interned. The reference's `name` stays the ORIGINAL (the edge
  /// targets the real definition); the alias is what the importing file's bare uses say, so
  /// the import-binding pre-pass keys on it.
  pub alias: Option<NameId<'i>>,
}

impl<'i> Reference<'i> {
  pub fn new(
    interner: &'i Interner,
    from: NodeId,
    from_path: &str,
    name: &str,
    kind: RefKind,
  ) -> Self {
    Self {
      from,
      from_path: interner.intern(from_path),
      name: interner.intern(name),
      kind,
      evidence: (0, 0),
      qualifier: None,
      form: RefForm::Bare,
      alias: None,
      receiver_type: None,
      receiver_type_origin: 0xFF,
    }
  }

  /// [`Reference::new`] with an already-interned path — apply loops intern each file's path
  /// once instead of re-hashing it per reference.
  pub fn with_interned_path(
    interner: &'i Interner,
    from: NodeId,
    from_path: NameId<'i>,
    name: &str,
    kind: RefKind,
  ) -> Self {
    Self {
      from,
      from_path,
      name: interner.intern(name),
      kind,
      evidence: (0, 0),
      qualifier: None,
      form: RefForm::Bare,
      alias: None,
      receiver_type: None,
      receiver_type_origin: 0xFF,
    }
  }

  pub fn with_evidence(mut self, start: u32, end: u32) -> Self {
    self.evidence = (start, end);
    self
  }

  pub fn with_qualifier(self, interner: &'i Interner, qualifier: Option<String>) -> Self {
    self.with_qualifier_ref(interner, qualifier.as_deref())
  }

  /// [`Reference::with_qualifier`] over a borrowed qualifier — the view-replay path, which
  /// interns straight from mapped bytes without an owned intermediate.
  pub fn with_qualifier_ref(mut self, interner: &'i Interner, qualifier: Option<&str>) -> Self {
    self.qualifier = qualifier.map(|q| interner.intern(q));
    self
  }

  /// Attach the captured receiver type (view-replay path: borrowed text, interned here).
  pub fn with_receiver_type_ref(
    mut self,
    interner: &'i Interner,
    ty: Option<&str>,
    origin: u8,
  ) -> Self {
    self.receiver_type = ty.map(|t| interner.intern(t));
    self.receiver_type_origin = if self.receiver_type.is_some() { origin } else { 0xFF };
    self
  }

  pub fn with_form(mut self, form: RefForm) -> Self {
    self.form = form;
    self
  }

  /// [`Reference::with_qualifier_ref`]'s sibling for the aliased-import rebinding.
  pub fn with_alias_ref(mut self, interner: &'i Interner, alias: Option<&str>) -> Self {
    self.alias = alias.map(|a| interner.intern(a));
    self
  }
}
