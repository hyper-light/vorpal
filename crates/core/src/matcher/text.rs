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
pub fn regex_required_literals(pattern: &str) -> Vec<String> {
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

/// The literal branches of a regex, as a disjunction of conjunctions: a match satisfies at
/// least one branch, and every literal of that branch occurs inside it. `TODO|FIXME` yields
/// `[[TODO], [FIXME]]`; `foo(bar|baz)` yields `[[foo, bar], [foo, baz]]`; a pattern with no
/// alternation yields one branch equal to [`regex_required_literals`]. Bounded at
/// [`MAX_LITERAL_BRANCHES`] branches — past that the alternation collapses to the literals
/// common to its branches (still sound, less selective). The candidate path unions the
/// per-branch candidate sets, so an alternation prunes instead of scanning everything.
pub fn regex_literal_branches(pattern: &str) -> Vec<Vec<String>> {
  use regex_syntax::hir::{Hir, HirKind};
  fn dnf(hir: &Hir) -> Vec<Vec<String>> {
    match hir.kind() {
      HirKind::Literal(literal) => match std::str::from_utf8(&literal.0) {
        Ok(text) if !text.is_empty() => vec![vec![text.to_string()]],
        _ => vec![Vec::new()],
      },
      HirKind::Concat(parts) => {
        let mut acc: Vec<Vec<String>> = vec![Vec::new()];
        for part in parts {
          let branches = dnf(part);
          if acc.len() * branches.len() > MAX_LITERAL_BRANCHES {
            // Too many combinations: keep what every branch of `part` requires.
            let common = common_literals(&branches);
            for a in acc.iter_mut() {
              a.extend(common.iter().cloned());
            }
            continue;
          }
          let mut next = Vec::with_capacity(acc.len() * branches.len());
          for a in &acc {
            for b in &branches {
              let mut merged = a.clone();
              merged.extend(b.iter().cloned());
              next.push(merged);
            }
          }
          acc = next;
        }
        acc
      }
      HirKind::Capture(capture) => dnf(&capture.sub),
      HirKind::Repetition(repetition) if repetition.min >= 1 => dnf(&repetition.sub),
      HirKind::Alternation(branches) => {
        let mut out: Vec<Vec<String>> = Vec::new();
        for branch in branches {
          out.extend(dnf(branch));
        }
        if out.len() > MAX_LITERAL_BRANCHES {
          return vec![common_literals(&out)];
        }
        out
      }
      _ => vec![Vec::new()],
    }
  }
  fn common_literals(branches: &[Vec<String>]) -> Vec<String> {
    let mut iter = branches.iter();
    let Some(first) = iter.next() else {
      return Vec::new();
    };
    let mut common: Vec<String> = first.clone();
    for branch in iter {
      common.retain(|l| branch.contains(l));
    }
    common
  }
  let Ok(hir) = regex_syntax::Parser::new().parse(pattern) else {
    return vec![Vec::new()];
  };
  let mut branches = dnf(&hir);
  for branch in branches.iter_mut() {
    branch.sort_unstable();
    branch.dedup();
  }
  branches.sort();
  branches.dedup();
  branches
}

/// Cap on literal branches a regex plan expands to (each branch costs one candidate walk).
pub const MAX_LITERAL_BRANCHES: usize = 16;

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

  #[test]
  fn literal_branches_distribute_alternations() {
    assert_eq!(regex_literal_branches("TODO|FIXME"), vec![vec!["FIXME".to_string()], vec!["TODO".to_string()]]);
    assert_eq!(
      regex_literal_branches("foo(bar|baz)"),
      vec![vec!["bar".to_string(), "foo".to_string()], vec!["baz".to_string(), "foo".to_string()]]
    );
    assert_eq!(regex_literal_branches("hello"), vec![vec!["hello".to_string()]]);
    assert_eq!(regex_literal_branches("[a-z]+"), vec![Vec::<String>::new()]);
    // a branch with no literal keeps the union honest: it requires nothing
    assert_eq!(regex_literal_branches("abc|[0-9]+"), vec![Vec::<String>::new(), vec!["abc".to_string()]]);
    // past the cap the alternation collapses to its common literals (here: none)
    let wide = (0..20).map(|i| format!("lit{i}")).collect::<Vec<_>>().join("|");
    assert_eq!(regex_literal_branches(&wide), vec![Vec::<String>::new()]);
  }
}
