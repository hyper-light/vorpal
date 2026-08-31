//! Tokenizer for the query text. Total: every byte sequence lexes to tokens or a typed
//! error with an offset — no panics, no silent skips (the fuzz test pins this).

use crate::QueryError;

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Tok {
  /// Bare identifier or keyword (keywords are matched case-insensitively by the parser).
  Ident(String),
  /// `'…'` or `"…"` with `\\`, `\'`, `\"` escapes.
  Str(String),
  Int(u64),
  Float(f64),
  LParen,
  RParen,
  LBracket,
  RBracket,
  LBrace,
  RBrace,
  Colon,
  Comma,
  Dot,
  DotDot,
  Pipe,
  Star,
  Lt,
  Gt,
  Dash,
  Eq,
  /// `<>` or `!=`.
  Ne,
  /// `=~` (regex match).
  Match,
  Plus,
  Slash,
  Percent,
}

pub(crate) struct Lexed {
  pub(crate) tokens: Vec<(Tok, usize)>,
}

pub(crate) fn lex(text: &str) -> Result<Lexed, QueryError> {
  let bytes = text.as_bytes();
  let mut tokens = Vec::new();
  let mut i = 0usize;
  while i < bytes.len() {
    let b = bytes[i];
    match b {
      b' ' | b'\t' | b'\r' | b'\n' => {
        i += 1;
      }
      b'(' => {
        tokens.push((Tok::LParen, i));
        i += 1;
      }
      b')' => {
        tokens.push((Tok::RParen, i));
        i += 1;
      }
      b'[' => {
        tokens.push((Tok::LBracket, i));
        i += 1;
      }
      b']' => {
        tokens.push((Tok::RBracket, i));
        i += 1;
      }
      b'{' => {
        tokens.push((Tok::LBrace, i));
        i += 1;
      }
      b'}' => {
        tokens.push((Tok::RBrace, i));
        i += 1;
      }
      b':' => {
        tokens.push((Tok::Colon, i));
        i += 1;
      }
      b',' => {
        tokens.push((Tok::Comma, i));
        i += 1;
      }
      b'|' => {
        tokens.push((Tok::Pipe, i));
        i += 1;
      }
      b'*' => {
        tokens.push((Tok::Star, i));
        i += 1;
      }
      b'+' => {
        tokens.push((Tok::Plus, i));
        i += 1;
      }
      b'/' => {
        tokens.push((Tok::Slash, i));
        i += 1;
      }
      b'%' => {
        tokens.push((Tok::Percent, i));
        i += 1;
      }
      b'>' => {
        tokens.push((Tok::Gt, i));
        i += 1;
      }
      b'-' => {
        tokens.push((Tok::Dash, i));
        i += 1;
      }
      b'=' => {
        if bytes.get(i + 1) == Some(&b'~') {
          tokens.push((Tok::Match, i));
          i += 2;
        } else {
          tokens.push((Tok::Eq, i));
          i += 1;
        }
      }
      b'<' => {
        if bytes.get(i + 1) == Some(&b'>') {
          tokens.push((Tok::Ne, i));
          i += 2;
        } else {
          tokens.push((Tok::Lt, i));
          i += 1;
        }
      }
      b'!' => {
        if bytes.get(i + 1) == Some(&b'=') {
          tokens.push((Tok::Ne, i));
          i += 2;
        } else {
          return Err(QueryError::parse(i, "unexpected '!' (did you mean '!=' ?)"));
        }
      }
      b'.' => {
        if bytes.get(i + 1) == Some(&b'.') {
          tokens.push((Tok::DotDot, i));
          i += 2;
        } else {
          tokens.push((Tok::Dot, i));
          i += 1;
        }
      }
      b'\'' | b'"' => {
        let quote = b;
        let start = i;
        i += 1;
        let mut out = String::new();
        loop {
          match bytes.get(i) {
            None => return Err(QueryError::parse(start, "unterminated string")),
            Some(&c) if c == quote => {
              i += 1;
              break;
            }
            Some(b'\\') => {
              match bytes.get(i + 1) {
                Some(&e) if e == quote || e == b'\\' => {
                  out.push(e as char);
                  i += 2;
                }
                _ => return Err(QueryError::parse(i, "unknown escape (only \\\\ and the quote)")),
              }
            }
            Some(_) => {
              // Consume one full UTF-8 scalar (the input is a &str, so boundaries exist).
              let rest = &text[i..];
              let ch = match rest.chars().next() {
                Some(ch) => ch,
                None => return Err(QueryError::parse(i, "unterminated string")),
              };
              out.push(ch);
              i += ch.len_utf8();
            }
          }
        }
        tokens.push((Tok::Str(out), start));
      }
      b'0'..=b'9' => {
        let start = i;
        while i < bytes.len() && bytes[i].is_ascii_digit() {
          i += 1;
        }
        // A fraction needs a digit right after the point, so `1..3` stays Int DotDot Int.
        if bytes.get(i) == Some(&b'.') && bytes.get(i + 1).is_some_and(|b| b.is_ascii_digit()) {
          i += 1;
          while i < bytes.len() && bytes[i].is_ascii_digit() {
            i += 1;
          }
          let value: f64 = text[start..i]
            .parse()
            .map_err(|_| QueryError::parse(start, "malformed number"))?;
          tokens.push((Tok::Float(value), start));
        } else {
          let value: u64 = text[start..i]
            .parse()
            .map_err(|_| QueryError::parse(start, "integer out of range (u64)"))?;
          tokens.push((Tok::Int(value), start));
        }
      }
      b'A'..=b'Z' | b'a'..=b'z' | b'_' => {
        let start = i;
        while i < bytes.len() && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_') {
          i += 1;
        }
        tokens.push((Tok::Ident(text[start..i].to_string()), start));
      }
      _ => {
        let ch = text[i..].chars().next().map(|c| c.to_string()).unwrap_or_default();
        return Err(QueryError::parse(i, format!("unexpected character '{ch}'")));
      }
    }
  }
  Ok(Lexed { tokens })
}
