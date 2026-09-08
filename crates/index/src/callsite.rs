//! Call patterns answered from the product's call references, no parse.
//!
//! Every call the extractor saw is a reference record with the call node's span and, since
//! product v22, its call shape: the named non-comment argument count and whether the callee
//! is a bare identifier spelled exactly the reference name. An ast-grep call pattern whose
//! arguments are all single metavariables (`f($A, $B)`) matches exactly the call nodes with a
//! plain callee `f` and that many named non-comment arguments (Smart strictness skips
//! comments and unnamed tokens); `f($$$)` matches every plain call of `f`; `f()` the
//! zero-argument ones. Those shapes read straight from the product. Anything else — a
//! qualified callee, a literal argument, `$A, $$$` (which needs its comma), a repeated
//! metavariable, a selector/context pattern — takes the parse path.
//!
//! Exact by construction relative to the whole-file parse: the references come from the
//! same parse the reference arm runs, so the only thing to get right is the filter above,
//! pinned by the fixture oracle (`tests/chunks.rs`) and the kernel parity phase.
//! `VORPAL_NO_CALLSITE_PATH=1` vetoes.

use std::sync::OnceLock;

use vorpal_core::matcher::PatternSpec;
use vorpal_core::meta_var::MetaVariable;
use vorpal_ingest::SgLang;
use vorpal_language::Language;

pub fn callsite_disabled() -> bool {
  static FLAG: OnceLock<bool> = OnceLock::new();
  *FLAG.get_or_init(|| std::env::var_os("VORPAL_NO_CALLSITE_PATH").is_some_and(|v| v == "1"))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Arity {
  Exact(u32),
  Any,
}

/// A call pattern the product answers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CallShape {
  pub callee: String,
  pub arity: Arity,
  /// The language's call node kind, for structural records.
  pub kind: &'static str,
}

/// Whether a call reference's shape is opaque: tree-sitter recovered inside the call (ast-grep
/// skips ERROR children and counts MISSING ones) or the callee is itself a call (ast-grep
/// matches the inner call the walk never records) — the reference cannot stand in for the
/// match, the file must parse.
#[inline]
pub fn shape_has_error(call_shape: u32) -> bool {
  call_shape & 2 == 2
}

impl CallShape {
  /// Whether a call reference with `call_shape` (see `ProductRef::call_shape`) is one of
  /// this pattern's matches. The caller has already matched the name, kind, and form, and
  /// has routed any error-bearing call (see [`shape_has_error`]) to the parse path.
  #[inline]
  pub fn admits(&self, call_shape: u32) -> bool {
    call_shape & 1 == 1
      && match self.arity {
        Arity::Any => true,
        Arity::Exact(n) => call_shape >> 2 == n,
      }
  }
}

/// The call shape of `spec` under `lang`, or `None` when the pattern is not one the product
/// answers.
pub fn shape_of(spec: &PatternSpec<'_>, lang: SgLang) -> Option<CallShape> {
  use vorpal_core::tree_sitter::LanguageExt;
  if callsite_disabled() || spec.selector.is_some() || spec.context.is_some() {
    return None;
  }
  let (call_kind, callee_field) = vorpal_ingest::call_callee_field(lang)?;
  // After `pre_process_pattern` every `$` is the language's expando character (C and
  // friends cannot lex `$` in an identifier) — extract metavariables against that.
  let meta = lang.expando_char();
  let src = lang.pre_process_pattern(spec.pattern).into_owned();
  let want = src.trim();
  let find_call = |root: &vorpal_core::Vorpal<vorpal_core::tree_sitter::StrDoc<SgLang>>| -> Option<(String, Arity)> {
    let call = root
      .root()
      .dfs()
      .find(|n| n.kind().as_ref() == call_kind && n.text().as_ref().trim() == want)?;
    let callee = call.field(callee_field)?;
    if !vorpal_ingest::is_leaf_kind(callee.kind().as_ref()) {
      return None;
    }
    let callee_text = callee.text().into_owned();
    if callee_text.contains(meta) || callee_text.is_empty() {
      return None;
    }
    let container = call.field("arguments").or_else(|| {
      call.children().find(|c| {
        matches!(
          c.kind().as_ref(),
          "arguments" | "argument_list" | "call_suffix"
        )
      })
    })?;
    let args: Vec<_> = container
      .children()
      .filter(|c| c.is_named() && !c.kind().contains("comment"))
      .collect();
    let mut names: Vec<String> = Vec::with_capacity(args.len());
    let mut multi = 0usize;
    for arg in &args {
      match lang.extract_meta_var(arg.text().as_ref()) {
        Some(MetaVariable::Capture(name, _)) => {
          if names.contains(&name) {
            return None; // `f($A, $A)` demands equal arguments — not an arity test
          }
          names.push(name);
        }
        Some(MetaVariable::Dropped(_)) => names.push(String::new()),
        Some(MetaVariable::Multiple | MetaVariable::MultiCapture(_)) => multi += 1,
        None => return None, // a literal argument
      }
    }
    let arity = match (multi, names.len()) {
      (0, n) => Arity::Exact(n as u32),
      (1, 0) => Arity::Any,
      _ => return None, // `$A, $$$` needs its comma; `$$$, $$$` is not a shape
    };
    Some((callee_text, arity))
  };
  let plain = lang.grep(&src);
  let found = find_call(&plain).or_else(|| {
    let root_kind = plain.root().children().find(|c| c.is_named()).map(|c| c.kind_id());
    let (context, _) = lang.contextual_fallback(&src, root_kind)?;
    find_call(&lang.grep(&context))
  })?;
  Some(CallShape {
    callee: found.0,
    arity: found.1,
    kind: call_kind,
  })
}

#[cfg(test)]
mod tests {
  use super::*;

  fn shape(pattern: &str, lang: SgLang) -> Option<CallShape> {
    shape_of(&PatternSpec::plain(pattern), lang)
  }

  #[test]
  fn call_shapes_are_recognized_and_everything_else_declines() {
    use vorpal_language::SupportLang;
    let c = SgLang::Builtin(SupportLang::C);
    let rust = SgLang::Builtin(SupportLang::Rust);
    let py = SgLang::Builtin(SupportLang::Python);
    assert_eq!(shape("kmalloc($A, $B)", c), Some(CallShape { callee: "kmalloc".into(), arity: Arity::Exact(2), kind: "call_expression" }));
    assert_eq!(shape("kfree($A)", c).map(|s| s.arity), Some(Arity::Exact(1)));
    assert_eq!(shape("f()", c).map(|s| s.arity), Some(Arity::Exact(0)));
    assert_eq!(shape("f($$$)", c).map(|s| s.arity), Some(Arity::Any));
    assert_eq!(shape("f($$$ARGS)", c).map(|s| s.arity), Some(Arity::Any));
    assert_eq!(shape("f($_, $_)", c).map(|s| s.arity), Some(Arity::Exact(2)));
    assert_eq!(shape("foo($A)", rust).map(|s| s.callee), Some("foo".into()));
    assert_eq!(shape("os.path.join($A, $B)", py), None, "qualified callee");
    assert_eq!(shape("stat($A)", py).map(|s| s.kind), Some("call"));
    assert_eq!(shape("$R = f($A)", c), None, "not a bare call");
    assert_eq!(shape("f($A, 0)", c), None, "literal argument");
    assert_eq!(shape("f($A, $A)", c), None, "repeated metavariable");
    assert_eq!(shape("f($A, $$$)", c), None, "mixed shape needs its comma");
    assert_eq!(shape("fs::metadata($A)", rust), None, "static path callee");
    assert_eq!(shape("if ($C) return $X;", c), None);
    let admits = |s: &CallShape, arity: u32, plain: bool| s.admits(arity << 2 | u32::from(plain));
    assert!(shape_has_error(2 << 2 | 2 | 1));
    assert!(!shape_has_error(2 << 2 | 1));
    let two = shape("f($A, $B)", c).unwrap();
    assert!(admits(&two, 2, true));
    assert!(!admits(&two, 3, true));
    assert!(!admits(&two, 2, false));
    assert!(admits(&shape("f($$$)", c).unwrap(), 7, true));
  }
}
