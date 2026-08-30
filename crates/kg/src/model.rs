//! Code-focused node kinds (§3.3), mapped from the extractor's `SymbolType`.

use vorpal_graph::EdgeType;
use vorpal_outline::model::SymbolType;

/// The kind of a code entity, stored as a `u8` in the node segment's HOT `kind` column.
#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SymbolKind {
  File = 0,
  Module = 1,
  Package = 2,
  Class = 3,
  Struct = 4,
  Interface = 5,
  Enum = 6,
  EnumMember = 7,
  Function = 8,
  Method = 9,
  Constructor = 10,
  Field = 11,
  Property = 12,
  Variable = 13,
  Constant = 14,
  Import = 15,
  TypeParameter = 16,
  /// An HTTP route registration — `GET /users/:id`; edges to its handler are `calls`.
  Route = 17,
  Other = 255,
}

impl SymbolKind {
  /// Parse a user-facing kind name (case-insensitive `Debug` spelling: `function`,
  /// `enummember`, …) — the CLI/MCP selector's `kind` filter.
  pub fn parse(text: &str) -> Option<Self> {
    let lower = text.to_ascii_lowercase();
    [
      Self::File,
      Self::Module,
      Self::Package,
      Self::Class,
      Self::Struct,
      Self::Interface,
      Self::Enum,
      Self::EnumMember,
      Self::Function,
      Self::Method,
      Self::Constructor,
      Self::Field,
      Self::Property,
      Self::Variable,
      Self::Constant,
      Self::Import,
      Self::TypeParameter,
      Self::Route,
      Self::Other,
    ]
    .into_iter()
    .find(|kind| format!("{kind:?}").to_ascii_lowercase() == lower)
  }

  pub fn tag(self) -> u8 {
    self as u8
  }

  /// Whether two same-named declarations of this kind can legitimately coexist and must be kept
  /// distinct by signature — i.e. callables that overload. Non-callable kinds (types, fields,
  /// imports, …) treat one name as one entity, so a `struct` and its `impl` block, or a type and
  /// a re-opened declaration, still share a single identity.
  pub fn is_overloadable(self) -> bool {
    matches!(
      self,
      SymbolKind::Function | SymbolKind::Method | SymbolKind::Constructor
    )
  }

  pub fn from_tag(tag: u8) -> Self {
    match tag {
      0 => SymbolKind::File,
      1 => SymbolKind::Module,
      2 => SymbolKind::Package,
      3 => SymbolKind::Class,
      4 => SymbolKind::Struct,
      5 => SymbolKind::Interface,
      6 => SymbolKind::Enum,
      7 => SymbolKind::EnumMember,
      8 => SymbolKind::Function,
      9 => SymbolKind::Method,
      10 => SymbolKind::Constructor,
      11 => SymbolKind::Field,
      12 => SymbolKind::Property,
      13 => SymbolKind::Variable,
      14 => SymbolKind::Constant,
      15 => SymbolKind::Import,
      16 => SymbolKind::TypeParameter,
      17 => SymbolKind::Route,
      _ => SymbolKind::Other,
    }
  }

  /// Map an extractor `SymbolType` (plus the import flag) to a code node kind.
  pub fn from_symbol_type(sym: SymbolType, is_import: bool) -> Self {
    if is_import {
      return SymbolKind::Import;
    }
    match sym {
      SymbolType::File => SymbolKind::File,
      SymbolType::Module | SymbolType::Namespace => SymbolKind::Module,
      SymbolType::Package => SymbolKind::Package,
      SymbolType::Class => SymbolKind::Class,
      SymbolType::Struct => SymbolKind::Struct,
      SymbolType::Interface => SymbolKind::Interface,
      SymbolType::Enum => SymbolKind::Enum,
      SymbolType::EnumMember => SymbolKind::EnumMember,
      SymbolType::Function => SymbolKind::Function,
      SymbolType::Method => SymbolKind::Method,
      SymbolType::Constructor => SymbolKind::Constructor,
      SymbolType::Field => SymbolKind::Field,
      SymbolType::Property => SymbolKind::Property,
      SymbolType::Variable => SymbolKind::Variable,
      SymbolType::Constant => SymbolKind::Constant,
      SymbolType::TypeParameter => SymbolKind::TypeParameter,
      SymbolType::Route => SymbolKind::Route,
      // Structural-language keys (JSON/YAML pairs, Nix bindings) read as properties.
      SymbolType::Key => SymbolKind::Property,
      _ => SymbolKind::Other,
    }
  }

  /// The containment edge for a member of this kind (§3.3 `has_method`/`has_field`/`defines`).
  pub fn containment_edge(self) -> EdgeType {
    match self {
      SymbolKind::Method | SymbolKind::Constructor => EdgeType::HAS_METHOD,
      SymbolKind::Field | SymbolKind::Property | SymbolKind::EnumMember => EdgeType::HAS_FIELD,
      _ => EdgeType::DEFINES,
    }
  }
}
