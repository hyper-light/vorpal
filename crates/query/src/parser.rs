//! Recursive-descent parser: token stream → [`crate::ir::Query`]. Grammar (v2):
//!
//! ```text
//! query   := MATCH pattern (WHERE pred)? stage* RETURN DISTINCT? items
//!            (ORDER BY ord (, ord)*)? (SKIP int)? (LIMIT int)? (UNION ALL? query)?
//! stage   := WITH DISTINCT? items (WHERE pred)? (ORDER BY …)? (SKIP int)? (LIMIT int)?
//!          | UNWIND expr AS ident
//! items   := expr (AS ident)? (, expr (AS ident)?)*
//! pattern := node (rel node)*
//! node    := '(' ident? (':' Kind ('|' Kind)*)? props? ')'
//! rel     := '<-' relbody? '-'  |  '-' relbody? '->'  |  '-' relbody? '-'
//! relbody := '[' ident? (':' name ('|' name)*)? range? props? ']'
//! range   := '*' int? ('..' int?)?
//! pred    := or ; or := and (OR and)* ; and := unary (AND unary)*
//! unary   := NOT unary | EXISTS '{' pattern '}' | '(' pred ')' | comparison
//! comparison := expr (cmpop expr | IN expr | IS NOT? NULL | ':' Kind ('|' Kind)*)?
//! expr    := term (('+'|'-') term)* ; term := factor (('*'|'/'|'%') factor)*
//! factor  := '-' factor | atom
//! atom    := literal | '[' exprs ']' | '(' expr ')' | CASE … END
//!          | ident '(' (DISTINCT? expr | '*')? ')' | ident ('.' ident)?
//! ```
//!
//! Unsupported Cypher (OPTIONAL MATCH, a second MATCH, XOR, map literals, path functions,
//! parameters) fails with an error that names the boundary, never a generic syntax error.

use crate::QueryError;
use crate::ir::*;
use crate::lexer::{Tok, lex};

pub(crate) const DEFAULT_VAR_DEPTH: u32 = 10;

const KEYWORDS: &[&str] = &[
  "MATCH", "WHERE", "RETURN", "WITH", "UNWIND", "UNION", "ALL", "AS", "ORDER", "BY", "SKIP",
  "LIMIT", "DISTINCT", "AND", "OR", "NOT", "XOR", "IN", "IS", "NULL", "TRUE", "FALSE",
  "CASE", "WHEN", "THEN", "ELSE", "END", "EXISTS", "STARTS", "ENDS", "CONTAINS", "ASC",
  "DESC", "OPTIONAL",
];

fn is_keyword(word: &str) -> bool {
  KEYWORDS.iter().any(|k| k.eq_ignore_ascii_case(word))
}

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

  fn peek2(&self) -> Option<&Tok> {
    self.tokens.get(self.at + 1).map(|(t, _)| t)
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
    match self.peek() {
      Some(Tok::Ident(w)) if !is_keyword(w) => {
        let w = w.clone();
        self.at += 1;
        Ok(w)
      }
      _ => Err(QueryError::parse(self.offset(), format!("expected {what}"))),
    }
  }

  /// A label identifier: label position (after `:` or `|`) is unambiguous, so
  /// keyword-spelled kind names are accepted here — `Union` is both a Cypher
  /// clause keyword and a symbol kind (extraction-coverage campaign), and
  /// `MATCH (n:Union)` must parse.
  fn label_ident(&mut self, what: &str) -> Result<String, QueryError> {
    match self.peek() {
      Some(Tok::Ident(w)) => {
        let w = w.clone();
        self.at += 1;
        Ok(w)
      }
      _ => Err(QueryError::parse(self.offset(), format!("expected {what}"))),
    }
  }

  fn int(&mut self, what: &str) -> Result<u64, QueryError> {
    match self.peek() {
      Some(Tok::Int(n)) => {
        let n = *n;
        self.at += 1;
        Ok(n)
      }
      _ => Err(QueryError::parse(self.offset(), format!("expected {what}"))),
    }
  }

  // ---------------------------------------------------------------- clauses

  fn query(&mut self) -> Result<Query, QueryError> {
    if self.at_kw("OPTIONAL") {
      return Err(QueryError::parse(
        self.offset(),
        "OPTIONAL MATCH is not supported (use EXISTS { … } in WHERE, or WITH)",
      ));
    }
    self.expect_kw("MATCH")?;
    let pattern = self.pattern()?;
    let predicate = if self.eat_kw("WHERE") {
      Some(self.pred_or()?)
    } else {
      None
    };
    let mut stages = Vec::new();
    loop {
      if self.at_kw("MATCH") {
        return Err(QueryError::parse(
          self.offset(),
          "a second MATCH is not supported — chain the pattern, or project through WITH",
        ));
      }
      if self.eat_kw("WITH") {
        stages.push(self.with_stage()?);
      } else if self.eat_kw("UNWIND") {
        let expr = self.expr()?;
        self.expect_kw("AS")?;
        let alias = self.ident("variable after AS")?;
        stages.push(Stage::Unwind { expr, alias });
      } else {
        break;
      }
    }
    self.expect_kw("RETURN")?;
    let distinct = self.eat_kw("DISTINCT");
    let items = self.items()?;
    let (order_by, skip, limit) = self.tail()?;
    let union = if self.eat_kw("UNION") {
      let all = self.eat_kw("ALL");
      Some(Box::new(UnionTail {
        all,
        query: self.query()?,
      }))
    } else {
      None
    };
    Ok(Query {
      pattern,
      predicate,
      stages,
      returns: ReturnClause { distinct, items },
      order_by,
      skip,
      limit,
      union,
    })
  }

  fn with_stage(&mut self) -> Result<Stage, QueryError> {
    let distinct = self.eat_kw("DISTINCT");
    let items = self.items()?;
    let predicate = if self.eat_kw("WHERE") {
      Some(self.pred_or()?)
    } else {
      None
    };
    let (order_by, skip, limit) = self.tail()?;
    Ok(Stage::With {
      distinct,
      items,
      predicate,
      order_by,
      skip,
      limit,
    })
  }

  /// `(ORDER BY …)? (SKIP n)? (LIMIT n)?`
  #[allow(clippy::type_complexity)]
  fn tail(&mut self) -> Result<(Vec<Ordering>, Option<u64>, Option<u64>), QueryError> {
    let mut order_by = Vec::new();
    if self.eat_kw("ORDER") {
      self.expect_kw("BY")?;
      loop {
        let key = self.expr()?;
        let descending = if self.eat_kw("DESC") {
          true
        } else {
          self.eat_kw("ASC");
          false
        };
        order_by.push(Ordering { key, descending });
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
    Ok((order_by, skip, limit))
  }

  fn items(&mut self) -> Result<Vec<Projection>, QueryError> {
    let mut items = Vec::new();
    loop {
      let expr = self.expr()?;
      let alias = if self.eat_kw("AS") {
        Some(self.ident("alias after AS")?)
      } else {
        None
      };
      items.push(Projection { expr, alias });
      if self.peek() == Some(&Tok::Comma) {
        self.at += 1;
        continue;
      }
      break;
    }
    Ok(items)
  }

  // ---------------------------------------------------------------- patterns

  fn pattern(&mut self) -> Result<Pattern, QueryError> {
    let left = self.node_pattern()?;
    let mut segments = Vec::new();
    while matches!(self.peek(), Some(Tok::Dash) | Some(Tok::Lt)) {
      let offset = self.offset();
      if segments.len() >= crate::MAX_SEGMENTS {
        return Err(QueryError::parse(
          offset,
          format!("patterns chain at most {} relationship segments", crate::MAX_SEGMENTS),
        ));
      }
      let rel = self.rel_pattern()?;
      let node = self.node_pattern()?;
      segments.push(PatternSegment { rel, node });
    }
    Ok(Pattern { left, segments })
  }

  fn node_pattern(&mut self) -> Result<NodePattern, QueryError> {
    self.expect(&Tok::LParen, "'(' opening a node pattern")?;
    let mut node = NodePattern::default();
    if let Some(Tok::Ident(w)) = self.peek() {
      if !is_keyword(w) {
        node.var = Some(self.ident("variable")?);
      }
    }
    if self.peek() == Some(&Tok::Colon) {
      self.at += 1;
      node.kinds.push(self.label_ident("kind label after ':'")?);
      while self.peek() == Some(&Tok::Pipe) {
        self.at += 1;
        if self.peek() == Some(&Tok::Colon) {
          self.at += 1;
        }
        node.kinds.push(self.label_ident("kind label after '|'")?);
      }
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
      Some(Tok::Float(f)) => Ok(PropValue::Float(f)),
      Some(Tok::Ident(w)) if w.eq_ignore_ascii_case("true") => Ok(PropValue::Bool(true)),
      Some(Tok::Ident(w)) if w.eq_ignore_ascii_case("false") => Ok(PropValue::Bool(false)),
      // Bare identifiers are accepted as text values ({grade: constrained} reads naturally).
      Some(Tok::Ident(w)) => Ok(PropValue::Text(w)),
      _ => {
        self.at = self.at.saturating_sub(1);
        Err(QueryError::parse(
          self.offset(),
          "expected a string, number, true/false, or bare word value",
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
    // An optional relationship variable is accepted and discarded (nodes bind, edges don't).
    if let Some(Tok::Ident(w)) = self.peek() {
      if !is_keyword(w) {
        self.at += 1;
      }
    }
    let mut types = Vec::new();
    if self.peek() == Some(&Tok::Colon) {
      self.at += 1;
      types.push(self.ident("relation name after ':'")?);
      while self.peek() == Some(&Tok::Pipe) {
        self.at += 1;
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
          Some(n) => Some((n, n)),
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
            format!("unknown relationship property '{key}' (supported: grade)"),
          ));
        }
      }
    }
    self.expect(&Tok::RBracket, "']' closing the relationship")?;
    Ok((types, range, grade))
  }

  // ---------------------------------------------------------------- predicates

  /// `or := and (OR and)*` — lowest precedence.
  fn pred_or(&mut self) -> Result<PredExpr, QueryError> {
    let mut terms = vec![self.pred_and()?];
    loop {
      if self.at_kw("XOR") {
        return Err(QueryError::parse(self.offset(), "XOR is not supported (use AND/OR/NOT)"));
      }
      if !self.eat_kw("OR") {
        break;
      }
      terms.push(self.pred_and()?);
    }
    Ok(if terms.len() == 1 {
      match terms.pop() {
        Some(only) => only,
        None => return Err(QueryError::parse(self.offset(), "empty predicate")),
      }
    } else {
      PredExpr::Or(terms)
    })
  }

  /// `and := unary (AND unary)*`.
  fn pred_and(&mut self) -> Result<PredExpr, QueryError> {
    let mut terms = vec![self.pred_unary()?];
    while self.eat_kw("AND") {
      terms.push(self.pred_unary()?);
    }
    Ok(if terms.len() == 1 {
      match terms.pop() {
        Some(only) => only,
        None => return Err(QueryError::parse(self.offset(), "empty predicate")),
      }
    } else {
      PredExpr::And(terms)
    })
  }

  /// `unary := NOT unary | EXISTS '{' pattern '}' | '(' pred ')' | comparison`.
  fn pred_unary(&mut self) -> Result<PredExpr, QueryError> {
    if self.eat_kw("NOT") {
      return Ok(PredExpr::Not(Box::new(self.pred_unary()?)));
    }
    if self.eat_kw("EXISTS") {
      self.expect(&Tok::LBrace, "'{' after EXISTS")?;
      let pattern = self.pattern()?;
      if self.at_kw("WHERE") {
        return Err(QueryError::parse(
          self.offset(),
          "EXISTS { … } takes a pattern only — put its WHERE in the outer clause",
        ));
      }
      self.expect(&Tok::RBrace, "'}' closing EXISTS")?;
      return Ok(PredExpr::Exists { pattern });
    }
    if self.peek() == Some(&Tok::LParen) {
      // `(a = 1 OR b = 2)` is a grouped predicate; `(x + 1) > 2` is an expression that
      // starts with a parenthesis. Try the group first; on failure, back off to an
      // expression-headed comparison.
      let save = self.at;
      self.at += 1;
      if let Ok(inner) = self.pred_or() {
        if self.peek() == Some(&Tok::RParen) {
          self.at += 1;
          // A comparison operator after the group means the group was an expression.
          if !self.at_cmp_start() {
            return Ok(inner);
          }
        }
      }
      self.at = save;
    }
    self.comparison()
  }

  fn at_cmp_start(&self) -> bool {
    matches!(
      self.peek(),
      Some(Tok::Eq)
        | Some(Tok::Ne)
        | Some(Tok::Match)
        | Some(Tok::Lt)
        | Some(Tok::Gt)
        | Some(Tok::Plus)
        | Some(Tok::Dash)
        | Some(Tok::Star)
        | Some(Tok::Slash)
        | Some(Tok::Percent)
    ) || self.at_kw("STARTS")
      || self.at_kw("ENDS")
      || self.at_kw("CONTAINS")
      || self.at_kw("IN")
      || self.at_kw("IS")
  }

  /// `comparison := expr (cmpop expr | IN expr | IS NOT? NULL | ':' Kind ('|' Kind)*)?`
  fn comparison(&mut self) -> Result<PredExpr, QueryError> {
    let left = self.expr()?;
    // `n:Label`
    if let (Expr::Var { var }, Some(Tok::Colon)) = (&left, self.peek()) {
      self.at += 1;
      let mut kinds = vec![self.label_ident("kind label after ':'")?];
      while self.peek() == Some(&Tok::Pipe) {
        self.at += 1;
        if self.peek() == Some(&Tok::Colon) {
          self.at += 1;
        }
        kinds.push(self.label_ident("kind label after '|'")?);
      }
      return Ok(PredExpr::HasLabel {
        var: var.clone(),
        kinds,
      });
    }
    if self.eat_kw("IS") {
      let negated = self.eat_kw("NOT");
      self.expect_kw("NULL")?;
      return Ok(PredExpr::IsNull {
        expr: left,
        negated,
      });
    }
    if self.eat_kw("IN") {
      let list = self.expr()?;
      return Ok(PredExpr::In { item: left, list });
    }
    let op = match self.peek() {
      Some(Tok::Eq) => {
        self.at += 1;
        CmpOp::Eq
      }
      Some(Tok::Ne) => {
        self.at += 1;
        CmpOp::Ne
      }
      Some(Tok::Match) => {
        self.at += 1;
        CmpOp::Matches
      }
      Some(Tok::Lt) => {
        self.at += 1;
        if self.peek() == Some(&Tok::Eq) {
          self.at += 1;
          CmpOp::Le
        } else {
          CmpOp::Lt
        }
      }
      Some(Tok::Gt) => {
        self.at += 1;
        if self.peek() == Some(&Tok::Eq) {
          self.at += 1;
          CmpOp::Ge
        } else {
          CmpOp::Gt
        }
      }
      Some(Tok::Ident(w)) if w.eq_ignore_ascii_case("STARTS") => {
        self.at += 1;
        self.expect_kw("WITH")?;
        CmpOp::StartsWith
      }
      Some(Tok::Ident(w)) if w.eq_ignore_ascii_case("ENDS") => {
        self.at += 1;
        self.expect_kw("WITH")?;
        CmpOp::EndsWith
      }
      Some(Tok::Ident(w)) if w.eq_ignore_ascii_case("CONTAINS") => {
        self.at += 1;
        CmpOp::Contains
      }
      // A bare expression in predicate position is tested for truth (`WHERE f.exported`).
      _ => {
        return Ok(PredExpr::Cmp {
          left,
          op: CmpOp::Eq,
          right: Expr::Lit(PropValue::Bool(true)),
        });
      }
    };
    let right = self.expr()?;
    Ok(PredExpr::Cmp { left, op, right })
  }

  // ---------------------------------------------------------------- expressions

  /// `expr := term (('+'|'-') term)*`
  fn expr(&mut self) -> Result<Expr, QueryError> {
    let mut left = self.term()?;
    loop {
      let op = match self.peek() {
        Some(Tok::Plus) => ArithOp::Add,
        Some(Tok::Dash) => ArithOp::Sub,
        _ => break,
      };
      self.at += 1;
      let right = self.term()?;
      left = Expr::Binary {
        op,
        left: Box::new(left),
        right: Box::new(right),
      };
    }
    Ok(left)
  }

  /// `term := factor (('*'|'/'|'%') factor)*`
  fn term(&mut self) -> Result<Expr, QueryError> {
    let mut left = self.factor()?;
    loop {
      let op = match self.peek() {
        Some(Tok::Star) => ArithOp::Mul,
        Some(Tok::Slash) => ArithOp::Div,
        Some(Tok::Percent) => ArithOp::Mod,
        _ => break,
      };
      self.at += 1;
      let right = self.factor()?;
      left = Expr::Binary {
        op,
        left: Box::new(left),
        right: Box::new(right),
      };
    }
    Ok(left)
  }

  /// `factor := '-' factor | atom`
  fn factor(&mut self) -> Result<Expr, QueryError> {
    if self.peek() == Some(&Tok::Dash) {
      self.at += 1;
      return Ok(Expr::Neg(Box::new(self.factor()?)));
    }
    self.atom()
  }

  fn atom(&mut self) -> Result<Expr, QueryError> {
    let offset = self.offset();
    match self.peek().cloned() {
      Some(Tok::Int(n)) => {
        self.at += 1;
        Ok(Expr::Lit(PropValue::Int(n)))
      }
      Some(Tok::Float(f)) => {
        self.at += 1;
        Ok(Expr::Lit(PropValue::Float(f)))
      }
      Some(Tok::Str(s)) => {
        self.at += 1;
        Ok(Expr::Lit(PropValue::Text(s)))
      }
      Some(Tok::LBracket) => {
        self.at += 1;
        let mut items = Vec::new();
        if self.peek() != Some(&Tok::RBracket) {
          loop {
            items.push(self.expr()?);
            if self.peek() == Some(&Tok::Comma) {
              self.at += 1;
              continue;
            }
            break;
          }
        }
        self.expect(&Tok::RBracket, "']' closing the list")?;
        Ok(Expr::List(items))
      }
      Some(Tok::LParen) => {
        self.at += 1;
        let inner = self.expr()?;
        self.expect(&Tok::RParen, "')' closing the expression")?;
        Ok(inner)
      }
      Some(Tok::LBrace) => Err(QueryError::parse(
        offset,
        "map literals are not supported (return separate columns instead)",
      )),
      Some(Tok::Ident(word)) => {
        if word.eq_ignore_ascii_case("true") {
          self.at += 1;
          return Ok(Expr::Lit(PropValue::Bool(true)));
        }
        if word.eq_ignore_ascii_case("false") {
          self.at += 1;
          return Ok(Expr::Lit(PropValue::Bool(false)));
        }
        if word.eq_ignore_ascii_case("null") {
          self.at += 1;
          return Ok(Expr::Null);
        }
        if word.eq_ignore_ascii_case("case") {
          self.at += 1;
          return self.case_expr();
        }
        if self.peek2() == Some(&Tok::LParen) {
          // Function or aggregate call — the name may be a keyword-shaped word like
          // `count`, which is why the check precedes the keyword refusal.
          self.at += 2;
          return self.call(word);
        }
        if is_keyword(&word) {
          return Err(QueryError::parse(
            offset,
            format!("expected an expression, found '{word}'"),
          ));
        }
        self.at += 1;
        if self.peek() == Some(&Tok::Dot) {
          self.at += 1;
          let prop = self.ident("property name")?;
          return Ok(Expr::Prop { var: word, prop });
        }
        Ok(Expr::Var { var: word })
      }
      _ => Err(QueryError::parse(offset, "expected an expression")),
    }
  }

  /// After `name(` — arguments up to `)`.
  fn call(&mut self, name: String) -> Result<Expr, QueryError> {
    let lower = name.to_ascii_lowercase();
    let agg = match lower.as_str() {
      "count" => Some(AggFn::Count),
      "sum" => Some(AggFn::Sum),
      "avg" => Some(AggFn::Avg),
      "min" => Some(AggFn::Min),
      "max" => Some(AggFn::Max),
      "collect" => Some(AggFn::Collect),
      _ => None,
    };
    if let Some(func) = agg {
      if self.peek() == Some(&Tok::Star) {
        self.at += 1;
        self.expect(&Tok::RParen, "')' closing count(*)")?;
        if func != AggFn::Count {
          return Err(QueryError::parse(self.offset(), "only count takes *"));
        }
        return Ok(Expr::Agg {
          func,
          distinct: false,
          arg: None,
        });
      }
      let distinct = self.eat_kw("DISTINCT");
      let arg = self.expr()?;
      self.expect(&Tok::RParen, "')' closing the aggregate")?;
      return Ok(Expr::Agg {
        func,
        distinct,
        arg: Some(Box::new(arg)),
      });
    }
    let mut args = Vec::new();
    if self.peek() != Some(&Tok::RParen) {
      loop {
        args.push(self.expr()?);
        if self.peek() == Some(&Tok::Comma) {
          self.at += 1;
          continue;
        }
        break;
      }
    }
    self.expect(&Tok::RParen, "')' closing the call")?;
    Ok(Expr::Call { name: lower, args })
  }

  /// After `CASE` — `[subject] (WHEN a THEN b)+ [ELSE c] END`.
  fn case_expr(&mut self) -> Result<Expr, QueryError> {
    let subject = if self.at_kw("WHEN") {
      None
    } else {
      Some(Box::new(self.expr()?))
    };
    let mut whens = Vec::new();
    while self.eat_kw("WHEN") {
      let when = if subject.is_some() {
        self.expr()?
      } else {
        // Subject-less arms are predicates: `WHEN f.exported AND f.in_degree > 3 THEN …`.
        crate::expr::pred_to_expr(self.pred_or()?)
      };
      self.expect_kw("THEN")?;
      let then = self.expr()?;
      whens.push((when, then));
    }
    if whens.is_empty() {
      return Err(QueryError::parse(self.offset(), "CASE needs at least one WHEN"));
    }
    let otherwise = if self.eat_kw("ELSE") {
      Some(Box::new(self.expr()?))
    } else {
      None
    };
    self.expect_kw("END")?;
    Ok(Expr::Case {
      subject,
      whens,
      otherwise,
    })
  }
}
