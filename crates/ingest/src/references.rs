//! AST reference extraction (§3.1): call sites and imports across every supported language.
//!
//! Walks the tree-sitter parse for call and import nodes (AST-based, never substring matching),
//! extracts the referenced identifier, and attributes it to a definition scope: **calls** to the
//! innermost enclosing item/member, **imports** to the outermost scope (the file node), so
//! `importers_of` reports files.
//!
//! The per-language [`RefSpec`] table covers four verified grammar shapes:
//! - **multi-kind callees** (PHP's four call kinds carry different callee fields),
//! - **positional callees** (Kotlin/Swift/HCL call nodes have no callee field),
//! - **defs-are-calls** (Elixir `def`/`defmodule` are `call` nodes; Ruby `require`, Lua
//!   `require`, Bash `source` are calls that mean *import*) via callee-text rules,
//! - **string imports** (`import "./x"`): the quote-stripped module string is kept verbatim so
//!   path-like names stay honestly unresolved rather than faking an edge to a same-named symbol.
//!
//! All node kinds and field names come from each pinned grammar's `node-types.json`.

use std::borrow::Cow;
use std::collections::{HashMap, HashSet};
use std::ops::Range;
use std::sync::LazyLock;

use vorpal_core::tree_sitter::StrDoc;
use vorpal_core::{Language, Node};
use vorpal_kg::{EntityIdentity, NodeId};
use vorpal_lang_registry::SgLang;
use vorpal_language::SupportLang;
use vorpal_resolve::{RefForm, RefKind};

type SgNode<'t> = Node<'t, StrDoc<SgLang>>;
/// The walk's node type, exported for the typefacts capture module (same doc, same lifetime).
pub(crate) type SgNodeAlias<'t> = SgNode<'t>;

/// One extracted reference, file-locally attributed: `from` indexes the file's local
/// definition layout (see `local_layout`). Deliberately path-free — the enclosing file's path
/// is applied once at link time instead of being allocated per reference at extraction time
/// (~30k dead `String`s per index of this repo under the previous `Reference`-based emission).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RawRef<'t> {
  pub(crate) from: NodeId,
  pub(crate) name: Cow<'t, str>,
  pub(crate) kind: RefKind,
  pub(crate) start: u32,
  pub(crate) end: u32,
  pub(crate) qualifier: Option<Cow<'t, str>>,
  pub(crate) form: RefForm,
  /// Aliased-import local rebinding (`as z`), when the grammar provides one.
  pub(crate) alias: Option<Cow<'t, str>>,
  /// The receiver's SIMPLE spelling for method-form calls (`x.helper()` → `x`), captured
  /// only when the receiver is a bare name a file-local binding could type (G-M1). Complex
  /// receiver expressions stay `None` — never guessed at.
  pub(crate) receiver: Option<Cow<'t, str>>,
  /// Per-argument records at the call site (G-M1, consumed by data-flow in G-M3).
  pub(crate) args: Vec<RawArg<'t>>,
}

/// One call-site argument: position, traceability class, keyword name (Python), and — for
/// traceable classes only — the expression text capped at 64 bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RawArg<'t> {
  pub(crate) index: u16,
  pub(crate) class: ArgClass,
  pub(crate) kw_name: Option<Cow<'t, str>>,
  pub(crate) expr: Option<Cow<'t, str>>,
}

/// Argument shape classification — what a static data-flow pass can and cannot follow.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ArgClass {
  /// A bare variable name: traceable.
  Var = 0,
  /// A field/member access chain: traceable as an access path.
  FieldAccess = 1,
  /// The result of a nested call: traceable one hop (the producing call).
  CallResult = 2,
  /// A literal: a value, not a flow.
  Literal = 3,
  /// Anything else (arithmetic, closures, comprehensions …): opaque.
  Other = 4,
}

impl<'t> RawRef<'t> {
  fn plain(from: NodeId, name: Cow<'t, str>, kind: RefKind, start: u32, end: u32) -> Self {
    Self {
      from,
      name,
      receiver: None,
      args: Vec::new(),
      kind,
      start,
      end,
      qualifier: None,
      form: RefForm::Bare,
      alias: None,
    }
  }
}

/// How to locate the referenced sub-node inside a matched call/import node.
#[derive(Clone, Copy)]
enum Sel {
  /// `node.field(name)` — the common case.
  Field(&'static str),
  /// Last child of a (possibly repeated) field — e.g. the final segment of Scala's import path.
  FieldLast(&'static str),
  /// First *named* child — for fieldless positional grammars (Kotlin, Swift, HCL).
  FirstNamedChild,
  /// First descendants (pre-order, not descending into matches) whose kind is listed — for
  /// fieldless imports (Java `import_declaration`, PHP `namespace_use_declaration`).
  ChildOfKind(&'static [&'static str]),
}

#[derive(Clone, Copy)]
struct CallSpec {
  kind: &'static str,
  callee: Sel,
  /// Field on the *call node* holding the receiver value, for grammars where the callee
  /// selector skips it (Java `method_invocation.object`, PHP `member_call_expression.object`).
  receiver_field: Option<&'static str>,
  /// Field on the *call node* holding a static scope (PHP `scoped_call_expression.scope`).
  scope_field: Option<&'static str>,
}

/// The common case: form evidence (if any) lives inside the callee expression itself.
const CALL_DEFAULTS: CallSpec = CallSpec {
  kind: "",
  callee: Sel::FirstNamedChild,
  receiver_field: None,
  scope_field: None,
};

/// Where an import construct grammar-provides the module its target comes from. Captured as a
/// qualifier so resolution can corroborate the target against that module's file (or owner)
/// instead of guessing among same-named definitions — and can refuse to bind at all when the
/// named module is outside the corpus (`from vendored import parse` must not edge to a
/// coincidentally-named local `parse`).
#[derive(Clone, Copy)]
enum QualSource {
  /// No usable qualifier: the import target resolves as a bare name.
  None,
  /// A field on the import node names the source module (`from a.b import c` → `module_name`
  /// field, qualifier `b` — the final segment, matching per-file module stems).
  NodeField(&'static str),
  /// The selected target is (or, via a fieldless wrapper like `use_as_clause`, wraps) a scoped
  /// path whose `path` prefix qualifies its final segment (`use crate::r_def::r_target` →
  /// qualifier `r_def`).
  TargetPath,
}

struct ImportSpec {
  kind: &'static str,
  target: Sel,
  /// The target is a string/path literal (strip delimiters, keep the module string verbatim).
  string_target: bool,
  /// Where the import's source-module qualifier lives, when the grammar provides one.
  qualifier: QualSource,
}

/// Classification of a call by its extracted callee text.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum TextAction {
  /// A definition form (`def`, `defmodule`, …): emit nothing and suppress the definition-head
  /// call (`def foo(x)` parses `foo(x)` as a call — it is a definition, not a call site).
  SkipDefinition,
  /// The call imports its first argument (`require 'x'`, `source ./x`, `alias Foo.Bar`).
  ImportFirstArg,
}

/// Where a route construct's handler name lives.
#[derive(Clone, Copy)]
enum HandlerAt {
  /// The last argument that names something (Express `app.get("/x", auth, handler)`, Go
  /// `HandleFunc("/x", h)`, Django `path("x", views.detail, name=…)`); closures, objects,
  /// literals, and keyword arguments name nothing and are skipped.
  LastArgument,
  /// The argument at `index` (0-based, named children), unwrapped once when it is itself a
  /// call — axum's `route("/x", get(handler))` names the handler inside `get(…)`.
  UnwrappedArgument(u8),
  /// The declaration the construct decorates: the nearest ancestor of one of `ancestors`
  /// (optionally through its `via` field), whose `name` field is the handler.
  DecoratedDefinition {
    ancestors: &'static [&'static str],
    via: Option<&'static str>,
  },
  /// The next named sibling of one of `kinds` — its `name` field is the handler (Rust
  /// attribute items, TypeScript method decorators).
  NextSibling(&'static [&'static str]),
}

/// An HTTP route registration construct. The outline rule with the same predicate creates
/// the `Route` item spanning the construct; the walk emits a `calls` reference from that
/// item to the handler, so a route "calls" its handler like any other caller and every
/// caller/reachability/impact surface sees endpoints without special cases.
struct RouteSpec {
  kind: &'static str,
  /// Navigation from the construct to the node whose name is the verb or registrar (`get`,
  /// `HandleFunc`, `GetMapping`), checked against `names`.
  name: &'static [Sel],
  names: &'static [&'static str],
  /// Navigation to the argument list holding the path literal.
  args: &'static [Sel],
  /// Accept any string literal as the path. Otherwise the literal must start with `/` or be
  /// a `VERB /path` pattern — the shape that separates `app.get("/x", h)` from `map.get(k)`.
  path_any: bool,
  handler: HandlerAt,
}

/// An HTTP client call site (`fetch("/api/users")`, `requests.get(url)`): matching nodes
/// record a request (method + literal URL) for link-time matching against `Route` nodes.
/// Literal URLs only — a URL built from variables records nothing.
struct RequestSpec {
  kind: &'static str,
  /// Navigation to the callee node.
  name: &'static [Sel],
  /// Callee names that ARE the HTTP verb (`get`, `Post`).
  verb_names: &'static [&'static str],
  /// Callee names implying GET (`fetch` without an options argument is a GET).
  get_names: &'static [&'static str],
  /// Callee names that EMIT an event (`emit`, `publish`): the record's method is `EVENT`
  /// and the first string literal (any shape) is the topic — matched against `Channel`
  /// registrations, where fan-out is expected and every match links.
  event_names: &'static [&'static str],
  /// Receiver spellings that mark an HTTP client (`axios`, `requests`, `http`); empty
  /// means the callee name alone suffices (`fetch`).
  receivers: &'static [&'static str],
  args: &'static [Sel],
  /// The argument (0-based, named children) carrying the method as a string literal
  /// (`http.NewRequest("GET", url, …)`); the verb from the name is ignored then.
  method_from_arg: Option<u8>,
}

/// An implements/extends construct: matching nodes emit `implements` references for their type
/// targets (`impl Trait for T`, `class C implements I`, `class Sub extends Base`).
struct ImplSpec {
  kind: &'static str,
  /// Where the implemented types live; `None` collects type leaves from the node itself.
  target: Option<Sel>,
}

pub(crate) struct RefSpec {
  calls: &'static [CallSpec],
  imports: &'static [ImportSpec],
  /// `(callee text, action)` — exact match, applied after callee extraction succeeds.
  text_rules: &'static [(&'static str, TextAction)],
  /// Leaf kinds marking a type USE (grammars with a distinct `type_identifier`); definition
  /// names and implements-construct targets are excluded by the walk.
  types: &'static [&'static str],
  implements: &'static [ImplSpec],
  /// Node kinds declaring generic type parameters (`type_parameters`, …). Names they bind are
  /// *binders*, not type uses: every mention within the declaring item's span is suppressed
  /// (`fn f<T>(x: T) -> T` emits no `of_type` reference at all for `T`).
  type_params: &'static [&'static str],
  /// Type-position keywords/placeholders that are never definitions (`Self`, `_`): emitting
  /// them as uses would only manufacture forever-unresolved references.
  type_placeholders: &'static [&'static str],
  /// Callee-expression kinds that are static namespace paths (`Kg::load`) — their path part is
  /// grammar-guaranteed namespace evidence.
  static_callee_kinds: &'static [&'static str],
  /// Callee-expression kinds that are member accesses on a value (`x.helper()`) — the receiver
  /// is opaque unless it is a self-receiver keyword.
  method_callee_kinds: &'static [&'static str],
  /// Receiver spellings that denote the enclosing type (`self`, `this`, `$this`).
  self_receivers: &'static [&'static str],
  /// HTTP route registration constructs (see [`RouteSpec`]).
  routes: &'static [RouteSpec],
  /// HTTP client call constructs (see [`RequestSpec`]).
  requests: &'static [RequestSpec],
}

const NONE_TEXT: &[(&str, TextAction)] = &[];
const NO_TYPES: &[&str] = &[];
const NO_IMPL: &[ImplSpec] = &[];
const NO_KINDS: &[&str] = &[];
const TYPE_ID: &[&str] = &["type_identifier"];

/// Baseline spec: no extraction of any kind; language tables override what they support.
const SPEC_DEFAULTS: RefSpec = RefSpec {
  calls: &[],
  imports: &[],
  text_rules: NONE_TEXT,
  types: NO_TYPES,
  implements: NO_IMPL,
  type_params: NO_KINDS,
  type_placeholders: NO_KINDS,
  static_callee_kinds: NO_KINDS,
  method_callee_kinds: NO_KINDS,
  self_receivers: NO_KINDS,
  routes: &[],
  requests: &[],
};

// ---- Owned spec data (F-M4) ---------------------------------------------------------------
//
// The walk consumes these owned twins so specs can come from *data* (serialized YAML for
// dynamic languages) as well as from the `&'static` authoring consts above. The consts remain
// the builtin authoring format; `From<&RefSpec>` lifts them into data once, at dispatch-table
// build. Field names mirror the static structs 1:1 so the walk reads identically.

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum SelData {
  Field(String),
  FieldLast(String),
  FirstNamedChild,
  ChildOfKind(Vec<String>),
}

impl From<&Sel> for SelData {
  fn from(sel: &Sel) -> Self {
    match sel {
      Sel::Field(name) => SelData::Field((*name).into()),
      Sel::FieldLast(name) => SelData::FieldLast((*name).into()),
      Sel::FirstNamedChild => SelData::FirstNamedChild,
      Sel::ChildOfKind(kinds) => {
        SelData::ChildOfKind(kinds.iter().map(|k| (*k).into()).collect())
      }
    }
  }
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum QualSourceData {
  None,
  NodeField(String),
  TargetPath,
}

impl From<QualSource> for QualSourceData {
  fn from(source: QualSource) -> Self {
    match source {
      QualSource::None => QualSourceData::None,
      QualSource::NodeField(field) => QualSourceData::NodeField(field.into()),
      QualSource::TargetPath => QualSourceData::TargetPath,
    }
  }
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct CallSpecData {
  pub(crate) kind: String,
  pub(crate) callee: SelData,
  pub(crate) receiver_field: Option<String>,
  pub(crate) scope_field: Option<String>,
}

impl From<&CallSpec> for CallSpecData {
  fn from(spec: &CallSpec) -> Self {
    Self {
      kind: spec.kind.into(),
      callee: SelData::from(&spec.callee),
      receiver_field: spec.receiver_field.map(Into::into),
      scope_field: spec.scope_field.map(Into::into),
    }
  }
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ImportSpecData {
  pub(crate) kind: String,
  pub(crate) target: SelData,
  pub(crate) string_target: bool,
  pub(crate) qualifier: QualSourceData,
}

impl From<&ImportSpec> for ImportSpecData {
  fn from(spec: &ImportSpec) -> Self {
    Self {
      kind: spec.kind.into(),
      target: SelData::from(&spec.target),
      string_target: spec.string_target,
      qualifier: spec.qualifier.into(),
    }
  }
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ImplSpecData {
  pub(crate) kind: String,
  pub(crate) target: Option<SelData>,
}

impl From<&ImplSpec> for ImplSpecData {
  fn from(spec: &ImplSpec) -> Self {
    Self {
      kind: spec.kind.into(),
      target: spec.target.as_ref().map(SelData::from),
    }
  }
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum HandlerAtData {
  LastArgument,
  UnwrappedArgument(u8),
  DecoratedDefinition {
    ancestors: Vec<String>,
    via: Option<String>,
  },
  NextSibling(Vec<String>),
}

impl From<&HandlerAt> for HandlerAtData {
  fn from(at: &HandlerAt) -> Self {
    let owned = |list: &[&str]| -> Vec<String> { list.iter().map(|s| (*s).into()).collect() };
    match at {
      HandlerAt::LastArgument => HandlerAtData::LastArgument,
      HandlerAt::UnwrappedArgument(index) => HandlerAtData::UnwrappedArgument(*index),
      HandlerAt::DecoratedDefinition { ancestors, via } => HandlerAtData::DecoratedDefinition {
        ancestors: owned(ancestors),
        via: via.map(Into::into),
      },
      HandlerAt::NextSibling(kinds) => HandlerAtData::NextSibling(owned(kinds)),
    }
  }
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct RouteSpecData {
  pub(crate) kind: String,
  pub(crate) name: Vec<SelData>,
  pub(crate) names: Vec<String>,
  pub(crate) args: Vec<SelData>,
  pub(crate) path_any: bool,
  pub(crate) handler: HandlerAtData,
}

impl From<&RouteSpec> for RouteSpecData {
  fn from(spec: &RouteSpec) -> Self {
    Self {
      kind: spec.kind.into(),
      name: spec.name.iter().map(SelData::from).collect(),
      names: spec.names.iter().map(|s| (*s).into()).collect(),
      args: spec.args.iter().map(SelData::from).collect(),
      path_any: spec.path_any,
      handler: HandlerAtData::from(&spec.handler),
    }
  }
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct RequestSpecData {
  pub(crate) kind: String,
  pub(crate) name: Vec<SelData>,
  pub(crate) verb_names: Vec<String>,
  pub(crate) get_names: Vec<String>,
  pub(crate) event_names: Vec<String>,
  pub(crate) receivers: Vec<String>,
  pub(crate) args: Vec<SelData>,
  pub(crate) method_from_arg: Option<u8>,
}

impl From<&RequestSpec> for RequestSpecData {
  fn from(spec: &RequestSpec) -> Self {
    let owned = |list: &[&str]| -> Vec<String> { list.iter().map(|s| (*s).into()).collect() };
    Self {
      kind: spec.kind.into(),
      name: spec.name.iter().map(SelData::from).collect(),
      verb_names: owned(spec.verb_names),
      get_names: owned(spec.get_names),
      event_names: owned(spec.event_names),
      receivers: owned(spec.receivers),
      args: spec.args.iter().map(SelData::from).collect(),
      method_from_arg: spec.method_from_arg,
    }
  }
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct RefSpecData {
  pub(crate) calls: Vec<CallSpecData>,
  pub(crate) imports: Vec<ImportSpecData>,
  pub(crate) text_rules: Vec<(String, TextAction)>,
  pub(crate) types: Vec<String>,
  pub(crate) implements: Vec<ImplSpecData>,
  pub(crate) type_params: Vec<String>,
  pub(crate) type_placeholders: Vec<String>,
  pub(crate) static_callee_kinds: Vec<String>,
  pub(crate) method_callee_kinds: Vec<String>,
  pub(crate) self_receivers: Vec<String>,
  pub(crate) routes: Vec<RouteSpecData>,
  pub(crate) requests: Vec<RequestSpecData>,
}

impl From<&RefSpec> for RefSpecData {
  fn from(spec: &RefSpec) -> Self {
    let owned = |list: &[&str]| -> Vec<String> { list.iter().map(|s| (*s).into()).collect() };
    Self {
      calls: spec.calls.iter().map(CallSpecData::from).collect(),
      imports: spec.imports.iter().map(ImportSpecData::from).collect(),
      text_rules: spec
        .text_rules
        .iter()
        .map(|(text, action)| ((*text).into(), *action))
        .collect(),
      types: owned(spec.types),
      implements: spec.implements.iter().map(ImplSpecData::from).collect(),
      type_params: owned(spec.type_params),
      type_placeholders: owned(spec.type_placeholders),
      static_callee_kinds: owned(spec.static_callee_kinds),
      method_callee_kinds: owned(spec.method_callee_kinds),
      self_receivers: owned(spec.self_receivers),
      routes: spec.routes.iter().map(RouteSpecData::from).collect(),
      requests: spec.requests.iter().map(RequestSpecData::from).collect(),
    }
  }
}

const RUST: RefSpec = RefSpec {
  calls: &[CallSpec {
    kind: "call_expression",
    callee: Sel::Field("function"),
    ..CALL_DEFAULTS
  }],
  imports: &[ImportSpec {
    kind: "use_declaration",
    target: Sel::Field("argument"),
    string_target: false,
    qualifier: QualSource::TargetPath,
  }],
  types: TYPE_ID,
  implements: &[ImplSpec {
    kind: "impl_item",
    target: Some(Sel::Field("trait")),
  }],
  type_params: &["type_parameters"],
  // `Self` names the enclosing impl type; `_` is inference. Neither can be a definition.
  type_placeholders: &["Self", "_"],
  static_callee_kinds: &["scoped_identifier"],
  method_callee_kinds: &["field_expression"],
  self_receivers: &["self"],
  routes: &[
    // axum: `.route("/x", get(handler))` — the handler sits inside the method call.
    RouteSpec {
      kind: "call_expression",
      name: &[Sel::Field("function")],
      names: &["route"],
      args: &[Sel::Field("arguments")],
      path_any: false,
      handler: HandlerAt::UnwrappedArgument(1),
    },
    // actix-web / rocket: `#[get("/x")]` decorates the next function item.
    RouteSpec {
      kind: "attribute_item",
      name: &[
        Sel::ChildOfKind(&["attribute"]),
        Sel::ChildOfKind(&["identifier", "scoped_identifier"]),
      ],
      names: &["get", "post", "put", "delete", "patch", "head", "options", "route"],
      args: &[Sel::ChildOfKind(&["attribute"]), Sel::Field("arguments")],
      path_any: false,
      handler: HandlerAt::NextSibling(&["function_item"]),
    },
  ],
  requests: &[RequestSpec {
    // reqwest: client.get("http://…"), reqwest::get(…).
    kind: "call_expression",
    name: &[Sel::Field("function")],
    verb_names: &["get", "post", "put", "delete", "patch", "head"],
    get_names: &[],
    event_names: &[],
    receivers: &["client", "reqwest", "http_client"],
    args: &[Sel::Field("arguments")],
    method_from_arg: None,
  }],
  ..SPEC_DEFAULTS
};

const PYTHON: RefSpec = RefSpec {
  calls: &[CallSpec {
    kind: "call",
    callee: Sel::Field("function"),
    ..CALL_DEFAULTS
  }],
  imports: &[
    ImportSpec {
      kind: "import_from_statement",
      target: Sel::Field("name"),
      string_target: false,
      qualifier: QualSource::NodeField("module_name"),
    },
    ImportSpec {
      kind: "import_statement",
      target: Sel::ChildOfKind(&["dotted_name", "aliased_import"]),
      string_target: false,
      qualifier: QualSource::None,
    },
  ],
  implements: &[ImplSpec {
    kind: "class_definition",
    target: Some(Sel::Field("superclasses")),
  }],
  method_callee_kinds: &["attribute"],
  self_receivers: &["self", "cls"],
  routes: &[
    // Flask / FastAPI: `@app.get("/items/{id}")` decorates the definition below it.
    RouteSpec {
      kind: "decorator",
      name: &[Sel::ChildOfKind(&["call"]), Sel::Field("function")],
      names: &[
        "get", "post", "put", "delete", "patch", "head", "options", "route", "api_route",
        "websocket",
      ],
      args: &[Sel::ChildOfKind(&["call"]), Sel::Field("arguments")],
      path_any: false,
      handler: HandlerAt::DecoratedDefinition {
        ancestors: &["decorated_definition"],
        via: Some("definition"),
      },
    },
    // Django: `path("users/<int:id>/", views.detail)` inside `urlpatterns`.
    RouteSpec {
      kind: "call",
      name: &[Sel::Field("function")],
      names: &["path"],
      args: &[Sel::Field("arguments")],
      path_any: true,
      handler: HandlerAt::LastArgument,
    },
    // Event listeners: `bus.subscribe("user.created", handler)`.
    RouteSpec {
      kind: "call",
      name: &[Sel::Field("function")],
      names: &["subscribe", "on"],
      args: &[Sel::Field("arguments")],
      path_any: true,
      handler: HandlerAt::LastArgument,
    },
  ],
  requests: &[
    RequestSpec {
      // requests.get("http://svc/x"), httpx.post(...), session/client verbs.
      kind: "call",
      name: &[Sel::Field("function")],
      verb_names: &["get", "post", "put", "delete", "patch", "head", "options", "request"],
      get_names: &[],
      event_names: &[],
      receivers: &["requests", "httpx", "client", "session", "http", "api"],
      args: &[Sel::Field("arguments")],
      method_from_arg: None,
    },
    RequestSpec {
      // bus.emit("user.created") / broker.publish("topic", …).
      kind: "call",
      name: &[Sel::Field("function")],
      verb_names: &[],
      get_names: &[],
      event_names: &["emit", "publish", "dispatch"],
      receivers: &[],
      args: &[Sel::Field("arguments")],
      method_from_arg: None,
    },
  ],
  ..SPEC_DEFAULTS
};

const GO: RefSpec = RefSpec {
  calls: &[CallSpec {
    kind: "call_expression",
    callee: Sel::Field("function"),
    ..CALL_DEFAULTS
  }],
  imports: &[ImportSpec {
    kind: "import_spec",
    target: Sel::Field("path"),
    string_target: true,
    qualifier: QualSource::None,
  }],
  types: TYPE_ID,
  type_params: &["type_parameter_list"],
  method_callee_kinds: &["selector_expression"],
  routes: &[
    // nc.Subscribe("subject", handler) — NATS-style listeners (Channel items).
    RouteSpec {
      kind: "call_expression",
      name: &[Sel::Field("function")],
      names: &["Subscribe"],
      args: &[Sel::Field("arguments")],
      path_any: true,
      handler: HandlerAt::LastArgument,
    },
    // net/http, gorilla, gin, echo, chi, fiber: `HandleFunc("/x", h)` / `r.GET("/x", h)` /
    // Go 1.22 `mux.HandleFunc("GET /x", h)` patterns.
    RouteSpec {
      kind: "call_expression",
      name: &[Sel::Field("function")],
      names: &[
        "GET", "POST", "PUT", "DELETE", "PATCH", "HEAD", "OPTIONS", "Get", "Post", "Put",
        "Delete", "Patch", "Head", "Options", "Any", "HandleFunc", "Handle",
      ],
      args: &[Sel::Field("arguments")],
      path_any: false,
      handler: HandlerAt::LastArgument,
    },
  ],
  requests: &[
    // nc.Publish("subject", data) — NATS-style emitters.
    RequestSpec {
      kind: "call_expression",
      name: &[Sel::Field("function")],
      verb_names: &[],
      get_names: &[],
      event_names: &["Publish"],
      receivers: &[],
      args: &[Sel::Field("arguments")],
      method_from_arg: None,
    },
    // http.Get(url) / http.Head / http.PostForm — package-level client calls.
    RequestSpec {
      kind: "call_expression",
      name: &[Sel::Field("function")],
      verb_names: &["Get", "Post", "Head", "PostForm"],
      get_names: &[],
      event_names: &[],
      receivers: &["http", "client", "resty"],
      args: &[Sel::Field("arguments")],
      method_from_arg: None,
    },
    // http.NewRequest("GET", url, …) — the method is the first argument.
    RequestSpec {
      kind: "call_expression",
      name: &[Sel::Field("function")],
      verb_names: &["NewRequest", "NewRequestWithContext"],
      get_names: &[],
      event_names: &[],
      receivers: &["http"],
      args: &[Sel::Field("arguments")],
      method_from_arg: Some(0),
    },
  ],
  ..SPEC_DEFAULTS
};

/// JavaScript / TypeScript / Tsx share one grammar family for calls + ES imports + `require`.
const JS_LIKE: RefSpec = RefSpec {
  calls: &[CallSpec {
    kind: "call_expression",
    callee: Sel::Field("function"),
    ..CALL_DEFAULTS
  }],
  imports: &[ImportSpec {
    kind: "import_statement",
    target: Sel::Field("source"),
    string_target: true,
    qualifier: QualSource::None,
  }],
  text_rules: &[("require", TextAction::ImportFirstArg)],
  types: TYPE_ID,
  // `class_heritage` covers both TS (`extends`/`implements` clauses within) and JS (bare
  // `extends B`) in one row.
  implements: &[ImplSpec {
    kind: "class_heritage",
    target: None,
  }],
  type_params: &["type_parameters"],
  method_callee_kinds: &["member_expression"],
  self_receivers: &["this"],
  routes: &[
    // Express / Koa / Fastify / Hono: `app.get("/x", …, handler)`.
    RouteSpec {
      kind: "call_expression",
      name: &[Sel::Field("function")],
      names: &["get", "post", "put", "delete", "patch", "head", "options", "all"],
      args: &[Sel::Field("arguments")],
      path_any: false,
      handler: HandlerAt::LastArgument,
    },
    // NestJS: `@Get("cats/:id")` decorates the next method definition.
    RouteSpec {
      kind: "decorator",
      name: &[Sel::ChildOfKind(&["call_expression"]), Sel::Field("function")],
      names: &["Get", "Post", "Put", "Delete", "Patch", "Head", "Options", "All"],
      args: &[Sel::ChildOfKind(&["call_expression"]), Sel::Field("arguments")],
      path_any: true,
      handler: HandlerAt::NextSibling(&["method_definition"]),
    },
    // Event listeners: `bus.on("user.created", handler)` — the registration is a Channel
    // item (outline rule) and calls its handler like a route does.
    RouteSpec {
      kind: "call_expression",
      name: &[Sel::Field("function")],
      names: &["on", "once", "addListener", "subscribe"],
      args: &[Sel::Field("arguments")],
      path_any: true,
      handler: HandlerAt::LastArgument,
    },
  ],
  requests: &[
    // fetch("/api/users") — GET unless options say otherwise (v1 records GET).
    RequestSpec {
      kind: "call_expression",
      name: &[Sel::Field("function")],
      verb_names: &[],
      get_names: &["fetch"],
      event_names: &[],
      receivers: &[],
      args: &[Sel::Field("arguments")],
      method_from_arg: None,
    },
    // axios.get("/x") and friends — verb-named member calls on known client receivers.
    RequestSpec {
      kind: "call_expression",
      name: &[Sel::Field("function")],
      verb_names: &["get", "post", "put", "delete", "patch", "head", "options"],
      get_names: &[],
      event_names: &[],
      receivers: &["axios", "http", "https", "client", "api", "ky", "got", "superagent", "agent"],
      args: &[Sel::Field("arguments")],
      method_from_arg: None,
    },
    // bus.emit("user.created", …) — event emitters, any receiver; the topic must still
    // match a registered Channel to link, so the match is the precision gate.
    RequestSpec {
      kind: "call_expression",
      name: &[Sel::Field("function")],
      verb_names: &[],
      get_names: &[],
      event_names: &["emit", "publish", "dispatch", "trigger", "broadcast"],
      receivers: &[],
      args: &[Sel::Field("arguments")],
      method_from_arg: None,
    },
  ],
  ..SPEC_DEFAULTS
};

const C_LIKE: RefSpec = RefSpec {
  calls: &[CallSpec {
    kind: "call_expression",
    callee: Sel::Field("function"),
    ..CALL_DEFAULTS
  }],
  imports: &[ImportSpec {
    kind: "preproc_include",
    target: Sel::Field("path"),
    string_target: true,
    qualifier: QualSource::None,
  }],
  types: TYPE_ID,
  type_params: &["template_parameter_list"],
  static_callee_kinds: &["qualified_identifier"],
  method_callee_kinds: &["field_expression"],
  self_receivers: &["this"],
  ..SPEC_DEFAULTS
};

const JAVA: RefSpec = RefSpec {
  calls: &[CallSpec {
    kind: "method_invocation",
    callee: Sel::Field("name"),
    receiver_field: Some("object"),
    ..CALL_DEFAULTS
  }],
  imports: &[ImportSpec {
    kind: "import_declaration",
    target: Sel::ChildOfKind(&["scoped_identifier", "identifier"]),
    string_target: false,
    qualifier: QualSource::None,
  }],
  types: TYPE_ID,
  implements: &[
    ImplSpec {
      kind: "superclass",
      target: None,
    },
    ImplSpec {
      kind: "super_interfaces",
      target: None,
    },
    ImplSpec {
      kind: "extends_interfaces",
      target: None,
    },
  ],
  type_params: &["type_parameters"],
  // `var` is local-variable type inference (a reserved type name since Java 10, never a
  // definable type) — the grammar surfaces it as a `type_identifier` leaf (verified by probe).
  type_placeholders: &["var"],
  self_receivers: &["this"],
  routes: &[
    // Spring: `@GetMapping("/users")` / `@RequestMapping(value = "/x", …)` on a method.
    RouteSpec {
      kind: "annotation",
      name: &[Sel::Field("name")],
      names: &[
        "GetMapping", "PostMapping", "PutMapping", "DeleteMapping", "PatchMapping",
        "RequestMapping",
      ],
      args: &[Sel::Field("arguments")],
      path_any: true,
      handler: HandlerAt::DecoratedDefinition {
        ancestors: &["method_declaration"],
        via: None,
      },
    },
  ],
  ..SPEC_DEFAULTS
};

const CSHARP: RefSpec = RefSpec {
  calls: &[CallSpec {
    kind: "invocation_expression",
    callee: Sel::Field("function"),
    ..CALL_DEFAULTS
  }],
  imports: &[ImportSpec {
    kind: "using_directive",
    target: Sel::Field("name"),
    string_target: false,
    qualifier: QualSource::None,
  }],
  implements: &[ImplSpec {
    kind: "base_list",
    target: None,
  }],
  type_params: &["type_parameter_list"],
  method_callee_kinds: &["member_access_expression"],
  self_receivers: &["this"],
  routes: &[
    // ASP.NET: `[HttpGet("users/{id}")]` on a method or local function.
    RouteSpec {
      kind: "attribute",
      name: &[Sel::Field("name")],
      names: &["HttpGet", "HttpPost", "HttpPut", "HttpDelete", "HttpPatch", "HttpHead", "HttpOptions"],
      args: &[Sel::ChildOfKind(&["attribute_argument_list"])],
      path_any: true,
      handler: HandlerAt::DecoratedDefinition {
        ancestors: &["method_declaration", "local_function_statement"],
        via: None,
      },
    },
  ],
  ..SPEC_DEFAULTS
};

const KOTLIN: RefSpec = RefSpec {
  calls: &[CallSpec {
    kind: "call_expression",
    callee: Sel::FirstNamedChild,
    ..CALL_DEFAULTS
  }],
  imports: &[ImportSpec {
    kind: "import_header",
    target: Sel::ChildOfKind(&["identifier"]),
    string_target: false,
    qualifier: QualSource::None,
  }],
  types: TYPE_ID,
  type_params: &["type_parameters"],
  method_callee_kinds: &["navigation_expression"],
  self_receivers: &["this"],
  ..SPEC_DEFAULTS
};

const SWIFT: RefSpec = RefSpec {
  calls: &[CallSpec {
    kind: "call_expression",
    callee: Sel::FirstNamedChild,
    ..CALL_DEFAULTS
  }],
  imports: &[ImportSpec {
    kind: "import_declaration",
    target: Sel::ChildOfKind(&["identifier"]),
    string_target: false,
    qualifier: QualSource::None,
  }],
  types: TYPE_ID,
  type_params: &["type_parameters"],
  // Swift's `Self` (the enclosing/dynamic type) reaches the walk as a type leaf (verified by
  // probe); it is a keyword, never a definable type.
  type_placeholders: &["Self"],
  method_callee_kinds: &["navigation_expression"],
  self_receivers: &["self"],
  ..SPEC_DEFAULTS
};

const RUBY: RefSpec = RefSpec {
  calls: &[CallSpec {
    kind: "call",
    callee: Sel::Field("method"),
    receiver_field: Some("receiver"),
    ..CALL_DEFAULTS
  }],
  text_rules: &[
    ("require", TextAction::ImportFirstArg),
    ("require_relative", TextAction::ImportFirstArg),
  ],
  self_receivers: &["self"],
  ..SPEC_DEFAULTS
};

const PHP: RefSpec = RefSpec {
  calls: &[
    CallSpec {
      kind: "function_call_expression",
      callee: Sel::Field("function"),
      ..CALL_DEFAULTS
    },
    CallSpec {
      kind: "member_call_expression",
      callee: Sel::Field("name"),
      receiver_field: Some("object"),
      ..CALL_DEFAULTS
    },
    CallSpec {
      kind: "nullsafe_member_call_expression",
      callee: Sel::Field("name"),
      receiver_field: Some("object"),
      ..CALL_DEFAULTS
    },
    CallSpec {
      kind: "scoped_call_expression",
      callee: Sel::Field("name"),
      scope_field: Some("scope"),
      ..CALL_DEFAULTS
    },
  ],
  imports: &[ImportSpec {
    kind: "namespace_use_declaration",
    target: Sel::ChildOfKind(&["namespace_name", "name"]),
    string_target: false,
    qualifier: QualSource::None,
  }],
  self_receivers: &["$this", "self", "static"],
  ..SPEC_DEFAULTS
};

const DART: RefSpec = RefSpec {
  calls: &[
    CallSpec {
      kind: "call_expression",
      callee: Sel::Field("function"),
      ..CALL_DEFAULTS
    },
    CallSpec {
      kind: "constructor_invocation",
      callee: Sel::Field("constructor"),
      ..CALL_DEFAULTS
    },
  ],
  imports: &[ImportSpec {
    kind: "import_specification",
    target: Sel::Field("uri"),
    string_target: true,
    qualifier: QualSource::None,
  }],
  self_receivers: &["this"],
  ..SPEC_DEFAULTS
};

const SCALA: RefSpec = RefSpec {
  calls: &[CallSpec {
    kind: "call_expression",
    callee: Sel::Field("function"),
    ..CALL_DEFAULTS
  }],
  imports: &[ImportSpec {
    kind: "import_declaration",
    target: Sel::FieldLast("path"),
    string_target: false,
    qualifier: QualSource::None,
  }],
  self_receivers: &["this"],
  ..SPEC_DEFAULTS
};

const LUA: RefSpec = RefSpec {
  calls: &[CallSpec {
    kind: "function_call",
    callee: Sel::Field("name"),
    ..CALL_DEFAULTS
  }],
  text_rules: &[("require", TextAction::ImportFirstArg)],
  self_receivers: &["self"],
  ..SPEC_DEFAULTS
};

const BASH: RefSpec = RefSpec {
  calls: &[CallSpec {
    kind: "command",
    callee: Sel::Field("name"),
    ..CALL_DEFAULTS
  }],
  text_rules: &[
    ("source", TextAction::ImportFirstArg),
    (".", TextAction::ImportFirstArg),
  ],
  ..SPEC_DEFAULTS
};

const ELIXIR: RefSpec = RefSpec {
  calls: &[CallSpec {
    kind: "call",
    callee: Sel::Field("target"),
    ..CALL_DEFAULTS
  }],
  text_rules: &[
    ("def", TextAction::SkipDefinition),
    ("defp", TextAction::SkipDefinition),
    ("defmodule", TextAction::SkipDefinition),
    ("defmacro", TextAction::SkipDefinition),
    ("defmacrop", TextAction::SkipDefinition),
    ("defimpl", TextAction::SkipDefinition),
    ("defprotocol", TextAction::SkipDefinition),
    ("defstruct", TextAction::SkipDefinition),
    ("defdelegate", TextAction::SkipDefinition),
    ("defguard", TextAction::SkipDefinition),
    ("defguardp", TextAction::SkipDefinition),
    ("defexception", TextAction::SkipDefinition),
    ("import", TextAction::ImportFirstArg),
    ("alias", TextAction::ImportFirstArg),
    ("require", TextAction::ImportFirstArg),
    ("use", TextAction::ImportFirstArg),
  ],
  ..SPEC_DEFAULTS
};

const HASKELL: RefSpec = RefSpec {
  calls: &[CallSpec {
    kind: "apply",
    callee: Sel::Field("function"),
    ..CALL_DEFAULTS
  }],
  imports: &[ImportSpec {
    kind: "import",
    target: Sel::Field("module"),
    string_target: false,
    qualifier: QualSource::None,
  }],
  ..SPEC_DEFAULTS
};

const SOLIDITY: RefSpec = RefSpec {
  // The pinned grammar's call_expression carries its callee as a child `expression` wrapper,
  // not a `function` field (verified by parse probe).
  calls: &[CallSpec {
    kind: "call_expression",
    callee: Sel::FirstNamedChild,
    ..CALL_DEFAULTS
  }],
  imports: &[
    ImportSpec {
      kind: "import_directive",
      target: Sel::Field("import_name"),
      string_target: false,
      qualifier: QualSource::None,
    },
    ImportSpec {
      kind: "import_directive",
      target: Sel::Field("source"),
      string_target: true,
      qualifier: QualSource::None,
    },
  ],
  ..SPEC_DEFAULTS
};

const NIX: RefSpec = RefSpec {
  calls: &[CallSpec {
    kind: "apply_expression",
    callee: Sel::Field("function"),
    ..CALL_DEFAULTS
  }],
  text_rules: &[("import", TextAction::ImportFirstArg)],
  ..SPEC_DEFAULTS
};

const HCL: RefSpec = RefSpec {
  calls: &[CallSpec {
    kind: "function_call",
    callee: Sel::FirstNamedChild,
    ..CALL_DEFAULTS
  }],
  ..SPEC_DEFAULTS
};

/// SQL (dialect-tolerant): function invocations are the one reliable reference class.
const SQL: RefSpec = RefSpec {
  calls: &[CallSpec {
    kind: "invocation",
    callee: Sel::FirstNamedChild,
    ..CALL_DEFAULTS
  }],
  ..SPEC_DEFAULTS
};

/// Objective-C rides the C surface (call_expression/preproc_include/type_identifier) and adds
/// message sends and @interface superclasses.
const OBJC: RefSpec = RefSpec {
  calls: &[
    CallSpec {
      kind: "call_expression",
      callee: Sel::Field("function"),
      ..CALL_DEFAULTS
    },
    CallSpec {
      kind: "message_expression",
      callee: Sel::Field("method"),
      receiver_field: Some("receiver"),
      ..CALL_DEFAULTS
    },
  ],
  imports: &[ImportSpec {
    kind: "preproc_include",
    target: Sel::Field("path"),
    string_target: true,
    qualifier: QualSource::None,
  }],
  implements: &[
    ImplSpec {
      kind: "class_interface",
      target: Some(Sel::Field("superclass")),
    },
    ImplSpec {
      kind: "class_implementation",
      target: Some(Sel::Field("superclass")),
    },
  ],
  types: TYPE_ID,
  self_receivers: &["self"],
  ..SPEC_DEFAULTS
};

const PERL: RefSpec = RefSpec {
  calls: &[
    CallSpec {
      kind: "call_expression_with_bareword",
      callee: Sel::FirstNamedChild,
      ..CALL_DEFAULTS
    },
    CallSpec {
      kind: "call_expression_with_args_with_brackets",
      callee: Sel::FirstNamedChild,
      ..CALL_DEFAULTS
    },
    CallSpec {
      kind: "call_expression_with_spaced_args",
      callee: Sel::FirstNamedChild,
      ..CALL_DEFAULTS
    },
    CallSpec {
      kind: "method_invocation",
      callee: Sel::Field("function_name"),
      receiver_field: Some("object"),
      ..CALL_DEFAULTS
    },
  ],
  imports: &[
    ImportSpec {
      kind: "use_no_statement",
      target: Sel::Field("package_name"),
      string_target: false,
      qualifier: QualSource::None,
    },
    ImportSpec {
      kind: "require_statement",
      target: Sel::Field("package_name"),
      string_target: false,
      qualifier: QualSource::None,
    },
  ],
  ..SPEC_DEFAULTS
};

/// Zig: `@import("x")` is a builtin call whose first argument names the module.
const ZIG: RefSpec = RefSpec {
  calls: &[
    CallSpec {
      kind: "call_expression",
      callee: Sel::Field("function"),
      ..CALL_DEFAULTS
    },
    CallSpec {
      kind: "builtin_function",
      callee: Sel::ChildOfKind(&["builtin_identifier"]),
      ..CALL_DEFAULTS
    },
  ],
  text_rules: &[("@import", TextAction::ImportFirstArg)],
  ..SPEC_DEFAULTS
};

const ERLANG: RefSpec = RefSpec {
  calls: &[CallSpec {
    kind: "call",
    callee: Sel::Field("expr"),
    ..CALL_DEFAULTS
  }],
  imports: &[ImportSpec {
    kind: "import_attribute",
    target: Sel::Field("module"),
    string_target: false,
    qualifier: QualSource::None,
  }],
  implements: &[ImplSpec {
    kind: "behaviour_attribute",
    target: Some(Sel::Field("name")),
  }],
  ..SPEC_DEFAULTS
};

const OCAML: RefSpec = RefSpec {
  calls: &[CallSpec {
    kind: "application_expression",
    callee: Sel::Field("function"),
    ..CALL_DEFAULTS
  }],
  imports: &[
    ImportSpec {
      kind: "open_module",
      target: Sel::Field("module"),
      string_target: false,
      qualifier: QualSource::None,
    },
    ImportSpec {
      kind: "include_module",
      target: Sel::Field("module"),
      string_target: false,
      qualifier: QualSource::None,
    },
  ],
  ..SPEC_DEFAULTS
};

/// R: `library(x)` / `require(x)` are ordinary calls importing their first argument.
const R_SPEC: RefSpec = RefSpec {
  calls: &[CallSpec {
    kind: "call",
    callee: Sel::Field("function"),
    ..CALL_DEFAULTS
  }],
  text_rules: &[
    ("library", TextAction::ImportFirstArg),
    ("require", TextAction::ImportFirstArg),
    ("requireNamespace", TextAction::ImportFirstArg),
  ],
  ..SPEC_DEFAULTS
};

const JULIA: RefSpec = RefSpec {
  calls: &[CallSpec {
    kind: "call_expression",
    callee: Sel::FirstNamedChild,
    ..CALL_DEFAULTS
  }],
  imports: &[
    ImportSpec {
      kind: "import_statement",
      target: Sel::ChildOfKind(&["identifier", "scoped_identifier", "import_path"]),
      string_target: false,
      qualifier: QualSource::None,
    },
    ImportSpec {
      kind: "using_statement",
      target: Sel::ChildOfKind(&["identifier", "scoped_identifier", "import_path"]),
      string_target: false,
      qualifier: QualSource::None,
    },
  ],
  ..SPEC_DEFAULTS
};

/// PowerShell: cmdlet/function calls are `command` nodes named by their command_name.
const POWERSHELL: RefSpec = RefSpec {
  calls: &[CallSpec {
    kind: "command",
    callee: Sel::Field("command_name"),
    ..CALL_DEFAULTS
  }],
  ..SPEC_DEFAULTS
};

/// One node's dispatch outcome in the fused walk — mirrors the priority of the original
/// if-chain (imports > types > implements > calls); a kind belongs to exactly one chain arm.
#[derive(Clone, Copy, PartialEq, Eq, Default)]
enum Chain {
  #[default]
  None,
  Import,
  Type,
  Implements(u16),
  Call(u16),
}

/// A [`RefSpec`] with its node-kind names resolved to the grammar's numeric kind ids (§12
/// build-once dispatch): the walk indexes one dense table per node instead of comparing the
/// kind string against every spec entry.
pub(crate) struct ResolvedRefSpec {
  pub(crate) spec: std::sync::Arc<RefSpecData>,
  /// `kind_id → chain arm`, dense over the ids the spec mentions.
  chain: Vec<Chain>,
  /// `kind_id → declares type parameters` (checked independently of the chain).
  type_params: Vec<bool>,
  /// Resolved kind id per `spec.imports` entry (multiple import specs may share a kind).
  import_kind_ids: Vec<u16>,
  /// Resolved kind id per `spec.routes` entry (routes share kinds with calls, so they
  /// dispatch beside the chain, never through it).
  route_kind_ids: Vec<u16>,
  /// Resolved kind id per `spec.requests` entry — same beside-the-chain dispatch.
  request_kind_ids: Vec<u16>,
}

impl ResolvedRefSpec {
  pub(crate) fn build(lang: SgLang, spec: std::sync::Arc<RefSpecData>) -> Self {
    let id_of = |kind: &str| -> Option<u16> {
      // `kind_to_id` returns 0 for kinds absent from the pinned grammar; such entries could
      // never have matched by string either, so they simply don't dispatch.
      match lang.kind_to_id(kind) {
        0 => None,
        id => Some(id),
      }
    };
    let import_kind_ids: Vec<u16> = spec
      .imports
      .iter()
      .map(|i| id_of(&i.kind).unwrap_or(0))
      .collect();
    let route_kind_ids: Vec<u16> = spec
      .routes
      .iter()
      .map(|r| id_of(&r.kind).unwrap_or(0))
      .collect();
    let request_kind_ids: Vec<u16> = spec
      .requests
      .iter()
      .map(|r| id_of(&r.kind).unwrap_or(0))
      .collect();

    let max_id = spec
      .calls
      .iter()
      .map(|c| c.kind.as_str())
      .chain(spec.imports.iter().map(|i| i.kind.as_str()))
      .chain(spec.implements.iter().map(|i| i.kind.as_str()))
      .chain(spec.types.iter().map(String::as_str))
      .chain(spec.type_params.iter().map(String::as_str))
      .filter_map(id_of)
      .max()
      .unwrap_or(0) as usize;

    let mut chain = vec![Chain::None; max_id + 1];
    let mut type_params = vec![false; max_id + 1];
    // Insert in reverse priority so higher-priority arms overwrite on (unlikely) kind overlap;
    // within calls/implements, reverse entry order so the FIRST spec entry sharing a kind wins
    // — the same tie-break the sequential `find()` dispatch had.
    for (idx, call) in spec.calls.iter().enumerate().rev() {
      if let Some(id) = id_of(&call.kind) {
        chain[id as usize] = Chain::Call(idx as u16);
      }
    }
    for (idx, imp) in spec.implements.iter().enumerate().rev() {
      if let Some(id) = id_of(&imp.kind) {
        chain[id as usize] = Chain::Implements(idx as u16);
      }
    }
    for kind in &spec.types {
      if let Some(id) = id_of(kind) {
        chain[id as usize] = Chain::Type;
      }
    }
    for imp in &spec.imports {
      if let Some(id) = id_of(&imp.kind) {
        chain[id as usize] = Chain::Import;
      }
    }
    for kind in &spec.type_params {
      if let Some(id) = id_of(kind) {
        type_params[id as usize] = true;
      }
    }
    Self {
      spec,
      chain,
      type_params,
      import_kind_ids,
      route_kind_ids,
      request_kind_ids,
    }
  }

  #[inline]
  fn chain_at(&self, kind_id: u16) -> Chain {
    self
      .chain
      .get(kind_id as usize)
      .copied()
      .unwrap_or(Chain::None)
  }

  #[inline]
  fn declares_type_params(&self, kind_id: u16) -> bool {
    self
      .type_params
      .get(kind_id as usize)
      .copied()
      .unwrap_or(false)
  }
}

/// Kind ids resolved once per language, process-wide.
static RESOLVED_SPECS: LazyLock<HashMap<SgLang, ResolvedRefSpec>> = LazyLock::new(|| {
  use SupportLang as L;
  let all = [
    L::Rust,
    L::Python,
    L::Go,
    L::JavaScript,
    L::TypeScript,
    L::Tsx,
    L::C,
    L::Cpp,
    L::Java,
    L::CSharp,
    L::Kotlin,
    L::Swift,
    L::Ruby,
    L::Php,
    L::Dart,
    L::Scala,
    L::Lua,
    L::Bash,
    L::Elixir,
    L::Haskell,
    L::Solidity,
    L::Nix,
    L::Hcl,
    L::Sql,
    L::ObjectiveC,
    L::Perl,
    L::Zig,
    L::Erlang,
    L::OCaml,
    L::R,
    L::Julia,
    L::PowerShell,
  ];
  all
    .into_iter()
    // Slim builds: a disabled grammar's kind_to_id is an unimplemented!() stub — specs
    // resolve only for compiled-in languages (their files are never walked anyway).
    .filter(|lang| lang.is_enabled())
    .map(SgLang::from)
    .filter_map(|lang| {
      let data = std::sync::Arc::new(RefSpecData::from(ref_spec(lang)?));
      Some((lang, ResolvedRefSpec::build(lang, data)))
    })
    .collect()
});

/// Every builtin (language name, walk data) pair — the round-trip expressiveness test's
/// ground truth. Test-only: production dispatch reads `RESOLVED_SPECS`.
#[cfg(test)]
pub(crate) fn builtin_specs_for_test() -> Vec<(String, RefSpecData)> {
  use SupportLang as L;
  let all = [
    L::Rust,
    L::Python,
    L::Go,
    L::JavaScript,
    L::TypeScript,
    L::Tsx,
    L::C,
    L::Cpp,
    L::Java,
    L::CSharp,
    L::Kotlin,
    L::Swift,
    L::Ruby,
    L::Php,
    L::Dart,
    L::Scala,
    L::Lua,
    L::Bash,
    L::Elixir,
    L::Haskell,
    L::Solidity,
    L::Nix,
    L::Hcl,
    L::Sql,
    L::ObjectiveC,
    L::Perl,
    L::Zig,
    L::Erlang,
    L::OCaml,
    L::R,
    L::Julia,
    L::PowerShell,
  ];
  all
    .into_iter()
    .map(SgLang::from)
    .filter_map(|lang| Some((lang.to_string(), RefSpecData::from(ref_spec(lang)?))))
    .collect()
}

/// Kind-id-resolved typefact tables, process-wide (G-M1) — beside the ref specs so both
/// resolve exactly once against the same grammars.
static RESOLVED_TYPEFACTS: LazyLock<HashMap<SgLang, crate::typefacts::ResolvedTypeFacts>> =
  LazyLock::new(|| {
    // `type_spec` is the single authority on which languages capture — enumerating every
    // enabled builtin here means a new capture table can never be silently dropped by a
    // stale second list (the RESOLVED_SPECS hazard, relearned once with Go/Java).
    SupportLang::all_langs()
      .iter()
      .filter(|lang| lang.is_enabled())
      .map(|lang| SgLang::from(*lang))
      .filter_map(|lang| {
        let spec = crate::typefacts::type_spec(lang)?;
        Some((lang, crate::typefacts::ResolvedTypeFacts::build(lang, spec)))
      })
      .collect()
  });

pub(crate) fn resolved_typefacts(lang: SgLang) -> Option<&'static crate::typefacts::ResolvedTypeFacts> {
  RESOLVED_TYPEFACTS.get(&lang)
}

/// The kind-id-resolved extraction spec for `lang`, if it has one.
pub(crate) fn resolved_ref_spec(lang: SgLang) -> Option<&'static ResolvedRefSpec> {
  RESOLVED_SPECS.get(&lang)
}

/// Reference-extraction spec for a language. Pure-structural languages (CSS, HTML, JSON,
/// Markdown, YAML) have no call/import semantics and return `None`.
pub(crate) fn ref_spec(lang: SgLang) -> Option<&'static RefSpec> {
  use SupportLang as L;
  // Dynamic languages gain serialized specs in F-M4; until then they are structural-only.
  let SgLang::Builtin(lang) = lang else {
    return None;
  };
  match lang {
    L::Rust => Some(&RUST),
    L::Python => Some(&PYTHON),
    L::Go => Some(&GO),
    L::JavaScript | L::TypeScript | L::Tsx => Some(&JS_LIKE),
    L::C | L::Cpp => Some(&C_LIKE),
    L::Java => Some(&JAVA),
    L::CSharp => Some(&CSHARP),
    L::Kotlin => Some(&KOTLIN),
    L::Swift => Some(&SWIFT),
    L::Ruby => Some(&RUBY),
    L::Php => Some(&PHP),
    L::Dart => Some(&DART),
    L::Scala => Some(&SCALA),
    L::Lua => Some(&LUA),
    L::Bash => Some(&BASH),
    L::Elixir => Some(&ELIXIR),
    L::Haskell => Some(&HASKELL),
    L::Solidity => Some(&SOLIDITY),
    L::Sql => Some(&SQL),
    L::ObjectiveC => Some(&OBJC),
    L::Perl => Some(&PERL),
    L::Zig => Some(&ZIG),
    L::Erlang => Some(&ERLANG),
    L::OCaml => Some(&OCAML),
    L::R => Some(&R_SPEC),
    L::Julia => Some(&JULIA),
    L::PowerShell => Some(&POWERSHELL),
    L::Nix => Some(&NIX),
    L::Hcl => Some(&HCL),
    _ => None,
  }
}

/// Innermost-containing-span lookups for offsets arriving in non-decreasing (document) order —
/// exactly the fused walk's emission order, since pre-order visit starts never decrease. An
/// active-span stack advances with the offsets, so a whole file costs O(spans + lookups)
/// instead of the previous O(lookups × spans) rescan (~1M range checks per index of this
/// repo). The answer is still "minimum-length span containing the offset" — computed over the
/// (tiny) active set, which by the monotonic contract equals the containing set — so the
/// semantics match the specification implementation's linear scan for any span shape.
pub(crate) struct SpanCursor<'a> {
  spans: &'a [(Range<usize>, NodeId)],
  /// Next span (spans are in document order) not yet considered for activation.
  next: usize,
  /// Indices of admitted spans; stale (already-ended) entries are retired lazily.
  active: Vec<usize>,
}

impl<'a> SpanCursor<'a> {
  pub(crate) fn new(spans: &'a [(Range<usize>, NodeId)]) -> Self {
    Self {
      spans,
      next: 0,
      active: Vec::new(),
    }
  }

  /// The innermost definition containing `offset`. Offsets must be non-decreasing across
  /// calls (the walk's document order guarantees this).
  pub(crate) fn enclosing(&mut self, offset: usize) -> Option<NodeId> {
    while let Some(&top) = self.active.last() {
      if self.spans[top].0.end <= offset {
        self.active.pop();
      } else {
        break;
      }
    }
    while self.next < self.spans.len() && self.spans[self.next].0.start <= offset {
      if self.spans[self.next].0.end > offset {
        self.active.push(self.next);
      }
      self.next += 1;
    }
    self
      .active
      .iter()
      .map(|&i| &self.spans[i])
      .filter(|(range, _)| range.contains(&offset))
      .min_by_key(|(range, _)| range.end - range.start)
      .map(|(_, id)| id.to_owned())
  }
}

/// One recorded HTTP client call site: the enclosing definition, the method, and the
/// literal URL — matched against `Route` templates at link time.
pub(crate) struct RawRequest<'t> {
  pub(crate) from: NodeId,
  pub(crate) method: String,
  pub(crate) path: Cow<'t, str>,
  pub(crate) start: u32,
  pub(crate) end: u32,
}

/// A walk emission awaiting the post-pass: definite references pass through in visit order;
/// type-use candidates wait for the complete binder set (a `type_parameters` declaration may
/// be visited after uses of its binder, so the shadow filter can only run once the walk ends).
enum Pending<'t> {
  Ready(RawRef<'t>),
  TypeUse {
    from: NodeId,
    name: Cow<'t, str>,
    start: u32,
    end: u32,
  },
}

/// Emit `calls` and `imports` references from the parse tree — one fused traversal (§12):
/// each node costs a single dense kind-id table lookup that dispatches import / type /
/// implements / call handling, and type-parameter binder collection rides the same walk
/// instead of a second full pass. `entities` maps each local definition id in `def_spans` to
/// its borrowed identity — the source of enclosing-owner names for `self.`/`Self::` attribution.
#[cfg_attr(not(test), allow(dead_code))] // the thin no-facts wrapper is the test harness's surface
pub(crate) fn extract_references<'t>(
  root: SgNode<'t>,
  resolved: &ResolvedRefSpec,
  def_spans: &[(Range<usize>, NodeId)],
  entities: &[EntityIdentity<'_>],
  out: &mut Vec<RawRef<'t>>,
) {
  extract_references_with_facts(
    root,
    resolved,
    None,
    def_spans,
    entities,
    out,
    &mut Vec::new(),
    &mut Vec::new(),
    None,
  );
}

/// [`extract_references`] with type-fact capture riding the SAME dfs (G-M1): binding sites
/// dispatch through the resolved typefact table exactly like reference sites dispatch through
/// the chain table — one walk, two outputs.
#[allow(clippy::too_many_arguments)] // the one fused walk: every output rides the same cursor
pub(crate) fn extract_references_with_facts<'t>(
  root: SgNode<'t>,
  resolved: &ResolvedRefSpec,
  typefacts: Option<&crate::typefacts::ResolvedTypeFacts>,
  def_spans: &[(Range<usize>, NodeId)],
  entities: &[EntityIdentity<'_>],
  out: &mut Vec<RawRef<'t>>,
  bindings: &mut Vec<crate::typefacts::RawBinding<'t>>,
  requests_out: &mut Vec<RawRequest<'t>>,
  mut signer: Option<&mut crate::signature::Signer>,
) {
  let spec = &*resolved.spec;
  // Generic type-parameter binders: (declaring item's span, binder name). Mentions of a binder
  // inside its declaring span are local bindings, not type uses.
  let mut binders: Vec<(Range<usize>, Cow<'t, str>)> = Vec::new();
  // Definition-head calls suppressed by a SkipDefinition rule (`def foo(x)` → `foo(x)`).
  let mut suppressed: HashSet<usize> = HashSet::new();
  // Dedup for implements references: one edge per (from, name) per file — borrowed keys, so
  // deduplication itself allocates nothing.
  let mut seen_impls: HashSet<(u64, Cow<'t, str>)> = HashSet::new();
  let mut pending: Vec<Pending<'t>> = Vec::new();
  let mut span_cursor = SpanCursor::new(def_spans);
  // Single-cursor pre-order (document order): `children()` allocates a fresh tree cursor per
  // call, so the previous stack walk paid one C-side cursor malloc per visited node (~350k per
  // index of this repo); `dfs()` streams the whole file over ONE cursor. Anonymous token
  // leaves still stream past, but a skipped iterator step is free where a cursor was not.
  for node in root.dfs() {
    // Near-clone signatures (v16) read every leaf token — anonymous ones included — before
    // the named-only dispatch below.
    if let Some(signer) = signer.as_deref_mut() {
      signer.visit(&node);
    }
    if !node.is_named() {
      continue;
    }
    let kind_id = node.kind_id();
    if let Some(facts) = typefacts {
      if let Some(bind) = facts.arm(kind_id) {
        crate::typefacts::capture_at(bind, &node, bindings);
      }
    }
    if resolved.declares_type_params(kind_id) {
      collect_binders_in(&node, &mut binders);
    }
    // Route registrations dispatch beside the chain: their kinds are usually also call
    // kinds, and the same node legitimately yields both the framework call and the
    // route → handler reference.
    for (idx, &id) in resolved.route_kind_ids.iter().enumerate() {
      if id != 0 && id == kind_id {
        emit_route_handler(&node, &spec.routes[idx], spec, &mut span_cursor, &mut pending);
      }
    }
    for (idx, &id) in resolved.request_kind_ids.iter().enumerate() {
      if id != 0 && id == kind_id {
        emit_request(&node, &spec.requests[idx], &mut span_cursor, requests_out);
      }
    }
    match resolved.chain_at(kind_id) {
      Chain::None => {}
      Chain::Import => {
        for (ispec, &id) in spec.imports.iter().zip(&resolved.import_kind_ids) {
          if id == kind_id {
            emit_imports(&node, ispec, def_spans, &mut pending);
          }
        }
      }
      Chain::Type => stage_type_use(&node, spec, &mut span_cursor, &mut pending),
      Chain::Implements(idx) => emit_implements(
        &node,
        &spec.implements[idx as usize],
        spec,
        &mut span_cursor,
        &mut seen_impls,
        &mut pending,
      ),
      Chain::Call(idx) => {
        let cspec = &spec.calls[idx as usize];
        if suppressed.remove(&node.node_id()) || is_chain_link(&node, spec) {
          continue;
        }
        let Some(mut callee) = select(&node, &cspec.callee) else {
          continue;
        };
        // Drill through same-family call chains (Haskell curried `apply`, `f()()`), so one
        // chain yields one reference, attributed at the outermost node.
        while let Some(inner) = spec
          .calls
          .iter()
          .find(|c| c.kind == callee.kind().as_ref())
          .and_then(|c| select(&callee, &c.callee))
        {
          callee = inner;
        }
        let Some(name) = callee_name(&callee) else {
          continue;
        };
        let range = node.range();
        match spec
          .text_rules
          .iter()
          .find(|(text, _)| name.as_ref() == *text)
        {
          Some((_, TextAction::SkipDefinition)) => {
            if let Some(args) = node.children().find(|c| c.kind().as_ref() == "arguments") {
              if let Some(head) = args.children().find(|c| c.is_named()) {
                if spec.calls.iter().any(|c| c.kind == head.kind().as_ref()) {
                  suppressed.insert(head.node_id());
                }
              }
            }
          }
          Some((_, TextAction::ImportFirstArg)) => {
            if let (Some(arg), Some(from)) =
              (first_argument(&node), outermost(def_spans, range.start))
            {
              if let Some(import) = import_arg_name(&arg) {
                pending.push(Pending::Ready(RawRef::plain(
                  from,
                  import,
                  RefKind::Import,
                  range.start as u32,
                  range.end as u32,
                )));
              }
            }
          }
          None => {
            if let Some(from) = span_cursor.enclosing(range.start) {
              let (form, qualifier, receiver) =
                classify_call(&node, cspec, &callee, spec, entities, from);
              // Receiver/arg extras are persisted to feed typed-receiver resolution and
              // data-flow; languages without capture tables would carry them as dead pack
              // weight (measured on the kernel's C: +33% pack, +7% cold) — so capture is
              // exactly as wide as the typefacts launch set.
              let (receiver, args) = if typefacts.is_some() {
                (receiver, capture_args(&node))
              } else {
                (None, Vec::new())
              };
              pending.push(Pending::Ready(RawRef {
                from,
                name,
                kind: RefKind::Call,
                start: range.start as u32,
                end: range.end as u32,
                qualifier,
                form,
                alias: None,
                receiver,
                args,
              }));
            }
          }
        }
      }
    }
  }

  // Post-pass in visit order: binder-shadowed type uses drop; survivors dedup per
  // (enclosing definition, name) — the same outcome the two-walk version produced.
  let mut seen_types: HashSet<(u64, Cow<'t, str>)> = HashSet::new();
  for entry in pending {
    match entry {
      Pending::Ready(reference) => out.push(reference),
      Pending::TypeUse {
        from,
        name,
        start,
        end,
      } => {
        let shadowed = binders
          .iter()
          .any(|(scope, binder)| *binder == name && scope.contains(&(start as usize)));
        if !shadowed && seen_types.insert((from.raw(), name.clone())) {
          out.push(RawRef::plain(from, name, RefKind::Type, start, end));
        }
      }
    }
  }
}

/// Fields tried, in order, for a member access's receiver expression across grammars
/// (Rust `field_expression.value`, JS `member_expression.object`, Go
/// `selector_expression.operand`, C/C++ `field_expression.argument`,
/// C# `member_access_expression.expression`, Python `attribute.object`).
const RECEIVER_FIELDS: &[&str] = &["value", "object", "operand", "argument", "expression"];

/// Classify a call's syntactic form and extract its qualifier evidence (§3.3):
/// - a static path (`Kg::load`) yields `Static` + the path's final namespace segment;
/// - a self-receiver (`self.helper()`, `Self::helper()`) yields the enclosing item's name —
///   receiver typing the grammar *does* guarantee;
/// - any other member access yields `Method` with no qualifier: a variable name is not
///   namespace evidence, and resolution must not treat it as such.
fn classify_call<'t>(
  call: &SgNode<'t>,
  cspec: &CallSpecData,
  callee: &SgNode<'t>,
  spec: &RefSpecData,
  entities: &[EntityIdentity<'_>],
  from: NodeId,
) -> (RefForm, Option<Cow<'t, str>>, Option<Cow<'t, str>>) {
  let owner = || owner_of_entity(entities, from).map(Cow::Owned);
  let qualifier_of = |node: &SgNode<'t>| -> Option<Cow<'t, str>> {
    let text = callee_name(node).or_else(|| {
      let trimmed = trim_cow(node.text(), str::trim);
      (!trimmed.is_empty() && !trimmed.contains(char::is_whitespace)).then_some(trimmed)
    })?;
    if text.as_ref() == "Self" || spec.self_receivers.iter().any(|s| s.as_str() == text.as_ref()) {
      return owner();
    }
    // Module-relative path heads (`crate::`, `super::`) name no owner we can check.
    (!matches!(text.as_ref(), "crate" | "super")).then_some(text)
  };

  // Call-node-level fields first (Java/PHP/Ruby put receiver/scope beside the callee).
  if let Some(scope_field) = &cspec.scope_field {
    if let Some(scope) = call.field(scope_field.as_str()) {
      return (RefForm::Static, qualifier_of(&scope), None);
    }
  }
  if let Some(receiver_field) = &cspec.receiver_field {
    if let Some(receiver) = call.field(receiver_field.as_str()) {
      let (form, qualifier) = classify_receiver(&receiver, spec, owner);
      return (form, qualifier, simple_receiver_text(&receiver));
    }
    return (RefForm::Bare, None, None);
  }

  let callee_kind_cow = callee.kind();
  let callee_kind = callee_kind_cow.as_ref();
  if spec.static_callee_kinds.iter().any(|k| k.as_str() == callee_kind) {
    let scope = callee.field("path").or_else(|| callee.field("scope"));
    return (RefForm::Static, scope.as_ref().and_then(qualifier_of), None);
  }
  if spec.method_callee_kinds.iter().any(|k| k.as_str() == callee_kind) {
    return match RECEIVER_FIELDS.iter().find_map(|f| callee.field(f)) {
      Some(receiver) => {
        let (form, qualifier) = classify_receiver(&receiver, spec, owner);
        let simple = simple_receiver_text(&receiver);
        (form, qualifier, simple)
      }
      None => (RefForm::Method, None, None),
    };
  }
  (RefForm::Bare, None, None)
}

/// The receiver's spelling when — and only when — it is a bare simple name a file-local
/// binding could type (`x` in `x.helper()`); anything structured returns `None`.
fn simple_receiver_text<'t>(receiver: &SgNode<'t>) -> Option<Cow<'t, str>> {
  let kind_cow = receiver.kind();
  if !LEAF_KINDS.contains(&kind_cow.as_ref()) {
    return None;
  }
  let text = receiver.text();
  (!text.is_empty() && text.len() <= 64 && !text.contains(char::is_whitespace)).then_some(text)
}

/// Per-argument capture at a call node (G-M1): position, traceability class, keyword name,
/// and — for traceable classes — the expression text capped at 64 bytes. The container is
/// found the same way `first_argument` finds it; a call with no discoverable container
/// yields no records (counted nowhere because there is nothing to count — absence of an
/// arguments node is a grammar shape, not a skip).
fn capture_args<'t>(call: &SgNode<'t>) -> Vec<RawArg<'t>> {
  let container = call.field("arguments").or_else(|| {
    call.children().find(|c| {
      matches!(
        c.kind().as_ref(),
        "arguments" | "argument_list" | "call_suffix"
      )
    })
  });
  let Some(container) = container else {
    return Vec::new();
  };
  let mut args = Vec::new();
  for (index, child) in container.children().filter(|c| c.is_named()).enumerate() {
    if index > u16::MAX as usize {
      break;
    }
    let (node, kw_name) = keyword_split(&child);
    let class = classify_arg(&node);
    let expr = match class {
      ArgClass::Var | ArgClass::FieldAccess => {
        let text = node.text();
        (text.len() <= 64).then(|| text.clone())
      }
      // A CallResult's producing call is ITSELF an extracted reference at this same span —
      // the link stage joins by span; duplicating its text measured as pure pack weight.
      ArgClass::CallResult | ArgClass::Literal | ArgClass::Other => None,
    };
    args.push(RawArg {
      index: index as u16,
      class,
      kw_name,
      expr,
    });
  }
  args
}

/// Split a keyword-argument wrapper (`f(x=1)` → name `x`, value node) where the grammar has
/// one; everything else passes through.
fn keyword_split<'t>(node: &SgNode<'t>) -> (SgNode<'t>, Option<Cow<'t, str>>) {
  if matches!(node.kind().as_ref(), "keyword_argument" | "named_argument") {
    let name = node.field("name").map(|n| n.text());
    if let Some(value) = node.field("value") {
      return (value, name);
    }
  }
  (node.clone(), None)
}

fn classify_arg(node: &SgNode<'_>) -> ArgClass {
  let kind_cow = node.kind();
  let kind = kind_cow.as_ref();
  if LEAF_KINDS.contains(&kind) {
    return ArgClass::Var;
  }
  if DESCEND_KINDS.contains(&kind)
    || kind.contains("field")
    || kind.contains("member")
    // Python/Ruby spell member access `attribute`; Rust field access is `field_expression`
    // (caught above); OCaml uses `field_get_expression`.
    || kind == "attribute"
  {
    return ArgClass::FieldAccess;
  }
  if kind.contains("call") || kind == "application_expression" || kind == "invocation" {
    return ArgClass::CallResult;
  }
  if kind.contains("literal")
    || kind.contains("string")
    || kind.contains("number")
    || matches!(kind, "integer" | "float" | "true" | "false" | "nil" | "none" | "atom")
  {
    return ArgClass::Literal;
  }
  ArgClass::Other
}

/// Classify a member-access receiver (Java `obj.m()` / Python `Foo.bar()` / Rust
/// `x.helper()` / …):
/// - a **self keyword** proves the enclosing owner — [`RefForm::Method`] with the owner as a
///   qualifier the resolver may trust outright;
/// - a **plain single-token name** rides along as a HINT — [`RefForm::MethodHinted`]: if it
///   names an owner in the tree (`Foo.bar()` where class `Foo` has `bar`), resolution is
///   corroborated exactly like a static qualifier; if it names nothing (`obj` is just a
///   variable), the resolver falls back to plain Method semantics. Structural either way —
///   no capitalization heuristics;
/// - anything else (call results, chained accesses) is opaque: plain Method, no hint.
fn classify_receiver<'t>(
  receiver: &SgNode<'t>,
  spec: &RefSpecData,
  owner: impl FnOnce() -> Option<Cow<'t, str>>,
) -> (RefForm, Option<Cow<'t, str>>) {
  let text = trim_cow(receiver.text(), str::trim);
  if spec.self_receivers.iter().any(|s| s.as_str() == text.as_ref()) || text.as_ref() == "Self" {
    return (RefForm::Method, owner());
  }
  let plain_name = !text.is_empty()
    && text
      .chars()
      .all(|c| c.is_alphanumeric() || c == '_' || c == '$');
  if plain_name {
    (RefForm::MethodHinted, Some(text))
  } else {
    (RefForm::Method, None)
  }
}


/// The top-level item owning an already-resolved enclosing definition (`Kg.load` → `Kg`):
/// exactly the rendered entity path's `split('.').next()` (non-empty-filtered), reconstructed
/// from the borrowed identity by [`EntityIdentity::owner_segment`] so no per-entity path
/// `String` is ever built.
fn owner_of_entity(entities: &[EntityIdentity<'_>], from: NodeId) -> Option<String> {
  entities.get(from.raw() as usize)?.owner_segment()
}

/// Leaf kinds a type-parameter binder name can be.
const BINDER_LEAF_KINDS: &[&str] = &["type_identifier", "identifier", "simple_identifier"];

/// Collect the binders declared by one type-parameter node (`type_parameters`, …): each
/// parameter's *binder* name (the declared name only — never its bounds or defaults, which are
/// real type uses) scoped to the declaring item's full span. Runs inside the fused walk when
/// the dispatch table flags the node's kind.
fn collect_binders_in<'t>(node: &SgNode<'t>, binders: &mut Vec<(Range<usize>, Cow<'t, str>)>) {
  // The suppression scope is the whole declaring item (fn/struct/impl…, including its body).
  let scope = node.parent().map_or_else(|| node.range(), |p| p.range());
  for param in node.children().filter(|c| c.is_named()) {
    let binder = if BINDER_LEAF_KINDS.contains(&param.kind().as_ref()) {
      Some(param)
    } else {
      // `constrained_type_parameter` (Rust `T: Clone`) binds its `left`; named forms
      // (`type_parameter name: …`) bind their `name`.
      param
        .field("name")
        .or_else(|| param.field("left"))
        .filter(|n| BINDER_LEAF_KINDS.contains(&n.kind().as_ref()))
    };
    if let Some(binder) = binder {
      binders.push((scope.clone(), trim_cow(binder.text(), str::trim)));
    }
  }
}

/// Leaf kinds an implements construct's targets reduce to.
const IMPL_TARGET_KINDS: &[&str] = &["type_identifier", "identifier", "constant", "alias"];

/// A type-identifier leaf marks a type USE unless it is a definition's own name, sits inside
/// an implements construct (which emits `implements`, not `of_type`), or is a language
/// placeholder (`Self`, `_`). Survivors are *staged*: binder shadowing and per-definition
/// dedup run in the post-pass, once the walk has seen every `type_parameters` declaration.
fn stage_type_use<'t>(
  node: &SgNode<'t>,
  spec: &RefSpecData,
  span_cursor: &mut SpanCursor<'_>,
  pending: &mut Vec<Pending<'t>>,
) {
  let Some(parent) = node.parent() else {
    return;
  };
  if parent
    .field("name")
    .is_some_and(|name| name.node_id() == node.node_id())
  {
    return;
  }
  let mut ancestor = Some(parent);
  for _ in 0..2 {
    let Some(a) = ancestor else { break };
    if spec.implements.iter().any(|s| s.kind == a.kind().as_ref()) {
      return;
    }
    ancestor = a.parent();
  }
  let range = node.range();
  let (Some(name), Some(from)) = (callee_name(node), span_cursor.enclosing(range.start)) else {
    return;
  };
  if spec.type_placeholders.iter().any(|t| t.as_str() == name.as_ref()) {
    return;
  }
  pending.push(Pending::TypeUse {
    from,
    name,
    start: range.start as u32,
    end: range.end as u32,
  });
}

/// Emit an `implements` reference per implemented type: the construct's target selector (or the
/// node itself) is reduced to a name directly when possible, else to its type leaves.
fn emit_implements<'t>(
  node: &SgNode<'t>,
  ispec: &ImplSpecData,
  spec: &RefSpecData,
  span_cursor: &mut SpanCursor<'_>,
  seen: &mut HashSet<(u64, Cow<'t, str>)>,
  pending: &mut Vec<Pending<'t>>,
) {
  let range = node.range();
  let Some(from) = span_cursor.enclosing(range.start) else {
    return;
  };
  let targets: Vec<SgNode<'t>> = match &ispec.target {
    Some(sel) => select_all(node, sel),
    None => vec![node.clone()],
  };
  for target in targets {
    let names: Vec<Cow<'t, str>> = if let Some(name) = callee_name(&target) {
      vec![name]
    } else {
      first_descendants_of_kinds(&target, IMPL_TARGET_KINDS)
        .iter()
        .filter_map(callee_name)
        .collect()
    };
    for name in names {
      if spec.type_placeholders.iter().any(|t| t.as_str() == name.as_ref()) {
        continue;
      }
      if seen.insert((from.raw(), name.clone())) {
        pending.push(Pending::Ready(RawRef::plain(
          from,
          name,
          RefKind::Implements,
          range.start as u32,
          range.end as u32,
        )));
      }
    }
  }
}

/// A call node that is its same-kind parent's selected callee is a chain link, not a call site.
fn is_chain_link(node: &SgNode<'_>, spec: &RefSpecData) -> bool {
  let Some(parent) = node.parent() else {
    return false;
  };
  let parent_kind = parent.kind();
  if parent_kind.as_ref() != node.kind().as_ref() {
    return false;
  }
  spec
    .calls
    .iter()
    .find(|c| c.kind == parent_kind.as_ref())
    .and_then(|c| select(&parent, &c.callee))
    .is_some_and(|callee| callee.node_id() == node.node_id())
}

fn select<'t>(node: &SgNode<'t>, sel: &SelData) -> Option<SgNode<'t>> {
  match sel {
    SelData::Field(name) => node.field(name.as_str()),
    SelData::FieldLast(name) => node.field_children(name.as_str()).last(),
    SelData::FirstNamedChild => node.children().find(|c| c.is_named()),
    SelData::ChildOfKind(kinds) => first_descendants_of_kinds(node, kinds).into_iter().next(),
  }
}

/// Apply a navigation path of selectors, hop by hop.
fn select_path<'t>(node: &SgNode<'t>, path: &[SelData]) -> Option<SgNode<'t>> {
  let mut current = node.clone();
  for sel in path {
    current = select(&current, sel)?;
  }
  Some(current)
}

/// Is `text` a Go 1.22 style `VERB /path` pattern?
fn verb_pattern(text: &str) -> bool {
  matches!(
    text.split_once(' '),
    Some((verb, rest))
      if !verb.is_empty() && rest.starts_with('/') && verb.bytes().all(|b| b.is_ascii_uppercase())
  )
}

/// The first string literal under `args` (pre-order) as its quote-stripped text — `None`
/// when absent or (unless `any`) it neither starts with `/` nor is a `VERB /path` pattern.
fn route_path_literal<'t>(args: &SgNode<'t>, any: bool) -> Option<Cow<'t, str>> {
  let literal = args.dfs().skip(1).find(|n| {
    let kind = n.kind();
    kind.as_ref() == "string" || kind.as_ref().ends_with("string_literal")
  })?;
  let trimmed = trim_cow(literal.text(), |t| {
    t.trim_matches(|c| c == '"' || c == '\'' || c == '`')
  });
  if trimmed.is_empty() {
    return None;
  }
  (any || trimmed.starts_with('/') || verb_pattern(trimmed.as_ref())).then_some(trimmed)
}

/// A node that *refers* to something by name: an identifier leaf, or the language's
/// member/static access shapes (`views.detail`, `pkg.Handler`, `Ctrl::show`). Keyword
/// arguments, literals, and closures are none of these — they can never be a handler.
fn reference_shaped(node: &SgNode<'_>, spec: &RefSpecData) -> bool {
  let kind_cow = node.kind();
  let kind = kind_cow.as_ref();
  LEAF_KINDS.contains(&kind)
    || spec.method_callee_kinds.iter().any(|k| k == kind)
    || spec.static_callee_kinds.iter().any(|k| k == kind)
}

/// The handler node for a route construct, per its spec.
fn route_handler_node<'t>(
  construct: &SgNode<'t>,
  args: &SgNode<'t>,
  at: &HandlerAtData,
  spec: &RefSpecData,
) -> Option<SgNode<'t>> {
  match at {
    // The last reference-shaped argument is the handler — middleware-tolerant, and immune
    // to trailing `name="…"` keyword arguments or option objects.
    HandlerAtData::LastArgument => args
      .children()
      .filter(|c| c.is_named() && reference_shaped(c, spec))
      .last(),
    HandlerAtData::UnwrappedArgument(index) => {
      let arg = args.children().filter(|c| c.is_named()).nth(*index as usize)?;
      if arg.kind().as_ref().ends_with("call_expression") {
        let inner = arg.field("arguments")?;
        inner.children().find(|c| c.is_named())
      } else {
        Some(arg)
      }
    }
    HandlerAtData::DecoratedDefinition { ancestors, via } => {
      let holder = construct
        .ancestors()
        .find(|a| ancestors.iter().any(|k| a.kind().as_ref() == k.as_str()))?;
      let target = match via {
        Some(field) => holder.field(field.as_str())?,
        None => holder,
      };
      target.field("name")
    }
    HandlerAtData::NextSibling(kinds) => {
      let sibling = construct
        .next_all()
        .find(|s| s.is_named() && kinds.iter().any(|k| s.kind().as_ref() == k.as_str()))?;
      sibling.field("name")
    }
  }
}

/// Emit the route → handler `calls` reference for a matched route construct (see
/// [`RouteSpec`]). The predicate mirrors the outline rule that creates the `Route` item —
/// fixtures pin the two in sync per framework.
fn emit_route_handler<'t>(
  node: &SgNode<'t>,
  route: &RouteSpecData,
  spec: &RefSpecData,
  span_cursor: &mut SpanCursor<'_>,
  pending: &mut Vec<Pending<'t>>,
) {
  let Some(name_node) = select_path(node, &route.name) else {
    return;
  };
  let Some(verb) = callee_name(&name_node) else {
    return;
  };
  if !route.names.iter().any(|n| n == verb.as_ref()) {
    return;
  }
  let Some(args) = select_path(node, &route.args) else {
    return;
  };
  if route_path_literal(&args, route.path_any).is_none() {
    return;
  }
  let Some(handler) = route_handler_node(node, &args, &route.handler, spec) else {
    return;
  };
  let Some(name) = callee_name(&handler) else {
    return;
  };
  if name.is_empty() {
    return;
  }
  let Some(from) = span_cursor.enclosing(node.range().start) else {
    return;
  };
  // A member-shaped handler (`views.detail`, `handlers.Show`) carries its container as a
  // MethodHinted qualifier: corroborated exactly like any receiver hint (`import views` in
  // the registering file proves the module), and a hint can upgrade but never mask.
  let qualifier = ["object", "operand", "value", "scope", "path"]
    .iter()
    .find_map(|field| handler.field(field))
    .map(|container| trim_cow(container.text(), str::trim))
    .filter(|text| !text.is_empty() && !text.contains(char::is_whitespace));
  let range = handler.range();
  let mut raw = RawRef::plain(from, name, RefKind::Call, range.start as u32, range.end as u32);
  if qualifier.is_some() {
    raw.qualifier = qualifier;
    raw.form = RefForm::MethodHinted;
  }
  pending.push(Pending::Ready(raw));
}

/// The container (receiver/module) text of a callee node, when it is a single word:
/// `axios.get` → `axios`, `reqwest::get` → `reqwest`, `http.Get` → `http`.
fn container_text<'t>(callee: &SgNode<'t>) -> Option<Cow<'t, str>> {
  ["object", "operand", "value", "scope", "path"]
    .iter()
    .find_map(|field| callee.field(field))
    .map(|container| trim_cow(container.text(), str::trim))
    .filter(|text| !text.is_empty() && !text.contains(char::is_whitespace))
}

/// The first URL-shaped string literal under `args`: content starting `/`, `http://`, or
/// `https://`. Quote-stripped; `None` when every argument is dynamic.
fn request_url_literal<'t>(args: &SgNode<'t>) -> Option<Cow<'t, str>> {
  args
    .dfs()
    .skip(1)
    .filter(|n| {
      let kind = n.kind();
      kind.as_ref() == "string" || kind.as_ref().ends_with("string_literal")
    })
    .map(|literal| {
      trim_cow(literal.text(), |t| {
        t.trim_matches(|c| c == '"' || c == '\'' || c == '`')
      })
    })
    .find(|text| {
      text.starts_with('/') || text.starts_with("http://") || text.starts_with("https://")
    })
}

/// The first non-empty string literal under `args` — an event topic (any shape, but never
/// whitespace: `emit(f"...")` and message bodies stay out).
fn event_topic_literal<'t>(args: &SgNode<'t>) -> Option<Cow<'t, str>> {
  args
    .dfs()
    .skip(1)
    .filter(|n| {
      let kind = n.kind();
      kind.as_ref() == "string" || kind.as_ref().ends_with("string_literal")
    })
    .map(|literal| {
      trim_cow(literal.text(), |t| {
        t.trim_matches(|c| c == '"' || c == '\'' || c == '`')
      })
    })
    .find(|text| !text.is_empty() && !text.contains(char::is_whitespace) && text.len() <= 256)
}

/// Record an HTTP client call site (see [`RequestSpec`]) for link-time route matching.
fn emit_request<'t>(
  node: &SgNode<'t>,
  request: &RequestSpecData,
  span_cursor: &mut SpanCursor<'_>,
  out: &mut Vec<RawRequest<'t>>,
) {
  let Some(callee) = select_path(node, &request.name) else {
    return;
  };
  let Some(name) = callee_name(&callee) else {
    return;
  };
  let event = request.event_names.iter().any(|n| n == name.as_ref());
  let method = if request.verb_names.iter().any(|n| n == name.as_ref()) {
    name.to_ascii_uppercase()
  } else if request.get_names.iter().any(|n| n == name.as_ref()) {
    "GET".to_string()
  } else if event {
    "EVENT".to_string()
  } else {
    return;
  };
  if !request.receivers.is_empty() {
    let Some(receiver) = container_text(&callee) else {
      return;
    };
    if !request.receivers.iter().any(|r| r == receiver.as_ref()) {
      return;
    }
  }
  let Some(args) = select_path(node, &request.args) else {
    return;
  };
  let method = match request.method_from_arg {
    None => method,
    Some(index) => {
      // The method rides as a string argument (`http.NewRequest("GET", …)`).
      let Some(arg) = args.children().filter(|c| c.is_named()).nth(index as usize) else {
        return;
      };
      let text = trim_cow(arg.text(), |t| t.trim_matches(|c| c == '"' || c == '\'' || c == '`'));
      if text.is_empty() || !text.bytes().all(|b| b.is_ascii_uppercase()) {
        return;
      }
      text.to_string()
    }
  };
  let literal = if event {
    event_topic_literal(&args)
  } else {
    request_url_literal(&args)
  };
  let Some(path) = literal else {
    return;
  };
  let range = node.range();
  let Some(from) = span_cursor.enclosing(range.start) else {
    return;
  };
  out.push(RawRequest {
    from,
    method,
    path,
    start: range.start as u32,
    end: range.end as u32,
  });
}

/// All matching targets for an import node (repeated fields / multiple names per statement).
fn select_all<'t>(node: &SgNode<'t>, sel: &SelData) -> Vec<SgNode<'t>> {
  match sel {
    SelData::Field(name) => {
      let all: Vec<_> = node.field_children(name.as_str()).collect();
      if all.is_empty() {
        node.field(name.as_str()).into_iter().collect()
      } else {
        all
      }
    }
    SelData::FieldLast(name) => node.field_children(name.as_str()).last().into_iter().collect(),
    SelData::FirstNamedChild => node.children().find(|c| c.is_named()).into_iter().collect(),
    SelData::ChildOfKind(kinds) => first_descendants_of_kinds(node, kinds),
  }
}

/// Pre-order descendants whose kind is listed, without descending into matches (so an
/// `aliased_import` match does not also yield its inner `dotted_name`).
fn first_descendants_of_kinds<'t, K: AsRef<str>>(node: &SgNode<'t>, kinds: &[K]) -> Vec<SgNode<'t>> {
  let mut found = Vec::new();
  let mut queue: Vec<SgNode<'t>> = node.children().collect();
  let mut index = 0;
  while index < queue.len() {
    let current = queue[index].clone();
    index += 1;
    if kinds.iter().any(|k| k.as_ref() == current.kind().as_ref()) {
      found.push(current);
    } else {
      queue.extend(current.children());
    }
  }
  found
}

fn emit_imports<'t>(
  node: &SgNode<'t>,
  ispec: &ImportSpecData,
  def_spans: &[(Range<usize>, NodeId)],
  pending: &mut Vec<Pending<'t>>,
) {
  let range = node.range();
  let Some(from) = outermost(def_spans, range.start) else {
    return;
  };
  for target in select_all(node, &ispec.target) {
    let name = if ispec.string_target {
      literal_import_name(&target)
    } else {
      callee_name(&target)
    };
    if let Some(name) = name {
      let (form, qualifier) = match import_qualifier(node, &target, &ispec.qualifier) {
        Some(q) => (RefForm::Static, Some(q)),
        None => (RefForm::Bare, None),
      };
      pending.push(Pending::Ready(RawRef {
        from,
        name,
        kind: RefKind::Import,
        start: range.start as u32,
        end: range.end as u32,
        qualifier,
        form,
        alias: import_alias(&target),
        receiver: None,
        args: Vec::new(),
      }));
    }
  }
}

/// The local rebinding an aliased import introduces for `target`: `aliased_import`
/// (`from x import y as z`) and `use_as_clause` (`use a::b as c`) both carry it in their
/// `alias` field. `None` everywhere else — the imported name IS the local name.
fn import_alias<'t>(target: &SgNode<'t>) -> Option<Cow<'t, str>> {
  let alias = target.field("alias")?;
  let trimmed = trim_cow(alias.text(), str::trim);
  (!trimmed.is_empty() && !trimmed.contains(char::is_whitespace)).then_some(trimmed)
}

/// The source-module qualifier an import construct provides for `target`, reduced to its final
/// path segment (resolution compares qualifiers against owner names and per-file module stems,
/// which are single segments). `None` means the grammar carries no usable qualifier here — the
/// reference then stays a bare name, exactly as before qualifier capture. Relative heads
/// (`from . import x`, `use super::x`) qualify nothing checkable and return `None`.
fn import_qualifier<'t>(
  node: &SgNode<'t>,
  target: &SgNode<'t>,
  source: &QualSourceData,
) -> Option<Cow<'t, str>> {
  match source {
    QualSourceData::None => None,
    QualSourceData::NodeField(field) => {
      let module = node.field(field.as_str())?;
      let seg = trim_cow(module.text(), |s| {
        s.trim().rsplit('.').next().unwrap_or("").trim()
      });
      (!seg.is_empty() && !seg.contains(char::is_whitespace)).then_some(seg)
    }
    QualSourceData::TargetPath => {
      // A scoped path carries its own `name`; a fieldless wrapper (`use_as_clause`) holds the
      // real path in its `path` field instead — unwrap once, then take that path's prefix.
      let path_node = if target.field("name").is_some() {
        target.clone()
      } else {
        target.field("path")?
      };
      let module = path_node.field("path").or_else(|| path_node.field("scope"))?;
      let name = callee_name(&module)?;
      (!matches!(name.as_ref(), "crate" | "super" | "self")).then_some(name)
    }
  }
}

/// The first argument of a call, across grammar shapes: an `arguments` field (JS/Ruby/Lua), an
/// `arguments`/`argument_list` child (Elixir), or a direct `argument` field (Bash words, Nix
/// apply).
fn first_argument<'t>(node: &SgNode<'t>) -> Option<SgNode<'t>> {
  let args = node.field("arguments").or_else(|| {
    node.children().find(|c| {
      matches!(
        c.kind().as_ref(),
        "arguments" | "argument_list" | "call_suffix"
      )
    })
  });
  if let Some(args) = args {
    if let Some(first) = args.children().find(|c| c.is_named()) {
      return Some(first);
    }
  }
  node.field("argument")
}

/// The imported name carried by a call argument: an identifier/module (navigator) or a
/// string/path literal (delimiter-stripped, verbatim).
fn import_arg_name<'t>(arg: &SgNode<'t>) -> Option<Cow<'t, str>> {
  callee_name(arg).or_else(|| literal_import_name(arg))
}

/// Strip string delimiters only; keep the module string verbatim (`./util`, `fmt`,
/// `package:foo/bar.dart`). Path-like names are honestly unresolvable against symbol tables.
fn literal_import_name<'t>(node: &SgNode<'t>) -> Option<Cow<'t, str>> {
  let trimmed = trim_cow(node.text(), |s| {
    s.trim()
      .trim_matches(|c| matches!(c, '"' | '\'' | '`' | '<' | '>'))
  });
  (!trimmed.is_empty() && !trimmed.contains(char::is_whitespace)).then_some(trimmed)
}

/// Apply a borrowing trim to a `Cow`: a borrowed cow narrows to a sub-slice borrow (no
/// allocation); an owned cow re-owns only the trimmed text.
fn trim_cow<'t>(cow: Cow<'t, str>, trim: impl Fn(&str) -> &str) -> Cow<'t, str> {
  match cow {
    Cow::Borrowed(s) => Cow::Borrowed(trim(s)),
    Cow::Owned(s) => Cow::Owned(trim(&s).to_string()),
  }
}

/// Fields tried, in order, to navigate a callee/import expression toward its identifier.
/// `function` is deliberately absent: same-kind call chains are handled by the walk.
const NAV_FIELDS: &[&str] = &[
  "name",
  "field",
  "attribute",
  "property",
  "method",
  "suffix",
  "constructor",
  "module",
  "path",
  "right",
];

/// Kinds whose text is the referenced name (when they have no named children).
const LEAF_KINDS: &[&str] = &[
  "identifier",
  "field_identifier",
  "type_identifier",
  "property_identifier",
  "simple_identifier",
  "word",
  "command_name",
  "variable",
  "name",
  "constant",
  "module_id",
  "attr_identifier",
  // Erlang: unquoted atoms name functions/modules in call and import position.
  "atom",
  // Zig: `@import` and friends.
  "builtin_identifier",
  // OCaml: the leaves of value/module paths.
  "value_name",
  "module_name",
];

/// Fieldless wrapper kinds: recurse into the last named child (the rightmost simple name,
/// matching the `use b::target → target` precedent).
const DESCEND_KINDS: &[&str] = &[
  "navigation_expression",
  "navigation_suffix",
  "module",
  "namespace_name",
  "qualified_name",
  "variable_expression",
  "select_expression",
  "attrpath",
  "dotted_name",
  "scoped_identifier",
  "field_expression",
  "member_expression",
  "member_access_expression",
  "field_access",
  "scope_resolution",
  "dot",
  "aliased_import",
  // Solidity wraps callees in a generic `expression` node.
  "expression",
  // OCaml paths (`Mod.value`) and Erlang remote calls (`mod:fun`) end in their rightmost name.
  "value_path",
  "module_path",
  "constructor_path",
  "remote",
];

/// The rightmost identifier of a callee/import expression — one universal navigator; unmatched
/// kinds return `None` (no guessing).
fn callee_name<'t>(node: &SgNode<'t>) -> Option<Cow<'t, str>> {
  let kind_cow = node.kind();
  let kind = kind_cow.as_ref();
  // Elixir module references keep their dotted form (`Foo.Bar`), unlike path-style rightmost.
  if kind == "alias" {
    return Some(node.text());
  }
  if LEAF_KINDS.contains(&kind) {
    if let Some(last) = node.children().filter(|c| c.is_named()).last() {
      return callee_name(&last);
    }
    return Some(node.text());
  }
  for field in NAV_FIELDS {
    if let Some(child) = node.field(field) {
      return callee_name(&child);
    }
  }
  if DESCEND_KINDS.contains(&kind) {
    if let Some(last) = node.children().filter(|c| c.is_named()).last() {
      return callee_name(&last);
    }
  }
  None
}

/// The innermost (smallest) definition span containing `offset` — for call attribution.
#[cfg(test)] // production lookups go through `SpanCursor`; this is the specification oracle's
fn enclosing(def_spans: &[(Range<usize>, NodeId)], offset: usize) -> Option<NodeId> {
  def_spans
    .iter()
    .filter(|(range, _)| range.contains(&offset))
    .min_by_key(|(range, _)| range.end - range.start)
    .map(|(_, id)| *id)
}

/// The outermost (largest) definition span containing `offset` — the file, for import
/// attribution. The production layout always leads with the whole-file span (`0..usize::MAX`),
/// which trivially wins the max — answered in O(1) when that shape holds, with the general
/// scan kept for arbitrary span sets.
fn outermost(def_spans: &[(Range<usize>, NodeId)], offset: usize) -> Option<NodeId> {
  if let Some((range, id)) = def_spans.first() {
    if range.start == 0 && range.end == usize::MAX {
      return Some(*id);
    }
  }
  def_spans
    .iter()
    .filter(|(range, _)| range.contains(&offset))
    .max_by_key(|(range, _)| range.end - range.start)
    .map(|(_, id)| *id)
}

#[cfg(test)]
mod tests {
  use super::*;
  use vorpal_language::LanguageExt;

  /// Owned mirror of [`RawRef`] for helpers that outlive their parse tree.
  struct OwnedRef {
    name: String,
    kind: RefKind,
  }

  fn full_refs_for(lang: SupportLang, src: &str) -> Vec<OwnedRef> {
    let lang = SgLang::from(lang);
    let spec = resolved_ref_spec(lang).expect("language has a ref spec");
    let grep = lang.grep(src);
    let spans = vec![(0..usize::MAX, NodeId::new(0))];
    let entities = vec![EntityIdentity::FILE];
    let mut out = Vec::new();
    extract_references(grep.root(), spec, &spans, &entities, &mut out);
    out
      .into_iter()
      .map(|r| OwnedRef {
        name: r.name.into_owned(),
        kind: r.kind,
      })
      .collect()
  }

  fn refs_for(lang: SupportLang, src: &str) -> Vec<(String, RefKind)> {
    let mut refs: Vec<(String, RefKind)> = full_refs_for(lang, src)
      .into_iter()
      .map(|r| (r.name, r.kind))
      .collect();
    refs.sort_by(|a, b| a.0.cmp(&b.0));
    refs
  }

  #[test]
  fn elixir_defs_skip_calls_emit_and_imports_resolve() {
    let refs = refs_for(
      SupportLang::Elixir,
      "defmodule Foo do\n  import Enum\n  alias Foo.Bar\n\n  def run(x) do\n    baz(x)\n  end\nend\n",
    );
    assert!(
      refs.contains(&("baz".to_string(), RefKind::Call)),
      "{refs:?}"
    );
    assert!(
      refs.contains(&("Enum".to_string(), RefKind::Import)),
      "{refs:?}"
    );
    assert!(
      refs.contains(&("Foo.Bar".to_string(), RefKind::Import)),
      "{refs:?}"
    );
    // Definition forms emit nothing: no `def`/`defmodule` refs, no bogus self-call to `run`.
    assert!(
      refs
        .iter()
        .all(|(name, _)| name != "run" && name != "def" && name != "defmodule"),
      "{refs:?}"
    );
  }

  #[test]
  fn haskell_curried_application_emits_one_call() {
    let refs = refs_for(SupportLang::Haskell, "main = combine alpha beta\n");
    let calls: Vec<&(String, RefKind)> = refs.iter().filter(|(_, k)| *k == RefKind::Call).collect();
    assert_eq!(
      calls,
      vec![&("combine".to_string(), RefKind::Call)],
      "curried chain must emit exactly one call: {refs:?}"
    );
  }

  #[test]
  fn positional_callees_kotlin_swift_hcl() {
    let refs = refs_for(SupportLang::Kotlin, "fun main() {\n    greet(1)\n}\n");
    assert!(
      refs.contains(&("greet".to_string(), RefKind::Call)),
      "{refs:?}"
    );

    let refs = refs_for(SupportLang::Swift, "func main() {\n    greet(1)\n}\n");
    assert!(
      refs.contains(&("greet".to_string(), RefKind::Call)),
      "{refs:?}"
    );

    let refs = refs_for(SupportLang::Hcl, "locals {\n  a = upper(\"x\")\n}\n");
    assert!(
      refs.contains(&("upper".to_string(), RefKind::Call)),
      "{refs:?}"
    );
  }

  #[test]
  fn string_imports_keep_verbatim_module_strings() {
    let refs = refs_for(
      SupportLang::TypeScript,
      "import { x } from \"./util\";\nconst y = require(\"lodash\");\n",
    );
    assert!(
      refs.contains(&("./util".to_string(), RefKind::Import)),
      "{refs:?}"
    );
    assert!(
      refs.contains(&("lodash".to_string(), RefKind::Import)),
      "{refs:?}"
    );

    let refs = refs_for(SupportLang::C, "#include <stdio.h>\n");
    assert!(
      refs.contains(&("stdio.h".to_string(), RefKind::Import)),
      "{refs:?}"
    );

    let refs = refs_for(SupportLang::Ruby, "require 'json'\n");
    assert!(
      refs.contains(&("json".to_string(), RefKind::Import)),
      "{refs:?}"
    );

    let refs = refs_for(SupportLang::Bash, "source ./lib.sh\nhelper arg\n");
    assert!(
      refs.contains(&("./lib.sh".to_string(), RefKind::Import)),
      "{refs:?}"
    );
    assert!(
      refs.contains(&("helper".to_string(), RefKind::Call)),
      "{refs:?}"
    );
  }

  #[test]
  fn rust_static_and_self_calls_carry_qualifier_evidence() {
    let src = "impl Kg {\n  fn a(&self) {\n    self.helper();\n    Self::assoc();\n    Manifest::scan();\n    Vec::new();\n    plain();\n    value.method();\n  }\n}\n";
    let spec = resolved_ref_spec(SgLang::from(SupportLang::Rust)).unwrap();
    let grep = SgLang::from(SupportLang::Rust).grep(src);
    // Mimic extract_product's local layout: file + the `Kg` impl item spanning the source.
    let spans = vec![
      (0..usize::MAX, NodeId::new(0)),
      (0..src.len(), NodeId::new(1)),
    ];
    let entities = vec![
      EntityIdentity::FILE,
      EntityIdentity::new(None, "Kg", vorpal_kg::SymbolKind::Struct, ""),
    ];
    let mut out = Vec::new();
    extract_references(grep.root(), spec, &spans, &entities, &mut out);

    let find = |name: &str| out.iter().find(|r| r.name == name).expect(name);
    let helper = find("helper");
    assert_eq!(helper.form, RefForm::Method, "{helper:?}");
    assert_eq!(helper.qualifier.as_deref(), Some("Kg"), "self → owner");
    let assoc = find("assoc");
    assert_eq!(assoc.form, RefForm::Static, "{assoc:?}");
    assert_eq!(assoc.qualifier.as_deref(), Some("Kg"), "Self → owner");
    let scan = find("scan");
    assert_eq!(scan.form, RefForm::Static, "{scan:?}");
    assert_eq!(scan.qualifier.as_deref(), Some("Manifest"));
    let new = find("new");
    assert_eq!(new.form, RefForm::Static, "{new:?}");
    assert_eq!(new.qualifier.as_deref(), Some("Vec"));
    let plain = find("plain");
    assert_eq!(plain.form, RefForm::Bare, "{plain:?}");
    assert_eq!(plain.qualifier, None);
    let method = find("method");
    assert_eq!(method.form, RefForm::MethodHinted, "{method:?}");
    assert_eq!(
      method.qualifier.as_deref(),
      Some("value"),
      "a plain-name receiver rides as a HINT — the resolver corroborates it against owners \
       or drops it, never treats it as proof"
    );
  }

  #[test]
  fn generic_type_parameters_are_binders_not_type_uses() {
    let refs = refs_for(
      SupportLang::Rust,
      "fn convert<T: Clone, U>(x: T, y: U) -> Style {\n  render(x)\n}\n",
    );
    assert!(
      refs.iter().all(|(name, _)| name != "T" && name != "U"),
      "bound type params must not be uses: {refs:?}"
    );
    assert!(
      refs.contains(&("Clone".to_string(), RefKind::Type)),
      "trait bounds are real type uses: {refs:?}"
    );
    assert!(
      refs.contains(&("Style".to_string(), RefKind::Type)),
      "{refs:?}"
    );

    let refs = refs_for(
      SupportLang::TypeScript,
      "function pick<T>(x: T, s: Widget): T { return x; }\n",
    );
    assert!(
      refs.iter().all(|(name, _)| name != "T"),
      "TS type params are binders: {refs:?}"
    );
    assert!(
      refs.contains(&("Widget".to_string(), RefKind::Type)),
      "{refs:?}"
    );
  }

  #[test]
  fn placeholder_probe_java_var_swift_self_go_blank() {
    // Grammar probes: if a grammar surfaces these placeholders as type leaves, they must be
    // suppressed like Rust's `Self`/`_` — a failure here means the placeholder table needs the
    // entry, not that the assertion is wrong.
    let refs = refs_for(
      SupportLang::Java,
      "class A { void m() { var x = compute(); } }\n",
    );
    assert!(
      refs.iter().all(|(name, _)| name != "var"),
      "Java `var` is inference, not a type use: {refs:?}"
    );

    let refs = refs_for(
      SupportLang::Swift,
      "class A { func make() -> Self { return helper() } }\n",
    );
    assert!(
      refs.iter().all(|(name, _)| name != "Self"),
      "Swift `Self` is the enclosing type, not a use: {refs:?}"
    );

    let refs = refs_for(SupportLang::Go, "func f() {\n  var _ = g()\n}\n");
    assert!(
      refs.iter().all(|(name, _)| name != "_"),
      "Go blank identifier is not a type use: {refs:?}"
    );
  }

  #[test]
  fn go_type_parameters_are_binders() {
    let refs = refs_for(
      SupportLang::Go,
      "func Map[T any](x T, w Widget) T {\n  return x\n}\n",
    );
    assert!(
      refs.iter().all(|(name, _)| name != "T"),
      "Go type params are binders: {refs:?}"
    );
    assert!(
      refs.contains(&("Widget".to_string(), RefKind::Type)),
      "{refs:?}"
    );
  }

  #[test]
  fn rust_type_placeholders_are_never_uses() {
    let refs = refs_for(
      SupportLang::Rust,
      "impl Widget {\n  fn build() -> Self {\n    let x: Vec<_> = source();\n    Self { x }\n  }\n}\n",
    );
    assert!(
      refs.iter().all(|(name, _)| name != "Self" && name != "_"),
      "Self/_ are placeholders, not definitions: {refs:?}"
    );
  }

  /// Explicit-stack pre-order in *document order* (reversed child pushes) — the same visit
  /// order the fused walk's single-cursor `dfs()` produces, so outputs compare order-exactly.
  fn push_children<'t>(stack: &mut Vec<SgNode<'t>>, node: &SgNode<'t>) {
    let children: Vec<_> = node.children().collect();
    for child in children.into_iter().rev() {
      stack.push(child);
    }
  }

  /// The pre-fusion two-walk, string-dispatch algorithm, kept verbatim as the behavioral
  /// specification: the fused kind-id walk must reproduce its output reference-for-reference,
  /// in order, for any input.
  fn reference_impl<'t>(
    root: SgNode<'t>,
    resolved: &ResolvedRefSpec,
    def_spans: &[(Range<usize>, NodeId)],
    entities: &[EntityIdentity<'_>],
    out: &mut Vec<RawRef<'t>>,
  ) {
    let spec = &*resolved.spec;
    let mut binders: Vec<(Range<usize>, Cow<'t, str>)> = Vec::new();
    let mut stack = vec![root.clone()];
    while let Some(node) = stack.pop() {
      push_children(&mut stack, &node);
      if spec.type_params.iter().any(|t| t.as_str() == node.kind().as_ref()) {
        collect_binders_in(&node, &mut binders);
      }
    }

    let mut suppressed: HashSet<usize> = HashSet::new();
    let mut seen: HashSet<(u64, Cow<'t, str>, u8)> = HashSet::new();
    let mut stack = vec![root];
    while let Some(node) = stack.pop() {
      push_children(&mut stack, &node);
      let kind_cow = node.kind();
      let kind = kind_cow.as_ref();

      let mut is_import_node = false;
      for ispec in spec.imports.iter().filter(|i| i.kind == kind) {
        is_import_node = true;
        let range = node.range();
        if let Some(from) = outermost(def_spans, range.start) {
          for target in select_all(&node, &ispec.target) {
            let name = if ispec.string_target {
              literal_import_name(&target)
            } else {
              callee_name(&target)
            };
            if let Some(name) = name {
              let (form, qualifier) = match import_qualifier(&node, &target, &ispec.qualifier) {
                Some(q) => (RefForm::Static, Some(q)),
                None => (RefForm::Bare, None),
              };
              out.push(RawRef {
                from,
                name,
                kind: RefKind::Import,
                start: range.start as u32,
                end: range.end as u32,
                qualifier,
                form,
                receiver: None,
                args: Vec::new(),
                alias: import_alias(&target),
              });
            }
          }
        }
      }
      if is_import_node {
        continue;
      }

      if spec.types.iter().any(|t| t.as_str() == kind) {
        let Some(parent) = node.parent() else {
          continue;
        };
        if parent
          .field("name")
          .is_some_and(|name| name.node_id() == node.node_id())
        {
          continue;
        }
        let mut skip = false;
        let mut ancestor = Some(parent);
        for _ in 0..2 {
          let Some(a) = ancestor else { break };
          if spec.implements.iter().any(|s| s.kind == a.kind().as_ref()) {
            skip = true;
            break;
          }
          ancestor = a.parent();
        }
        if skip {
          continue;
        }
        let range = node.range();
        let (Some(name), Some(from)) = (callee_name(&node), enclosing(def_spans, range.start))
        else {
          continue;
        };
        if spec.type_placeholders.iter().any(|t| t.as_str() == name.as_ref()) {
          continue;
        }
        if binders
          .iter()
          .any(|(scope, binder)| *binder == name && scope.contains(&range.start))
        {
          continue;
        }
        if seen.insert((from.raw(), name.clone(), 0)) {
          out.push(RawRef::plain(
            from,
            name,
            RefKind::Type,
            range.start as u32,
            range.end as u32,
          ));
        }
        continue;
      }

      if let Some(ispec) = spec.implements.iter().find(|s| s.kind == kind) {
        let range = node.range();
        let Some(from) = enclosing(def_spans, range.start) else {
          continue;
        };
        let targets: Vec<SgNode<'t>> = match &ispec.target {
          Some(sel) => select_all(&node, sel),
          None => vec![node.clone()],
        };
        for target in targets {
          let names: Vec<Cow<'t, str>> = if let Some(name) = callee_name(&target) {
            vec![name]
          } else {
            first_descendants_of_kinds(&target, IMPL_TARGET_KINDS)
              .iter()
              .filter_map(callee_name)
              .collect()
          };
          for name in names {
            if spec.type_placeholders.iter().any(|t| t.as_str() == name.as_ref()) {
              continue;
            }
            if seen.insert((from.raw(), name.clone(), 1)) {
              out.push(RawRef::plain(
                from,
                name,
                RefKind::Implements,
                range.start as u32,
                range.end as u32,
              ));
            }
          }
        }
        continue;
      }

      let Some(cspec) = spec.calls.iter().find(|c| c.kind == kind) else {
        continue;
      };
      if suppressed.remove(&node.node_id()) || is_chain_link(&node, spec) {
        continue;
      }
      let Some(mut callee) = select(&node, &cspec.callee) else {
        continue;
      };
      while let Some(inner) = spec
        .calls
        .iter()
        .find(|c| c.kind == callee.kind().as_ref())
        .and_then(|c| select(&callee, &c.callee))
      {
        callee = inner;
      }
      let Some(name) = callee_name(&callee) else {
        continue;
      };
      let range = node.range();
      match spec
        .text_rules
        .iter()
        .find(|(text, _)| name.as_ref() == *text)
      {
        Some((_, TextAction::SkipDefinition)) => {
          if let Some(args) = node.children().find(|c| c.kind().as_ref() == "arguments") {
            if let Some(head) = args.children().find(|c| c.is_named()) {
              if spec.calls.iter().any(|c| c.kind == head.kind().as_ref()) {
                suppressed.insert(head.node_id());
              }
            }
          }
        }
        Some((_, TextAction::ImportFirstArg)) => {
          if let (Some(arg), Some(from)) =
            (first_argument(&node), outermost(def_spans, range.start))
          {
            if let Some(import) = import_arg_name(&arg) {
              out.push(RawRef::plain(
                from,
                import,
                RefKind::Import,
                range.start as u32,
                range.end as u32,
              ));
            }
          }
        }
        None => {
          if let Some(from) = enclosing(def_spans, range.start) {
            let (form, qualifier, _receiver) =
              classify_call(&node, cspec, &callee, spec, entities, from);
            // The twin mirrors the fused walk's no-typefacts configuration (the only one the
            // differential harness runs): extras are gated off there, so they are here.
            // Extras semantics have their own pinning suite (typefacts_capture.rs).
            out.push(RawRef {
              from,
              name,
              kind: RefKind::Call,
              start: range.start as u32,
              end: range.end as u32,
              qualifier,
              form,
              alias: None,
              receiver: None,
              args: Vec::new(),
            });
          }
        }
      }
    }
  }

  fn assert_fused_matches_reference(
    lang: SupportLang,
    src: &str,
    def_spans: &[(Range<usize>, NodeId)],
    entities: &[EntityIdentity<'_>],
    context: &str,
  ) {
    let lang = SgLang::from(lang);
    let Some(resolved) = resolved_ref_spec(lang) else {
      return;
    };
    let grep = lang.grep(src);
    let mut fused = Vec::new();
    extract_references(grep.root(), resolved, def_spans, entities, &mut fused);
    let mut reference = Vec::new();
    reference_impl(grep.root(), resolved, def_spans, entities, &mut reference);
    assert_eq!(fused, reference, "fused walk diverged on {context}");
  }

  #[test]
  fn fused_walk_matches_reference_implementation_on_battery() {
    use SupportLang as L;
    let battery: &[(SupportLang, &str)] = &[
      (
        L::Rust,
        "impl<T: Clone> Kg<T> {\n  fn a(&self) -> Self {\n    self.helper();\n    Self::assoc();\n    Manifest::scan();\n    Vec::new();\n    plain();\n    value.method();\n    let x: Style = util::mk();\n    x\n  }\n}\nimpl Widget for Kg<u8> {}\nuse std::collections::HashMap;\n",
      ),
      (
        L::TypeScript,
        "import { x } from \"./util\";\nconst y = require(\"lodash\");\nfunction pick<T>(v: T, s: Widget): T { return v; }\nclass A extends Base implements Iface { m() { this.n(); other.p(); } }\n",
      ),
      (
        L::Elixir,
        "defmodule Foo do\n  import Enum\n  alias Foo.Bar\n  def run(x) do\n    baz(x)\n  end\nend\n",
      ),
      (L::Haskell, "main = combine alpha beta\n"),
      (
        L::Go,
        "import \"fmt\"\nfunc Map[T any](x T, w Widget) T {\n  fmt.Println(x)\n  helper()\n  return x\n}\n",
      ),
      (
        L::Java,
        "import util.Helper;\nclass A extends B implements C {\n  void m() { var x = compute(); this.n(); obj.p(); }\n}\n",
      ),
      (
        L::Python,
        "from util import helper\nimport os\nclass A(Base):\n  def m(self):\n    self.n()\n    helper()\n    obj.p()\n",
      ),
      (
        L::Php,
        "<?php\nuse App\\Util;\nclass A { function m() { $this->n(); Util::stat(); helper(); $o->p(); } }\n",
      ),
      (L::C, "#include <stdio.h>\nvoid f(void) { g(); s.h(); }\n"),
      (
        L::Solidity,
        "import {A} from \"./a.sol\";\ncontract C { function f() public { g(); } }\n",
      ),
      (L::Ruby, "require 'json'\nobj.call_me\nhelper\n"),
      (L::Bash, "source ./lib.sh\nhelper arg\n"),
      (L::Kotlin, "fun main() {\n  greet(1)\n  obj.method()\n}\n"),
      (L::Swift, "func main() {\n  greet(1)\n  obj.method()\n}\n"),
      (L::Lua, "require 'mod'\nhelper()\nobj:method()\n"),
      (L::Nix, "let f = import ./x.nix; in f { a = helper 1; }\n"),
      (L::Hcl, "locals {\n  a = upper(\"x\")\n}\n"),
    ];
    let spans = vec![(0..usize::MAX, NodeId::new(0))];
    let entities = vec![EntityIdentity::FILE];
    for (lang, src) in battery {
      assert_fused_matches_reference(*lang, src, &spans, &entities, &format!("{lang:?} snippet"));
    }
  }

  #[test]
  fn fused_walk_matches_reference_implementation_on_this_workspace() {
    // Real-world sweep: every handled source file in this workspace, with the production
    // definition layout (items + members), so owner attribution paths are exercised too.
    let crates_root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
      .parent()
      .expect("crates dir");
    let extractor = crate::OutlineExtractor::new().expect("default rules compile");
    let mut checked = 0usize;
    for entry in ignore::Walk::new(crates_root) {
      let Ok(entry) = entry else { continue };
      if !entry.file_type().is_some_and(|t| t.is_file()) {
        continue;
      }
      let path = entry.path().to_string_lossy().into_owned();
      let Some(lang) = SupportLang::from_path(&path) else {
        continue;
      };
      if resolved_ref_spec(SgLang::from(lang)).is_none() {
        continue;
      }
      let Ok(source) = std::fs::read_to_string(entry.path()) else {
        continue;
      };
      let Some(product) = extractor.extract_product(&path, &source) else {
        continue;
      };
      let (entities, spans) = crate::outline_extractor::local_layout(&product.items);
      assert_fused_matches_reference(lang, &source, &spans, &entities, &path);
      checked += 1;
    }
    assert!(checked > 100, "swept only {checked} files — sweep broken?");
  }

  /// Stage-split throughput over this workspace — the standing §12/§7.5 measurement, kept as
  /// an ignored test so it compiles with the crate and cannot drift from the real pipeline.
  /// Run: `cargo test --release -p vorpal-ingest --ignored bench_extraction -- --nocapture`
  #[test]
  #[ignore = "benchmark: run explicitly with --ignored --nocapture"]
  fn bench_extraction_stages() {
    let repo_root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
      .parent()
      .and_then(std::path::Path::parent)
      .expect("repo root");
    let extractor = crate::OutlineExtractor::new().expect("rules compile");
    let mut files: Vec<(String, SupportLang, String)> = Vec::new();
    for entry in ignore::Walk::new(repo_root) {
      let Ok(entry) = entry else { continue };
      if !entry.file_type().is_some_and(|t| t.is_file()) {
        continue;
      }
      let path = entry.path().to_string_lossy().into_owned();
      if !extractor.handles(&path) {
        continue;
      }
      let Some(lang) = SupportLang::from_path(&path) else {
        continue;
      };
      if let Ok(source) = std::fs::read_to_string(entry.path()) {
        files.push((path, lang, source));
      }
    }
    let bytes: usize = files.iter().map(|(_, _, s)| s.len()).sum();
    eprintln!("{} files, {bytes} bytes", files.len());

    let time = |label: &str, f: &dyn Fn() -> usize| {
      let mut best = f64::MAX;
      let mut touched = 0;
      for _ in 0..3 {
        let t = std::time::Instant::now();
        touched = f();
        best = best.min(t.elapsed().as_secs_f64());
      }
      eprintln!(
        "{label:<22} {:>8.1} ms   {:>6.1} MB/s   (touched {touched})",
        best * 1e3,
        bytes as f64 / 1e6 / best
      );
    };

    time("parse only", &|| {
      files
        .iter()
        .map(|(_, lang, source)| lang.grep(source).root().range().len())
        .sum()
    });
    time("full extract_product", &|| {
      files
        .iter()
        .filter_map(|(path, _, source)| extractor.extract_product(path, source))
        .map(|p| p.items.len() + p.refs.len())
        .sum()
    });

    let dir = std::env::temp_dir().join("vorpal-bench-products");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let products: Vec<(String, crate::FileProduct)> = files
      .iter()
      .filter_map(|(path, _, source)| {
        Some((path.clone(), extractor.extract_product(path, source)?))
      })
      .collect();
    time("save products", &|| {
      for (path, product) in &products {
        crate::save_product(&dir.join(crate::cache_file_name(path)), product).unwrap();
      }
      products.len()
    });
    time("replay products", &|| {
      products
        .iter()
        .map(|(path, _)| {
          let p = crate::load_product(&dir.join(crate::cache_file_name(path))).unwrap();
          p.items.len() + p.refs.len()
        })
        .sum()
    });
    let total: u64 = std::fs::read_dir(&dir)
      .unwrap()
      .flatten()
      .filter_map(|e| e.metadata().ok().map(|m| m.len()))
      .sum();
    eprintln!("products cache        {:>8.1} KB", total as f64 / 1e3);
    let _ = std::fs::remove_dir_all(&dir);

    let interner = vorpal_resolve::Interner::default();
    time("apply (serial)", &|| {
      let mut writer = vorpal_kg::KgWriter::new();
      let mut references = Vec::new();
      for (path, product) in &products {
        crate::pipeline::apply_product(&interner, path, product.clone(), &mut writer, &mut references);
      }
      writer.node_count() + references.len()
    });
    time("apply (sharded)", &|| {
      let cloned: Vec<_> = products.clone();
      let (writer, references) = crate::apply_products_sharded(&interner, cloned);
      writer.node_count() + references.len()
    });
    time("apply+link (sharded)", &|| {
      let cloned: Vec<_> = products.clone();
      let (writer, references) = crate::apply_products_sharded(&interner, cloned);
      let (kg, stats) =
        crate::link_writer(&interner, writer, references, &vorpal_resolve::Resolver::new());
      kg.node_count() + stats.resolved as usize
    });
  }

  #[test]
  fn structural_languages_have_no_ref_spec() {
    for lang in [
      SupportLang::Css,
      SupportLang::Html,
      SupportLang::Json,
      SupportLang::Markdown,
      SupportLang::Yaml,
    ] {
      assert!(ref_spec(SgLang::from(lang)).is_none(), "{lang:?}");
    }
  }
}
