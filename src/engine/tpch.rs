//! TPC-H query parser + interpreter.
//!
//! A self-contained SQL parser and row-based interpreter that handles the
//! TPC-H SF=1 query set. Uses the existing [`Token`] lexer but produces a
//! richer AST, then executes with type-aware evaluation (Float64 columns
//! stored as `f64::to_bits` are correctly interpreted).

use crate::catalog::Catalog;
use crate::datasource::csv::{tpch_schema, TpchType};
use crate::datasource::table::Table;
use crate::engine::result::{QueryResult, ResultColumn};
use crate::exec::fm_index::StringSearchColumn;
use crate::sql::lexer::{tokenize, Token};
use crate::Error;
use std::collections::{HashMap, HashSet};

// =========================================================================
// Column type tracking
// =========================================================================

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ColType {
    Int,
    Float,
    Date,
    String,
}

pub fn tpch_col_types(table_name: &str) -> Vec<ColType> {
    tpch_schema(table_name)
        .unwrap_or_default()
        .iter()
        .map(|(_, t)| match t {
            TpchType::Int64 => ColType::Int,
            TpchType::Float64 => ColType::Float,
            TpchType::Date => ColType::Date,
            TpchType::String => ColType::String,
        })
        .collect()
}

// =========================================================================
// AST
// =========================================================================

#[derive(Debug, Clone)]
pub enum Expr2 {
    Col(String),
    Int(i64),
    Float(f64),
    Str(String),
    Date(i32),
    BinOp { op: BinOp2, left: Box<Expr2>, right: Box<Expr2> },
    Like { expr: Box<Expr2>, pattern: Box<Expr2>, negated: bool },
    Between { expr: Box<Expr2>, low: Box<Expr2>, high: Box<Expr2>, negated: bool },
    InList { expr: Box<Expr2>, list: Vec<Expr2>, negated: bool },
    InSubquery { expr: Box<Expr2>, query: Box<SelectQuery2>, negated: bool },
    Exists { query: Box<SelectQuery2>, negated: bool },
    Case { whens: Vec<(Expr2, Expr2)>, else_: Option<Box<Expr2>> },
    Extract { field: String, expr: Box<Expr2> },
    Substr { expr: Box<Expr2>, start: Box<Expr2>, len: Box<Expr2> },
    Agg { func: AggFunc, arg: Box<Expr2>, distinct: bool },
    CountStar,
    Subquery(Box<SelectQuery2>),
    Not(Box<Expr2>),
    Neg(Box<Expr2>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BinOp2 { Add, Sub, Mul, Div, Eq, Ne, Lt, Gt, Le, Ge, And, Or }

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AggFunc { Sum, Avg, Count, Min, Max, CountDistinct }

#[derive(Debug, Clone)]
pub struct TableRef { pub name: String, pub alias: Option<String> }

#[derive(Debug, Clone)]
pub enum FromItem {
    Table(TableRef),
    Derived(Box<SelectQuery2>, Option<String>),
}

#[derive(Debug, Clone)]
pub struct JoinClause2 { pub join_type: JoinType2, pub table: FromItem, pub on: Expr2 }

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JoinType2 { Inner, Left }

#[derive(Debug, Clone)]
pub struct SelectItem2 { pub expr: Expr2, pub alias: Option<String> }

#[derive(Debug, Clone)]
pub struct SelectQuery2 {
    pub select: Vec<SelectItem2>,
    pub from: Vec<FromItem>,
    pub joins: Vec<JoinClause2>,
    pub where_clause: Option<Expr2>,
    pub group_by: Vec<Expr2>,
    pub having: Option<Expr2>,
    pub order_by: Vec<(Expr2, bool)>,
    pub limit: Option<usize>,
}

// =========================================================================
// Parser
// =========================================================================

pub fn parse_tpch(sql: &str) -> Result<SelectQuery2, String> {
    let tokens = tokenize(sql)?;
    let mut p = TpchParser { tokens, pos: 0 };
    let q = p.parse_select()?;
    match p.peek() {
        Token::Semicolon | Token::EOF => Ok(q),
        other => Err(format!("unexpected trailing token: {other:?}")),
    }
}

struct TpchParser { tokens: Vec<Token>, pos: usize }

impl TpchParser {
    fn peek(&self) -> &Token { &self.tokens[self.pos] }
    fn peek_at(&self, n: usize) -> &Token { self.tokens.get(self.pos + n).unwrap_or(&Token::EOF) }
    fn next(&mut self) -> Token {
        let t = self.tokens.get(self.pos).cloned().unwrap_or(Token::EOF);
        if !matches!(t, Token::EOF) { self.pos += 1; }
        t
    }
    fn match_kw(&mut self, kw: &str) -> bool {
        if let Token::Keyword(k) = self.peek() { if k == kw { self.pos += 1; return true; } }
        false
    }
    fn match_ident_or_kw(&mut self, name: &str) -> bool {
        match self.peek() {
            Token::Ident(s) if s.eq_ignore_ascii_case(name) => { self.pos += 1; true }
            Token::Keyword(k) if k.eq_ignore_ascii_case(name) => { self.pos += 1; true }
            _ => false,
        }
    }
    fn expect_kw(&mut self, kw: &str) -> Result<(), String> {
        if self.match_kw(kw) { return Ok(()); }
        Err(format!("expected keyword {kw}, got {:?}", self.peek()))
    }
    fn match_op(&mut self, op: &str) -> bool {
        if let Token::Op(o) = self.peek() { if o == op { self.pos += 1; return true; } }
        false
    }
    fn is_op(&self, ops: &[&str]) -> bool {
        if let Token::Op(o) = self.peek() { ops.contains(&o.as_str()) } else { false }
    }
    fn match_lp(&mut self) -> bool {
        if matches!(self.peek(), Token::LParen) { self.pos += 1; true } else { false }
    }
    fn expect_lp(&mut self) -> Result<(), String> {
        if self.match_lp() { Ok(()) } else { Err(format!("expected '(', got {:?}", self.peek())) }
    }
    fn expect_rp(&mut self) -> Result<(), String> {
        if matches!(self.peek(), Token::RParen) { self.pos += 1; Ok(()) }
        else { Err(format!("expected ')', got {:?}", self.peek())) }
    }
    fn match_comma(&mut self) -> bool {
        if matches!(self.peek(), Token::Comma) { self.pos += 1; true } else { false }
    }
    fn is_clause_boundary(&self) -> bool {
        if let Token::Keyword(k) = self.peek() {
            matches!(k.as_str(), "FROM" | "WHERE" | "GROUP" | "ORDER" | "HAVING" | "LIMIT"
                | "AND" | "OR" | "JOIN" | "LEFT" | "INNER" | "ON" | "AS" | "WHEN" | "THEN"
                | "ELSE" | "END" | "BY")
        } else { matches!(self.peek(), Token::Comma | Token::EOF | Token::RParen | Token::Semicolon) }
    }

    fn parse_ident_name(&mut self) -> Result<String, String> {
        match self.peek().clone() {
            Token::Ident(s) => { self.next(); Ok(s) }
            Token::Keyword(k) => { self.next(); Ok(k.to_lowercase()) }
            other => Err(format!("expected identifier, got {other:?}")),
        }
    }

    // --- SELECT ---

    fn parse_select(&mut self) -> Result<SelectQuery2, String> {
        self.expect_kw("SELECT")?;
        let _ = self.match_kw("DISTINCT");
        let select = self.parse_select_list()?;
        self.expect_kw("FROM")?;
        let from = self.parse_from_list()?;

        let mut joins = Vec::new();
        loop {
            if self.match_ident_or_kw("LEFT") {
                let _ = self.match_ident_or_kw("OUTER");
                self.expect_kw("JOIN")?;
                let table = self.parse_from_item()?;
                self.expect_kw("ON")?;
                let on = self.parse_expr()?;
                joins.push(JoinClause2 { join_type: JoinType2::Left, table, on });
            } else if self.match_ident_or_kw("INNER") {
                self.expect_kw("JOIN")?;
                let table = self.parse_from_item()?;
                self.expect_kw("ON")?;
                let on = self.parse_expr()?;
                joins.push(JoinClause2 { join_type: JoinType2::Inner, table, on });
            } else if self.match_kw("JOIN") {
                let table = self.parse_from_item()?;
                self.expect_kw("ON")?;
                let on = self.parse_expr()?;
                joins.push(JoinClause2 { join_type: JoinType2::Inner, table, on });
            } else { break; }
        }

        let where_clause = if self.match_kw("WHERE") { Some(self.parse_expr()?) } else { None };
        let group_by = if self.match_kw("GROUP") {
            self.expect_kw("BY")?;
            self.parse_expr_list()?
        } else { Vec::new() };
        let having = if self.match_kw("HAVING") { Some(self.parse_expr()?) } else { None };
        let order_by = if self.match_kw("ORDER") {
            self.expect_kw("BY")?;
            self.parse_order_list()?
        } else { Vec::new() };
        let limit = if self.match_ident_or_kw("LIMIT") { Some(self.parse_usize()?) } else { None };

        Ok(SelectQuery2 { select, from, joins, where_clause, group_by, having, order_by, limit })
    }

    fn parse_select_list(&mut self) -> Result<Vec<SelectItem2>, String> {
        let mut items = Vec::new();
        loop {
            let expr = self.parse_expr()?;
            let alias = if self.match_kw("AS") {
                Some(self.parse_ident_name()?)
            } else if let Token::Ident(_) = self.peek() {
                if self.is_clause_boundary() { None }
                else { Some(self.parse_ident_name()?) }
            } else { None };
            items.push(SelectItem2 { expr, alias });
            if !self.match_comma() { break; }
        }
        Ok(items)
    }

    fn parse_from_list(&mut self) -> Result<Vec<FromItem>, String> {
        let mut items = Vec::new();
        loop {
            items.push(self.parse_from_item()?);
            if !self.match_comma() { break; }
        }
        Ok(items)
    }

    fn parse_from_item(&mut self) -> Result<FromItem, String> {
        if matches!(self.peek(), Token::LParen) {
            let save = self.pos;
            self.next();
            if let Token::Keyword(k) = self.peek() {
                if k == "SELECT" {
                    let sub = self.parse_select()?;
                    self.expect_rp()?;
                    let alias = if self.match_kw("AS") { Some(self.parse_ident_name()?) }
                        else if let Token::Ident(_) = self.peek() { Some(self.parse_ident_name()?) }
                        else { None };
                    return Ok(FromItem::Derived(Box::new(sub), alias));
                }
            }
            self.pos = save;
        }
        let name = self.parse_ident_name()?;
        let alias = if self.match_kw("AS") { Some(self.parse_ident_name()?) }
            else if let Token::Ident(_) = self.peek() {
                if self.is_clause_boundary() { None }
                else { Some(self.parse_ident_name()?) }
            } else { None };
        Ok(FromItem::Table(TableRef { name, alias }))
    }

    fn parse_expr_list(&mut self) -> Result<Vec<Expr2>, String> {
        let mut items = Vec::new();
        loop {
            if let Token::Int(_) = self.peek() { self.next(); }
            else { items.push(self.parse_expr()?); }
            if !self.match_comma() { break; }
        }
        Ok(items)
    }

    fn parse_order_list(&mut self) -> Result<Vec<(Expr2, bool)>, String> {
        let mut items = Vec::new();
        loop {
            let expr = self.parse_expr()?;
            let asc = if self.match_ident_or_kw("DESC") { false }
                else { let _ = self.match_ident_or_kw("ASC"); true };
            items.push((expr, asc));
            if !self.match_comma() { break; }
        }
        Ok(items)
    }

    fn parse_usize(&mut self) -> Result<usize, String> {
        if let Token::Int(i) = self.peek() {
            if *i < 0 { return Err(format!("expected non-negative, got {i}")); }
            let u = *i as usize;
            self.next();
            return Ok(u);
        }
        Err(format!("expected integer, got {:?}", self.peek()))
    }

    // --- Expressions ---

    fn parse_expr(&mut self) -> Result<Expr2, String> { self.parse_or() }

    fn parse_or(&mut self) -> Result<Expr2, String> {
        let mut left = self.parse_and()?;
        while self.match_kw("OR") {
            let right = self.parse_and()?;
            left = Expr2::BinOp { op: BinOp2::Or, left: Box::new(left), right: Box::new(right) };
        }
        Ok(left)
    }

    fn parse_and(&mut self) -> Result<Expr2, String> {
        let mut left = self.parse_not()?;
        while self.match_kw("AND") {
            let right = self.parse_not()?;
            left = Expr2::BinOp { op: BinOp2::And, left: Box::new(left), right: Box::new(right) };
        }
        Ok(left)
    }

    fn parse_not(&mut self) -> Result<Expr2, String> {
        if self.match_kw("NOT") {
            if self.match_ident_or_kw("EXISTS") {
                self.expect_lp()?;
                let sub = self.parse_select()?;
                self.expect_rp()?;
                return Ok(Expr2::Exists { query: Box::new(sub), negated: true });
            }
            let inner = self.parse_not()?;
            return Ok(Expr2::Not(Box::new(inner)));
        }
        if self.match_ident_or_kw("EXISTS") {
            self.expect_lp()?;
            let sub = self.parse_select()?;
            self.expect_rp()?;
            return Ok(Expr2::Exists { query: Box::new(sub), negated: false });
        }
        self.parse_comparison()
    }

    fn parse_comparison(&mut self) -> Result<Expr2, String> {
        let left = self.parse_additive()?;

        if self.is_op(&["=", "!=", "<>", "<", ">", "<=", ">="]) {
            let op_str = if let Token::Op(o) = self.peek() { o.clone() } else { unreachable!() };
            self.next();
            let right = self.parse_additive()?;
            let op = match op_str.as_str() {
                "=" => BinOp2::Eq, "!=" | "<>" => BinOp2::Ne,
                "<" => BinOp2::Lt, ">" => BinOp2::Gt, "<=" => BinOp2::Le, ">=" => BinOp2::Ge,
                _ => unreachable!(),
            };
            return Ok(Expr2::BinOp { op, left: Box::new(left), right: Box::new(right) });
        }

        if self.match_ident_or_kw("LIKE") {
            let pattern = self.parse_additive()?;
            return Ok(Expr2::Like { expr: Box::new(left), pattern: Box::new(pattern), negated: false });
        }
        if self.match_kw("NOT") {
            if self.match_ident_or_kw("LIKE") {
                let pattern = self.parse_additive()?;
                return Ok(Expr2::Like { expr: Box::new(left), pattern: Box::new(pattern), negated: true });
            }
            if self.match_ident_or_kw("IN") { return self.parse_in_rest(left, true); }
            if self.match_ident_or_kw("BETWEEN") { return self.parse_between_rest(left, true); }
            self.pos -= 1;
            return Ok(left);
        }
        if self.match_ident_or_kw("IN") { return self.parse_in_rest(left, false); }
        if self.match_ident_or_kw("BETWEEN") { return self.parse_between_rest(left, false); }
        Ok(left)
    }

    fn parse_in_rest(&mut self, left: Expr2, negated: bool) -> Result<Expr2, String> {
        self.expect_lp()?;
        if let Token::Keyword(k) = self.peek() {
            if k == "SELECT" {
                let sub = self.parse_select()?;
                self.expect_rp()?;
                return Ok(Expr2::InSubquery { expr: Box::new(left), query: Box::new(sub), negated });
            }
        }
        let mut list = Vec::new();
        loop {
            list.push(self.parse_expr()?);
            if !self.match_comma() { break; }
        }
        self.expect_rp()?;
        Ok(Expr2::InList { expr: Box::new(left), list, negated })
    }

    fn parse_between_rest(&mut self, left: Expr2, negated: bool) -> Result<Expr2, String> {
        let low = self.parse_additive()?;
        self.expect_kw("AND")?;
        let high = self.parse_additive()?;
        Ok(Expr2::Between { expr: Box::new(left), low: Box::new(low), high: Box::new(high), negated })
    }

    fn parse_additive(&mut self) -> Result<Expr2, String> {
        let mut left = self.parse_multiplicative()?;
        loop {
            if self.is_op(&["+", "-"]) {
                let op_str = if let Token::Op(o) = self.peek() { o.clone() } else { unreachable!() };
                self.next();
                let right = self.parse_multiplicative()?;
                let op = if op_str == "+" { BinOp2::Add } else { BinOp2::Sub };
                left = Expr2::BinOp { op, left: Box::new(left), right: Box::new(right) };
            } else { break; }
        }
        Ok(left)
    }

    fn parse_multiplicative(&mut self) -> Result<Expr2, String> {
        let mut left = self.parse_unary()?;
        loop {
            if self.is_op(&["*", "/"]) {
                let op_str = if let Token::Op(o) = self.peek() { o.clone() } else { unreachable!() };
                self.next();
                let right = self.parse_unary()?;
                let op = if op_str == "*" { BinOp2::Mul } else { BinOp2::Div };
                left = Expr2::BinOp { op, left: Box::new(left), right: Box::new(right) };
            } else { break; }
        }
        Ok(left)
    }

    fn parse_unary(&mut self) -> Result<Expr2, String> {
        if self.match_op("-") { return Ok(Expr2::Neg(Box::new(self.parse_unary()?))); }
        if self.match_op("+") { return self.parse_unary(); }
        self.parse_primary()
    }

    fn parse_primary(&mut self) -> Result<Expr2, String> {
        match self.peek().clone() {
            Token::Int(i) => { self.next(); Ok(Expr2::Int(i)) }
            Token::Float(f) => { self.next(); Ok(Expr2::Float(f)) }
            Token::String(s) => { self.next(); Ok(Expr2::Str(s)) }
            Token::Keyword(kw) => {
                let ku = kw.to_uppercase();
                match ku.as_str() {
                    "DATE" => {
                        self.next();
                        if let Token::String(s) = self.peek().clone() {
                            self.next();
                            if let Ok(d) = crate::types::Date::from_str(&s) { return Ok(Expr2::Date(d.0)); }
                            return Ok(Expr2::Str(s));
                        }
                        Err("expected string after DATE".into())
                    }
                    "CASE" => self.parse_case(),
                    "EXTRACT" => { self.next(); self.parse_extract() },
                    "COUNT" | "SUM" | "AVG" | "MIN" | "MAX" => { self.next(); self.parse_agg_call(&ku) },
                        // Keyword as column name — check for func call
                    _ => {
                        if matches!(self.peek_at(1), Token::LParen) {
                            self.next();
                            return self.parse_agg_call(&ku);
                        }
                        self.next();
                        Ok(Expr2::Col(kw.to_lowercase()))
                    }
                }
            }
            Token::Ident(name) => {
                self.next();
                let lower = name.to_lowercase();
                if matches!(self.peek(), Token::LParen) {
                    match lower.as_str() {
                        "substr" | "substring" => return self.parse_substr(),
                        "extract" => return self.parse_extract(),
                        "exists" => {
                            self.expect_lp()?;
                            let sub = self.parse_select()?;
                            self.expect_rp()?;
                            return Ok(Expr2::Exists { query: Box::new(sub), negated: false });
                        }
                        _ => return self.parse_agg_call(&lower.to_uppercase()),
                    }
                }
                // Check for qualified name: ident . ident
                if self.match_op(".") {
                    let col = self.parse_ident_name()?;
                    return Ok(Expr2::Col(format!("{}.{}", name, col)));
                }
                Ok(Expr2::Col(name))
            }
            Token::LParen => {
                self.next();
                if let Token::Keyword(k) = self.peek() {
                    if k == "SELECT" {
                        let sub = self.parse_select()?;
                        self.expect_rp()?;
                        return Ok(Expr2::Subquery(Box::new(sub)));
                    }
                }
                let e = self.parse_expr()?;
                self.expect_rp()?;
                Ok(e)
            }
            other => Err(format!("expected expression, got {other:?}")),
        }
    }

    fn parse_case(&mut self) -> Result<Expr2, String> {
        self.expect_kw("CASE")?;
        let mut whens = Vec::new();
        while self.match_kw("WHEN") {
            let cond = self.parse_expr()?;
            self.expect_kw("THEN")?;
            let result = self.parse_expr()?;
            whens.push((cond, result));
        }
        let else_ = if self.match_kw("ELSE") { Some(Box::new(self.parse_expr()?)) } else { None };
        self.expect_kw("END")?;
        Ok(Expr2::Case { whens, else_ })
    }

    fn parse_extract(&mut self) -> Result<Expr2, String> {
        // EXTRACT keyword/ident already consumed by caller
        self.expect_lp()?;
        let field = self.parse_ident_name()?;
        self.expect_kw("FROM")?;
        let expr = self.parse_expr()?;
        self.expect_rp()?;
        Ok(Expr2::Extract { field, expr: Box::new(expr) })
    }

    fn parse_substr(&mut self) -> Result<Expr2, String> {
        // 'substr' ident already consumed
        self.expect_lp()?;
        let expr = self.parse_expr()?;
        if !self.match_comma() { return Err("expected ',' in substr".into()); }
        let start = self.parse_expr()?;
        if !self.match_comma() { return Err("expected ',' in substr".into()); }
        let len = self.parse_expr()?;
        self.expect_rp()?;
        Ok(Expr2::Substr { expr: Box::new(expr), start: Box::new(start), len: Box::new(len) })
    }

    fn parse_agg_call(&mut self, func_upper: &str) -> Result<Expr2, String> {
        // Function name keyword/ident already consumed by caller
        self.expect_lp()?;
        let distinct = self.match_kw("DISTINCT");
        let arg = if self.match_op("*") {
            Expr2::CountStar
        } else {
            self.parse_expr()?
        };
        self.expect_rp()?;
        let func = match func_upper {
            "SUM" => if distinct { AggFunc::Sum } else { AggFunc::Sum },
            "AVG" => AggFunc::Avg,
            "COUNT" => if distinct { AggFunc::CountDistinct } else { AggFunc::Count },
            "MIN" => AggFunc::Min,
            "MAX" => AggFunc::Max,
            _ => return Err(format!("unsupported aggregate: {func_upper}")),
        };
        Ok(Expr2::Agg { func, arg: Box::new(arg), distinct })
    }
}

// =========================================================================
// Interpreter
// =========================================================================

struct ExecTable {
    columns: Vec<Vec<u64>>,
    column_names: Vec<String>,
    col_types: Vec<ColType>,
    string_columns: Vec<Option<StringSearchColumn>>,
    row_count: usize,
    col_map: HashMap<String, usize>,
}

impl ExecTable {
    fn from_catalog(table: &Table, alias: &str) -> Self {
        let col_types = tpch_col_types(&table.name);
        let mut col_map = HashMap::new();
        for (i, name) in table.column_names.iter().enumerate() {
            let lower = name.to_lowercase();
            col_map.entry(name.to_lowercase()).or_insert(i);
            col_map.entry(format!("{}.{}", alias.to_lowercase(), name.to_lowercase())).or_insert(i);
            if alias != table.name {
                col_map.entry(format!("{}.{}", table.name.to_lowercase(), name.to_lowercase())).or_insert(i);
            }
        }
        ExecTable {
            columns: table.columns.clone(),
            column_names: table.column_names.clone(),
            col_types,
            string_columns: table.string_columns.clone(),
            row_count: table.row_count,
            col_map,
        }
    }

    fn lookup_col(&self, name: &str) -> Option<usize> {
        self.col_map.get(&name.to_lowercase()).copied()
    }
}

#[derive(Debug, Clone)]
enum Value2 {
    Int(i64),
    Float(f64),
    Str(String),
    Date(i32),
    Null,
}

impl Value2 {
    fn as_f64(&self) -> Option<f64> {
        match self {
            Value2::Int(i) => Some(*i as f64),
            Value2::Float(f) => Some(*f),
            Value2::Date(d) => Some(*d as f64),
            Value2::Null => None,
            Value2::Str(s) => s.parse().ok(),
        }
    }
    fn as_i64(&self) -> Option<i64> {
        match self {
            Value2::Int(i) => Some(*i),
            Value2::Float(f) => Some(*f as i64),
            Value2::Date(d) => Some(*d as i64),
            Value2::Null => None,
            Value2::Str(s) => s.parse().ok(),
        }
    }
    fn as_str(&self) -> Option<&str> {
        match self { Value2::Str(s) => Some(s), _ => None }
    }
    fn to_u64(&self) -> u64 {
        match self {
            Value2::Int(i) => *i as u64,
            Value2::Float(f) => f.to_bits(),
            Value2::Date(d) => *d as u32 as u64,
            Value2::Null => 0,
            Value2::Str(s) => xxhash_rust::xxh3::xxh3_64(s.as_bytes()),
        }
    }
}

pub fn execute_tpch(query: &SelectQuery2, catalog: &Catalog) -> Result<QueryResult, Error> {
    TpchExec { catalog }.execute(query)
}

struct TpchExec<'a> { catalog: &'a Catalog }

impl<'a> TpchExec<'a> {
    fn execute(&self, query: &SelectQuery2) -> Result<QueryResult, Error> {
        // 1. Load all FROM tables
        let mut tables: Vec<ExecTable> = Vec::new();
        for item in &query.from {
            tables.push(self.resolve_from_item(item)?);
        }

        // 2. Handle explicit JOINs on the first table
        for join in &query.joins {
            let right = self.resolve_from_item(&join.table)?;
            let left = tables.pop().unwrap();
            tables.push(self.hash_join(left, right, &join.on, join.join_type)?);
        }

        // 3. Build base table — use hash joins for implicit multi-table joins
        let base = if tables.len() == 1 {
            tables.into_iter().next().unwrap()
        } else {
            self.join_tables_smart(tables, &query.where_clause)?
        };

        // 4. Apply WHERE filter
        let mask = if let Some(ref wc) = query.where_clause {
            self.build_mask(wc, &base)?
        } else { vec![true; base.row_count] };

        // 5. GROUP BY + aggregates
        if !query.group_by.is_empty() || self.has_agg(&query.select) {
            return self.execute_grouped(query, &base, &mask);
        }

        // 6. Non-grouped: filter, project, order, limit
        let indices: Vec<usize> = (0..base.row_count).filter(|&i| mask[i]).collect();
        let result = self.project(&query.select, &base, &indices)?;
        let mut result = if !query.order_by.is_empty() {
            self.apply_order_by(result, &query.order_by, &base, &indices)?
        } else { result };

        if let Some(limit) = query.limit {
            if result.row_count > limit {
                for col in &mut result.columns { col.values.truncate(limit); }
                result.row_count = limit;
            }
        }
        Ok(result)
    }

    /// Smart join: extract equi-join predicates from WHERE, apply single-table
    /// filters first, then hash-join tables left-to-right.
    fn join_tables_smart(&self, tables: Vec<ExecTable>, where_clause: &Option<Expr2>) -> Result<ExecTable, Error> {
        let conjuncts = self.split_conjuncts(where_clause);
        let mut tables = tables;

        // Apply single-table filters to reduce row counts
        for i in 0..tables.len() {
            for conj in &conjuncts {
                let referenced = self.expr_table_refs(conj, &tables);
                if referenced.len() == 1 && referenced.contains(&i) {
                    let mask = self.build_mask(conj, &tables[i])?;
                    let indices: Vec<usize> = (0..tables[i].row_count).filter(|&r| mask[r]).collect();
                    tables[i] = self.filter_table(&tables[i], &indices);
                }
            }
        }

        // Join tables left-to-right using equi-join predicates
        let mut joined = tables.remove(0);
        while !tables.is_empty() {
            let mut best_idx = 0;
            let mut best_keys: Vec<JoinKey2> = Vec::new();
            for (i, table) in tables.iter().enumerate() {
                let keys = self.find_join_keys(&joined, table, &conjuncts);
                if !keys.is_empty() && (best_keys.is_empty() || table.row_count < tables[best_idx].row_count) {
                    best_idx = i;
                    best_keys = keys;
                }
            }
            let right = tables.remove(best_idx);
            if best_keys.is_empty() {
                // No equi-join found — fall back to cross join (rare, may be slow)
                joined = self.cross_join(joined, right);
            } else {
                // Build a dummy ON expression for hash_join
                let on = Expr2::BinOp {
                    op: BinOp2::Eq,
                    left: Box::new(Expr2::Col(String::new())),
                    right: Box::new(Expr2::Col(String::new())),
                };
                joined = self.hash_join_with_keys(joined, right, &best_keys, JoinType2::Inner)?;
            }
        }
        Ok(joined)
    }

    fn hash_join_with_keys(&self, left: ExecTable, right: ExecTable, keys: &[JoinKey2], jt: JoinType2) -> Result<ExecTable, Error> {
        let mut build: HashMap<Vec<u64>, Vec<usize>> = HashMap::new();
        for r in 0..right.row_count {
            let key: Vec<u64> = keys.iter().map(|k| right.columns[k.right][r]).collect();
            build.entry(key).or_default().push(r);
        }
        let ncol = left.columns.len() + right.columns.len();
        let mut out_cols: Vec<Vec<u64>> = (0..ncol).map(|_| Vec::new()).collect();
        let mut out_types = left.col_types.clone();
        out_types.extend(right.col_types.iter().copied());
        let mut out_strings = left.string_columns.clone();
        out_strings.extend(right.string_columns.iter().cloned());
        let mut out_names = left.column_names.clone();
        out_names.extend(right.column_names.clone());
        let mut row_count = 0;
        for l in 0..left.row_count {
            let key: Vec<u64> = keys.iter().map(|k| left.columns[k.left][l]).collect();
            let matches = build.get(&key).cloned().unwrap_or_default();
            if matches.is_empty() {
                if jt == JoinType2::Left {
                    for (c, col) in left.columns.iter().enumerate() { out_cols[c].push(col[l]); }
                    for c in 0..right.columns.len() { out_cols[left.columns.len() + c].push(0); }
                    row_count += 1;
                }
            } else {
                for r in &matches {
                    for (c, col) in left.columns.iter().enumerate() { out_cols[c].push(col[l]); }
                    for (c, col) in right.columns.iter().enumerate() { out_cols[left.columns.len() + c].push(col[*r]); }
                    row_count += 1;
                }
            }
        }
        let mut col_map = HashMap::new();
        for (i, name) in out_names.iter().enumerate() { col_map.entry(name.to_lowercase()).or_insert(i); }
        for (k, v) in &left.col_map { col_map.insert(k.clone(), *v); }
        let off = left.columns.len();
        for (k, v) in &right.col_map { col_map.insert(k.clone(), *v + off); }
        Ok(ExecTable { columns: out_cols, column_names: out_names, col_types: out_types, string_columns: out_strings, row_count, col_map })
    }

    fn filter_table(&self, table: &ExecTable, indices: &[usize]) -> ExecTable {
        let mut new_cols = Vec::with_capacity(table.columns.len());
        for col in &table.columns {
            new_cols.push(indices.iter().map(|&i| col[i]).collect());
        }
        // Rebuild string columns if present
        let mut new_strings = Vec::with_capacity(table.string_columns.len());
        for (i, sc) in table.string_columns.iter().enumerate() {
            if let Some(ref scol) = sc {
                let strings: Vec<String> = indices.iter().map(|&r| scol.get(r).to_string()).collect();
                new_strings.push(Some(StringSearchColumn::new(strings)));
            } else {
                new_strings.push(None);
            }
        }
        ExecTable {
            columns: new_cols,
            column_names: table.column_names.clone(),
            col_types: table.col_types.clone(),
            string_columns: new_strings,
            row_count: indices.len(),
            col_map: table.col_map.clone(),
        }
    }

    /// Split a WHERE clause into AND-conjuncts.
    fn split_conjuncts(&self, where_clause: &Option<Expr2>) -> Vec<Expr2> {
        match where_clause {
            None => Vec::new(),
            Some(expr) => {
                let mut result = Vec::new();
                self.collect_conjuncts(expr, &mut result);
                result
            }
        }
    }

    fn collect_conjuncts(&self, expr: &Expr2, out: &mut Vec<Expr2>) {
        match expr {
            Expr2::BinOp { op: BinOp2::And, left, right } => {
                self.collect_conjuncts(left, out);
                self.collect_conjuncts(right, out);
            }
            _ => out.push(expr.clone()),
        }
    }

    /// Find which tables an expression references.
    fn expr_table_refs(&self, expr: &Expr2, tables: &[ExecTable]) -> HashSet<usize> {
        let mut refs = HashSet::new();
        self.collect_table_refs(expr, tables, &mut refs);
        refs
    }

    fn collect_table_refs(&self, expr: &Expr2, tables: &[ExecTable], refs: &mut HashSet<usize>) {
        match expr {
            Expr2::Col(name) => {
                for (i, t) in tables.iter().enumerate() {
                    if t.lookup_col(name).is_some() {
                        refs.insert(i);
                    }
                }
            }
            Expr2::BinOp { left, right, .. } => {
                self.collect_table_refs(left, tables, refs);
                self.collect_table_refs(right, tables, refs);
            }
            Expr2::Like { expr, pattern, .. } => {
                self.collect_table_refs(expr, tables, refs);
                self.collect_table_refs(pattern, tables, refs);
            }
            Expr2::Between { expr, low, high, .. } => {
                self.collect_table_refs(expr, tables, refs);
                self.collect_table_refs(low, tables, refs);
                self.collect_table_refs(high, tables, refs);
            }
            Expr2::InList { expr, list, .. } => {
                self.collect_table_refs(expr, tables, refs);
                for item in list { self.collect_table_refs(item, tables, refs); }
            }
            Expr2::Case { whens, else_ } => {
                for (c, r) in whens {
                    self.collect_table_refs(c, tables, refs);
                    self.collect_table_refs(r, tables, refs);
                }
                if let Some(e) = else_ { self.collect_table_refs(e, tables, refs); }
            }
            Expr2::Extract { expr, .. } | Expr2::Neg(expr) | Expr2::Not(expr) => {
                self.collect_table_refs(expr, tables, refs);
            }
            Expr2::Substr { expr, start, len } => {
                self.collect_table_refs(expr, tables, refs);
                self.collect_table_refs(start, tables, refs);
                self.collect_table_refs(len, tables, refs);
            }
            _ => {}
        }
    }

    /// Find equi-join keys between two tables from a list of conjuncts.
    fn find_join_keys(&self, left: &ExecTable, right: &ExecTable, conjuncts: &[Expr2]) -> Vec<JoinKey2> {
        let mut keys = Vec::new();
        for conj in conjuncts {
            if let Expr2::BinOp { op: BinOp2::Eq, left: l, right: r } = conj {
                if let (Some(lk), Some(rk)) = (self.col_in(l, left), self.col_in(r, right)) {
                    keys.push(JoinKey2 { left: lk, right: rk });
                } else if let (Some(rk), Some(lk)) = (self.col_in(l, right), self.col_in(r, left)) {
                    keys.push(JoinKey2 { left: lk, right: rk });
                }
            }
        }
        keys
    }

    fn has_agg(&self, select: &[SelectItem2]) -> bool {
        select.iter().any(|s| self.expr_has_agg(&s.expr))
    }
    fn expr_has_agg(&self, e: &Expr2) -> bool {
        match e {
            Expr2::Agg { .. } | Expr2::CountStar => true,
            Expr2::BinOp { left, right, .. } => self.expr_has_agg(left) || self.expr_has_agg(right),
            Expr2::Case { whens, else_ } => whens.iter().any(|(c, r)| self.expr_has_agg(c) || self.expr_has_agg(r))
                || else_.as_ref().map(|e| self.expr_has_agg(e)).unwrap_or(false),
            Expr2::Like { expr, pattern, .. } => self.expr_has_agg(expr) || self.expr_has_agg(pattern),
            Expr2::Between { expr, low, high, .. } => self.expr_has_agg(expr) || self.expr_has_agg(low) || self.expr_has_agg(high),
            Expr2::InList { expr, list, .. } => self.expr_has_agg(expr) || list.iter().any(|e| self.expr_has_agg(e)),
            Expr2::Neg(e) | Expr2::Not(e) | Expr2::Extract { expr: e, .. } => self.expr_has_agg(e),
            Expr2::Substr { expr, start, len } => self.expr_has_agg(expr) || self.expr_has_agg(start) || self.expr_has_agg(len),
            _ => false,
        }
    }

    fn resolve_from(&self, from: &[FromItem]) -> Result<ExecTable, Error> {
        if from.is_empty() { return Err(Error::Other("no FROM clause".into())); }
        let mut base = self.resolve_from_item(&from[0])?;
        for item in &from[1..] {
            let right = self.resolve_from_item(item)?;
            base = self.cross_join(base, right);
        }
        Ok(base)
    }

    fn resolve_from_item(&self, item: &FromItem) -> Result<ExecTable, Error> {
        match item {
            FromItem::Table(t) => {
                let table = self.catalog.get(&t.name)
                    .ok_or_else(|| Error::NotFound(format!("table '{}'", t.name)))?;
                let alias = t.alias.as_deref().unwrap_or(&t.name);
                Ok(ExecTable::from_catalog(table, alias))
            }
            FromItem::Derived(subquery, alias) => {
                let result = self.execute(subquery)?;
                self.result_to_exec_table(&result, alias.as_deref().unwrap_or("derived"))
            }
        }
    }

    fn result_to_exec_table(&self, result: &QueryResult, alias: &str) -> Result<ExecTable, Error> {
        let mut col_map = HashMap::new();
        let mut column_names = Vec::new();
        let mut columns = Vec::new();
        let mut col_types = Vec::new();
        let mut string_columns = Vec::new();
        for (i, col) in result.columns.iter().enumerate() {
            column_names.push(col.name.clone());
            columns.push(col.values.clone());
            col_types.push(self.infer_result_type(&col.name));
            string_columns.push(None);
            let lower = col.name.to_lowercase();
            col_map.entry(col.name.to_lowercase()).or_insert(i);
            col_map.entry(format!("{}.{}", alias.to_lowercase(), col.name.to_lowercase())).or_insert(i);
        }
        Ok(ExecTable { columns, column_names, col_types, string_columns, row_count: result.row_count, col_map })
    }

    fn infer_result_type(&self, name: &str) -> ColType {
        let l = name.to_lowercase();
        if l.contains("year") || l.contains("count") || l.contains("custdist") || l.contains("cntrycode")
            || l.contains("order") || l.contains("partkey") || l.contains("suppkey") || l.contains("custkey")
            || l.contains("size") || l.contains("numwait") || l.contains("numcust") || l.contains("supplier_cnt")
        { ColType::Int } else { ColType::Float }
    }

    // --- Cross join ---

    fn cross_join(&self, left: ExecTable, right: ExecTable) -> ExecTable {
        let lr = left.row_count;
        let rr = right.row_count;
        if lr == 0 || rr == 0 {
            return ExecTable {
                columns: left.columns.iter().chain(right.columns.iter()).map(|_| Vec::new()).collect(),
                column_names: left.column_names.iter().chain(right.column_names.iter()).cloned().collect(),
                col_types: left.col_types.iter().chain(right.col_types.iter()).copied().collect(),
                string_columns: left.string_columns.iter().chain(right.string_columns.iter()).cloned().collect(),
                row_count: 0,
                col_map: HashMap::new(),
            };
        }
        let total = lr * rr;
        let mut columns = Vec::with_capacity(left.columns.len() + right.columns.len());
        for col in &left.columns {
            let mut nc = Vec::with_capacity(total);
            for l in 0..lr { let v = col[l]; for _ in 0..rr { nc.push(v); } }
            columns.push(nc);
        }
        for col in &right.columns {
            let mut nc = Vec::with_capacity(total);
            for _ in 0..lr { for r in 0..rr { nc.push(col[r]); } }
            columns.push(nc);
        }
        let mut col_types = left.col_types.clone();
        col_types.extend(right.col_types.iter().copied());
        let mut string_columns = left.string_columns.clone();
        string_columns.extend(right.string_columns.iter().cloned());
        let mut column_names = left.column_names.clone();
        column_names.extend(right.column_names.clone());
        let mut col_map = HashMap::new();
        for (i, name) in column_names.iter().enumerate() {
            col_map.entry(name.to_lowercase()).or_insert(i);
        }
        for (k, v) in &left.col_map { col_map.insert(k.clone(), *v); }
        let off = left.columns.len();
        for (k, v) in &right.col_map { col_map.insert(k.clone(), *v + off); }
        ExecTable { columns, column_names, col_types, string_columns, row_count: total, col_map }
    }

    // --- Hash join ---

    fn hash_join(&self, left: ExecTable, right: ExecTable, on: &Expr2, jt: JoinType2) -> Result<ExecTable, Error> {
        let keys = self.extract_join_keys(on, &left, &right)?;
        if keys.is_empty() { return Ok(self.cross_join(left, right)); }

        let mut build: HashMap<Vec<u64>, Vec<usize>> = HashMap::new();
        for r in 0..right.row_count {
            let key: Vec<u64> = keys.iter().map(|k| right.columns[k.right][r]).collect();
            build.entry(key).or_default().push(r);
        }

        let ncol = left.columns.len() + right.columns.len();
        let mut out_cols: Vec<Vec<u64>> = (0..ncol).map(|_| Vec::new()).collect();
        let mut out_types = left.col_types.clone();
        out_types.extend(right.col_types.iter().copied());
        let mut out_strings = left.string_columns.clone();
        out_strings.extend(right.string_columns.iter().cloned());
        let mut out_names = left.column_names.clone();
        out_names.extend(right.column_names.clone());
        let mut row_count = 0;

        for l in 0..left.row_count {
            let key: Vec<u64> = keys.iter().map(|k| left.columns[k.left][l]).collect();
            let matches = build.get(&key).cloned().unwrap_or_default();
            if matches.is_empty() {
                if jt == JoinType2::Left {
                    for (c, col) in left.columns.iter().enumerate() { out_cols[c].push(col[l]); }
                    for c in 0..right.columns.len() { out_cols[left.columns.len() + c].push(0); }
                    row_count += 1;
                }
            } else {
                for r in &matches {
                    for (c, col) in left.columns.iter().enumerate() { out_cols[c].push(col[l]); }
                    for (c, col) in right.columns.iter().enumerate() { out_cols[left.columns.len() + c].push(col[*r]); }
                    row_count += 1;
                }
            }
        }

        let mut col_map = HashMap::new();
        for (i, name) in out_names.iter().enumerate() { col_map.entry(name.to_lowercase()).or_insert(i); }
        for (k, v) in &left.col_map { col_map.insert(k.clone(), *v); }
        let off = left.columns.len();
        for (k, v) in &right.col_map { col_map.insert(k.clone(), *v + off); }

        Ok(ExecTable { columns: out_cols, column_names: out_names, col_types: out_types, string_columns: out_strings, row_count, col_map })
    }

    fn extract_join_keys(&self, on: &Expr2, left: &ExecTable, right: &ExecTable) -> Result<Vec<JoinKey2>, Error> {
        let mut keys = Vec::new();
        self.collect_keys(on, left, right, &mut keys);
        Ok(keys)
    }

    fn collect_keys(&self, on: &Expr2, left: &ExecTable, right: &ExecTable, keys: &mut Vec<JoinKey2>) {
        match on {
            Expr2::BinOp { op: BinOp2::And, left: l, right: r } => {
                self.collect_keys(l, left, right, keys);
                self.collect_keys(r, left, right, keys);
            }
            Expr2::BinOp { op: BinOp2::Eq, left: l, right: r } => {
                if let (Some(lk), Some(rk)) = (self.col_in(l, left), self.col_in(r, right)) {
                    keys.push(JoinKey2 { left: lk, right: rk });
                } else if let (Some(rk), Some(lk)) = (self.col_in(l, right), self.col_in(r, left)) {
                    keys.push(JoinKey2 { left: lk, right: rk });
                }
            }
            _ => {}
        }
    }

    fn col_in(&self, expr: &Expr2, table: &ExecTable) -> Option<usize> {
        if let Expr2::Col(name) = expr { table.lookup_col(name) } else { None }
    }

    // --- WHERE ---

    fn build_mask(&self, expr: &Expr2, table: &ExecTable) -> Result<Vec<bool>, Error> {
        let mut mask = vec![true; table.row_count];
        self.eval_bool_mask(expr, table, &mut mask)?;
        Ok(mask)
    }

    fn eval_bool_mask(&self, expr: &Expr2, table: &ExecTable, mask: &mut [bool]) -> Result<(), Error> {
        match expr {
            Expr2::BinOp { op: BinOp2::And, left, right } => {
                self.eval_bool_mask(left, table, mask)?;
                let mut rm = vec![true; table.row_count];
                self.eval_bool_mask(right, table, &mut rm)?;
                for i in 0..table.row_count { mask[i] = mask[i] && rm[i]; }
                Ok(())
            }
            Expr2::BinOp { op: BinOp2::Or, left, right } => {
                let mut lm = vec![true; table.row_count];
                self.eval_bool_mask(left, table, &mut lm)?;
                let mut rm = vec![true; table.row_count];
                self.eval_bool_mask(right, table, &mut rm)?;
                for i in 0..table.row_count { mask[i] = lm[i] || rm[i]; }
                Ok(())
            }
            _ => {
                for i in 0..table.row_count {
                    let v = self.eval(expr, table, i)?;
                    mask[i] = mask[i] && self.truthy(&v);
                }
                Ok(())
            }
        }
    }

    fn truthy(&self, v: &Value2) -> bool {
        match v { Value2::Int(i) => *i != 0, Value2::Float(f) => *f != 0.0, Value2::Null => false, _ => false }
    }

    // --- Expression evaluation ---

    fn eval(&self, expr: &Expr2, t: &ExecTable, row: usize) -> Result<Value2, Error> {
        match expr {
            Expr2::Col(name) => {
                let idx = t.lookup_col(name).ok_or_else(|| Error::NotFound(format!("column '{}'", name)))?;
                let cell = t.columns[idx].get(row).copied().unwrap_or(0);
                Ok(match t.col_types[idx] {
                    ColType::Int => Value2::Int(cell as i64),
                    ColType::Float => Value2::Float(f64::from_bits(cell)),
                    ColType::Date => Value2::Date(cell as u32 as i32),
                    ColType::String => {
                        if let Some(ref sc) = t.string_columns[idx] {
                            Value2::Str(sc.get(row).to_string())
                        } else {
                            Value2::Int(cell as i64) // hash value after join
                        }
                    }
                })
            }
            Expr2::Int(i) => Ok(Value2::Int(*i)),
            Expr2::Float(f) => Ok(Value2::Float(*f)),
            Expr2::Str(s) => Ok(Value2::Str(s.clone())),
            Expr2::Date(d) => Ok(Value2::Date(*d)),
            Expr2::Neg(e) => {
                let v = self.eval(e, t, row)?;
                match v { Value2::Int(i) => Ok(Value2::Int(-i)), Value2::Float(f) => Ok(Value2::Float(-f)), _ => Ok(Value2::Null) }
            }
            Expr2::Not(e) => {
                let v = self.eval(e, t, row)?;
                Ok(Value2::Int(if !self.truthy(&v) { 1 } else { 0 }))
            }
            Expr2::BinOp { op, left, right } => {
                let lv = self.eval(left, t, row)?;
                let rv = self.eval(right, t, row)?;
                Ok(self.binop(*op, &lv, &rv))
            }
            Expr2::Like { expr, pattern, negated } => {
                let ev = self.eval(expr, t, row)?;
                let pv = self.eval(pattern, t, row)?;
                let r = match (&ev, &pv) {
                    (Value2::Str(s), Value2::Str(p)) => self.like(s, p),
                    (Value2::Int(h), Value2::Str(p)) => {
                        // Hashed string vs literal — can't do LIKE on hash.
                        // Fallback: exact match if no wildcards.
                        if !p.contains('%') && !p.contains('_') {
                            *h as u64 == xxhash_rust::xxh3::xxh3_64(p.as_bytes())
                        } else { false }
                    }
                    _ => false,
                };
                Ok(Value2::Int(if if *negated { !r } else { r } { 1 } else { 0 }))
            }
            Expr2::Between { expr, low, high, negated } => {
                let v = self.eval(expr, t, row)?;
                let lo = self.eval(low, t, row)?;
                let hi = self.eval(high, t, row)?;
                let in_range = self.cmp_le(&lo, &v) && self.cmp_le(&v, &hi);
                Ok(Value2::Int(if if *negated { !in_range } else { in_range } { 1 } else { 0 }))
            }
            Expr2::InList { expr, list, negated } => {
                let v = self.eval(expr, t, row)?;
                let mut found = false;
                for item in list {
                    let iv = self.eval(item, t, row)?;
                    if self.cmp_eq(&v, &iv) { found = true; break; }
                }
                Ok(Value2::Int(if if *negated { !found } else { found } { 1 } else { 0 }))
            }
            Expr2::Case { whens, else_ } => {
                for (cond, result) in whens {
                    let cv = self.eval(cond, t, row)?;
                    if self.truthy(&cv) { return self.eval(result, t, row); }
                }
                if let Some(e) = else_ { return self.eval(e, t, row); }
                Ok(Value2::Null)
            }
            Expr2::Extract { field, expr } => {
                let v = self.eval(expr, t, row)?;
                Ok(self.extract(field, &v))
            }
            Expr2::Substr { expr, start, len } => {
                let sv = self.eval(expr, t, row)?;
                let st = self.eval(start, t, row)?;
                let ln = self.eval(len, t, row)?;
                Ok(self.substr(&sv, &st, &ln))
            }
            Expr2::Subquery(q) => {
                let r = self.execute(q)?;
                let val = r.columns.first().and_then(|c| c.values.first()).copied().unwrap_or(0);
                let name = r.columns.first().map(|c| c.name.as_str()).unwrap_or("");
                Ok(match self.infer_result_type(name) {
                    ColType::Float => Value2::Float(f64::from_bits(val)),
                    _ => Value2::Int(val as i64),
                })
            }
            Expr2::Exists { query, negated } => {
                let r = self.execute(query)?;
                let ex = r.row_count > 0;
                Ok(Value2::Int(if if *negated { !ex } else { ex } { 1 } else { 0 }))
            }
            Expr2::InSubquery { expr, query, negated } => {
                let v = self.eval(expr, t, row)?;
                let r = self.execute(query)?;
                let mut found = false;
                if let Some(col) = r.columns.first() {
                    for &cell in &col.values {
                        let iv = Value2::Int(cell as i64);
                        if self.cmp_eq(&v, &iv) { found = true; break; }
                    }
                }
                Ok(Value2::Int(if if *negated { !found } else { found } { 1 } else { 0 }))
            }
            Expr2::Agg { .. } | Expr2::CountStar => Err(Error::Other("aggregate in non-agg context".into())),
        }
    }

    fn binop(&self, op: BinOp2, lv: &Value2, rv: &Value2) -> Value2 {
        match op {
            BinOp2::Add | BinOp2::Sub | BinOp2::Mul | BinOp2::Div => {
                let lf = lv.as_f64();
                let rf = rv.as_f64();
                match (lf, rf) {
                    (Some(l), Some(r)) => {
                        let res = match op {
                            BinOp2::Add => l + r, BinOp2::Sub => l - r, BinOp2::Mul => l * r,
                            BinOp2::Div => { if r == 0.0 { return Value2::Null; } l / r },
                            _ => unreachable!(),
                        };
                        // Keep as int if both are ints and op is not div
                        if matches!(lv, Value2::Int(_)) && matches!(rv, Value2::Int(_)) && op != BinOp2::Div {
                            let li = lv.as_i64().unwrap();
                            let ri = rv.as_i64().unwrap();
                            let ir = match op {
                                BinOp2::Add => li.wrapping_add(ri),
                                BinOp2::Sub => li.wrapping_sub(ri),
                                BinOp2::Mul => li.wrapping_mul(ri),
                                _ => unreachable!(),
                            };
                            return Value2::Int(ir);
                        }
                        Value2::Float(res)
                    }
                    _ => Value2::Null,
                }
            }
            BinOp2::Eq => Value2::Int(if self.cmp_eq(lv, rv) { 1 } else { 0 }),
            BinOp2::Ne => Value2::Int(if !self.cmp_eq(lv, rv) { 1 } else { 0 }),
            BinOp2::Lt => Value2::Int(if self.cmp_lt(lv, rv) { 1 } else { 0 }),
            BinOp2::Gt => Value2::Int(if self.cmp_lt(rv, lv) { 1 } else { 0 }),
            BinOp2::Le => Value2::Int(if self.cmp_le(lv, rv) { 1 } else { 0 }),
            BinOp2::Ge => Value2::Int(if self.cmp_le(rv, lv) { 1 } else { 0 }),
            BinOp2::And => Value2::Int(if self.truthy(lv) && self.truthy(rv) { 1 } else { 0 }),
            BinOp2::Or => Value2::Int(if self.truthy(lv) || self.truthy(rv) { 1 } else { 0 }),
        }
    }

    fn cmp_eq(&self, a: &Value2, b: &Value2) -> bool {
        match (a, b) {
            (Value2::Null, _) | (_, Value2::Null) => false,
            (Value2::Str(x), Value2::Str(y)) => x == y,
            (Value2::Int(i), Value2::Str(s)) => *i as u64 == xxhash_rust::xxh3::xxh3_64(s.as_bytes()),
            (Value2::Str(s), Value2::Int(i)) => xxhash_rust::xxh3::xxh3_64(s.as_bytes()) == *i as u64,
            _ => {
                let af = a.as_f64();
                let bf = b.as_f64();
                match (af, bf) { (Some(x), Some(y)) => x == y, _ => false }
            }
        }
    }
    fn cmp_lt(&self, a: &Value2, b: &Value2) -> bool {
        match (a, b) {
            (Value2::Null, _) | (_, Value2::Null) => false,
            (Value2::Str(x), Value2::Str(y)) => x < y,
            _ => {
                let af = a.as_f64();
                let bf = b.as_f64();
                match (af, bf) { (Some(x), Some(y)) => x < y, _ => false }
            }
        }
    }
    fn cmp_le(&self, a: &Value2, b: &Value2) -> bool { self.cmp_lt(a, b) || self.cmp_eq(a, b) }

    fn like(&self, s: &str, pattern: &str) -> bool {
        let sb = s.as_bytes();
        let pb = pattern.as_bytes();
        let mut si = 0; let mut pi = 0;
        let mut star_s = usize::MAX; let mut star_p = usize::MAX;
        while si < sb.len() {
            if pi < pb.len() && (pb[pi] == b'_' || pb[pi] == sb[si]) { si += 1; pi += 1; }
            else if pi < pb.len() && pb[pi] == b'%' { star_p = pi; star_s = si; pi += 1; }
            else if star_p != usize::MAX { pi = star_p + 1; star_s += 1; si = star_s; }
            else { return false; }
        }
        while pi < pb.len() && pb[pi] == b'%' { pi += 1; }
        pi == pb.len()
    }

    fn extract(&self, field: &str, v: &Value2) -> Value2 {
        let days = match v {
            Value2::Date(d) => *d, Value2::Int(i) => *i as i32,
            Value2::Float(f) => *f as i32, _ => return Value2::Null,
        };
        let date = crate::types::Date::from_u64(days as u64);
        let (y, m, d) = date.to_ymd();
        let r = match field.to_lowercase().as_str() {
            "year" => y as i64, "month" => m as i64, "day" => d as i64, _ => y as i64,
        };
        Value2::Int(r)
    }

    fn substr(&self, s: &Value2, start: &Value2, len: &Value2) -> Value2 {
        let s = match s.as_str() { Some(s) => s, None => return Value2::Null };
        let st = start.as_i64().unwrap_or(1).max(1) as usize;
        let ln = len.as_i64().unwrap_or(0) as usize;
        let si = st.saturating_sub(1);
        if si >= s.len() { return Value2::Str(String::new()); }
        let ei = (si + ln).min(s.len());
        Value2::Str(s[si..ei].to_string())
    }

    // --- GROUP BY + aggregates ---

    fn execute_grouped(&self, query: &SelectQuery2, t: &ExecTable, mask: &[bool]) -> Result<QueryResult, Error> {
        let indices: Vec<usize> = (0..t.row_count).filter(|&i| mask[i]).collect();

        if query.group_by.is_empty() {
            return self.execute_scalar_agg(query, t, &indices);
        }

        // Group rows
        let mut groups: HashMap<Vec<u64>, Vec<usize>> = HashMap::new();
        for &idx in &indices {
            let mut key = Vec::with_capacity(query.group_by.len());
            for gb in &query.group_by {
                let v = self.eval(gb, t, idx)?;
                key.push(match &v {
                    Value2::Int(i) => *i as u64,
                    Value2::Float(f) => f.to_bits(),
                    Value2::Date(d) => *d as u32 as u64,
                    Value2::Null => u64::MAX,
                    Value2::Str(s) => xxhash_rust::xxh3::xxh3_64(s.as_bytes()),
                });
            }
            groups.entry(key).or_default().push(idx);
        }

        let group_indices: Vec<&Vec<usize>> = groups.values().collect();

        // HAVING
        let filtered: Vec<usize> = if let Some(ref having) = query.having {
            let mut v = Vec::new();
            for (gi, gidxs) in group_indices.iter().enumerate() {
                let hv = self.eval_agg_expr(having, t, gidxs)?;
                if self.truthy(&hv) { v.push(gi); }
            }
            v
        } else { (0..group_indices.len()).collect() };

        // Build result
        let mut cols = Vec::new();
        for item in &query.select {
            let name = item.alias.clone().unwrap_or_else(|| self.expr_name(&item.expr));
            let values: Vec<u64> = filtered.iter().map(|&gi| {
                let gidxs = group_indices[gi];
                self.eval_agg_expr(&item.expr, t, gidxs).unwrap_or(Value2::Null).to_u64()
            }).collect();
            cols.push(ResultColumn { name, values });
        }

        let mut result = QueryResult { columns: cols, row_count: filtered.len(), elapsed_us: 0 };

        if !query.order_by.is_empty() {
            result = self.apply_order_by_grouped(result, &query.order_by)?;
        }
        if let Some(limit) = query.limit {
            if result.row_count > limit {
                for col in &mut result.columns { col.values.truncate(limit); }
                result.row_count = limit;
            }
        }
        Ok(result)
    }

    fn execute_scalar_agg(&self, query: &SelectQuery2, t: &ExecTable, indices: &[usize]) -> Result<QueryResult, Error> {
        let mut cols = Vec::new();
        for item in &query.select {
            let name = item.alias.clone().unwrap_or_else(|| self.expr_name(&item.expr));
            let v = self.eval_agg_expr(&item.expr, t, indices)?;
            cols.push(ResultColumn { name, values: vec![v.to_u64()] });
        }
        Ok(QueryResult { columns: cols, row_count: 1, elapsed_us: 0 })
    }

    fn eval_agg_expr(&self, expr: &Expr2, t: &ExecTable, indices: &[usize]) -> Result<Value2, Error> {
        match expr {
            Expr2::CountStar => Ok(Value2::Int(indices.len() as i64)),
            Expr2::Agg { func, arg, distinct } => {
                let mut values: Vec<Value2> = Vec::with_capacity(indices.len());
                for &idx in indices { values.push(self.eval(arg, t, idx)?); }

                if *distinct {
                    let mut seen = HashSet::new();
                    values.retain(|v| {
                        let key = match v {
                            Value2::Int(i) => format!("i{}", i),
                            Value2::Float(f) => format!("f{}", f.to_bits()),
                            Value2::Str(s) => format!("s{}", s),
                            _ => "null".to_string(),
                        };
                        seen.insert(key)
                    });
                }

                Ok(match func {
                    AggFunc::Count => Value2::Int(values.len() as i64),
                    AggFunc::CountDistinct => {
                        let mut seen = HashSet::new();
                        for v in &values {
                            let key = match v {
                                Value2::Int(i) => format!("i{}", i),
                                Value2::Float(f) => format!("f{}", f.to_bits()),
                                Value2::Str(s) => format!("s{}", s),
                                _ => "null".to_string(),
                            };
                            seen.insert(key);
                        }
                        Value2::Int(seen.len() as i64)
                    }
                    AggFunc::Sum => {
                        let mut sum = 0.0f64;
                        let mut all_int = true;
                        for v in &values {
                            if !matches!(v, Value2::Int(_)) { all_int = false; }
                            if let Some(f) = v.as_f64() { sum += f; }
                        }
                        if all_int {
                            let mut isum = 0i64;
                            for v in &values { if let Some(i) = v.as_i64() { isum = isum.wrapping_add(i); } }
                            Value2::Int(isum)
                        } else { Value2::Float(sum) }
                    }
                    AggFunc::Avg => {
                        let mut sum = 0.0f64; let mut cnt = 0u64;
                        for v in &values { if let Some(f) = v.as_f64() { sum += f; cnt += 1; } }
                        if cnt == 0 { Value2::Null } else { Value2::Float(sum / cnt as f64) }
                    }
                    AggFunc::Min => {
                        let mut min: Option<f64> = None;
                        for v in &values { if let Some(f) = v.as_f64() { min = Some(min.map_or(f, |m| m.min(f))); } }
                        min.map(Value2::Float).unwrap_or(Value2::Null)
                    }
                    AggFunc::Max => {
                        let mut max: Option<f64> = None;
                        for v in &values { if let Some(f) = v.as_f64() { max = Some(max.map_or(f, |m| m.max(f))); } }
                        max.map(Value2::Float).unwrap_or(Value2::Null)
                    }
                })
            }
            // Non-agg expr in grouped context — eval on first row of group
            Expr2::BinOp { op, left, right } => {
                // If either side is an aggregate, evaluate recursively
                if self.expr_has_agg(left) || self.expr_has_agg(right) {
                    let lv = self.eval_agg_expr(left, t, indices)?;
                    let rv = self.eval_agg_expr(right, t, indices)?;
                    Ok(self.binop(*op, &lv, &rv))
                } else if indices.is_empty() {
                    Ok(Value2::Null)
                } else {
                    self.eval(expr, t, indices[0])
                }
            }
            Expr2::Case { whens, else_ } => {
                if whens.iter().any(|(c, _)| self.expr_has_agg(c)) || else_.as_ref().map(|e| self.expr_has_agg(e)).unwrap_or(false) {
                    // Aggregated case — evaluate each branch's aggregate
                    for (cond, result) in whens {
                        let cv = self.eval_agg_expr(cond, t, indices)?;
                        if self.truthy(&cv) { return self.eval_agg_expr(result, t, indices); }
                    }
                    if let Some(e) = else_ { return self.eval_agg_expr(e, t, indices); }
                    Ok(Value2::Null)
                } else if indices.is_empty() {
                    Ok(Value2::Null)
                } else {
                    self.eval(expr, t, indices[0])
                }
            }
            Expr2::Neg(e) if self.expr_has_agg(e) => {
                let v = self.eval_agg_expr(e, t, indices)?;
                Ok(match v { Value2::Int(i) => Value2::Int(-i), Value2::Float(f) => Value2::Float(-f), _ => Value2::Null })
            }
            _ => {
                if indices.is_empty() { Ok(Value2::Null) } else { self.eval(expr, t, indices[0]) }
            }
        }
    }

    fn expr_name(&self, expr: &Expr2) -> String {
        match expr {
            Expr2::Col(n) => n.clone(),
            Expr2::CountStar => "count".to_string(),
            Expr2::Agg { func, .. } => format!("{:?}", func).to_lowercase(),
            Expr2::Int(i) => i.to_string(),
            Expr2::Float(f) => f.to_string(),
            Expr2::Str(s) => s.clone(),
            Expr2::Date(d) => d.to_string(),
            _ => "expr".to_string(),
        }
    }

    // --- Projection ---

    fn project(&self, select: &[SelectItem2], t: &ExecTable, indices: &[usize]) -> Result<QueryResult, Error> {
        let mut cols = Vec::new();
        for item in select {
            let name = item.alias.clone().unwrap_or_else(|| self.expr_name(&item.expr));
            let values: Vec<u64> = indices.iter().map(|&i| {
                self.eval(&item.expr, t, i).unwrap_or(Value2::Null).to_u64()
            }).collect();
            cols.push(ResultColumn { name, values });
        }
        Ok(QueryResult { columns: cols, row_count: indices.len(), elapsed_us: 0 })
    }

    // --- ORDER BY ---

    fn apply_order_by(&self, result: QueryResult, order_by: &[(Expr2, bool)], t: &ExecTable, indices: &[usize]) -> Result<QueryResult, Error> {
        if order_by.is_empty() || result.row_count <= 1 { return Ok(result); }
        let mut sort_keys: Vec<Vec<(f64, bool)>> = Vec::with_capacity(result.row_count);
        for row_idx in 0..result.row_count {
            let mut keys = Vec::new();
            for (expr, asc) in order_by {
                let name = self.expr_name(expr);
                let v = if let Some(col) = result.columns.iter().find(|c| c.name == name || c.name.eq_ignore_ascii_case(&name)) {
                    f64::from_bits(col.values[row_idx])
                } else {
                    let src_row = indices.get(row_idx).copied().unwrap_or(0);
                    self.eval(expr, t, src_row).map(|v| v.as_f64().unwrap_or(0.0)).unwrap_or(0.0)
                };
                keys.push((v, *asc));
            }
            sort_keys.push(keys);
        }
        let mut order: Vec<usize> = (0..result.row_count).collect();
        order.sort_by(|&a, &b| {
            for (i, (va, asc)) in sort_keys[a].iter().enumerate() {
                let vb = sort_keys[b][i].0;
                let cmp = va.partial_cmp(&vb).unwrap_or(std::cmp::Ordering::Equal);
                let cmp = if *asc { cmp } else { cmp.reverse() };
                if cmp != std::cmp::Ordering::Equal { return cmp; }
            }
            std::cmp::Ordering::Equal
        });
        let new_cols: Vec<ResultColumn> = result.columns.iter().map(|c| {
            let values: Vec<u64> = order.iter().map(|&i| c.values[i]).collect();
            ResultColumn { name: c.name.clone(), values }
        }).collect();
        Ok(QueryResult { columns: new_cols, row_count: result.row_count, elapsed_us: 0 })
    }

    fn apply_order_by_grouped(&self, result: QueryResult, order_by: &[(Expr2, bool)]) -> Result<QueryResult, Error> {
        if order_by.is_empty() || result.row_count <= 1 { return Ok(result); }
        let mut sort_keys: Vec<Vec<(f64, bool)>> = Vec::with_capacity(result.row_count);
        for row_idx in 0..result.row_count {
            let mut keys = Vec::new();
            for (expr, asc) in order_by {
                let name = self.expr_name(expr);
                let v = result.columns.iter().find(|c| c.name == name || c.name.eq_ignore_ascii_case(&name))
                    .map(|col| f64::from_bits(col.values[row_idx])).unwrap_or(0.0);
                keys.push((v, *asc));
            }
            sort_keys.push(keys);
        }
        let mut order: Vec<usize> = (0..result.row_count).collect();
        order.sort_by(|&a, &b| {
            for (i, (va, asc)) in sort_keys[a].iter().enumerate() {
                let vb = sort_keys[b][i].0;
                let cmp = va.partial_cmp(&vb).unwrap_or(std::cmp::Ordering::Equal);
                let cmp = if *asc { cmp } else { cmp.reverse() };
                if cmp != std::cmp::Ordering::Equal { return cmp; }
            }
            std::cmp::Ordering::Equal
        });
        let new_cols: Vec<ResultColumn> = result.columns.iter().map(|c| {
            let values: Vec<u64> = order.iter().map(|&i| c.values[i]).collect();
            ResultColumn { name: c.name.clone(), values }
        }).collect();
        Ok(QueryResult { columns: new_cols, row_count: result.row_count, elapsed_us: 0 })
    }
}

#[derive(Debug, Clone, Copy)]
struct JoinKey2 { left: usize, right: usize }

// =========================================================================
// Public entry point
// =========================================================================

/// Parse and execute a TPC-H SQL query against the catalog.
pub fn parse_and_execute(sql: &str, catalog: &Catalog) -> Result<QueryResult, Error> {
    let query = parse_tpch(sql).map_err(Error::Parse)?;
    execute_tpch(&query, catalog)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_simple_select() {
        let q = parse_tpch("SELECT count(*) FROM lineitem").unwrap();
        assert_eq!(q.from.len(), 1);
        assert_eq!(q.select.len(), 1);
    }

    #[test]
    fn test_parse_implicit_join() {
        let q = parse_tpch("SELECT count(*) FROM orders, lineitem WHERE o_orderkey = l_orderkey").unwrap();
        assert_eq!(q.from.len(), 2);
    }

    #[test]
    fn test_parse_group_by_having() {
        let q = parse_tpch("SELECT l_returnflag, count(*) FROM lineitem GROUP BY l_returnflag HAVING count(*) > 10").unwrap();
        assert_eq!(q.group_by.len(), 1);
        assert!(q.having.is_some());
    }

    #[test]
    fn test_parse_case_when() {
        let q = parse_tpch("SELECT case WHEN x = 1 THEN 10 ELSE 0 END FROM t").unwrap();
        assert!(matches!(&q.select[0].expr, Expr2::Case { .. }));
    }

    #[test]
    fn test_parse_extract() {
        let q = parse_tpch("SELECT extract(year FROM l_shipdate) FROM lineitem").unwrap();
        assert!(matches!(&q.select[0].expr, Expr2::Extract { .. }));
    }

    #[test]
    fn test_parse_between() {
        let q = parse_tpch("SELECT count(*) FROM t WHERE x BETWEEN 1 AND 10").unwrap();
        match &q.where_clause.unwrap() {
            Expr2::Between { .. } => {}
            other => panic!("expected Between, got {other:?}"),
        }
    }

    #[test]
    fn test_parse_in_list() {
        let q = parse_tpch("SELECT count(*) FROM t WHERE x IN (1, 2, 3)").unwrap();
        match &q.where_clause.unwrap() {
            Expr2::InList { list, .. } => assert_eq!(list.len(), 3),
            other => panic!("expected InList, got {other:?}"),
        }
    }

    #[test]
    fn test_parse_qualified_name() {
        let q = parse_tpch("SELECT l1.l_orderkey FROM lineitem l1").unwrap();
        match &q.select[0].expr {
            Expr2::Col(n) => assert_eq!(n, "l1.l_orderkey"),
            other => panic!("expected Col, got {other:?}"),
        }
    }

    #[test]
    fn test_parse_left_join() {
        let q = parse_tpch("SELECT count(*) FROM customer LEFT OUTER JOIN orders ON c_custkey = o_custkey").unwrap();
        assert_eq!(q.joins.len(), 1);
        assert_eq!(q.joins[0].join_type, JoinType2::Left);
    }

    #[test]
    fn test_parse_derived_table() {
        let q = parse_tpch("SELECT x FROM (SELECT count(*) AS x FROM t) AS dt").unwrap();
        assert_eq!(q.from.len(), 1);
        match &q.from[0] {
            FromItem::Derived(_, Some(alias)) => assert_eq!(alias, "dt"),
            other => panic!("expected Derived, got {other:?}"),
        }
    }

    #[test]
    fn test_parse_not_exists() {
        let q = parse_tpch("SELECT count(*) FROM t WHERE NOT exists (SELECT 1 FROM t2 WHERE t2.x = t.x)").unwrap();
        match &q.where_clause.unwrap() {
            Expr2::Exists { negated: true, .. } => {}
            other => panic!("expected Exists negated, got {other:?}"),
        }
    }

    #[test]
    fn test_parse_substr() {
        let q = parse_tpch("SELECT substr(c_phone, 1, 2) FROM customer").unwrap();
        assert!(matches!(&q.select[0].expr, Expr2::Substr { .. }));
    }

    #[test]
    fn test_parse_arithmetic_in_agg() {
        let q = parse_tpch("SELECT sum(l_extendedprice * (1 - l_discount)) FROM lineitem").unwrap();
        match &q.select[0].expr {
            Expr2::Agg { func: AggFunc::Sum, .. } => {}
            other => panic!("expected Sum agg, got {other:?}"),
        }
    }

    #[test]
    fn test_like_match() {
        let cat = Catalog::new(); let exec = TpchExec { catalog: &cat };
        assert!(exec.like("hello world", "%hello%"));
        assert!(exec.like("hello", "hello"));
        assert!(exec.like("hello world", "hello%"));
        assert!(exec.like("hello world", "%world"));
        assert!(!exec.like("hello", "world"));
        assert!(exec.like("PROMO STEEL", "PROMO%"));
    }
}
