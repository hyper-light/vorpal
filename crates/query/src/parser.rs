//! Recursive-descent parser: token stream → [`crate::ir::Query`]. Grammar (v1):
//!
//! ```text
//! query   := MATCH pattern (WHERE pred (AND pred)*)? RETURN returns
//!            (ORDER BY ord (, ord)*)? (SKIP int)? (LIMIT int)?
//! pattern := node (rel node)?
//! node    := '(' var? (':' Kind)? props? ')'
//! rel     := '<-' relbody? '-'  |  '-' relbody? '->'  |  '-' relbody? '-'
//! relbody := '[' (':' name ('|' name)*)? range? props? ']'
//! range   := '*' int? ('..' int?)?
//! ```
//!
//! Unsupported Cypher (OR, NOT, WITH, OPTIONAL MATCH, multi-segment patterns, regex
//! operators) fails with an error that names the v1 boundary, never a generic syntax error.

use crate::QueryError;
use crate::ir::*;
use crate::lexer::{Tok, lex};

pub(crate) const DEFAULT_VAR_DEPTH: u32 = 10;

pub(crate) fn parse_text(text: &str) -> Result<Query, QueryError> {
  let lexed = lex(text)?;
  let mut p = Parser {
    tokens: lexed.tokens,
    at: 0,
    end: text.len(),
  };
  let query = p.query()?;
  if let Some((_, offset)) = p.peek_at() {
    return Err(QueryError::parse(offset, "trailing input after the query"));
  }
  Ok(query)
}

struct Parser {
  tokens: Vec<(Tok, usize)>,
  at: usize,
  end: usize,
}

impl Parser {
  fn peek(&self) -> Option<&Tok> {
    self.tokens.get(self.at).map(|(t, _)| t)
  }

  fn peek_at(&self) -> Option<(&Tok, usize)> {
    self.tokens.get(self.at).map(|(t, o)| (t, *o))
  }

  fn offset(&self) -> usize {
    self.tokens.get(self.at).map(|(_, o)| *o).unwrap_or(self.end)
  }

  fn bump(&mut self) -> Option<Tok> {
    let tok = self.tokens.get(self.at).map(|(t, _)| t.clone());
    if tok.is_some() {
      self.at += 1;
    }
    tok
  }

  fn expect(&mut self, want: &Tok, what: &str) -> Result<(), QueryError> {
    if self.peek() == Some(want) {
      self.at += 1;
      Ok(())
    } else {
      Err(QueryError::parse(self.offset(), format!("expected {what}")))
    }
  }

  /// Case-insensitive keyword check without consuming.
  fn at_kw(&self, kw: &str) -> bool {
    matches!(self.peek(), Some(Tok::Ident(w)) if w.eq_ignore_ascii_case(kw))
  }

  fn eat_kw(&mut self, kw: &str) -> bool {
    if self.at_kw(kw) {
      self.at += 1;
      true
    } else {
      false
    }
  }

  fn expect_kw(&mut self, kw: &str) -> Result<(), QueryError> {
    if self.eat_kw(kw) {
      Ok(())
    } else {
      Err(QueryError::parse(self.offset(), format!("expected {kw}")))
    }
  }

  fn ident(&mut self, what: &str) -> Result<String, QueryError> {
    match self.bump() {
      Some(Tok::Ident(w)) => Ok(w),
      _ => {
        self.at = self.at.saturating_sub(1);
        Err(QueryError::parse(self.offset(), format!("expected {what}")))
      }
    }
  }

  fn int(&mut self, what: &str) -> Result<u64, QueryError> {
    match self.bump() {
      Some(Tok::Int(n)) => Ok(n),
      _ => {
        self.at = self.at.saturating_sub(1);
        Err(QueryError::parse(self.offset(), format!("expected {what}")))
      }
    }
  }

  fn query(&mut self) -> Result<Query, QueryError> {
    self.expect_kw("MATCH")?;
    let pattern = self.pattern()?;
    let mut predicates = Vec::new();
    if self.eat_kw("WHERE") {
      loop {
        predicates.push(self.predicate()?);
        if self.eat_kw("AND") {
          continue;
        }
        if self.at_kw("OR") || self.at_kw("NOT") || self.at_kw("XOR") {
          return Err(QueryError::parse(
            self.offset(),
            "only AND-combined predicates are supported in v1 (no OR/NOT)",
          ));
        }
        break;
      }
    }
    if self.at_kw("WITH") || self.at_kw("OPTIONAL") {
      return Err(QueryError::parse(
        self.offset(),
        "WITH / OPTIONAL MATCH are not supported in v1 (single MATCH + WHERE + RETURN)",
      ));
    }
    self.expect_kw("RETURN")?;
    let returns = self.returns()?;
    let mut order_by = Vec::new();
    if self.eat_kw("ORDER") {
      self.expect_kw("BY")?;
      loop {
        order_by.push(self.ordering()?);
        if self.peek() == Some(&Tok::Comma) {
          self.at += 1;
          continue;
        }
        break;
      }
    }
    let skip = if self.eat_kw("SKIP") {
      Some(self.int("row count after SKIP")?)
    } else {
      None
    };
    let limit = if self.eat_kw("LIMIT") {
      Some(self.int("row count after LIMIT")?)
    } else {
      None
    };
    Ok(Query {
      pattern,
      predicates,
      returns,
      order_by,
      skip,
      limit,
    })
  }

  fn pattern(&mut self) -> Result<Pattern, QueryError> {
    let left = self.node_pattern()?;
    let (rel, right) = match self.peek() {
      Some(Tok::Dash) | Some(Tok::Lt) => {
        let rel = self.rel_pattern()?;
        let right = self.node_pattern()?;
        if matches!(self.peek(), Some(Tok::Dash) | Some(Tok::Lt)) {
          return Err(QueryError::parse(
            self.offset(),
            "multi-segment patterns are not supported in v1 (one relationship per MATCH)",
          ));
        }
        (Some(rel), Some(right))
      }
      _ => (None, None),
    };
    Ok(Pattern { left, rel, right })
  }

  fn node_pattern(&mut self) -> Result<NodePattern, QueryError> {
    self.expect(&Tok::LParen, "'(' opening a node pattern")?;
    let mut node = NodePattern::default();
    if let Some(Tok::Ident(_)) = self.peek() {
      node.var = Some(self.ident("variable")?);
    }
    if self.peek() == Some(&Tok::Colon) {
      self.at += 1;
      node.kind = Some(self.ident("kind label after ':'")?);
    }
    if self.peek() == Some(&Tok::LBrace) {
      node.props = self.props()?;
    }
    self.expect(&Tok::RParen, "')' closing the node pattern")?;
    Ok(node)
  }

  fn props(&mut self) -> Result<Vec<(String, PropValue)>, QueryError> {
    self.expect(&Tok::LBrace, "'{'")?;
    let mut props = Vec::new();
    if self.peek() != Some(&Tok::RBrace) {
      loop {
        let key = self.ident("property name")?;
        self.expect(&Tok::Colon, "':' after the property name")?;
        props.push((key, self.value()?));
        if self.peek() == Some(&Tok::Comma) {
          self.at += 1;
          continue;
        }
        break;
      }
    }
    self.expect(&Tok::RBrace, "'}' closing the property map")?;
    Ok(props)
  }

  fn value(&mut self) -> Result<PropValue, QueryError> {
    match self.bump() {
      Some(Tok::Str(s)) => Ok(PropValue::Text(s)),
      Some(Tok::Int(n)) => Ok(PropValue::Int(n)),
      Some(Tok::Ident(w)) if w.eq_ignore_ascii_case("true") => Ok(PropValue::Bool(true)),
      Some(Tok::Ident(w)) if w.eq_ignore_ascii_case("false") => Ok(PropValue::Bool(false)),
      // Bare identifiers are accepted as text values ({grade: constrained} reads naturally).
      Some(Tok::Ident(w)) => Ok(PropValue::Text(w)),
      _ => {
        self.at = self.at.saturating_sub(1);
        Err(QueryError::parse(
          self.offset(),
          "expected a string, integer, true/false, or bare word value",
        ))
      }
    }
  }

  fn rel_pattern(&mut self) -> Result<RelPattern, QueryError> {
    // '<-' body? '-'  |  '-' body? '->'  |  '-' body? '-'
    let inbound_start = if self.peek() == Some(&Tok::Lt) {
      self.at += 1;
      self.expect(&Tok::Dash, "'-' after '<'")?;
      true
    } else {
      self.expect(&Tok::Dash, "'-' starting a relationship")?;
      false
    };
    let (types, range, grade) = if self.peek() == Some(&Tok::LBracket) {
      self.rel_body()?
    } else {
      (Vec::new(), None, None)
    };
    self.expect(&Tok::Dash, "'-' closing the relationship")?;
    let direction = if inbound_start {
      if self.peek() == Some(&Tok::Gt) {
        return Err(QueryError::parse(
          self.offset(),
          "a relationship cannot point both ways (<-…->)",
        ));
      }
      RelDirection::In
    } else if self.peek() == Some(&Tok::Gt) {
      self.at += 1;
      RelDirection::Out
    } else {
      RelDirection::Both
    };
    Ok(RelPattern {
      types,
      direction,
      range,
      grade,
    })
  }

  #[allow(clippy::type_complexity)]
  fn rel_body(
    &mut self,
  ) -> Result<(Vec<String>, Option<(u32, u32)>, Option<String>), QueryError> {
    self.expect(&Tok::LBracket, "'['")?;
    // An optional relationship variable is accepted and discarded (v1 binds nodes only).
    if let Some(Tok::Ident(_)) = self.peek() {
      self.at += 1;
    }
    let mut types = Vec::new();
    if self.peek() == Some(&Tok::Colon) {
      self.at += 1;
      types.push(self.ident("relation name after ':'")?);
      while self.peek() == Some(&Tok::Pipe) {
        self.at += 1;
        // Tolerate the `|:name` spelling Cypher allows.
        if self.peek() == Some(&Tok::Colon) {
          self.at += 1;
        }
        types.push(self.ident("relation name after '|'")?);
      }
    }
    let range = if self.peek() == Some(&Tok::Star) {
      self.at += 1;
      let min = if let Some(Tok::Int(_)) = self.peek() {
        Some(self.int("range minimum")? as u32)
      } else {
        None
      };
      if self.peek() == Some(&Tok::DotDot) {
        self.at += 1;
        let max = if let Some(Tok::Int(_)) = self.peek() {
          Some(self.int("range maximum")? as u32)
        } else {
          None
        };
        Some((min.unwrap_or(1), max.unwrap_or(DEFAULT_VAR_DEPTH)))
      } else {
        match min {
          // `*3` = exactly three hops.
          Some(n) => Some((n, n)),
          // Bare `*` = the documented 1..=10 default.
          None => Some((1, DEFAULT_VAR_DEPTH)),
        }
      }
    } else {
      None
    };
    let mut grade = None;
    if self.peek() == Some(&Tok::LBrace) {
      for (key, value) in self.props()? {
        if key.eq_ignore_ascii_case("grade") {
          match value {
            PropValue::Text(text) => grade = Some(text),
            _ => {
              return Err(QueryError::parse(
                self.offset(),
                "grade takes a name: exact | constrained | heuristic",
              ));
            }
          }
        } else {
          return Err(QueryError::parse(
            self.offset(),
            format!("unknown relationship property '{key}' (v1 supports 'grade')"),
          ));
        }
      }
    }
    self.expect(&Tok::RBracket, "']' closing the relationship")?;
    Ok((types, range, grade))
  }

  fn predicate(&mut self) -> Result<Predicate, QueryError> {
    let var = self.ident("variable in a WHERE predicate")?;
    self.expect(&Tok::Dot, "'.' after the variable")?;
    let prop = self.ident("property name")?;
    let target = PropRef { var, prop };
    let (op, value) = match self.peek() {
      Some(Tok::Eq) => {
        self.at += 1;
        (CmpOp::Eq, self.value()?)
      }
      Some(Tok::Ne) => {
        self.at += 1;
        (CmpOp::Ne, self.value()?)
      }
      Some(Tok::Ident(w)) if w.eq_ignore_ascii_case("STARTS") => {
        self.at += 1;
        self.expect_kw("WITH")?;
        (CmpOp::StartsWith, self.value()?)
      }
      Some(Tok::Ident(w)) if w.eq_ignore_ascii_case("ENDS") => {
        self.at += 1;
        self.expect_kw("WITH")?;
        (CmpOp::EndsWith, self.value()?)
      }
      Some(Tok::Ident(w)) if w.eq_ignore_ascii_case("CONTAINS") => {
        self.at += 1;
        (CmpOp::Contains, self.value()?)
      }
      Some(Tok::Ident(w)) if w.eq_ignore_ascii_case("IN") => {
        return Err(QueryError::parse(
          self.offset(),
          "IN lists are not supported in v1 (use = or STARTS/ENDS WITH/CONTAINS)",
        ));
      }
      _ => {
        return Err(QueryError::parse(
          self.offset(),
          "expected =, <>, !=, STARTS WITH, ENDS WITH, or CONTAINS",
        ));
      }
    };
    Ok(Predicate { target, op, value })
  }

  fn returns(&mut self) -> Result<Returns, QueryError> {
    let mut projections: Vec<Projection> = Vec::new();
    let mut count: Option<Option<PropRef>> = None;
    loop {
      if self.at_kw("COUNT") {
        let offset = self.offset();
        self.at += 1;
        self.expect(&Tok::LParen, "'(' after COUNT")?;
        let inner = if self.peek() == Some(&Tok::Star) {
          self.at += 1;
          None
        } else {
          self.expect_kw("DISTINCT")?;
          let var = self.ident("variable inside COUNT(DISTINCT …)")?;
          self.expect(&Tok::Dot, "'.' (COUNT(DISTINCT var.prop))")?;
          let prop = self.ident("property inside COUNT(DISTINCT …)")?;
          Some(PropRef { var, prop })
        };
        self.expect(&Tok::RParen, "')' closing COUNT")?;
        if count.replace(inner).is_some() {
          return Err(QueryError::parse(offset, "at most one COUNT per RETURN"));
        }
      } else {
        let var = self.ident("projection (var or var.prop)")?;
        let expr = if self.peek() == Some(&Tok::Dot) {
          self.at += 1;
          let prop = self.ident("property name")?;
          ProjExpr::Prop { var, prop }
        } else {
          ProjExpr::Var { var }
        };
        let alias = if self.eat_kw("AS") {
          Some(self.ident("alias after AS")?)
        } else {
          None
        };
        projections.push(Projection { expr, alias });
      }
      if self.peek() == Some(&Tok::Comma) {
        self.at += 1;
        continue;
      }
      break;
    }
    match count {
      None => Ok(Returns::Rows(projections)),
      Some(distinct) => {
        if projections.len() > 1 {
          return Err(QueryError::parse(
            self.offset(),
            "a counted RETURN groups by at most one key (RETURN x.prop, COUNT(*))",
          ));
        }
        Ok(Returns::Count {
          distinct,
          group: projections.into_iter().next(),
        })
      }
    }
  }

  fn ordering(&mut self) -> Result<Ordering, QueryError> {
    let head = self.ident("ORDER BY key")?;
    let key = if self.peek() == Some(&Tok::Dot) {
      self.at += 1;
      let prop = self.ident("property name")?;
      format!("{head}.{prop}")
    } else {
      head
    };
    let descending = if self.eat_kw("DESC") {
      true
    } else {
      self.eat_kw("ASC");
      false
    };
    Ok(Ordering { key, descending })
  }
}
