#![cfg(test)]

use super::*;
use vorpal_core::Pattern;
use vorpal_core::matcher::MatcherExt;
use vorpal_core::tree_sitter::LanguageExt;

fn smart(query: &str, source: &str) -> bool {
  let cand = SupportLang::C.grep(source);
  let pattern = Pattern::try_new_smart(query, SupportLang::C).expect("pattern builds");
  pattern.find_node(cand.root()).is_some()
}

#[test]
fn one_argument_call_patterns_find_calls_in_every_position() {
  // tree-sitter C reads `schedule_timeout($A)` alone as a macro_type_specifier; the smart
  // constructor re-parses it in statement context and roots at the call.
  assert!(smart("schedule_timeout($A)", "long f(void) { long r; r = schedule_timeout(5); return r; }"));
  assert!(smart("schedule_timeout($A)", "void f(void) { schedule_timeout(3); }"));
  assert!(smart("schedule_timeout($A)", "int f(void) { if (schedule_timeout(1)) { return 1; } return 0; }"));
  assert!(smart("kfree($$$)", "void f(void *p) { kfree(p); }"));
  assert!(smart("kmalloc($A, $B)", "void *f(void) { return kmalloc(8, GFP_KERNEL); }"));
}

#[test]
fn smart_pattern_keeps_the_callee_exact() {
  assert!(!smart("schedule_timeout($A)", "void f(void) { schedule_timeout_x(5); }"));
  assert!(!smart("schedule_timeout($A)", "void f(void) { int schedule_timeout; }"));
}

#[test]
fn non_call_sources_keep_their_first_parse() {
  // A declaration pattern must not be retargeted into a call.
  let pattern = Pattern::try_new_smart("int $A;", SupportLang::C).unwrap();
  let plain = Pattern::try_new("int $A;", SupportLang::C).unwrap();
  assert_eq!(pattern.root_kind_id(), plain.root_kind_id());
  assert!(smart("int $A;", "int x;"));
  assert!(c_family_call_fallback(SupportLang::C, "int $A;", None).is_none());
  assert!(c_family_call_fallback(SupportLang::C, "f(a) + g(b)", None).is_none());
  assert!(c_family_call_fallback(SupportLang::C, "(a)(b)", None).is_none());
  assert!(c_family_call_fallback(SupportLang::Rust, "f($A)", None).is_none());
}

#[test]
fn cpp_shares_the_fallback() {
  let cand = SupportLang::Cpp.grep("void f() { ns::g(1); }");
  let pattern = Pattern::try_new_smart("ns::g($A)", SupportLang::Cpp).unwrap();
  assert!(pattern.find_node(cand.root()).is_some());
}
