//! Pre-parse literal filter: SIMD substring finders (`memchr::memmem`) over a matcher's
//! required literals, checked against raw bytes with per-token AND semantics. A file missing
//! any required literal cannot match, so it is skipped before tree-sitter or a regex ever runs.
//! Purely a necessary condition — matching semantics are unchanged. Lives in `vorpal-core` so
//! every consumer (the CLI scan path, the index's structural search, the MCP tools) shares one
//! definition; the CLI re-exports it.

use crate::matcher::Pattern;

/// Cap on prebuilt finders per matcher: the longest (most selective) literals win the slots.
pub const MAX_PREFILTER_LITERALS: usize = 8;

/// Per-token AND of substring finders over a matcher's required literals.
pub struct Prefilter {
  finders: Vec<memchr::memmem::Finder<'static>>,
}

impl Prefilter {
  /// Finders for `literals`, deduplicated and kept longest-first up to
  /// [`MAX_PREFILTER_LITERALS`]. An empty list yields a vacuous filter that admits everything.
  pub fn from_literals(mut literals: Vec<&str>) -> Self {
    literals.sort_unstable();
    literals.dedup();
    literals.sort_by_key(|l| std::cmp::Reverse(l.len()));
    let finders = literals
      .into_iter()
      .filter(|l| !l.is_empty())
      .take(MAX_PREFILTER_LITERALS)
      .map(|l| memchr::memmem::Finder::new(l.as_bytes()).into_owned())
      .collect();
    Self { finders }
  }

  /// The prefilter of a compiled pattern's required literals.
  pub fn for_pattern(pattern: &Pattern) -> Self {
    Self::from_literals(pattern.required_literals())
  }

  /// True when the filter requires nothing (every file must be scanned).
  pub fn is_vacuous(&self) -> bool {
    self.finders.is_empty()
  }

  /// The literals this filter requires, longest first (for planning a coarser candidate set).
  pub fn literals(&self) -> impl Iterator<Item = &[u8]> {
    self.finders.iter().map(|f| f.needle())
  }

  /// Every occurrence of the anchor literal (the longest one) in `content`, appended to
  /// `out` ascending. A match must contain every required literal, so every match contains
  /// an anchor occurrence: the chunks holding these offsets are the only chunks that can
  /// hold a match. `None` when the filter is vacuous.
  pub fn anchor_positions(&self, content: &[u8], out: &mut Vec<usize>) -> Option<()> {
    let finder = self.finders.first()?;
    out.extend(finder.find_iter(content));
    Some(())
  }

  /// True when every required literal occurs in `content` (vacuously true with no literals).
  pub fn may_match_bytes(&self, content: &[u8]) -> bool {
    self
      .finders
      .iter()
      .all(|finder| finder.find(content).is_some())
  }

  /// [`Self::may_match_bytes`] over a string.
  pub fn may_match(&self, content: &str) -> bool {
    self.may_match_bytes(content.as_bytes())
  }
}

#[cfg(test)]
mod test {
  use super::*;

  #[test]
  fn dedups_and_keeps_longest_first_up_to_the_cap() {
    let literals: Vec<&str> = vec!["ab", "abcdef", "ab", "abc", "x", "yy", "zzz", "q", "rr", "sss", "tttt"];
    let filter = Prefilter::from_literals(literals);
    let kept: Vec<&[u8]> = filter.literals().collect();
    assert_eq!(kept.len(), MAX_PREFILTER_LITERALS);
    assert_eq!(kept[0], b"abcdef");
    assert!(kept.windows(2).all(|w| w[0].len() >= w[1].len()));
    assert!(!filter.is_vacuous());
  }

  #[test]
  fn vacuous_filter_admits_everything_and_and_semantics_hold() {
    assert!(Prefilter::from_literals(Vec::new()).is_vacuous());
    assert!(Prefilter::from_literals(Vec::new()).may_match_bytes(b""));
    let filter = Prefilter::from_literals(vec!["foo", "bar"]);
    assert!(filter.may_match("bar ... foo"));
    assert!(!filter.may_match("only foo here"));
    assert!(!filter.may_match_bytes(b"bar only"));
  }

  #[test]
  fn works_on_bytes_that_are_not_utf8() {
    let filter = Prefilter::from_literals(vec!["needle"]);
    let mut bytes = vec![0xff, 0xfe, 0x00];
    bytes.extend_from_slice(b"needle");
    bytes.push(0x80);
    assert!(filter.may_match_bytes(&bytes));
    assert!(!filter.may_match_bytes(&[0xff, 0xfe, 0x00, 0x80]));
  }
}
