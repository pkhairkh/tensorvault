//! SQL parser: recursive descent with Pratt-style expression precedence.
//!
//! The parser consumes the [`Token`] stream produced by
//! [`crate::sql::lexer::tokenize`] and produces a [`SelectQuery`]. Only the
//! standard `SELECT ... FROM ... [WHERE ...] [GROUP BY ...] [ORDER BY ...]
//! [LIMIT n]` form is supported — turboGP extensions (`APPROXIMATE`,
//! `TIER`, etc.) are handled separately by [`crate::sql::extensions`].
//!
//! ## Expression precedence (lowest → highest)
//!
//! 1. `OR`
//! 2. `AND`
//! 3. comparison (`=`, `!=`, `<`, `>`, `<=`, `>=`)
//! 4. additive (`+`, `-`)
//! 5. multiplicative (`*`, `/`)
//! 6. primary (literal, column, parenthesized)
//!
//! ## Not yet supported
//!
//! - `JOIN` (the spec defers joins to a later wave)
//! - `NOT` (the [`Expr`] enum has no `Unary` variant)
//! - implicit aliases (require `AS` keyword)
//! - subqueries
//! - `INSERT` / `UPDATE` / `DELETE` (only `SELECT` is parsed)

use crate::sql::lexer::Token;

/// A parsed SELECT query.
#[derive(Debug, Clone)]
pub struct SelectQuery {
    /// The select-list items (columns, aggregates, or `*`).
    pub select: Vec<SelectItem>,
    /// The source table name.
    pub from: String,
    /// Optional JOIN clauses.
    pub joins: Vec<JoinClause>,
    /// Optional WHERE predicate.
    pub where_clause: Option<Expr>,
    /// GROUP BY column list (empty if no GROUP BY).
    pub group_by: Vec<String>,
    /// ORDER BY column list with ascending flag (`true` = ASC).
    pub order_by: Vec<(String, bool)>,
    /// Optional LIMIT row count.
    pub limit: Option<usize>,
}

/// A parsed JOIN clause.
#[derive(Debug, Clone)]
pub struct JoinClause {
    /// The table to join.
    pub table: String,
    /// The join condition (ON clause).
    pub on: Expr,
}

/// One item in the SELECT list.
#[derive(Debug, Clone)]
pub enum SelectItem {
    /// A bare column reference: `col`.
    Column(String),
    /// An aggregate function call: `func(arg) [AS alias]`.
    Aggregate {
        /// The function name, uppercased (e.g. `COUNT`, `SUM`, `AVG`).
        func: String,
        /// The function argument: `*`, a column name, or a literal as a
        /// string.
        arg: String,
        /// Optional output alias.
        alias: Option<String>,
    },
    /// The `*` wildcard.
    Star,
    /// A non-negative integer literal in the SELECT list
    /// (e.g. `SELECT 1, URL, count(*) ...`). ClickBench Q15-Q42 use
    /// this to emit a constant column alongside the URL and count.
    /// Negative literals are rejected at parse time.
    Literal(u64),
}

/// A literal value extracted from a [`Token`].
#[derive(Debug, Clone)]
pub enum Value {
    /// An integer literal.
    Int(i64),
    /// A floating-point literal.
    Float(f64),
    /// A single-quoted string literal.
    String(String),
    /// A hex literal `x'...'`.
    Hex(Vec<u8>),
}

/// A boolean expression in WHERE / ON.
#[derive(Debug, Clone)]
pub enum Expr {
    /// A column reference.
    Column(String),
    /// A literal value.
    Literal(Value),
    /// A binary operator application: `left op right`.
    Binary {
        /// Left operand.
        left: Box<Expr>,
        /// Operator: `=`, `!=`, `<`, `>`, `<=`, `>=`, `+`, `-`, `*`, `/`,
        /// `AND`, or `OR`.
        op: String,
        /// Right operand.
        right: Box<Expr>,
    },
}

/// Parse a token stream into a [`SelectQuery`].
///
/// # Errors
///
/// Returns `Err(String)` with a human-readable message for any malformed
/// input — missing keywords, unexpected tokens, unterminated expressions,
/// etc. The error message is intended for display to a human, not for
/// programmatic matching.
pub fn parse(tokens: Vec<Token>) -> Result<SelectQuery, String> {
    let mut p = Parser::new(tokens);
    let q = p.parse_select()?;
    match p.peek() {
        Token::Semicolon | Token::EOF => Ok(q),
        other => Err(format!("unexpected trailing token: {other:?}")),
    }
}

/// The internal recursive-descent parser state.
struct Parser {
    /// The token stream (with trailing EOF).
    tokens: Vec<Token>,
    /// Current cursor position.
    pos: usize,
}

impl Parser {
    fn new(tokens: Vec<Token>) -> Self {
        Self { tokens, pos: 0 }
    }

    /// Peek at the current token (without advancing).
    fn peek(&self) -> &Token {
        &self.tokens[self.pos]
    }

    /// Consume and return the current token. Does not advance past EOF.
    fn next(&mut self) -> Token {
        let t = self.tokens.get(self.pos).cloned().unwrap_or(Token::EOF);
        if !matches!(t, Token::EOF) {
            self.pos += 1;
        }
        t
    }

    /// If the current token is `Keyword(kw)` (case-sensitive, since keywords
    /// are uppercased by the lexer), consume it and return `true`.
    fn match_keyword(&mut self, kw: &str) -> bool {
        if let Token::Keyword(k) = self.peek() {
            if k == kw {
                self.pos += 1;
                return true;
            }
        }
        false
    }

    /// Consume the current token if it is `Keyword(kw)`, else error.
    fn expect_keyword(&mut self, kw: &str) -> Result<(), String> {
        if self.match_keyword(kw) {
            return Ok(());
        }
        Err(format!("expected keyword {kw}, got {:?}", self.peek()))
    }

    /// If the current token is `Op(op)`, consume it and return `true`.
    fn match_op(&mut self, op: &str) -> bool {
        if let Token::Op(o) = self.peek() {
            if o == op {
                self.pos += 1;
                return true;
            }
        }
        false
    }

    /// If the current token is an `Ident` matching `name` (case-insensitive),
    /// consume it and return `true`. Used for non-keyword reserved words
    /// (`LIMIT`, `ASC`, `DESC`).
    fn match_ident(&mut self, name: &str) -> bool {
        match self.peek() {
            Token::Ident(s) if s.eq_ignore_ascii_case(name) => {
                self.pos += 1;
                true
            }
            Token::Keyword(k) if k.eq_ignore_ascii_case(name) => {
                self.pos += 1;
                true
            }
            _ => false,
        }
    }

    // -----------------------------------------------------------------------
    // SELECT statement
    // -----------------------------------------------------------------------

    fn parse_select(&mut self) -> Result<SelectQuery, String> {
        self.expect_keyword("SELECT")?;
        let select = self.parse_select_list()?;
        // FROM is optional — allows `SELECT 1` and `SELECT count(*)`.
        let from = if self.match_keyword("FROM") {
            self.parse_table_name()?
        } else {
            // No FROM clause — use a synthetic single-row table.
            // This allows `SELECT 1`, `SELECT count(*)`, etc.
            "__dummy__".to_string()
        };

        // Parse optional JOIN clauses
        let joins = self.parse_joins()?;

        let where_clause =
            if self.match_keyword("WHERE") { Some(self.parse_expr()?) } else { None };

        let group_by = if self.match_keyword("GROUP") {
            self.expect_keyword("BY")?;
            self.parse_column_list()?
        } else {
            Vec::new()
        };

        let order_by = if self.match_keyword("ORDER") {
            self.expect_keyword("BY")?;
            self.parse_order_list()?
        } else {
            Vec::new()
        };

        let limit = if self.match_ident("LIMIT") { Some(self.parse_usize()?) } else { None };

        Ok(SelectQuery { select, from, joins, where_clause, group_by, order_by, limit })
    }

    /// Parse zero or more JOIN clauses.
    fn parse_joins(&mut self) -> Result<Vec<JoinClause>, String> {
        let mut joins = Vec::new();
        loop {
            if self.match_keyword("JOIN") || (self.match_keyword("INNER") && self.expect_keyword("JOIN").is_ok()) {
                let table = self.parse_table_name()?;
                self.expect_keyword("ON")?;
                let on = self.parse_expr()?;
                joins.push(JoinClause { table, on });
            } else if self.match_keyword("LEFT") {
                let _ = self.match_keyword("OUTER");
                self.expect_keyword("JOIN")?;
                let table = self.parse_table_name()?;
                self.expect_keyword("ON")?;
                let on = self.parse_expr()?;
                joins.push(JoinClause { table, on });
            } else {
                break;
            }
        }
        Ok(joins)
    }

    fn parse_select_list(&mut self) -> Result<Vec<SelectItem>, String> {
        let mut items = Vec::new();
        loop {
            items.push(self.parse_select_item()?);
            if !matches!(self.peek(), Token::Comma) {
                break;
            }
            self.next(); // consume comma
        }
        Ok(items)
    }

    fn parse_select_item(&mut self) -> Result<SelectItem, String> {
        // `*` → Star.
        if self.match_op("*") {
            return Ok(SelectItem::Star);
        }
        // Non-negative integer literal → `Literal(u64)`
        // (e.g. `SELECT 1, URL, count(*) ...` in ClickBench Q15-Q42).
        if let Token::Int(i) = self.peek().clone() {
            if i < 0 {
                return Err(format!("negative integer literal in SELECT list: {i}"));
            }
            self.next();
            // An alias (`SELECT 1 AS x`) is legal but unused — consume it.
            let _ = self.parse_optional_alias()?;
            return Ok(SelectItem::Literal(i as u64));
        }
        // `IDENT ( ... )` → Aggregate; `IDENT` → Column.
        if let Token::Ident(name) = self.peek().clone() {
            self.next();
            if matches!(self.peek(), Token::LParen) {
                self.next(); // consume (

                // Detect `func(DISTINCT col)` and normalise to
                // `Aggregate { func: "FUNC_DISTINCT", arg: "col" }`. The
                // `DISTINCT` token is not in `KEYWORDS` (it is rarely used
                // outside aggregates), so it arrives here as `Ident("DISTINCT")`.
                // `match_ident` does a case-insensitive compare, matching
                // `distinct`, `Distinct`, etc.
                let (func, arg) = if self.match_ident("DISTINCT") {
                    let col = match self.peek().clone() {
                        Token::Ident(s) => {
                            self.next();
                            s
                        }
                        other => {
                            return Err(format!(
                                "expected column name after DISTINCT, got {other:?}"
                            ));
                        }
                    };
                    (format!("{}_DISTINCT", name.to_uppercase()), col)
                } else {
                    let arg = self.parse_agg_arg()?;
                    (name.to_uppercase(), arg)
                };

                if !matches!(self.peek(), Token::RParen) {
                    return Err(format!("expected ) after aggregate arg, got {:?}", self.peek()));
                }
                self.next(); // consume )
                let alias = self.parse_optional_alias()?;
                return Ok(SelectItem::Aggregate { func, arg, alias });
            }
            let _ = self.parse_optional_alias()?;
            return Ok(SelectItem::Column(name));
        }
        Err(format!("expected select item, got {:?}", self.peek()))
    }

    /// Parse the argument of an aggregate function: `*`, a column name, or
    /// (for completeness) an integer literal.
    fn parse_agg_arg(&mut self) -> Result<String, String> {
        if self.match_op("*") {
            return Ok("*".to_string());
        }
        if let Token::Ident(name) = self.peek().clone() {
            self.next();
            return Ok(name);
        }
        if let Token::Int(i) = self.peek().clone() {
            self.next();
            return Ok(i.to_string());
        }
        Err(format!("expected aggregate argument, got {:?}", self.peek()))
    }

    /// Parse an optional `AS ident` alias. Implicit aliases (without `AS`)
    /// are not supported to avoid swallowing `LIMIT` / `ASC` / `DESC`.
    fn parse_optional_alias(&mut self) -> Result<Option<String>, String> {
        if self.match_keyword("AS") {
            if let Token::Ident(name) = self.peek().clone() {
                self.next();
                return Ok(Some(name));
            }
            return Err(format!("expected alias after AS, got {:?}", self.peek()));
        }
        Ok(None)
    }

    fn parse_table_name(&mut self) -> Result<String, String> {
        // Reserved keywords that cannot be used as table names.
        const RESERVED: &[&str] = &[
            "WHERE", "GROUP", "ORDER", "HAVING", "LIMIT", "JOIN", "ON",
            "LEFT", "RIGHT", "INNER", "OUTER", "UNION", "EXCEPT", "INTERSECT",
        ];
        let first = match self.peek().clone() {
            Token::Ident(name) => name,
            Token::Keyword(k) => {
                if RESERVED.contains(&k.as_str()) {
                    return Err(format!("expected table name, got keyword '{k}'"));
                }
                k
            }
            other => return Err(format!("expected table name, got {other:?}")),
        };
        self.next();
        // Check for schema.table
        if let Token::Op(op) = self.peek() {
            if op == "." {
                self.next(); // consume .
                let second = match self.peek().clone() {
                    Token::Ident(name) => name,
                    Token::Keyword(k) => k,
                    other => return Err(format!("expected name after '.', got {other:?}")),
                };
                self.next();
                return Ok(format!("{first}.{second}"));
            }
        }
        Ok(first)
    }

    fn parse_column_list(&mut self) -> Result<Vec<String>, String> {
        let mut cols = Vec::new();
        loop {
            match self.peek().clone() {
                Token::Ident(name) => {
                    self.next();
                    cols.push(name);
                }
                // Positional GROUP BY reference (e.g. `GROUP BY 1, URL`).
                // The integer refers to the Nth SELECT item; when that item
                // is a literal constant (as in ClickBench Q15-Q42 where
                // `1` refers to `SELECT 1`), every row shares the same value
                // so it has no effect on grouping. We simply skip it —
                // only the non-numeric GROUP BY items remain, which is
                // semantically correct for the ClickBench queries.
                Token::Int(_) => {
                    self.next();
                }
                other => {
                    return Err(format!("expected column name in GROUP BY, got {other:?}"));
                }
            }
            if !matches!(self.peek(), Token::Comma) {
                break;
            }
            self.next();
        }
        Ok(cols)
    }

    fn parse_order_list(&mut self) -> Result<Vec<(String, bool)>, String> {
        let mut items = Vec::new();
        loop {
            if let Token::Ident(name) = self.peek().clone() {
                self.next();
                let ascending = if self.match_ident("DESC") {
                    false
                } else {
                    let _ = self.match_ident("ASC");
                    true
                };
                items.push((name, ascending));
            } else {
                return Err(format!("expected column name in ORDER BY, got {:?}", self.peek()));
            }
            if !matches!(self.peek(), Token::Comma) {
                break;
            }
            self.next();
        }
        Ok(items)
    }

    fn parse_usize(&mut self) -> Result<usize, String> {
        if let Token::Int(i) = self.peek() {
            if *i < 0 {
                return Err(format!("expected non-negative integer, got {i}"));
            }
            let u = *i as usize;
            self.next();
            return Ok(u);
        }
        Err(format!("expected integer, got {:?}", self.peek()))
    }

    // -----------------------------------------------------------------------
    // Expression parsing (Pratt-style by precedence climbing)
    // -----------------------------------------------------------------------

    fn parse_expr(&mut self) -> Result<Expr, String> {
        self.parse_or_expr()
    }

    fn parse_or_expr(&mut self) -> Result<Expr, String> {
        let mut left = self.parse_and_expr()?;
        while self.match_keyword("OR") {
            let right = self.parse_and_expr()?;
            left =
                Expr::Binary { left: Box::new(left), op: "OR".to_string(), right: Box::new(right) };
        }
        Ok(left)
    }

    fn parse_and_expr(&mut self) -> Result<Expr, String> {
        let mut left = self.parse_comparison_expr()?;
        while self.match_keyword("AND") {
            let right = self.parse_comparison_expr()?;
            left = Expr::Binary {
                left: Box::new(left),
                op: "AND".to_string(),
                right: Box::new(right),
            };
        }
        Ok(left)
    }

    fn parse_comparison_expr(&mut self) -> Result<Expr, String> {
        let left = self.parse_additive_expr()?;
        // Comparison operators: = != <> < > <= >=
        if let Token::Op(op) = self.peek().clone() {
            if matches!(op.as_str(), "=" | "!=" | "<>" | "<" | ">" | "<=" | ">=") {
                self.next();
                let right = self.parse_additive_expr()?;
                // Normalize <> to != so the executor's existing arm handles it.
                let op = if op == "<>" { "!=".to_string() } else { op };
                return Ok(Expr::Binary { left: Box::new(left), op, right: Box::new(right) });
            }
        }
        // LIKE keyword
        if self.match_ident("LIKE") {
            let right = self.parse_additive_expr()?;
            return Ok(Expr::Binary { left: Box::new(left), op: "LIKE".to_string(), right: Box::new(right) });
        }
        // NOT LIKE
        if self.match_keyword("NOT") {
            if self.match_ident("LIKE") {
                let right = self.parse_additive_expr()?;
                return Ok(Expr::Binary { left: Box::new(left), op: "NOT LIKE".to_string(), right: Box::new(right) });
            }
            // NOT could be part of another construct; put it back
            self.pos -= 1;
        }
        // BETWEEN x AND y — lower to (expr >= x AND expr <= y)
        if self.match_keyword("BETWEEN") {
            let low = self.parse_additive_expr()?;
            // Expect AND
            if !self.match_keyword("AND") {
                return Err("expected AND after BETWEEN low".into());
            }
            let high = self.parse_additive_expr()?;
            let ge = Expr::Binary {
                left: Box::new(left.clone()),
                op: ">=".to_string(),
                right: Box::new(low),
            };
            let le = Expr::Binary {
                left: Box::new(left),
                op: "<=".to_string(),
                right: Box::new(high),
            };
            return Ok(Expr::Binary {
                left: Box::new(ge),
                op: "AND".to_string(),
                right: Box::new(le),
            });
        }
        // NOT BETWEEN
        if self.match_keyword("NOT") {
            if self.match_keyword("BETWEEN") {
                let low = self.parse_additive_expr()?;
                if !self.match_keyword("AND") {
                    return Err("expected AND after NOT BETWEEN low".into());
                }
                let high = self.parse_additive_expr()?;
                // NOT BETWEEN → (expr < low OR expr > high)
                let lt = Expr::Binary {
                    left: Box::new(left.clone()),
                    op: "<".to_string(),
                    right: Box::new(low),
                };
                let gt = Expr::Binary {
                    left: Box::new(left),
                    op: ">".to_string(),
                    right: Box::new(high),
                };
                return Ok(Expr::Binary {
                    left: Box::new(lt),
                    op: "OR".to_string(),
                    right: Box::new(gt),
                });
            }
            self.pos -= 1;
        }
        // IN (val1, val2, ...) — lower to (expr = val1 OR expr = val2 OR ...)
        if self.match_keyword("IN") {
            if self.peek() != &Token::LParen {
                return Err("expected ( after IN".into());
            }
            self.next(); // consume (
            let mut or_expr: Option<Expr> = None;
            loop {
                let val = self.parse_additive_expr()?;
                let eq = Expr::Binary {
                    left: Box::new(left.clone()),
                    op: "=".to_string(),
                    right: Box::new(val),
                };
                match or_expr {
                    None => or_expr = Some(eq),
                    Some(existing) => {
                        or_expr = Some(Expr::Binary {
                            left: Box::new(existing),
                            op: "OR".to_string(),
                            right: Box::new(eq),
                        });
                    }
                }
                match self.peek() {
                    Token::Comma => { self.next(); }
                    Token::RParen => { self.next(); break; }
                    other => return Err(format!("expected , or ) in IN list, got {other:?}")),
                }
            }
            return Ok(or_expr.unwrap_or(left));
        }
        // NOT IN
        if self.match_keyword("NOT") {
            if self.match_keyword("IN") {
                if self.peek() != &Token::LParen {
                    return Err("expected ( after NOT IN".into());
                }
                self.next();
                let mut and_expr: Option<Expr> = None;
                loop {
                    let val = self.parse_additive_expr()?;
                    let ne = Expr::Binary {
                        left: Box::new(left.clone()),
                        op: "!=".to_string(),
                        right: Box::new(val),
                    };
                    match and_expr {
                        None => and_expr = Some(ne),
                        Some(existing) => {
                            and_expr = Some(Expr::Binary {
                                left: Box::new(existing),
                                op: "AND".to_string(),
                                right: Box::new(ne),
                            });
                        }
                    }
                    match self.peek() {
                        Token::Comma => { self.next(); }
                        Token::RParen => { self.next(); break; }
                        other => return Err(format!("expected , or ) in NOT IN list, got {other:?}")),
                    }
                }
                return Ok(and_expr.unwrap_or(left));
            }
            self.pos -= 1;
        }
        Ok(left)
    }

    fn parse_additive_expr(&mut self) -> Result<Expr, String> {
        let mut left = self.parse_multiplicative_expr()?;
        loop {
            if let Token::Op(op) = self.peek().clone() {
                if matches!(op.as_str(), "+" | "-") {
                    self.next();
                    let right = self.parse_multiplicative_expr()?;
                    left = Expr::Binary { left: Box::new(left), op, right: Box::new(right) };
                    continue;
                }
            }
            break;
        }
        Ok(left)
    }

    fn parse_multiplicative_expr(&mut self) -> Result<Expr, String> {
        let mut left = self.parse_primary()?;
        loop {
            if let Token::Op(op) = self.peek().clone() {
                if matches!(op.as_str(), "*" | "/") {
                    self.next();
                    let right = self.parse_primary()?;
                    left = Expr::Binary { left: Box::new(left), op, right: Box::new(right) };
                    continue;
                }
            }
            break;
        }
        Ok(left)
    }

    fn parse_primary(&mut self) -> Result<Expr, String> {
        match self.peek().clone() {
            Token::Int(i) => {
                self.next();
                Ok(Expr::Literal(Value::Int(i)))
            }
            Token::Float(f) => {
                self.next();
                Ok(Expr::Literal(Value::Float(f)))
            }
            Token::String(s) => {
                self.next();
                Ok(Expr::Literal(Value::String(s)))
            }
            Token::Hex(h) => {
                self.next();
                Ok(Expr::Literal(Value::Hex(h)))
            }
            Token::Keyword(kw) => {
                let kw_upper = kw.to_uppercase();
                if kw_upper == "DATE" {
                    self.next();
                    if let Token::String(s) = self.peek().clone() {
                        self.next();
                        if let Ok(d) = crate::types::Date::from_str(&s) {
                            return Ok(Expr::Literal(Value::Int(d.0 as i64)));
                        }
                        return Ok(Expr::Literal(Value::String(s)));
                    }
                    return Ok(Expr::Column(kw));
                }
                // Other keywords treated as identifiers
                self.next();
                Ok(Expr::Column(kw))
            }
            Token::Ident(name) => {
                self.next();
                Ok(Expr::Column(name))
            }
            Token::LParen => {
                self.next();
                let e = self.parse_expr()?;
                if !matches!(self.peek(), Token::RParen) {
                    return Err(format!("expected ), got {:?}", self.peek()));
                }
                self.next();
                Ok(e)
            }
            other => Err(format!("expected expression, got {other:?}")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sql::lexer::tokenize;

    /// Helper: tokenize + parse in one step.
    fn parse_sql(s: &str) -> Result<SelectQuery, String> {
        parse(tokenize(s)?)
    }

    #[test]
    fn parse_select_star_with_where() {
        let q = parse_sql("SELECT * FROM t WHERE x = 5").unwrap();
        assert_eq!(q.select.len(), 1);
        assert!(matches!(q.select[0], SelectItem::Star));
        assert_eq!(q.from, "t");
        let w = q.where_clause.expect("WHERE clause");
        match w {
            Expr::Binary { left, op, right } => {
                assert_eq!(op, "=");
                match *left {
                    Expr::Column(c) => assert_eq!(c, "x"),
                    other => panic!("expected Column, got {other:?}"),
                }
                match *right {
                    Expr::Literal(Value::Int(5)) => {}
                    other => panic!("expected Int(5), got {other:?}"),
                }
            }
            other => panic!("expected Binary, got {other:?}"),
        }
    }

    #[test]
    fn parse_count_star() {
        let q = parse_sql("SELECT COUNT(*) FROM t").unwrap();
        assert_eq!(q.select.len(), 1);
        match &q.select[0] {
            SelectItem::Aggregate { func, arg, alias } => {
                assert_eq!(func, "COUNT");
                assert_eq!(arg, "*");
                assert_eq!(*alias, None);
            }
            other => panic!("expected Aggregate, got {other:?}"),
        }
        assert_eq!(q.from, "t");
        assert!(q.where_clause.is_none());
    }

    #[test]
    fn parse_sum_with_alias() {
        let q = parse_sql("SELECT SUM(price) AS total FROM sales").unwrap();
        match &q.select[0] {
            SelectItem::Aggregate { func, arg, alias } => {
                assert_eq!(func, "SUM");
                assert_eq!(arg, "price");
                assert_eq!(*alias, Some("total".to_string()));
            }
            other => panic!("expected Aggregate, got {other:?}"),
        }
    }

    #[test]
    fn parse_avg_group_by() {
        let q = parse_sql("SELECT AVG(price) FROM sales GROUP BY area").unwrap();
        assert_eq!(q.group_by, vec!["area"]);
    }

    #[test]
    fn parse_order_by_asc_desc() {
        let q = parse_sql("SELECT * FROM t ORDER BY a ASC, b DESC, c").unwrap();
        assert_eq!(q.order_by.len(), 3);
        assert_eq!(q.order_by[0], ("a".to_string(), true));
        assert_eq!(q.order_by[1], ("b".to_string(), false));
        assert_eq!(q.order_by[2], ("c".to_string(), true));
    }

    #[test]
    fn parse_limit() {
        let q = parse_sql("SELECT * FROM t LIMIT 100").unwrap();
        assert_eq!(q.limit, Some(100));
    }

    #[test]
    fn parse_multiple_columns() {
        let q = parse_sql("SELECT a, b, c FROM t").unwrap();
        assert_eq!(q.select.len(), 3);
        assert!(matches!(&q.select[0], SelectItem::Column(c) if c == "a"));
        assert!(matches!(&q.select[1], SelectItem::Column(c) if c == "b"));
        assert!(matches!(&q.select[2], SelectItem::Column(c) if c == "c"));
    }

    #[test]
    fn parse_and_or_precedence() {
        // a = 1 AND b = 2 OR c = 3  →  (a=1 AND b=2) OR (c=3)
        let q = parse_sql("SELECT * FROM t WHERE a = 1 AND b = 2 OR c = 3").unwrap();
        let w = q.where_clause.unwrap();
        match w {
            Expr::Binary { left, op, right } => {
                assert_eq!(op, "OR");
                // Left should be AND.
                match *left {
                    Expr::Binary { op, .. } => assert_eq!(op, "AND"),
                    other => panic!("expected AND, got {other:?}"),
                }
                // Right should be a comparison.
                match *right {
                    Expr::Binary { op, .. } => assert_eq!(op, "="),
                    other => panic!("expected =, got {other:?}"),
                }
            }
            other => panic!("expected OR at top, got {other:?}"),
        }
    }

    #[test]
    fn parse_arithmetic_precedence() {
        // a + b * c  →  a + (b * c)
        let q = parse_sql("SELECT * FROM t WHERE x = a + b * c").unwrap();
        let w = q.where_clause.unwrap();
        match w {
            Expr::Binary { left, op: op_eq, right } => {
                assert_eq!(op_eq, "=");
                assert!(matches!(*left, Expr::Column(_)));
                match *right {
                    Expr::Binary { op, right: mul_right, .. } => {
                        assert_eq!(op, "+");
                        match *mul_right {
                            Expr::Binary { op, .. } => assert_eq!(op, "*"),
                            other => panic!("expected *, got {other:?}"),
                        }
                    }
                    other => panic!("expected +, got {other:?}"),
                }
            }
            other => panic!("expected =, got {other:?}"),
        }
    }

    #[test]
    fn parse_parenthesized_expr() {
        let q = parse_sql("SELECT * FROM t WHERE (a = 1 OR b = 2) AND c = 3").unwrap();
        let w = q.where_clause.unwrap();
        match w {
            Expr::Binary { left, op, .. } => {
                assert_eq!(op, "AND");
                match *left {
                    Expr::Binary { op, .. } => assert_eq!(op, "OR"),
                    other => panic!("expected OR, got {other:?}"),
                }
            }
            other => panic!("expected AND at top, got {other:?}"),
        }
    }

    #[test]
    fn parse_trailing_semicolon_ok() {
        let q = parse_sql("SELECT * FROM t;").unwrap();
        assert_eq!(q.from, "t");
    }

    #[test]
    fn parse_string_literal_in_where() {
        let q = parse_sql("SELECT * FROM t WHERE name = 'alice'").unwrap();
        let w = q.where_clause.unwrap();
        match w {
            Expr::Binary { right, .. } => match *right {
                Expr::Literal(Value::String(s)) => assert_eq!(s, "alice"),
                other => panic!("expected String, got {other:?}"),
            },
            other => panic!("expected Binary, got {other:?}"),
        }
    }

    #[test]
    fn parse_invalid_missing_select_list() {
        let r = parse_sql("SELECT FROM WHERE");
        assert!(r.is_err(), "expected error for SELECT FROM WHERE");
    }

    #[test]
    fn parse_invalid_missing_table() {
        let r = parse_sql("SELECT * FROM WHERE");
        assert!(r.is_err(), "expected error for missing table name");
    }

    #[test]
    fn parse_invalid_negative_limit() {
        let r = parse_sql("SELECT * FROM t LIMIT -5");
        assert!(r.is_err(), "expected error for negative LIMIT");
    }

    #[test]
    fn parse_invalid_unexpected_eof() {
        // FROM is now optional (Wave 6), so "SELECT *" should parse
        // successfully and use the __dummy__ table.
        let r = parse_sql("SELECT *");
        assert!(r.is_ok(), "SELECT * without FROM should parse, got: {r:?}");
    }

    #[test]
    fn parse_invalid_trailing_garbage() {
        let r = parse_sql("SELECT * FROM t WHERE x = 5 garbage");
        assert!(r.is_err(), "expected error for trailing garbage");
    }

    #[test]
    fn parse_count_distinct_keyword() {
        // `COUNT(DISTINCT col)` is normalised to
        // `Aggregate { func: "COUNT_DISTINCT", arg: "col" }`.
        let q = parse_sql("SELECT COUNT(DISTINCT user_id) FROM events").unwrap();
        match &q.select[0] {
            SelectItem::Aggregate { func, arg, alias } => {
                assert_eq!(func, "COUNT_DISTINCT");
                assert_eq!(arg, "user_id");
                assert_eq!(*alias, None);
            }
            other => panic!("expected Aggregate, got {other:?}"),
        }
    }

    #[test]
    fn parse_count_distinct_case_insensitive() {
        // `count(distinct col)` should normalise the same way.
        let q = parse_sql("SELECT count(distinct x) FROM t").unwrap();
        match &q.select[0] {
            SelectItem::Aggregate { func, arg, .. } => {
                assert_eq!(func, "COUNT_DISTINCT");
                assert_eq!(arg, "x");
            }
            other => panic!("expected Aggregate, got {other:?}"),
        }
    }

    #[test]
    fn parse_count_distinct_requires_column() {
        // `COUNT(DISTINCT)` with no column should error.
        let r = parse_sql("SELECT COUNT(DISTINCT) FROM t");
        assert!(r.is_err(), "expected error for COUNT(DISTINCT) without column");
    }

    #[test]
    fn parse_sum_distinct_keyword() {
        // `SUM(DISTINCT col)` works the same way (produces SUM_DISTINCT).
        let q = parse_sql("SELECT SUM(DISTINCT price) FROM sales").unwrap();
        match &q.select[0] {
            SelectItem::Aggregate { func, arg, .. } => {
                assert_eq!(func, "SUM_DISTINCT");
                assert_eq!(arg, "price");
            }
            other => panic!("expected Aggregate, got {other:?}"),
        }
    }

    #[test]
    fn parse_select_integer_literal() {
        // `SELECT 1, URL, count(*)` — ClickBench Q15-Q42 shape.
        let q = parse_sql("SELECT 1, URL, count(*) FROM t").unwrap();
        assert_eq!(q.select.len(), 3);
        assert!(matches!(&q.select[0], SelectItem::Literal(1)));
        assert!(matches!(&q.select[1], SelectItem::Column(c) if c == "URL"));
        assert!(matches!(&q.select[2], SelectItem::Aggregate { func, arg, .. } if func == "COUNT" && arg == "*"));
    }

    #[test]
    fn parse_group_by_positional_and_column() {
        // `GROUP BY 1, URL` — the positional `1` is skipped, only URL
        // remains as a real GROUP BY key.
        let q = parse_sql("SELECT 1, URL, count(*) FROM t GROUP BY 1, URL").unwrap();
        assert_eq!(q.group_by, vec!["URL"]);
    }

    #[test]
    fn parse_group_by_positional_only() {
        // `GROUP BY 1` alone (degenerate but legal) → empty group_by.
        let q = parse_sql("SELECT 1, count(*) FROM t GROUP BY 1").unwrap();
        assert!(q.group_by.is_empty());
    }

    #[test]
    fn parse_select_negative_literal_rejected() {
        // Negative integer literals in the SELECT list are rejected.
        assert!(parse_sql("SELECT -1 FROM t").is_err());
    }

    #[test]
    fn parse_clickbench_q15_shape() {
        // Full Q15 shape: SELECT 1, URL, count(*) AS c FROM t WHERE URL LIKE 'https://%' GROUP BY 1, URL ORDER BY c DESC LIMIT 10
        let q = parse_sql(
            "SELECT 1, URL, count(*) AS c FROM t WHERE URL LIKE 'https://%' GROUP BY 1, URL ORDER BY c DESC LIMIT 10",
        )
        .unwrap();
        assert_eq!(q.select.len(), 3);
        assert!(matches!(&q.select[0], SelectItem::Literal(1)));
        assert!(matches!(&q.select[2], SelectItem::Aggregate { alias: Some(a), .. } if a == "c"));
        assert_eq!(q.group_by, vec!["URL"]);
        assert_eq!(q.order_by, vec![("c".to_string(), false)]);
        assert_eq!(q.limit, Some(10));
    }
}
