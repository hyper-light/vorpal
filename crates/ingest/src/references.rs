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

use std::collections::HashSet;
use std::ops::Range;

use vorpal_core::Node;
use vorpal_core::tree_sitter::StrDoc;
use vorpal_kg::NodeId;
use vorpal_language::SupportLang;
use vorpal_resolve::{RefKind, Reference};

type SgNode<'t> = Node<'t, StrDoc<SupportLang>>;

/// How to locate the referenced sub-node inside a matched call/import node.
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

struct CallSpec {
  kind: &'static str,
  callee: Sel,
}

struct ImportSpec {
  kind: &'static str,
  target: Sel,
  /// The target is a string/path literal (strip delimiters, keep the module string verbatim).
  string_target: bool,
}

/// Classification of a call by its extracted callee text.
enum TextAction {
  /// A definition form (`def`, `defmodule`, …): emit nothing and suppress the definition-head
  /// call (`def foo(x)` parses `foo(x)` as a call — it is a definition, not a call site).
  SkipDefinition,
  /// The call imports its first argument (`require 'x'`, `source ./x`, `alias Foo.Bar`).
  ImportFirstArg,
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
}

const NONE_TEXT: &[(&str, TextAction)] = &[];
const NO_TYPES: &[&str] = &[];
const NO_IMPL: &[ImplSpec] = &[];
const TYPE_ID: &[&str] = &["type_identifier"];

const RUST: RefSpec = RefSpec {
  calls: &[CallSpec {
    kind: "call_expression",
    callee: Sel::Field("function"),
  }],
  imports: &[ImportSpec {
    kind: "use_declaration",
    target: Sel::Field("argument"),
    string_target: false,
  }],
  text_rules: NONE_TEXT,
  types: TYPE_ID,
  implements: &[ImplSpec {
    kind: "impl_item",
    target: Some(Sel::Field("trait")),
  }],
};

const PYTHON: RefSpec = RefSpec {
  calls: &[CallSpec {
    kind: "call",
    callee: Sel::Field("function"),
  }],
  imports: &[
    ImportSpec {
      kind: "import_from_statement",
      target: Sel::Field("name"),
      string_target: false,
    },
    ImportSpec {
      kind: "import_statement",
      target: Sel::ChildOfKind(&["dotted_name", "aliased_import"]),
      string_target: false,
    },
  ],
  text_rules: NONE_TEXT,
  types: NO_TYPES,
  implements: &[ImplSpec {
    kind: "class_definition",
    target: Some(Sel::Field("superclasses")),
  }],
};

const GO: RefSpec = RefSpec {
  calls: &[CallSpec {
    kind: "call_expression",
    callee: Sel::Field("function"),
  }],
  imports: &[ImportSpec {
    kind: "import_spec",
    target: Sel::Field("path"),
    string_target: true,
  }],
  text_rules: NONE_TEXT,
  types: TYPE_ID,
  implements: NO_IMPL,
};

/// JavaScript / TypeScript / Tsx share one grammar family for calls + ES imports + `require`.
const JS_LIKE: RefSpec = RefSpec {
  calls: &[CallSpec {
    kind: "call_expression",
    callee: Sel::Field("function"),
  }],
  imports: &[ImportSpec {
    kind: "import_statement",
    target: Sel::Field("source"),
    string_target: true,
  }],
  text_rules: &[("require", TextAction::ImportFirstArg)],
  types: TYPE_ID,
  // `class_heritage` covers both TS (`extends`/`implements` clauses within) and JS (bare
  // `extends B`) in one row.
  implements: &[ImplSpec {
    kind: "class_heritage",
    target: None,
  }],
};

const C_LIKE: RefSpec = RefSpec {
  calls: &[CallSpec {
    kind: "call_expression",
    callee: Sel::Field("function"),
  }],
  imports: &[ImportSpec {
    kind: "preproc_include",
    target: Sel::Field("path"),
    string_target: true,
  }],
  text_rules: NONE_TEXT,
  types: TYPE_ID,
  implements: NO_IMPL,
};

const JAVA: RefSpec = RefSpec {
  calls: &[CallSpec {
    kind: "method_invocation",
    callee: Sel::Field("name"),
  }],
  imports: &[ImportSpec {
    kind: "import_declaration",
    target: Sel::ChildOfKind(&["scoped_identifier", "identifier"]),
    string_target: false,
  }],
  text_rules: NONE_TEXT,
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
};

const CSHARP: RefSpec = RefSpec {
  calls: &[CallSpec {
    kind: "invocation_expression",
    callee: Sel::Field("function"),
  }],
  imports: &[ImportSpec {
    kind: "using_directive",
    target: Sel::Field("name"),
    string_target: false,
  }],
  text_rules: NONE_TEXT,
  types: NO_TYPES,
  implements: &[ImplSpec {
    kind: "base_list",
    target: None,
  }],
};

const KOTLIN: RefSpec = RefSpec {
  calls: &[CallSpec {
    kind: "call_expression",
    callee: Sel::FirstNamedChild,
  }],
  imports: &[ImportSpec {
    kind: "import_header",
    target: Sel::ChildOfKind(&["identifier"]),
    string_target: false,
  }],
  text_rules: NONE_TEXT,
  types: TYPE_ID,
  implements: NO_IMPL,
};

const SWIFT: RefSpec = RefSpec {
  calls: &[CallSpec {
    kind: "call_expression",
    callee: Sel::FirstNamedChild,
  }],
  imports: &[ImportSpec {
    kind: "import_declaration",
    target: Sel::ChildOfKind(&["identifier"]),
    string_target: false,
  }],
  text_rules: NONE_TEXT,
  types: TYPE_ID,
  implements: NO_IMPL,
};

const RUBY: RefSpec = RefSpec {
  calls: &[CallSpec {
    kind: "call",
    callee: Sel::Field("method"),
  }],
  imports: &[],
  text_rules: &[
    ("require", TextAction::ImportFirstArg),
    ("require_relative", TextAction::ImportFirstArg),
  ],
  types: NO_TYPES,
  implements: NO_IMPL,
};

const PHP: RefSpec = RefSpec {
  calls: &[
    CallSpec {
      kind: "function_call_expression",
      callee: Sel::Field("function"),
    },
    CallSpec {
      kind: "member_call_expression",
      callee: Sel::Field("name"),
    },
    CallSpec {
      kind: "nullsafe_member_call_expression",
      callee: Sel::Field("name"),
    },
    CallSpec {
      kind: "scoped_call_expression",
      callee: Sel::Field("name"),
    },
  ],
  imports: &[ImportSpec {
    kind: "namespace_use_declaration",
    target: Sel::ChildOfKind(&["namespace_name", "name"]),
    string_target: false,
  }],
  text_rules: NONE_TEXT,
  types: NO_TYPES,
  implements: NO_IMPL,
};

const DART: RefSpec = RefSpec {
  calls: &[
    CallSpec {
      kind: "call_expression",
      callee: Sel::Field("function"),
    },
    CallSpec {
      kind: "constructor_invocation",
      callee: Sel::Field("constructor"),
    },
  ],
  imports: &[ImportSpec {
    kind: "import_specification",
    target: Sel::Field("uri"),
    string_target: true,
  }],
  text_rules: NONE_TEXT,
  types: NO_TYPES,
  implements: NO_IMPL,
};

const SCALA: RefSpec = RefSpec {
  calls: &[CallSpec {
    kind: "call_expression",
    callee: Sel::Field("function"),
  }],
  imports: &[ImportSpec {
    kind: "import_declaration",
    target: Sel::FieldLast("path"),
    string_target: false,
  }],
  text_rules: NONE_TEXT,
  types: NO_TYPES,
  implements: NO_IMPL,
};

const LUA: RefSpec = RefSpec {
  calls: &[CallSpec {
    kind: "function_call",
    callee: Sel::Field("name"),
  }],
  imports: &[],
  text_rules: &[("require", TextAction::ImportFirstArg)],
  types: NO_TYPES,
  implements: NO_IMPL,
};

const BASH: RefSpec = RefSpec {
  calls: &[CallSpec {
    kind: "command",
    callee: Sel::Field("name"),
  }],
  imports: &[],
  text_rules: &[
    ("source", TextAction::ImportFirstArg),
    (".", TextAction::ImportFirstArg),
  ],
  types: NO_TYPES,
  implements: NO_IMPL,
};

const ELIXIR: RefSpec = RefSpec {
  calls: &[CallSpec {
    kind: "call",
    callee: Sel::Field("target"),
  }],
  imports: &[],
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
  types: NO_TYPES,
  implements: NO_IMPL,
};

const HASKELL: RefSpec = RefSpec {
  calls: &[CallSpec {
    kind: "apply",
    callee: Sel::Field("function"),
  }],
  imports: &[ImportSpec {
    kind: "import",
    target: Sel::Field("module"),
    string_target: false,
  }],
  text_rules: NONE_TEXT,
  types: NO_TYPES,
  implements: NO_IMPL,
};

const SOLIDITY: RefSpec = RefSpec {
  // The pinned grammar's call_expression carries its callee as a child `expression` wrapper,
  // not a `function` field (verified by parse probe).
  calls: &[CallSpec {
    kind: "call_expression",
    callee: Sel::FirstNamedChild,
  }],
  imports: &[
    ImportSpec {
      kind: "import_directive",
      target: Sel::Field("import_name"),
      string_target: false,
    },
    ImportSpec {
      kind: "import_directive",
      target: Sel::Field("source"),
      string_target: true,
    },
  ],
  text_rules: NONE_TEXT,
  types: NO_TYPES,
  implements: NO_IMPL,
};

const NIX: RefSpec = RefSpec {
  calls: &[CallSpec {
    kind: "apply_expression",
    callee: Sel::Field("function"),
  }],
  imports: &[],
  text_rules: &[("import", TextAction::ImportFirstArg)],
  types: NO_TYPES,
  implements: NO_IMPL,
};

const HCL: RefSpec = RefSpec {
  calls: &[CallSpec {
    kind: "function_call",
    callee: Sel::FirstNamedChild,
  }],
  imports: &[],
  text_rules: NONE_TEXT,
  types: NO_TYPES,
  implements: NO_IMPL,
};

/// Reference-extraction spec for a language. Pure-structural languages (CSS, HTML, JSON,
/// Markdown, YAML) have no call/import semantics and return `None`.
pub(crate) fn ref_spec(lang: SupportLang) -> Option<&'static RefSpec> {
  use SupportLang as L;
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
    L::Nix => Some(&NIX),
    L::Hcl => Some(&HCL),
    _ => None,
  }
}

/// Emit `calls` and `imports` references from the parse tree.
pub(crate) fn extract_references(
  root: SgNode<'_>,
  spec: &RefSpec,
  def_spans: &[(Range<usize>, NodeId)],
  path: &str,
  out: &mut Vec<Reference>,
) {
  // Definition-head calls suppressed by a SkipDefinition rule (`def foo(x)` → `foo(x)`).
  let mut suppressed: HashSet<usize> = HashSet::new();
  // Dedup for type/implements references: one edge per (from, name, kind) per file.
  let mut seen: HashSet<(u64, String, u8)> = HashSet::new();
  let mut stack = vec![root];
  while let Some(node) = stack.pop() {
    for child in node.children() {
      stack.push(child);
    }
    let kind_cow = node.kind();
    let kind = kind_cow.as_ref();

    let mut is_import_node = false;
    for ispec in spec.imports.iter().filter(|i| i.kind == kind) {
      is_import_node = true;
      emit_imports(&node, ispec, def_spans, path, out);
    }
    if is_import_node {
      continue;
    }

    if spec.types.contains(&kind) {
      emit_type_use(&node, spec, def_spans, path, &mut seen, out);
      continue;
    }
    if let Some(ispec) = spec.implements.iter().find(|s| s.kind == kind) {
      emit_implements(&node, ispec, def_spans, path, &mut seen, out);
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
    match spec.text_rules.iter().find(|(text, _)| *text == name) {
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
        if let (Some(arg), Some(from)) = (first_argument(&node), outermost(def_spans, range.start))
        {
          if let Some(import) = import_arg_name(&arg) {
            out.push(
              Reference::new(from, path, import, RefKind::Import)
                .with_evidence(range.start as u32, range.end as u32),
            );
          }
        }
      }
      None => {
        if let Some(from) = enclosing(def_spans, range.start) {
          out.push(
            Reference::new(from, path, name, RefKind::Call)
              .with_evidence(range.start as u32, range.end as u32),
          );
        }
      }
    }
  }
}

/// Leaf kinds an implements construct's targets reduce to.
const IMPL_TARGET_KINDS: &[&str] = &["type_identifier", "identifier", "constant", "alias"];

/// A type-identifier leaf marks a type USE unless it is a definition's own name or sits inside
/// an implements construct (which emits `implements`, not `of_type`).
fn emit_type_use(
  node: &SgNode<'_>,
  spec: &RefSpec,
  def_spans: &[(Range<usize>, NodeId)],
  path: &str,
  seen: &mut HashSet<(u64, String, u8)>,
  out: &mut Vec<Reference>,
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
  let (Some(name), Some(from)) = (callee_name(node), enclosing(def_spans, range.start)) else {
    return;
  };
  if seen.insert((from.raw(), name.clone(), 0)) {
    out.push(
      Reference::new(from, path, name, RefKind::Type)
        .with_evidence(range.start as u32, range.end as u32),
    );
  }
}

/// Emit an `implements` reference per implemented type: the construct's target selector (or the
/// node itself) is reduced to a name directly when possible, else to its type leaves.
fn emit_implements(
  node: &SgNode<'_>,
  ispec: &ImplSpec,
  def_spans: &[(Range<usize>, NodeId)],
  path: &str,
  seen: &mut HashSet<(u64, String, u8)>,
  out: &mut Vec<Reference>,
) {
  let range = node.range();
  let Some(from) = enclosing(def_spans, range.start) else {
    return;
  };
  let targets: Vec<SgNode<'_>> = match &ispec.target {
    Some(sel) => select_all(node, sel),
    None => vec![node.clone()],
  };
  for target in targets {
    let names: Vec<String> = if let Some(name) = callee_name(&target) {
      vec![name]
    } else {
      first_descendants_of_kinds(&target, IMPL_TARGET_KINDS)
        .iter()
        .filter_map(callee_name)
        .collect()
    };
    for name in names {
      if seen.insert((from.raw(), name.clone(), 1)) {
        out.push(
          Reference::new(from, path, name, RefKind::Implements)
            .with_evidence(range.start as u32, range.end as u32),
        );
      }
    }
  }
}

/// A call node that is its same-kind parent's selected callee is a chain link, not a call site.
fn is_chain_link(node: &SgNode<'_>, spec: &RefSpec) -> bool {
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

fn select<'t>(node: &SgNode<'t>, sel: &Sel) -> Option<SgNode<'t>> {
  match sel {
    Sel::Field(name) => node.field(name),
    Sel::FieldLast(name) => node.field_children(name).last(),
    Sel::FirstNamedChild => node.children().find(|c| c.is_named()),
    Sel::ChildOfKind(kinds) => first_descendants_of_kinds(node, kinds).into_iter().next(),
  }
}

/// All matching targets for an import node (repeated fields / multiple names per statement).
fn select_all<'t>(node: &SgNode<'t>, sel: &Sel) -> Vec<SgNode<'t>> {
  match sel {
    Sel::Field(name) => {
      let all: Vec<_> = node.field_children(name).collect();
      if all.is_empty() {
        node.field(name).into_iter().collect()
      } else {
        all
      }
    }
    Sel::FieldLast(name) => node.field_children(name).last().into_iter().collect(),
    Sel::FirstNamedChild => node.children().find(|c| c.is_named()).into_iter().collect(),
    Sel::ChildOfKind(kinds) => first_descendants_of_kinds(node, kinds),
  }
}

/// Pre-order descendants whose kind is listed, without descending into matches (so an
/// `aliased_import` match does not also yield its inner `dotted_name`).
fn first_descendants_of_kinds<'t>(node: &SgNode<'t>, kinds: &[&str]) -> Vec<SgNode<'t>> {
  let mut found = Vec::new();
  let mut queue: Vec<SgNode<'t>> = node.children().collect();
  let mut index = 0;
  while index < queue.len() {
    let current = queue[index].clone();
    index += 1;
    if kinds.contains(&current.kind().as_ref()) {
      found.push(current);
    } else {
      queue.extend(current.children());
    }
  }
  found
}

fn emit_imports(
  node: &SgNode<'_>,
  ispec: &ImportSpec,
  def_spans: &[(Range<usize>, NodeId)],
  path: &str,
  out: &mut Vec<Reference>,
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
      out.push(
        Reference::new(from, path, name, RefKind::Import)
          .with_evidence(range.start as u32, range.end as u32),
      );
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
fn import_arg_name(arg: &SgNode<'_>) -> Option<String> {
  callee_name(arg).or_else(|| literal_import_name(arg))
}

/// Strip string delimiters only; keep the module string verbatim (`./util`, `fmt`,
/// `package:foo/bar.dart`). Path-like names are honestly unresolvable against symbol tables.
fn literal_import_name(node: &SgNode<'_>) -> Option<String> {
  let text = node.text();
  let trimmed = text
    .trim()
    .trim_matches(|c| matches!(c, '"' | '\'' | '`' | '<' | '>'));
  (!trimmed.is_empty() && !trimmed.contains(char::is_whitespace)).then(|| trimmed.to_string())
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
];

/// The rightmost identifier of a callee/import expression — one universal navigator; unmatched
/// kinds return `None` (no guessing).
fn callee_name(node: &SgNode<'_>) -> Option<String> {
  let kind_cow = node.kind();
  let kind = kind_cow.as_ref();
  // Elixir module references keep their dotted form (`Foo.Bar`), unlike path-style rightmost.
  if kind == "alias" {
    return Some(node.text().into_owned());
  }
  if LEAF_KINDS.contains(&kind) {
    if let Some(last) = node.children().filter(|c| c.is_named()).last() {
      return callee_name(&last);
    }
    return Some(node.text().into_owned());
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
fn enclosing(def_spans: &[(Range<usize>, NodeId)], offset: usize) -> Option<NodeId> {
  def_spans
    .iter()
    .filter(|(range, _)| range.contains(&offset))
    .min_by_key(|(range, _)| range.end - range.start)
    .map(|(_, id)| *id)
}

/// The outermost (largest) definition span containing `offset` — the file, for import attribution.
fn outermost(def_spans: &[(Range<usize>, NodeId)], offset: usize) -> Option<NodeId> {
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

  fn refs_for(lang: SupportLang, src: &str) -> Vec<(String, RefKind)> {
    let spec = ref_spec(lang).expect("language has a ref spec");
    let grep = lang.grep(src);
    let spans = vec![(0..usize::MAX, NodeId::new(0))];
    let mut out = Vec::new();
    extract_references(grep.root(), spec, &spans, "test", &mut out);
    let mut refs: Vec<(String, RefKind)> = out.into_iter().map(|r| (r.name, r.kind)).collect();
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
  fn structural_languages_have_no_ref_spec() {
    for lang in [
      SupportLang::Css,
      SupportLang::Html,
      SupportLang::Json,
      SupportLang::Markdown,
      SupportLang::Yaml,
    ] {
      assert!(ref_spec(lang).is_none(), "{lang:?}");
    }
  }
}
