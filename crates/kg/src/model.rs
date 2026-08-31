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
  /// An event/message listener registration — `EVENT user.created`; edges to its handler
  /// are `calls`, and emitters reach it through `notifies`.
  Channel = 18,
  /// A macro definition — C/C++/ObjC `#define`, Rust `macro_rules!`, Swift macros,
  /// Erlang `-define`, CMake `macro()`, … (extraction-coverage campaign 2026-08-31:
  /// the kernel alone holds 6.1M `#define`s; "sees everything" includes them).
  Macro = 19,
  /// A union type definition (C/C++ `union`, Rust `union`, Zig `union`).
  Union = 20,
  /// A type alias — C/C++ `typedef`/`using X = Y`, Rust `type`, Go/Kotlin/Swift/
  /// Dart aliases.
  TypeAlias = 21,
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
      Self::Channel,
      Self::Macro,
      Self::Union,
      Self::TypeAlias,
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

  /// THE candidate law: whether definitions of this kind enter the symbol table as
  /// name-resolution candidates. One definition, consulted by every table feed —
  /// never hand-spelled at call sites (a new feed that forgets a special case is
  /// the silent-gap failure class).
  ///
  /// * `File` — path map, not a name candidate (targets of path-form imports).
  /// * `Import` — wiring, not a definition: offering it as a target let a
  ///   `use foo` steal edges meant for the real `foo`.
  ///
  /// Macros ARE candidates — but they bind by INCLUSION, not name-globality: the
  /// resolver's include-reachability gate admits a macro candidate only when its
  /// defining file is the reference's own file or transitively included by it
  /// (`vorpal_resolve::IncludeReach`). Name-global macro candidacy measured as
  /// 8.1M ambiguous call edges from 48 vendored parser.c copies; the gate turns
  /// those same-named duplicates into unique, correct resolutions.
  pub fn is_resolution_candidate(self) -> bool {
    !matches!(self, SymbolKind::File | SymbolKind::Import)
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
      18 => SymbolKind::Channel,
      19 => SymbolKind::Macro,
      20 => SymbolKind::Union,
      21 => SymbolKind::TypeAlias,
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
      SymbolType::Channel => SymbolKind::Channel,
      SymbolType::Macro => SymbolKind::Macro,
      SymbolType::Union => SymbolKind::Union,
      SymbolType::TypeAlias => SymbolKind::TypeAlias,
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
