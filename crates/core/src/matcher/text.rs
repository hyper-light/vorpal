use super::Matcher;
use crate::Doc;
use crate::Node;
use crate::meta_var::MetaVarEnv;

use bit_set::BitSet;
use regex::{Error as RegexError, Regex};
use thiserror::Error;

use std::borrow::Cow;

#[derive(Debug, Error)]
pub enum RegexMatcherError {
  #[error("Parsing text matcher fails.")]
  Regex(#[from] RegexError),
}

#[derive(Clone)]
pub struct RegexMatcher {
  regex: Regex,
  /// Substrings every match must contain, extracted once at compile time — the regex's
  /// contribution to the §12 pre-parse literal prefilter (`[A-Z]+_SUSPEND` requires
  /// `_SUSPEND`). Purely a necessary condition; empty when nothing is provably required.
  required_literals: Vec<String>,
}

impl RegexMatcher {
  pub fn try_new(text: &str) -> Result<Self, RegexMatcherError> {
    Ok(RegexMatcher {
      regex: Regex::new(text)?,
      required_literals: regex_required_literals(text),
    })
  }

  /// Substrings provably contained in every match of this regex (possibly none).
  pub fn required_literals(&self) -> &[String] {
    &self.required_literals
  }
}

/// Conservative required-literal analysis over the parsed regex (`regex-syntax` HIR): a
/// literal is reported only when **every** match must contain it — concatenations require the
/// union of their parts, alternations only what every branch requires, `min ≥ 1` repetitions
/// require their body's literals, and everything uncertain (classes, lookarounds, `min = 0`,
/// non-UTF-8 fragments) requires nothing. Case-insensitive parts compile to classes in the
/// HIR, so they naturally contribute nothing rather than a wrong-case literal.
fn regex_required_literals(pattern: &str) -> Vec<String> {
  use regex_syntax::hir::{Hir, HirKind};
  fn walk(hir: &Hir, out: &mut Vec<String>) {
    match hir.kind() {
      HirKind::Literal(literal) => {
        if let Ok(text) = std::str::from_utf8(&literal.0) {
          if !text.is_empty() {
            out.push(text.to_string());
          }
        }
      }
      HirKind::Concat(parts) => {
        for part in parts {
          walk(part, out);
        }
      }
      HirKind::Capture(capture) => walk(&capture.sub, out),
      HirKind::Repetition(repetition) if repetition.min >= 1 => walk(&repetition.sub, out),
      HirKind::Alternation(branches) => {
        let mut per_branch = branches.iter().map(|branch| {
          let mut literals = Vec::new();
          walk(branch, &mut literals);
          literals
        });
        let Some(first) = per_branch.next() else {
          return;
        };
        let common = per_branch.fold(first, |acc, branch| {
          acc.into_iter().filter(|l| branch.contains(l)).collect()
        });
        out.extend(common);
      }
      // Classes, lookarounds (`^`, `$`, `\b`), empty, and `min = 0` repetitions promise
      // nothing about match content.
      _ => {}
    }
  }
  let Ok(hir) = regex_syntax::Parser::new().parse(pattern) else {
    return Vec::new();
  };
  let mut out = Vec::new();
  walk(&hir, &mut out);
  out.sort_unstable();
  out.dedup();
  out
}

impl Matcher for RegexMatcher {
  fn match_node_with_env<'tree, D: Doc>(
    &self,
    node: Node<'tree, D>,
    _env: &mut Cow<MetaVarEnv<'tree, D>>,
  ) -> Option<Node<'tree, D>> {
    self.regex.is_match(&node.text()).then_some(node)
  }

  fn potential_kinds(&self) -> Option<BitSet> {
    None
  }
}

#[cfg(test)]
mod required_literal_tests {
  use super::*;

  fn literals(pattern: &str) -> Vec<String> {
    regex_required_literals(pattern)
  }

  #[test]
  fn extracts_required_literals_conservatively() {
    // The canonical benchmark shape: the class contributes nothing, the literal is required.
    assert_eq!(literals("[A-Z]+_SUSPEND"), vec!["_SUSPEND"]);
    assert_eq!(literals("^[A-Z]+_SUSPEND$"), vec!["_SUSPEND"]);
    // Plain literals and concatenations.
    assert_eq!(literals("foo_bar"), vec!["foo_bar"]);
    assert_eq!(literals(r"foo\d+bar"), vec!["bar", "foo"]);
    // Alternation requires only what every branch requires.
    assert_eq!(literals("(get|set)_value"), vec!["_value"]);
    assert!(literals("(foo|bar)").is_empty());
    // A `min = 0` repetition promises nothing; `min ≥ 1` requires its body.
    assert!(literals("(foo)*").is_empty());
    assert_eq!(literals("(foo)+"), vec!["foo"]);
    assert!(literals("(foo)?").is_empty());
    // Case-insensitive parts compile to classes: nothing extractable, never a wrong-case
    // literal that would skip files containing the other casings.
    assert!(literals("(?i)suspend").is_empty());
    // Anchors and word boundaries alone require nothing.
    assert!(literals(r"^\w+$").is_empty());
    // Invalid patterns extract nothing (the matcher constructor rejects them separately).
    assert!(literals("(unclosed").is_empty());
  }

  #[test]
  fn matcher_exposes_its_literals() {
    let matcher = RegexMatcher::try_new("[A-Z]+_SUSPEND").expect("valid regex");
    assert_eq!(matcher.required_literals(), ["_SUSPEND".to_string()]);
    let free = RegexMatcher::try_new("[a-z]+").expect("valid regex");
    assert!(free.required_literals().is_empty());
  }
}
