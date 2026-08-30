//! Type-fact capture (G-M1): per-language static tables describing where a file binds a name
//! to a type — explicit annotations, constructor-shaped initializers, typed parameters, typed
//! fields — mirroring the `RefSpec` discipline (consts resolved to kind ids once, dispatched
//! by dense table in the SAME fused walk as reference extraction; capture cost is
//! proportional to binding sites, not tokens).
//!
//! Capture is mechanical, never inferential: an annotation's text is recorded as written; a
//! constructor-shaped initializer records the constructor's simple name. Whether that name IS
//! a type — and whether the binding narrows a method call's candidates — is the resolver's
//! judgment (G-M2), made against real candidates. Disagreeing bindings for one name poison it
//! to "no type": conservative by design.
//!
//! Launch languages: Rust, Python, TypeScript, TSX. `TYPEFACTS_VERSION` folds into the
//! extraction identity, so ANY change to these tables re-keys products without a format bump.

use std::borrow::Cow;

use vorpal_core::Language;
use vorpal_lang_registry::SgLang;
use vorpal_language::SupportLang;

/// Bump on ANY semantic change to the capture tables below — it folds into the extraction
/// identity, so stale products can never replay into a build with different capture rules.
pub const TYPEFACTS_VERSION: u64 = 1;

/// Where a binding's type knowledge came from — persisted with the product, mapped onto the
/// receiver-typed `ResolveReason`s in G-M2. Discriminants are the persisted tags.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum BindOrigin {
  /// An explicit type annotation (`let x: T`, `x: T = …`, `const x: T`).
  Annotated = 0,
  /// A constructor-shaped initializer (`T::new()`, `T(...)`, `new T()`, `T { .. }`).
  Constructed = 1,
  /// A typed function/method parameter.
  Param = 2,
  /// A typed field on an enclosing type.
  Field = 3,
}

impl BindOrigin {
  pub fn tag(self) -> u8 {
    self as u8
  }
  pub fn from_tag(tag: u8) -> Option<Self> {
    match tag {
      0 => Some(Self::Annotated),
      1 => Some(Self::Constructed),
      2 => Some(Self::Param),
      3 => Some(Self::Field),
      _ => None,
    }
  }
}

/// One captured binding, borrowing the parse.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RawBinding<'t> {
  pub(crate) name: Cow<'t, str>,
  /// The type text as written (annotations) or the constructor's simple name (constructed);
  /// `None` when the site binds a name without usable type evidence.
  pub(crate) ty: Option<Cow<'t, str>>,
  pub(crate) origin: BindOrigin,
  /// Byte offset of the binding site (for attributing params to their enclosing definition).
  pub(crate) start: u32,
}

/// One authoring row: at nodes of `kind`, the name lives under `name_field` and the type
/// (when annotated) under `type_field`; `value_field` names the initializer to try
/// constructor-shape recovery on when the annotation is absent.
pub(crate) struct BindSpec {
  kind: &'static str,
  origin: BindOrigin,
  name_field: &'static str,
  type_field: Option<&'static str>,
  value_field: Option<&'static str>,
}

pub(crate) struct TypeSpec {
  binds: &'static [BindSpec],
}

const RUST_TF: TypeSpec = TypeSpec {
  binds: &[
    BindSpec {
      kind: "let_declaration",
      origin: BindOrigin::Annotated,
      name_field: "pattern",
      type_field: Some("type"),
      value_field: Some("value"),
    },
    BindSpec {
      kind: "parameter",
      origin: BindOrigin::Param,
      name_field: "pattern",
      type_field: Some("type"),
      value_field: None,
    },
    BindSpec {
      kind: "field_declaration",
      origin: BindOrigin::Field,
      name_field: "name",
      type_field: Some("type"),
      value_field: None,
    },
  ],
};

const PYTHON_TF: TypeSpec = TypeSpec {
  binds: &[
    BindSpec {
      kind: "assignment",
      origin: BindOrigin::Annotated,
      name_field: "left",
      type_field: Some("type"),
      value_field: Some("right"),
    },
    BindSpec {
      kind: "typed_parameter",
      origin: BindOrigin::Param,
      // typed_parameter's name is its first named child (no field); handled by the
      // first-named-child fallback in `capture_at`.
      name_field: "",
      type_field: Some("type"),
      value_field: None,
    },
    BindSpec {
      kind: "typed_default_parameter",
      origin: BindOrigin::Param,
      name_field: "name",
      type_field: Some("type"),
      value_field: None,
    },
  ],
};

const TS_TF: TypeSpec = TypeSpec {
  binds: &[
    BindSpec {
      kind: "variable_declarator",
      origin: BindOrigin::Annotated,
      name_field: "name",
      type_field: Some("type"),
      value_field: Some("value"),
    },
    BindSpec {
      kind: "required_parameter",
      origin: BindOrigin::Param,
      name_field: "pattern",
      type_field: Some("type"),
      value_field: None,
    },
    BindSpec {
      kind: "optional_parameter",
      origin: BindOrigin::Param,
      name_field: "pattern",
      type_field: Some("type"),
      value_field: None,
    },
    BindSpec {
      kind: "public_field_definition",
      origin: BindOrigin::Field,
      name_field: "name",
      type_field: Some("type"),
      value_field: Some("value"),
    },
  ],
};

/// The capture tables for a language, if it has any (launch set: Rust, Python, TS, TSX).
pub(crate) fn type_spec(lang: SgLang) -> Option<&'static TypeSpec> {
  let SgLang::Builtin(lang) = lang else {
    return None;
  };
  match lang {
    SupportLang::Rust => Some(&RUST_TF),
    SupportLang::Python => Some(&PYTHON_TF),
    SupportLang::TypeScript | SupportLang::Tsx => Some(&TS_TF),
    _ => None,
  }
}

/// Kind-id-resolved dispatch, built once per (language, process) beside the ref specs.
pub(crate) struct ResolvedTypeFacts {
  /// kind_id → index into `spec.binds`, dense.
  arms: Vec<u16>,
  spec: &'static TypeSpec,
}

const NO_ARM: u16 = u16::MAX;

impl ResolvedTypeFacts {
  pub(crate) fn build(lang: SgLang, spec: &'static TypeSpec) -> Self {
    let mut max_id = 0u16;
    let ids: Vec<Option<u16>> = spec
      .binds
      .iter()
      .map(|bind| match lang.kind_to_id(bind.kind) {
        0 => None,
        id => {
          max_id = max_id.max(id);
          Some(id)
        }
      })
      .collect();
    let mut arms = vec![NO_ARM; max_id as usize + 1];
    // First matching row wins on (unlikely) kind overlap — authoring order is priority.
    for (index, id) in ids.iter().enumerate().rev() {
      if let Some(id) = id {
        arms[*id as usize] = index as u16;
      }
    }
    Self { arms, spec }
  }

  #[inline]
  pub(crate) fn arm(&self, kind_id: u16) -> Option<&'static BindSpec> {
    let index = *self.arms.get(kind_id as usize)?;
    if index == NO_ARM {
      return None;
    }
    self.spec.binds.get(index as usize)
  }
}

/// Capture one binding site. Total: a node that doesn't yield usable evidence produces
/// nothing — never a guess.
pub(crate) fn capture_at<'t>(
  bind: &'static BindSpec,
  node: &crate::references::SgNodeAlias<'t>,
  out: &mut Vec<RawBinding<'t>>,
) {
  // Name: the declared field, else the first named child (fieldless grammars).
  let name_node = if bind.name_field.is_empty() {
    node.children().find(|c| c.is_named())
  } else {
    node.field(bind.name_field)
  };
  let Some(name_node) = name_node else {
    return;
  };
  // Only simple-name bindings type receivers; destructuring patterns are skipped whole.
  let name = name_node.text();
  if name.is_empty() || name.len() > 64 || !is_simple_name(&name) {
    return;
  }

  let ty = bind
    .type_field
    .and_then(|f| node.field(f))
    .and_then(|ty_node| clean_type_text(ty_node.text()))
    .map(|t| (t, bind.origin));
  let ty = match ty {
    Some((text, origin)) => Some((text, origin)),
    None => bind
      .value_field
      .and_then(|f| node.field(f))
      .and_then(|value| constructor_name(&value))
      .map(|t| (t, BindOrigin::Constructed)),
  };
  let start = node.range().start as u32;
  match ty {
    Some((text, origin)) => out.push(RawBinding {
      name,
      ty: Some(text),
      origin,
      start,
    }),
    // A typed-parameter row with no recoverable type still records the PARAM name (entity
    // params list every parameter, typed or not).
    None if bind.origin == BindOrigin::Param => out.push(RawBinding {
      name,
      ty: None,
      origin: BindOrigin::Param,
      start,
    }),
    None => {}
  }
}

fn is_simple_name(text: &str) -> bool {
  !text.is_empty()
    && text
      .chars()
      .all(|c| c.is_alphanumeric() || c == '_' || c == '$')
}

/// Annotation text, cleaned: leading `:` and whitespace stripped, capped at 64 bytes.
fn clean_type_text(text: Cow<'_, str>) -> Option<Cow<'_, str>> {
  let trimmed = text.trim().trim_start_matches(':').trim();
  if trimmed.is_empty() || trimmed.len() > 64 {
    return None;
  }
  Some(Cow::Owned(trimmed.to_string()))
}

/// The constructor's simple name for constructor-shaped initializers:
/// `T::new(...)` → `T`; `new T(...)` → `T`; `T(...)` → `T`; `T { .. }` (Rust) → `T`.
/// Everything else is not constructor-shaped.
fn constructor_name<'t>(value: &crate::references::SgNodeAlias<'t>) -> Option<Cow<'t, str>> {
  let kind_cow = value.kind();
  match kind_cow.as_ref() {
    "struct_expression" => value.field("name").map(|n| n.text()),
    "new_expression" => value
      .field("constructor")
      .map(|n| n.text())
      .filter(|t| is_simple_name(t)),
    "call_expression" | "call" => {
      let callee = value.field("function")?;
      let text = callee.text();
      if let Some(prefix) = text.strip_suffix("::new") {
        let simple = prefix.rsplit("::").next().unwrap_or(prefix);
        return is_simple_name(simple).then(|| Cow::Owned(simple.to_string()));
      }
      is_simple_name(&text).then_some(text)
    }
    _ => None,
  }
}
