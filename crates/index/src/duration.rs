//! Human durations for the CLI/root-file override surfaces (`--dense-budget-timeout`,
//! `<root>/dense.budget`): `1h`, `5m30s`, `90s`, `2h15m`, or plain seconds
//! (`300`, `2.5`). Owned parser — the workspace carries no duration crate and the
//! grammar is three units — with typed refusals: malformed or ambiguous input
//! names the accepted forms instead of defaulting silently.

use std::fmt;

/// Why a duration did not parse — the message names the accepted forms.
#[derive(Debug, Clone, PartialEq)]
pub struct DurationError(pub String);

impl fmt::Display for DurationError {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    write!(
      f,
      "{}: accepted forms are `<h>h[<m>m][<s>s]`, `<m>m[<s>s]`, `<s>s`, or plain seconds (e.g. 1h, 5m30s, 90s, 2h15m, 300)",
      self.0
    )
  }
}

impl std::error::Error for DurationError {}

/// Parse a human duration into seconds. Units must appear in descending order
/// (h, m, s), each at most once; a bare number is seconds. Zero, negative,
/// non-finite, and empty inputs are refused (a budget must be positive).
pub fn parse_duration(text: &str) -> Result<f64, DurationError> {
  let text = text.trim();
  if text.is_empty() {
    return Err(DurationError("empty duration".to_string()));
  }
  // Plain seconds (compatibility form).
  if let Ok(seconds) = text.parse::<f64>() {
    return positive(seconds, text);
  }
  let mut total = 0.0f64;
  let mut rest = text;
  // Unit rank so `5m1h` and `1h1h` are refused as ambiguous/duplicate.
  let mut last_rank = 0u8;
  while !rest.is_empty() {
    let digits = rest
      .find(|c: char| !(c.is_ascii_digit() || c == '.'))
      .ok_or_else(|| DurationError(format!("`{text}`: missing unit after `{rest}`")))?;
    if digits == 0 {
      return Err(DurationError(format!("`{text}`: expected a number before `{rest}`")));
    }
    let value: f64 = rest[..digits]
      .parse()
      .map_err(|_| DurationError(format!("`{text}`: `{}` is not a number", &rest[..digits])))?;
    let unit = &rest[digits..digits + 1];
    let (rank, seconds) = match unit {
      "h" => (1u8, 3600.0),
      "m" => (2u8, 60.0),
      "s" => (3u8, 1.0),
      other => {
        return Err(DurationError(format!("`{text}`: unknown unit `{other}`")));
      }
    };
    if rank <= last_rank {
      return Err(DurationError(format!(
        "`{text}`: units must be h, m, s in that order, each at most once"
      )));
    }
    last_rank = rank;
    total += value * seconds;
    rest = &rest[digits + 1..];
  }
  positive(total, text)
}

fn positive(seconds: f64, text: &str) -> Result<f64, DurationError> {
  if seconds.partial_cmp(&0.0) == Some(std::cmp::Ordering::Greater) && seconds.is_finite() {
    Ok(seconds)
  } else {
    Err(DurationError(format!("`{text}`: a budget must be a positive, finite duration")))
  }
}

/// Render seconds back as the canonical `<h>h<m>m<s>s` form (fractional seconds
/// kept when present): 90 → `1m30s`, 3600 → `1h`, 2.5 → `2.5s`.
pub fn render_duration(seconds: f64) -> String {
  let whole = seconds.floor();
  let fraction = seconds - whole;
  let whole = whole as u64;
  let (h, m, s) = (whole / 3600, (whole % 3600) / 60, whole % 60);
  let mut out = String::new();
  if h > 0 {
    out.push_str(&format!("{h}h"));
  }
  if m > 0 {
    out.push_str(&format!("{m}m"));
  }
  if fraction > 0.0 {
    out.push_str(&format!("{}s", s as f64 + fraction));
  } else if s > 0 || out.is_empty() {
    out.push_str(&format!("{s}s"));
  }
  out
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn parses_the_documented_forms() {
    assert_eq!(parse_duration("1h").unwrap(), 3600.0);
    assert_eq!(parse_duration("5m30s").unwrap(), 330.0);
    assert_eq!(parse_duration("90s").unwrap(), 90.0);
    assert_eq!(parse_duration("2h15m").unwrap(), 8100.0);
    assert_eq!(parse_duration("300").unwrap(), 300.0);
    assert_eq!(parse_duration(" 2.5 ").unwrap(), 2.5);
  }

  #[test]
  fn refuses_malformed_ambiguous_and_non_positive() {
    for bad in ["", "abc", "5m1h", "1h1h", "10x", "m5", "0", "-3", "0s", "1.5.2s"] {
      let err = parse_duration(bad).unwrap_err().to_string();
      assert!(err.contains("accepted forms"), "{bad:?} → {err}");
    }
  }

  #[test]
  fn renders_canonically_and_round_trips() {
    assert_eq!(render_duration(90.0), "1m30s");
    assert_eq!(render_duration(3600.0), "1h");
    assert_eq!(render_duration(8100.0), "2h15m");
    assert_eq!(render_duration(2.5), "2.5s");
    assert_eq!(render_duration(0.0), "0s");
    for secs in [1.0, 59.0, 61.0, 3661.0, 86400.0, 0.25] {
      assert_eq!(parse_duration(&render_duration(secs)).unwrap(), secs);
    }
  }
}
