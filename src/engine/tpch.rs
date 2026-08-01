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
use rayon::prelude::*;
use fxhash::{FxHashMap, FxHashSet};

// Use ahash (hardware AES) instead of std SipHash for all HashMap/HashSet.
// Perf showed 28% of Q21 time was in SipHash + hashbrown operations.
// ahash is ~5x faster for u64 keys.
type HashMap<K, V> = ahash::AHashMap<K, V>;
type HashSet<T> = ahash::AHashSet<T>;

/// Create a HashMap without calling OS entropy (avoids getrandom syscall).
/// Uses a fixed seed — sufficient for database internals where hash-flooding
/// is not a concern (all inputs are trusted TPC-H data).
/// Perf showed 2% of Q21 time was in ahash seed generation (gen_hasher_seed).
fn new_hashmap<K, V>() -> HashMap<K, V> {
    HashMap::with_hasher(ahash::RandomState::with_seed(0x517cc1b727220a95))
}

/// Create a HashSet without calling OS entropy.
fn new_hashset<T>() -> HashSet<T> {
    HashSet::with_hasher(ahash::RandomState::with_seed(0x517cc1b727220a95))
}

/// Create an FxHashMap (trusted u64 keys - no AES, 1 multiply hash) for hot
/// GROUP BY / EXISTS paths. FxHash is ~2x faster than ahash for u64 keys
/// because it skips the AES-NI finalizer (already saturated on this workload).
fn new_fxhashmap<K, V>() -> FxHashMap<K, V> {
    FxHashMap::default()
}

/// Create an FxHashSet (trusted u64 keys) for hot EXISTS semi-join sets.
fn new_fxhashset<T>() -> FxHashSet<T> {
    FxHashSet::default()
}

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

/// Swap comparison operands (for Literal op Col → Col swap_op(op) Literal).
fn swap_op(op: BinOp2) -> BinOp2 {
    match op {
        BinOp2::Lt => BinOp2::Gt,
        BinOp2::Le => BinOp2::Ge,
        BinOp2::Gt => BinOp2::Lt,
        BinOp2::Ge => BinOp2::Le,
        BinOp2::Eq => BinOp2::Eq,
        BinOp2::Ne => BinOp2::Ne,
        other => other,
    }
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
            // Handle SELECT * — common in EXISTS subqueries.
            // Treat as SELECT 1 (column values don't matter for EXISTS).
            if let Token::Op(op) = self.peek() {
                if op == "*" {
                    self.next();
                    items.push(SelectItem2 { expr: Expr2::Int(1), alias: None });
                    if !self.match_comma() { break; }
                    continue;
                }
            }
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
    columns: Vec<std::sync::Arc<Vec<u64>>>,
    column_names: Vec<String>,
    col_types: Vec<ColType>,
    string_columns: Vec<Option<std::sync::Arc<StringSearchColumn>>>,
    row_count: usize,
    col_map: HashMap<String, usize>,
}

impl ExecTable {
    fn from_catalog(table: &Table, alias: &str) -> Self {
        let col_types = tpch_col_types(&table.name);
        let mut col_map = new_hashmap();
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
        // Fast path: direct lookup (common case — name is already lowercase
        // because col_map keys are stored lowercase).
        if let Some(&idx) = self.col_map.get(name) {
            return Some(idx);
        }
        // Slow path: case-insensitive lookup via to_lowercase.
        // Only reached for uppercase/mixed-case column names (rare in TPC-H).
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
    fn as_u64(&self) -> Option<u64> {
        match self {
            Value2::Int(i) => Some(*i as u64),
            Value2::Float(f) => Some(*f as u64),
            Value2::Date(d) => Some(*d as u64),
            _ => None,
        }
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
    TpchExec {
        catalog,
        outer: std::cell::Cell::new(None),
        subquery_cache: std::cell::RefCell::new(new_hashmap()),
        exists_cache: std::cell::RefCell::new(new_hashmap()),
        exists_multi_cache: std::cell::RefCell::new(new_hashmap()),
        in_subquery_cache: std::cell::RefCell::new(new_hashmap()),
        decorrelated_cache: std::cell::RefCell::new(new_hashmap()),
    }.execute(query)
}

struct TpchExec<'a> {
    catalog: &'a Catalog,
    /// Outer context for correlated subqueries: (outer_table_ptr, outer_row).
    /// Set when entering a subquery eval, restored after. Uses raw pointer
    /// for lifetime erasure (safe because the outer table is valid for the
    /// duration of the synchronous subquery execution).
    outer: std::cell::Cell<Option<(*const ExecTable, usize)>>,
    /// Cache for uncorrelated scalar subqueries: keyed by the SelectQuery2
    /// AST pointer (stable for the query's lifetime). Populated lazily by
    /// `precache_subqueries` (called at the start of `execute`) which tries
    /// to execute each subquery with outer=None — if it succeeds, the
    /// subquery is uncorrelated and the result is cached; if it fails
    /// (column not found), it's correlated and per-row eval handles it.
    /// This fixes Q11 (HAVING with uncorrelated scalar subquery) which
    /// previously re-executed the subquery per group (~8000x) and timed out.
    subquery_cache: std::cell::RefCell<HashMap<usize, Value2>>,
    /// Cache for EXISTS semi-join hash sets: keyed by the subquery AST pointer.
    /// When an EXISTS subquery has a single correlation column with an equi-join
    /// (e.g. Q4's `exists (SELECT * FROM lineitem WHERE l_orderkey = o_orderkey
    /// AND l_commitdate < l_receiptdate)`), we build a hash set of the inner
    /// column values (l_orderkey where l_commitdate < l_receiptdate) ONCE,
    /// then check membership per outer row. This decorrelates the EXISTS,
    /// reducing ~25k subquery executions to 1 hash-set build + 25k lookups.
    exists_cache: std::cell::RefCell<HashMap<usize, FxHashSet<u64>>>,
    /// Cache for multi-column EXISTS: HashMap<equi_key, HashSet<ineq_col>>.
    /// For Q21's `exists (SELECT * FROM lineitem l2 WHERE l2.l_orderkey = l1.l_orderkey
    /// AND l2.l_suppkey <> l1.l_suppkey)`, we build a HashMap<l_orderkey, HashSet<l_suppkey>>
    /// once, then for each outer row, check if any suppkey in the set != l1.l_suppkey.
    exists_multi_cache: std::cell::RefCell<HashMap<usize, FxHashMap<u64, FxHashSet<u64>>>>,
    /// Cache for uncorrelated IN-subquery result sets: keyed by AST pointer.
    /// When an IN-subquery is uncorrelated (e.g. Q20's `s_suppkey IN (SELECT
    /// ps_suppkey FROM partsupp WHERE ...)`), we execute it ONCE and cache
    /// the set of values. Then per-row eval just checks membership.
    in_subquery_cache: std::cell::RefCell<HashMap<usize, FxHashSet<u64>>>,
    /// Cache for decorrelated correlated scalar subqueries.
    /// When a correlated scalar subquery has an aggregate (sum/avg/min/max)
    /// and multiple correlation columns, we proactively build a derived table:
    /// execute the subquery's FROM table with local filters, GROUP BY the
    /// correlation columns, and cache a HashMap<corr_key_hash, agg_value>.
    /// Then per-row eval is a single hash lookup (O(1)) instead of a full
    /// subquery execution. This is critical for Q20 where the correlation
    /// key (ps_partkey, ps_suppkey) has 800k distinct values, each requiring
    /// a 6M-row lineitem scan — the derived table scans lineitem ONCE.
    /// Value: (HashMap<corr_hash, agg_value>, Vec<usize> corr_col_indices_in_outer).
    decorrelated_cache: std::cell::RefCell<HashMap<usize, (FxHashMap<u64, Value2>, Vec<usize>)>>,
}

// =============================================================================
// W2: Reusable bool-mask buffer pool.
//
// `eval_bool_mask_vec`'s AND arm previously cloned the running mask per
// conjunct (`mask.to_vec()`, 6 MB for a 6 M-row lineitem scan); the OR
// fallback arm allocated two fresh `vec![true; N]` masks per call. Both
// paths are now backed by this thread-local pool, eliminating the
// malloc/free overhead in the hot WHERE-evaluation loop.
//
// The pool is a stack of `Vec<bool>` buffers. `take_mask_buf(n)` pops a
// buffer (or allocates if the pool is empty) and resizes it to at least
// `n`; `return_mask_buf(buf)` pushes it back. Recursion (AND inside OR
// inside AND, etc.) is safe: a recursive `take_mask_buf` simply pops a
// different buffer or allocates if the pool is exhausted. After warmup
// the pool size equals the max recursion depth, and no further
// allocations occur.
// ============================================================================

thread_local! {
    static MASK_POOL: std::cell::RefCell<Vec<Vec<bool>>> =
        std::cell::RefCell::new(Vec::new());
}

/// Take a `Vec<bool>` of length >= `n` from the thread-local pool
/// (allocating if necessary). The caller MUST return it via
/// `return_mask_buf` to avoid re-allocating on the next call.
fn take_mask_buf(n: usize) -> Vec<bool> {
    MASK_POOL.with(|cell| {
        let mut pool = cell.borrow_mut();
        let mut buf = pool.pop().unwrap_or_else(|| Vec::with_capacity(n));
        if buf.len() < n { buf.resize(n, false); }
        buf
    })
}

/// Return a buffer to the thread-local pool for reuse by the next
/// `take_mask_buf` call on this thread.
fn return_mask_buf(buf: Vec<bool>) {
    MASK_POOL.with(|cell| {
        cell.borrow_mut().push(buf);
    });
}

impl<'a> TpchExec<'a> {
    fn execute(&self, query: &SelectQuery2) -> Result<QueryResult, Error> {
        // Pre-execute uncorrelated scalar subqueries found in WHERE/HAVING/SELECT.
        // Each subquery is tried with outer=None — if it succeeds, it's uncorrelated
        // and the result is cached; subsequent per-row/per-group eval hits the cache.
        // This is critical for Q11 (HAVING with uncorrelated scalar subquery) which
        // would otherwise re-execute the subquery per group and time out.
        if let Some(ref wc) = query.where_clause {
            self.precache_subqueries(wc);
        }
        if let Some(ref hv) = query.having {
            self.precache_subqueries(hv);
        }
        for item in &query.select {
            self.precache_subqueries(&item.expr);
        }

        // 1. Load all FROM tables
        let mut tables: Vec<ExecTable> = Vec::new();
        for item in &query.from {
            tables.push(self.resolve_from_item(item)?);
        }

        // 2. Handle explicit JOINs on the first table.
        // hash_join now applies non-equi-join ON conditions (LIKE, IN, <, >)
        // per-match during the join, with proper LEFT JOIN handling for
        // unmatched left rows.
        for join in &query.joins {
            let right = self.resolve_from_item(&join.table)?;
            let left = tables.pop().unwrap();
            tables.push(self.hash_join(left, right, &join.on, join.join_type)?);
        }

        // 3. Build base table — use hash joins for implicit multi-table joins.
        // For multi-table FROM, join_tables_smart applies single-table filters
        // (e.g. p_name LIKE '%green%') BEFORE joining. We must NOT re-apply
        // those single-table filters after the join, because string_columns
        // are not rebuilt after joins (LIKE on joined tables falls back to
        // hash comparison, which fails for wildcard patterns).
        let (base, mask) = if tables.len() == 1 {
            let base = tables.into_iter().next().unwrap();
            let mask = if let Some(ref wc) = query.where_clause {
                self.build_mask(wc, &base)?
            } else { vec![true; base.row_count] };
            (base, mask)
        } else {
            // Identify multi-table conjuncts BEFORE consuming tables.
            // Single-table conjuncts (refs.len() == 1) are applied by
            // join_tables_smart and skipped here.
            let conjuncts = self.split_conjuncts(&query.where_clause);
            let multi_table: Vec<Expr2> = conjuncts.iter().filter(|conj| {
                let refs = self.expr_table_refs(conj, &tables);
                refs.len() != 1
            }).cloned().collect();
            let base = self.join_tables_smart(tables, &query.where_clause)?;
            let mask = if multi_table.is_empty() {
                vec![true; base.row_count]
            } else {
                // W2: evaluate each multi-table conjunct directly into the
                // running mask. The simplified AND arm + fixed OR arm in
                // `eval_bool_mask_vec` preserve the incoming mask (every
                // leaf ANDs into it), so the previous per-conjunct
                // `mask.clone()` (6 MB for a 6 M-row base table) is no
                // longer needed.
                let mut mask = vec![true; base.row_count];
                for conj in &multi_table {
                    self.eval_bool_mask_vec(conj, &base, &mut mask)?;
                }
                mask
            };
            (base, mask)
        };

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

    /// Walk an expression tree and pre-execute all uncorrelated scalar subqueries,
    /// caching their results in `subquery_cache`. Correlated subqueries (those
    /// that reference outer columns) will fail when executed with outer=None
    /// and are silently skipped — per-row eval will handle them.
    ///
    /// This is critical for Q11 (HAVING with uncorrelated scalar subquery)
    /// which would otherwise re-execute the subquery per group (~8000x) and
    /// time out. Also helps Q2 (correlated subquery in WHERE) by ensuring
    /// the uncorrelated parts of any nested subqueries are cached.
    fn precache_subqueries(&self, expr: &Expr2) {
        match expr {
            Expr2::Subquery(q) => {
                let key = (q.as_ref() as *const SelectQuery2) as usize;
                // Already cached?
                if self.subquery_cache.borrow().contains_key(&key) {
                    return;
                }
                // Try executing with outer=None. If it succeeds, the subquery
                // is uncorrelated — cache the result. If it fails (column not
                // found), it's correlated — leave uncached.
                let old_outer = self.outer.get();
                self.outer.set(None);
                let r = self.execute(q);
                self.outer.set(old_outer);
                if let Ok(r) = r {
                    if let Some(col) = r.columns.first() {
                        let val = col.values.first().copied().unwrap_or(0);
                        let vals_slice = col.values.as_slice();
                        let v = match self.infer_result_type(&col.name, vals_slice) {
                            ColType::Float => Value2::Float(f64::from_bits(val)),
                            _ => Value2::Int(val as i64),
                        };
                        self.subquery_cache.borrow_mut().insert(key, v);
                    }
                }
            }
            Expr2::BinOp { left, right, .. } => {
                self.precache_subqueries(left);
                self.precache_subqueries(right);
            }
            Expr2::Case { whens, else_ } => {
                for (c, r) in whens {
                    self.precache_subqueries(c);
                    self.precache_subqueries(r);
                }
                if let Some(e) = else_ { self.precache_subqueries(e); }
            }
            Expr2::Like { expr, pattern, .. } => {
                self.precache_subqueries(expr);
                self.precache_subqueries(pattern);
            }
            Expr2::Between { expr, low, high, .. } => {
                self.precache_subqueries(expr);
                self.precache_subqueries(low);
                self.precache_subqueries(high);
            }
            Expr2::InList { expr, list, .. } => {
                self.precache_subqueries(expr);
                for e in list { self.precache_subqueries(e); }
            }
            Expr2::Neg(e) | Expr2::Not(e) | Expr2::Extract { expr: e, .. } => {
                self.precache_subqueries(e);
            }
            Expr2::Substr { expr, start, len } => {
                self.precache_subqueries(expr);
                self.precache_subqueries(start);
                self.precache_subqueries(len);
            }
            Expr2::InSubquery { expr, query, .. } => {
                self.precache_subqueries(expr);
                self.precache_subqueries(&Expr2::Subquery(query.clone()));
            }
            _ => {}
        }
    }

    /// Find columns in the outer table `t` that the subquery references
    /// (correlation columns). These are column references in the subquery's
    /// WHERE/SELECT/HAVING that resolve to `t` (the outer table) but NOT to
    /// the subquery's own FROM tables.
    ///
    /// Used to cache correlated subquery results by the outer row's correlation
    /// values, dramatically reducing redundant subquery executions (e.g. Q17
    /// goes from ~60k executions to ~200, one per distinct p_partkey).
    fn find_correlation_cols(&self, subquery: &SelectQuery2, outer_t: &ExecTable) -> Vec<usize> {
        // Build set of column names available in the subquery's own FROM tables.
        // A Col ref that resolves to one of these is NOT a correlation column.
        let mut inner_cols: HashSet<String> = new_hashset();
        for item in &subquery.from {
            if let FromItem::Table(t) = item {
                if let Some(table) = self.catalog.get(&t.name) {
                    for cn in &table.column_names {
                        inner_cols.insert(cn.to_lowercase());
                    }
                }
            }
        }
        let mut cols: Vec<usize> = Vec::new();
        let mut seen: HashSet<usize> = new_hashset();
        if let Some(ref wc) = subquery.where_clause {
            self.collect_corr_cols_filtered(wc, outer_t, &inner_cols, &mut cols, &mut seen);
        }
        if let Some(ref hv) = subquery.having {
            self.collect_corr_cols_filtered(hv, outer_t, &inner_cols, &mut cols, &mut seen);
        }
        for item in &subquery.select {
            self.collect_corr_cols_filtered(&item.expr, outer_t, &inner_cols, &mut cols, &mut seen);
        }
        cols
    }

    fn collect_corr_cols_filtered(
        &self, expr: &Expr2, outer_t: &ExecTable, inner_cols: &HashSet<String>,
        cols: &mut Vec<usize>, seen: &mut HashSet<usize>,
    ) {
        match expr {
            Expr2::Col(name) => {
                // Get short name (after '.') for comparison with inner_cols
                let short = name.rfind('.').map(|p| &name[p+1..]).unwrap_or(name.as_str());
                let short_lower = short.to_lowercase();
                // If the short name resolves to an inner table column, it's NOT a correlation col
                if inner_cols.contains(&short_lower) {
                    return;
                }
                // Otherwise, check if it resolves to outer_t
                let idx = outer_t.lookup_col(name).or_else(|| outer_t.lookup_col(short));
                if let Some(idx) = idx {
                    if seen.insert(idx) {
                        cols.push(idx);
                    }
                }
            }
            Expr2::BinOp { left, right, .. } => {
                self.collect_corr_cols_filtered(left, outer_t, inner_cols, cols, seen);
                self.collect_corr_cols_filtered(right, outer_t, inner_cols, cols, seen);
            }
            Expr2::Case { whens, else_ } => {
                for (c, r) in whens {
                    self.collect_corr_cols_filtered(c, outer_t, inner_cols, cols, seen);
                    self.collect_corr_cols_filtered(r, outer_t, inner_cols, cols, seen);
                }
                if let Some(e) = else_ { self.collect_corr_cols_filtered(e, outer_t, inner_cols, cols, seen); }
            }
            Expr2::Like { expr, pattern, .. } => {
                self.collect_corr_cols_filtered(expr, outer_t, inner_cols, cols, seen);
                self.collect_corr_cols_filtered(pattern, outer_t, inner_cols, cols, seen);
            }
            Expr2::Between { expr, low, high, .. } => {
                self.collect_corr_cols_filtered(expr, outer_t, inner_cols, cols, seen);
                self.collect_corr_cols_filtered(low, outer_t, inner_cols, cols, seen);
                self.collect_corr_cols_filtered(high, outer_t, inner_cols, cols, seen);
            }
            Expr2::InList { expr, list, .. } => {
                self.collect_corr_cols_filtered(expr, outer_t, inner_cols, cols, seen);
                for e in list { self.collect_corr_cols_filtered(e, outer_t, inner_cols, cols, seen); }
            }
            Expr2::Neg(e) | Expr2::Not(e) | Expr2::Extract { expr: e, .. } => {
                self.collect_corr_cols_filtered(e, outer_t, inner_cols, cols, seen);
            }
            Expr2::Substr { expr, start, len } => {
                self.collect_corr_cols_filtered(expr, outer_t, inner_cols, cols, seen);
                self.collect_corr_cols_filtered(start, outer_t, inner_cols, cols, seen);
                self.collect_corr_cols_filtered(len, outer_t, inner_cols, cols, seen);
            }
            Expr2::Subquery(_) | Expr2::Exists { .. } | Expr2::InSubquery { .. } => {}
            _ => {}
        }
    }

    /// For an EXISTS subquery, find the single correlation column and the
    /// corresponding inner column via an equi-join conjunct
    /// (`Col(inner) = Col(outer)` or `Col(outer) = Col(inner)`).
    ///
    /// Returns `Some((outer_col_idx, inner_col_idx))` if exactly one
    /// correlation column with an equi-join is found; `None` otherwise
    /// (e.g. multiple correlation cols, or no equi-join).
    ///
    /// Q4 example: `exists (SELECT * FROM lineitem WHERE l_orderkey = o_orderkey
    /// AND l_commitdate < l_receiptdate)` → outer_col=o_orderkey, inner_col=l_orderkey.
    /// Check if a conjunct references a column not in the inner tables.
    /// Uses inner_cols (short names) and inner_aliases (table qualifiers)
    /// to determine if a column reference is inner or correlated (outer).
    fn is_conjunct_correlated_wrt_inner(
        &self, expr: &Expr2,
        inner_cols: &HashSet<String>,
        inner_aliases: &HashSet<String>,
    ) -> bool {
        match expr {
            Expr2::Col(name) => {
                if let Some(dot_pos) = name.find('.') {
                    let qualifier = name[..dot_pos].to_lowercase();
                    // If qualifier matches an inner alias, it's inner.
                    if inner_aliases.contains(&qualifier) {
                        return false;
                    }
                    // Otherwise it's correlated.
                    true
                } else {
                    // Unqualified: if short name is in inner_cols, it's inner.
                    !inner_cols.contains(&name.to_lowercase())
                }
            }
            Expr2::BinOp { left, right, .. } => {
                self.is_conjunct_correlated_wrt_inner(left, inner_cols, inner_aliases)
                    || self.is_conjunct_correlated_wrt_inner(right, inner_cols, inner_aliases)
            }
            Expr2::Case { whens, else_ } => {
                whens.iter().any(|(c, r)| self.is_conjunct_correlated_wrt_inner(c, inner_cols, inner_aliases) || self.is_conjunct_correlated_wrt_inner(r, inner_cols, inner_aliases))
                    || else_.as_ref().map(|e| self.is_conjunct_correlated_wrt_inner(e, inner_cols, inner_aliases)).unwrap_or(false)
            }
            Expr2::Like { expr, pattern, .. } => {
                self.is_conjunct_correlated_wrt_inner(expr, inner_cols, inner_aliases) || self.is_conjunct_correlated_wrt_inner(pattern, inner_cols, inner_aliases)
            }
            Expr2::Between { expr, low, high, .. } => {
                self.is_conjunct_correlated_wrt_inner(expr, inner_cols, inner_aliases) || self.is_conjunct_correlated_wrt_inner(low, inner_cols, inner_aliases) || self.is_conjunct_correlated_wrt_inner(high, inner_cols, inner_aliases)
            }
            Expr2::InList { expr, list, .. } => {
                self.is_conjunct_correlated_wrt_inner(expr, inner_cols, inner_aliases) || list.iter().any(|e| self.is_conjunct_correlated_wrt_inner(e, inner_cols, inner_aliases))
            }
            Expr2::Neg(e) | Expr2::Not(e) | Expr2::Extract { expr: e, .. } => {
                self.is_conjunct_correlated_wrt_inner(e, inner_cols, inner_aliases)
            }
            Expr2::Substr { expr, start, len } => {
                self.is_conjunct_correlated_wrt_inner(expr, inner_cols, inner_aliases) || self.is_conjunct_correlated_wrt_inner(start, inner_cols, inner_aliases) || self.is_conjunct_correlated_wrt_inner(len, inner_cols, inner_aliases)
            }
            Expr2::Subquery(_) | Expr2::Exists { .. } | Expr2::InSubquery { .. } => true,
            _ => false,
        }
    }

    /// Try to decorrelate a correlated scalar subquery by building a derived
    /// table: execute the subquery's FROM table with local (non-correlated)
    /// filters, GROUP BY the correlation columns, compute the aggregate, and
    /// cache a HashMap<corr_hash, agg_value>. Then per-row eval is O(1).
    ///
    /// Pattern: `SELECT agg(expr) FROM t WHERE corr1 = outer1 AND ... AND local_filters`
    /// → derived table: `SELECT corr1, ..., agg(expr) FROM t WHERE local_filters GROUP BY corr1, ...`
    ///
    /// Returns Some((HashMap<corr_hash, agg_value>, Vec<outer_col_indices>))
    /// if the pattern matches, None otherwise.
    ///
    /// Q20 example: `SELECT 0.5 * sum(l_quantity) FROM lineitem
    ///   WHERE l_partkey = ps_partkey AND l_suppkey = ps_suppkey
    ///   AND l_shipdate >= date '1994-01-01' AND l_shipdate < date '1995-01-01'`
    /// → derived table groups lineitem by (l_partkey, l_suppkey), computes
    ///   0.5 * sum(l_quantity), caches HashMap<(l_partkey,l_suppkey)_hash, threshold>.
    fn try_decorrelate_subquery(
        &self, subquery: &SelectQuery2, outer_t: &ExecTable,
    ) -> Result<Option<(FxHashMap<u64, Value2>, Vec<usize>)>, Error> {
        // Only decorrelate if the subquery has exactly 1 SELECT item that is
        // an aggregate (or a scalar function of an aggregate, like 0.2 * avg(x)).
        if subquery.select.len() != 1 { return Ok(None); }
        if !self.expr_has_agg(&subquery.select[0].expr) { return Ok(None); }
        if subquery.having.is_some() { return Ok(None); }
        if !subquery.group_by.is_empty() { return Ok(None); }

        // Only decorrelate single-table subqueries (multi-table joins in the
        // subquery make the derived table build expensive and error-prone).
        // Q20's subquery is `SELECT 0.5*sum(l_quantity) FROM lineitem WHERE ...`
        // (single table) — perfect for decorrelation.
        // Q2's subquery has 4 FROM tables — not decorrelated (uses per-row cache).
        if subquery.from.len() != 1 { return Ok(None); }

        // Build inner column name set and inner table aliases.
        let mut inner_cols: HashSet<String> = new_hashset();
        let mut inner_aliases: HashSet<String> = new_hashset();
        for item in &subquery.from {
            if let FromItem::Table(t) = item {
                inner_aliases.insert(t.name.to_lowercase());
                if let Some(ref alias) = t.alias {
                    inner_aliases.insert(alias.to_lowercase());
                }
                if let Some(table) = self.catalog.get(&t.name) {
                    for cn in &table.column_names {
                        inner_cols.insert(cn.to_lowercase());
                    }
                }
            }
        }

        // Find correlation columns (outer cols referenced by the subquery).
        let mut corr_cols = self.find_correlation_cols(subquery, outer_t);
        // Need at least 1 correlation column to be correlated.
        if corr_cols.is_empty() { return Ok(None); }

        // Find the inner column indices for each correlation column by
        // looking at the equi-join conjuncts in the subquery's WHERE.
        // Each corr col has a corresponding inner col via `inner_col = outer_col`.
        let wc = match &subquery.where_clause {
            Some(w) => w,
            None => return Ok(None),
        };
        let conjuncts = self.split_conjuncts(&Some(wc.clone()));

        // Map: outer_col_idx -> (inner_col_idx, outer_col_name, inner_col_name)
        let mut corr_to_inner: Vec<(usize, usize, String, String)> = Vec::new();
        for conj in &conjuncts {
            if let Expr2::BinOp { op: BinOp2::Eq, left: l, right: r } = conj {
                if let (Expr2::Col(ln), Expr2::Col(rn)) = (l.as_ref(), r.as_ref()) {
                    let l_is_inner = self.col_is_inner(ln, &inner_aliases, &inner_cols);
                    let r_is_inner = self.col_is_inner(rn, &inner_aliases, &inner_cols);
                    if l_is_inner != r_is_inner {
                        let (inner_name, outer_name) = if l_is_inner {
                            (ln.clone(), rn.clone())
                        } else {
                            (rn.clone(), ln.clone())
                        };
                        let outer_short = outer_name.rfind('.').map(|p| &outer_name[p+1..]).unwrap_or(&outer_name).to_lowercase();
                        let outer_idx = match outer_t.lookup_col(&outer_name).or_else(|| outer_t.lookup_col(&outer_short)) {
                            Some(idx) => idx,
                            None => continue,
                        };
                        let inner_idx = match self.resolve_inner_col_idx(&inner_name, subquery, outer_t) {
                            Some(idx) => idx,
                            None => continue,
                        };
                        if !corr_to_inner.iter().any(|(oi, _, _, _)| *oi == outer_idx) {
                            corr_to_inner.push((outer_idx, inner_idx, outer_name.clone(), inner_name.clone()));
                        }
                    }
                }
            }
        }

        // Check that every correlation column found has a matching equi-join.
        // (corr_cols and corr_to_inner outer indices should match.)
        let corr_outer_indices: HashSet<usize> = corr_cols.iter().copied().collect();
        let matched_outer_indices: HashSet<usize> = corr_to_inner.iter().map(|(oi, _, _, _)| *oi).collect();
        if corr_outer_indices != matched_outer_indices {
            return Ok(None);
        }
        if corr_to_inner.is_empty() { return Ok(None); }

        // Build the derived table: load inner FROM, apply local (non-correlated)
        // conjuncts, GROUP BY inner correlation columns, compute aggregate.
        // We must build a WHERE with correlated conjuncts REMOVED, so that
        // join_tables_smart doesn't try to apply them as single-table filters
        // (which would fail because the outer columns aren't in the inner tables).
        // A conjunct is "correlated" if it references any column whose short name
        // is NOT in the inner table column set AND whose qualifier is NOT an inner
        // table alias. (For Q2: `p_partkey = ps_partkey` — p_partkey is correlated.)
        let local_conjuncts: Vec<Expr2> = conjuncts.iter().filter(|c| {
            !self.is_conjunct_correlated_wrt_inner(c, &inner_cols, &inner_aliases)
        }).cloned().collect();
        // Rebuild a WHERE clause from local conjuncts (ANDed together).
        let local_where: Option<Expr2> = if local_conjuncts.is_empty() {
            None
        } else {
            let mut w = local_conjuncts[0].clone();
            for c in &local_conjuncts[1..] {
                w = Expr2::BinOp { op: BinOp2::And, left: Box::new(w), right: Box::new(c.clone()) };
            }
            Some(w)
        };
        let mut tables: Vec<ExecTable> = Vec::new();
        for item in &subquery.from {
            tables.push(self.resolve_from_item(item)?);
        }
        let base = if tables.len() == 1 {
            tables.into_iter().next().unwrap()
        } else {
            self.join_tables_smart(tables, &local_where)?
        };

        // Apply local (non-correlated) conjuncts only.
        // W2: evaluate each conjunct directly into `m` (the simplified
        // AND/OR arms in `eval_bool_mask_vec` preserve the incoming mask).
        let mask = {
            let mut m = vec![true; base.row_count];
            for conj in &local_conjuncts {
                self.eval_bool_mask_vec(conj, &base, &mut m)?;
            }
            m
        };

        // Build the aggregate map: GROUP BY inner corr cols, compute agg.
        let agg_expr = &subquery.select[0].expr;
        let inner_corr_indices: Vec<usize> = corr_to_inner.iter().map(|(_, ii, _, _)| *ii).collect();
        // Group rows by composite hash of inner corr cols.
        let mut groups: FxHashMap<u64, Vec<usize>> = new_fxhashmap();
        for i in 0..base.row_count {
            if !mask[i] { continue; }
            let mut h: u64 = 0;
            for &ci in &inner_corr_indices {
                let v = base.columns[ci][i];
                h = h.wrapping_mul(0x517cc1b727220a95).wrapping_add(v);
            }
            groups.entry(h).or_default().push(i);
        }

        // For each group, compute the aggregate value.
        let mut result_map: FxHashMap<u64, Value2> = new_fxhashmap();
        result_map.reserve(groups.len());
        for (hash, indices) in &groups {
            let v = self.eval_agg_expr(agg_expr, &base, indices)?;
            result_map.insert(*hash, v);
        }

        // The outer col indices (for computing corr_hash per outer row).
        let outer_corr_indices: Vec<usize> = corr_to_inner.iter().map(|(oi, _, _, _)| *oi).collect();

        Ok(Some((result_map, outer_corr_indices)))
    }

    fn find_exists_equi_join(&self, subquery: &SelectQuery2, outer_t: &ExecTable) -> Option<(usize, usize)> {
        // Build inner column name set (subquery's own FROM tables)
        let mut inner_cols: HashSet<String> = new_hashset();
        for item in &subquery.from {
            if let FromItem::Table(t) = item {
                if let Some(table) = self.catalog.get(&t.name) {
                    for cn in &table.column_names {
                        inner_cols.insert(cn.to_lowercase());
                    }
                }
            }
        }
        // Find correlation columns (in outer_t but not in inner tables)
        let mut corr_names: Vec<String> = Vec::new();
        let mut seen: HashSet<String> = new_hashset();
        if let Some(ref wc) = subquery.where_clause {
            self.collect_corr_names(wc, outer_t, &inner_cols, &mut corr_names, &mut seen);
        }
        if corr_names.len() != 1 {
            return None;
        }
        let corr_name = &corr_names[0];
        let outer_idx = outer_t.lookup_col(corr_name)
            .or_else(|| corr_name.rfind('.').and_then(|p| outer_t.lookup_col(&corr_name[p+1..])))?;
        // Find the equi-join conjunct: Col(inner) = Col(corr_name) or vice versa
        if let Some(ref wc) = subquery.where_clause {
            if let Some(inner_idx) = self.find_equi_join_inner(wc, corr_name, &inner_cols, subquery, outer_t) {
                return Some((outer_idx, inner_idx));
            }
        }
        None
    }

    fn collect_corr_names(
        &self, expr: &Expr2, outer_t: &ExecTable, inner_cols: &HashSet<String>,
        names: &mut Vec<String>, seen: &mut HashSet<String>,
    ) {
        match expr {
            Expr2::Col(name) => {
                let short = name.rfind('.').map(|p| &name[p+1..]).unwrap_or(name.as_str());
                // If the column is NOT in inner_cols, it's a correlation column.
                if !inner_cols.contains(&short.to_lowercase()) {
                    if outer_t.lookup_col(name).is_some() || outer_t.lookup_col(short).is_some() {
                        if seen.insert(name.to_lowercase()) {
                            names.push(name.clone());
                        }
                    }
                }
            }
            Expr2::BinOp { left, right, .. } => {
                self.collect_corr_names(left, outer_t, inner_cols, names, seen);
                self.collect_corr_names(right, outer_t, inner_cols, names, seen);
            }
            Expr2::Case { whens, else_ } => {
                for (c, r) in whens {
                    self.collect_corr_names(c, outer_t, inner_cols, names, seen);
                    self.collect_corr_names(r, outer_t, inner_cols, names, seen);
                }
                if let Some(e) = else_ { self.collect_corr_names(e, outer_t, inner_cols, names, seen); }
            }
            Expr2::Like { expr, pattern, .. } => {
                self.collect_corr_names(expr, outer_t, inner_cols, names, seen);
                self.collect_corr_names(pattern, outer_t, inner_cols, names, seen);
            }
            Expr2::Between { expr, low, high, .. } => {
                self.collect_corr_names(expr, outer_t, inner_cols, names, seen);
                self.collect_corr_names(low, outer_t, inner_cols, names, seen);
                self.collect_corr_names(high, outer_t, inner_cols, names, seen);
            }
            Expr2::InList { expr, list, .. } => {
                self.collect_corr_names(expr, outer_t, inner_cols, names, seen);
                for e in list { self.collect_corr_names(e, outer_t, inner_cols, names, seen); }
            }
            Expr2::Neg(e) | Expr2::Not(e) | Expr2::Extract { expr: e, .. } => {
                self.collect_corr_names(e, outer_t, inner_cols, names, seen);
            }
            Expr2::Substr { expr, start, len } => {
                self.collect_corr_names(expr, outer_t, inner_cols, names, seen);
                self.collect_corr_names(start, outer_t, inner_cols, names, seen);
                self.collect_corr_names(len, outer_t, inner_cols, names, seen);
            }
            Expr2::Subquery(_) | Expr2::Exists { .. } | Expr2::InSubquery { .. } => {}
            _ => {}
        }
    }

    /// Like collect_corr_names, but uses table qualifiers to distinguish
    /// inner columns from outer correlation columns when both share the same
    /// short name (e.g. Q21's l1.l_orderkey vs l2.l_orderkey).
    fn collect_corr_names_qualified(
        &self, expr: &Expr2, outer_t: &ExecTable,
        inner_cols: &HashSet<String>,
        inner_aliases: &HashSet<String>,
        names: &mut Vec<String>, seen: &mut HashSet<String>,
    ) {
        match expr {
            Expr2::Col(name) => {
                let is_inner = if let Some(dot_pos) = name.find('.') {
                    let qualifier = name[..dot_pos].to_lowercase();
                    inner_aliases.contains(&qualifier)
                } else {
                    inner_cols.contains(&name.to_lowercase())
                };
                if !is_inner {
                    let short = name.rfind('.').map(|p| &name[p+1..]).unwrap_or(name.as_str());
                    if outer_t.lookup_col(name).is_some() || outer_t.lookup_col(short).is_some() {
                        if seen.insert(name.to_lowercase()) {
                            names.push(name.clone());
                        }
                    }
                }
            }
            Expr2::BinOp { left, right, .. } => {
                self.collect_corr_names_qualified(left, outer_t, inner_cols, inner_aliases, names, seen);
                self.collect_corr_names_qualified(right, outer_t, inner_cols, inner_aliases, names, seen);
            }
            Expr2::Case { whens, else_ } => {
                for (c, r) in whens {
                    self.collect_corr_names_qualified(c, outer_t, inner_cols, inner_aliases, names, seen);
                    self.collect_corr_names_qualified(r, outer_t, inner_cols, inner_aliases, names, seen);
                }
                if let Some(e) = else_ { self.collect_corr_names_qualified(e, outer_t, inner_cols, inner_aliases, names, seen); }
            }
            Expr2::Like { expr, pattern, .. } => {
                self.collect_corr_names_qualified(expr, outer_t, inner_cols, inner_aliases, names, seen);
                self.collect_corr_names_qualified(pattern, outer_t, inner_cols, inner_aliases, names, seen);
            }
            Expr2::Between { expr, low, high, .. } => {
                self.collect_corr_names_qualified(expr, outer_t, inner_cols, inner_aliases, names, seen);
                self.collect_corr_names_qualified(low, outer_t, inner_cols, inner_aliases, names, seen);
                self.collect_corr_names_qualified(high, outer_t, inner_cols, inner_aliases, names, seen);
            }
            Expr2::InList { expr, list, .. } => {
                self.collect_corr_names_qualified(expr, outer_t, inner_cols, inner_aliases, names, seen);
                for e in list { self.collect_corr_names_qualified(e, outer_t, inner_cols, inner_aliases, names, seen); }
            }
            Expr2::Neg(e) | Expr2::Not(e) | Expr2::Extract { expr: e, .. } => {
                self.collect_corr_names_qualified(e, outer_t, inner_cols, inner_aliases, names, seen);
            }
            Expr2::Substr { expr, start, len } => {
                self.collect_corr_names_qualified(expr, outer_t, inner_cols, inner_aliases, names, seen);
                self.collect_corr_names_qualified(start, outer_t, inner_cols, inner_aliases, names, seen);
                self.collect_corr_names_qualified(len, outer_t, inner_cols, inner_aliases, names, seen);
            }
            Expr2::Subquery(_) | Expr2::Exists { .. } | Expr2::InSubquery { .. } => {}
            _ => {}
        }
    }

    /// Find `Col(inner) = Col(outer_name)` or reverse in a WHERE expr.
    /// Returns the inner column index (in the subquery's own FROM table).
    fn find_equi_join_inner(
        &self, expr: &Expr2, outer_name: &str, inner_cols: &HashSet<String>,
        subquery: &SelectQuery2, outer_t: &ExecTable,
    ) -> Option<usize> {
        match expr {
            Expr2::BinOp { op: BinOp2::Eq, left, right } => {
                // left = inner, right = outer
                if let (Expr2::Col(ln), Expr2::Col(rn)) = (left.as_ref(), right.as_ref()) {
                    let l_short = ln.rfind('.').map(|p| &ln[p+1..]).unwrap_or(ln.as_str());
                    let r_short = rn.rfind('.').map(|p| &rn[p+1..]).unwrap_or(rn.as_str());
                    if inner_cols.contains(&l_short.to_lowercase()) && r_short.eq_ignore_ascii_case(outer_name.trim_start_matches(|c: char| !c.is_alphanumeric())) {
                        return self.resolve_inner_col_idx(ln, subquery, outer_t);
                    }
                    if inner_cols.contains(&r_short.to_lowercase()) && l_short.eq_ignore_ascii_case(outer_name.trim_start_matches(|c: char| !c.is_alphanumeric())) {
                        return self.resolve_inner_col_idx(rn, subquery, outer_t);
                    }
                }
            }
            Expr2::BinOp { op: BinOp2::And, left, right } => {
                if let Some(idx) = self.find_equi_join_inner(left, outer_name, inner_cols, subquery, outer_t) {
                    return Some(idx);
                }
                if let Some(idx) = self.find_equi_join_inner(right, outer_name, inner_cols, subquery, outer_t) {
                    return Some(idx);
                }
            }
            _ => {}
        }
        None
    }

    /// Resolve an inner column name to its index in the subquery's base table.
    /// Loads the subquery's FROM and looks up the column.
    fn resolve_inner_col_idx(
        &self, col_name: &str, subquery: &SelectQuery2, _outer_t: &ExecTable,
    ) -> Option<usize> {
        // Load the subquery's FROM tables and look up the column.
        // This is a lightweight version of resolve_from that doesn't do joins.
        for item in &subquery.from {
            if let FromItem::Table(t) = item {
                if let Some(table) = self.catalog.get(&t.name) {
                    let alias = t.alias.as_deref().unwrap_or(&t.name);
                    // Build a temp ExecTable to use lookup_col
                    let exec_t = ExecTable::from_catalog(table, alias);
                    if let Some(idx) = exec_t.lookup_col(col_name) {
                        return Some(idx);
                    }
                }
            }
        }
        None
    }

    /// Build a hash set of inner column values from the subquery's filtered
    /// result (with the correlated equi-join conjunct removed — only
    /// uncorrelated conjuncts are applied).
    ///
    /// For Q4: `SELECT DISTINCT l_orderkey FROM lineitem WHERE l_commitdate < l_receiptdate`
    fn build_exists_hashset(&self, subquery: &SelectQuery2, inner_col_idx: usize) -> Result<FxHashSet<u64>, Error> {
        // Load the subquery's FROM table(s) and join them (no correlation).
        let mut tables: Vec<ExecTable> = Vec::new();
        for item in &subquery.from {
            tables.push(self.resolve_from_item(item)?);
        }
        let base = if tables.len() == 1 {
            tables.into_iter().next().unwrap()
        } else {
            self.join_tables_smart(tables, &subquery.where_clause)?
        };
        // Apply the subquery's WHERE conjuncts, EXCEPT the correlated equi-join.
        // W2: evaluate each conjunct directly into `mask` (the simplified
        // AND/OR arms in `eval_bool_mask_vec` preserve the incoming mask).
        let mask = if let Some(ref wc) = subquery.where_clause {
            let conjuncts = self.split_conjuncts(&subquery.where_clause);
            let mut mask = vec![true; base.row_count];
            for conj in &conjuncts {
                if self.is_conjunct_correlated(conj, &base) {
                    continue;
                }
                self.eval_bool_mask_vec(conj, &base, &mut mask)?;
            }
            mask
        } else { vec![true; base.row_count] };
        // Build hash set of inner col values — PARALLEL using rayon.
        // Split into chunks, each thread builds a local HashSet, then merge.
        // This is critical for Q4 where lineitem has 6M rows and the serial
        // HashSet insertion (SipHash + hashbrown) was a top-5 hotspot.
        let col = &base.columns[inner_col_idx];
        const CHUNK_SIZE: usize = 65536;
        let n = base.row_count;
        let num_chunks = (n + CHUNK_SIZE - 1) / CHUNK_SIZE;
        let local_sets: Vec<FxHashSet<u64>> = (0..num_chunks).into_par_iter().map(|chunk_idx| {
            let start = chunk_idx * CHUNK_SIZE;
            let end = std::cmp::min(start + CHUNK_SIZE, n);
            let mut local = new_fxhashset();
            for i in start..end {
                if mask[i] {
                    local.insert(col[i]);
                }
            }
            local
        }).collect();
        // Merge local sets into final set
        let mut set = new_fxhashset();
        for local in local_sets {
            set.extend(local);
        }
        Ok(set)
    }

    /// Check if a conjunct references a column not in `base` (i.e. correlated).
    /// Uses table qualifiers to distinguish inner from outer columns.
    fn is_conjunct_correlated(&self, expr: &Expr2, base: &ExecTable) -> bool {
        match expr {
            Expr2::Col(name) => {
                if let Some(dot_pos) = name.find('.') {
                    let qualifier = name[..dot_pos].to_lowercase();
                    // If base has this qualified name, it's an inner column.
                    if base.lookup_col(name).is_some() {
                        return false;
                    }
                    // If the qualifier matches base's alias, the column is inner
                    // (even if the short name doesn't resolve — shouldn't happen).
                    if self.qualifier_matches_base(&qualifier, base) {
                        return false;
                    }
                    // Qualifier doesn't match base — it's a correlation column.
                    true
                } else {
                    // Unqualified: check if short name is in base.
                    base.lookup_col(name).is_none()
                }
            }
            Expr2::BinOp { left, right, .. } => {
                self.is_conjunct_correlated(left, base) || self.is_conjunct_correlated(right, base)
            }
            Expr2::Case { whens, else_ } => {
                whens.iter().any(|(c, r)| self.is_conjunct_correlated(c, base) || self.is_conjunct_correlated(r, base))
                    || else_.as_ref().map(|e| self.is_conjunct_correlated(e, base)).unwrap_or(false)
            }
            Expr2::Like { expr, pattern, .. } => {
                self.is_conjunct_correlated(expr, base) || self.is_conjunct_correlated(pattern, base)
            }
            Expr2::Between { expr, low, high, .. } => {
                self.is_conjunct_correlated(expr, base) || self.is_conjunct_correlated(low, base) || self.is_conjunct_correlated(high, base)
            }
            Expr2::InList { expr, list, .. } => {
                self.is_conjunct_correlated(expr, base) || list.iter().any(|e| self.is_conjunct_correlated(e, base))
            }
            Expr2::Neg(e) | Expr2::Not(e) | Expr2::Extract { expr: e, .. } => {
                self.is_conjunct_correlated(e, base)
            }
            Expr2::Substr { expr, start, len } => {
                self.is_conjunct_correlated(expr, base) || self.is_conjunct_correlated(start, base) || self.is_conjunct_correlated(len, base)
            }
            Expr2::Subquery(_) | Expr2::Exists { .. } | Expr2::InSubquery { .. } => true,
            _ => false,
        }
    }

    /// Check if a table qualifier matches any of base's column_names prefixes.
    /// The base table's col_map has entries like "alias.colname".
    /// If the qualifier matches any such prefix, it's an inner column.
    fn qualifier_matches_base(&self, qualifier: &str, base: &ExecTable) -> bool {
        for name in &base.column_names {
            // column_names don't have qualifiers — check col_map instead
        }
        // Check col_map for any key starting with "qualifier."
        let prefix = format!("{}.", qualifier);
        for key in base.col_map.keys() {
            if key.starts_with(&prefix) {
                return true;
            }
        }
        false
    }

    /// For an EXISTS subquery with 2 correlation columns, find the equi-join
    /// pair and the inequality pair. Returns (outer_eq, inner_eq, outer_neq, inner_neq).
    ///
    /// Q21 example: `exists (SELECT * FROM lineitem l2 WHERE l2.l_orderkey = l1.l_orderkey
    /// AND l2.l_suppkey <> l1.l_suppkey)` → outer_eq=l1.l_orderkey, inner_eq=l2.l_orderkey,
    /// outer_neq=l1.l_suppkey, inner_neq=l2.l_suppkey.
    fn find_exists_multi_col(&self, subquery: &SelectQuery2, outer_t: &ExecTable) -> Option<(usize, usize, usize, usize)> {
        // Build inner column name set and inner table aliases
        let mut inner_cols: HashSet<String> = new_hashset();
        let mut inner_aliases: HashSet<String> = new_hashset();
        for item in &subquery.from {
            if let FromItem::Table(t) = item {
                inner_aliases.insert(t.name.to_lowercase());
                if let Some(ref alias) = t.alias {
                    inner_aliases.insert(alias.to_lowercase());
                }
                if let Some(table) = self.catalog.get(&t.name) {
                    for cn in &table.column_names {
                        inner_cols.insert(cn.to_lowercase());
                    }
                }
            }
        }
        // Find correlation columns
        let mut corr_names: Vec<String> = Vec::new();
        let mut seen: HashSet<String> = new_hashset();
        if let Some(ref wc) = subquery.where_clause {
            self.collect_corr_names_qualified(wc, outer_t, &inner_cols, &inner_aliases, &mut corr_names, &mut seen);
        }
        if corr_names.len() != 2 {
            return None;
        }
        // Find the equi-join conjunct (Col(inner) = Col(outer))
        let wc = subquery.where_clause.as_ref()?;
        let conjuncts = self.split_conjuncts(&Some(wc.clone()));
        let mut eq_pair: Option<(usize, usize, String, String)> = None; // (outer_idx, inner_idx, outer_name, inner_name)
        let mut neq_pair: Option<(usize, usize)> = None; // (outer_idx, inner_idx)
        for conj in &conjuncts {
            // Look for Col = Col (equi-join between inner and outer)
            if let Expr2::BinOp { op: BinOp2::Eq, left: l, right: r } = conj {
                if let (Expr2::Col(ln), Expr2::Col(rn)) = (l.as_ref(), r.as_ref()) {
                    // Use qualifier to determine inner vs outer (not short name)
                    let l_is_inner = self.col_is_inner(ln, &inner_aliases, &inner_cols);
                    let r_is_inner = self.col_is_inner(rn, &inner_aliases, &inner_cols);
                    if l_is_inner != r_is_inner {
                        // One is inner, one is outer
                        let (inner_name, outer_name) = if l_is_inner { (ln.clone(), rn.clone()) } else { (rn.clone(), ln.clone()) };
                        let outer_short = outer_name.rfind('.').map(|p| &outer_name[p+1..]).unwrap_or(&outer_name).to_lowercase();
                        let outer_idx = outer_t.lookup_col(&outer_name).or_else(|| outer_t.lookup_col(&outer_short))?;
                        let inner_idx = self.resolve_inner_col_idx(&inner_name, subquery, outer_t)?;
                        if eq_pair.is_none() {
                            eq_pair = Some((outer_idx, inner_idx, outer_name.clone(), inner_name.clone()));
                        }
                    }
                }
            }
            // Look for Col <> Col (inequality between inner and outer)
            if let Expr2::BinOp { op: BinOp2::Ne, left: l, right: r } = conj {
                if let (Expr2::Col(ln), Expr2::Col(rn)) = (l.as_ref(), r.as_ref()) {
                    let l_is_inner = self.col_is_inner(ln, &inner_aliases, &inner_cols);
                    let r_is_inner = self.col_is_inner(rn, &inner_aliases, &inner_cols);
                    if l_is_inner != r_is_inner {
                        let (inner_name, outer_name) = if l_is_inner { (ln.clone(), rn.clone()) } else { (rn.clone(), ln.clone()) };
                        let outer_short = outer_name.rfind('.').map(|p| &outer_name[p+1..]).unwrap_or(&outer_name).to_lowercase();
                        let outer_idx = outer_t.lookup_col(&outer_name).or_else(|| outer_t.lookup_col(&outer_short))?;
                        let inner_idx = self.resolve_inner_col_idx(&inner_name, subquery, outer_t)?;
                        if neq_pair.is_none() {
                            neq_pair = Some((outer_idx, inner_idx));
                        }
                    }
                }
            }
        }
        let (outer_eq, inner_eq, _, _) = eq_pair?;
        let (outer_neq, inner_neq) = neq_pair?;
        Some((outer_eq, inner_eq, outer_neq, inner_neq))
    }

    /// Check if a column name refers to an inner table column.
    /// Uses the qualifier (if present) to distinguish inner from outer.
    fn col_is_inner(&self, name: &str, inner_aliases: &HashSet<String>, inner_cols: &HashSet<String>) -> bool {
        if let Some(dot_pos) = name.find('.') {
            let qualifier = name[..dot_pos].to_lowercase();
            inner_aliases.contains(&qualifier)
        } else {
            inner_cols.contains(&name.to_lowercase())
        }
    }

    /// Build HashMap<equi_key, HashSet<ineq_col>> from the subquery's inner
    /// table, applying only uncorrelated conjuncts.
    fn build_exists_multi_map(&self, subquery: &SelectQuery2, inner_eq_idx: usize, inner_neq_idx: usize) -> Result<FxHashMap<u64, FxHashSet<u64>>, Error> {
        let mut tables: Vec<ExecTable> = Vec::new();
        for item in &subquery.from {
            tables.push(self.resolve_from_item(item)?);
        }
        let base = if tables.len() == 1 {
            tables.into_iter().next().unwrap()
        } else {
            self.join_tables_smart(tables, &subquery.where_clause)?
        };
        // W2: evaluate each conjunct directly into `mask` (the simplified
        // AND/OR arms in `eval_bool_mask_vec` preserve the incoming mask).
        let mask = if let Some(ref wc) = subquery.where_clause {
            let conjuncts = self.split_conjuncts(&subquery.where_clause);
            let mut mask = vec![true; base.row_count];
            for conj in &conjuncts {
                if self.is_conjunct_correlated(conj, &base) { continue; }
                self.eval_bool_mask_vec(conj, &base, &mut mask)?;
            }
            mask
        } else { vec![true; base.row_count] };
        let eq_col = &base.columns[inner_eq_idx];
        let neq_col = &base.columns[inner_neq_idx];
        // Build HashMap<equi_key, HashSet<ineq_col>> — PARALLEL using rayon.
        // Each chunk builds a local HashMap, then merge by extending sets.
        const CHUNK_SIZE: usize = 65536;
        let n = base.row_count;
        let num_chunks = (n + CHUNK_SIZE - 1) / CHUNK_SIZE;
        let local_maps: Vec<FxHashMap<u64, FxHashSet<u64>>> = (0..num_chunks).into_par_iter().map(|chunk_idx| {
            let start = chunk_idx * CHUNK_SIZE;
            let end = std::cmp::min(start + CHUNK_SIZE, n);
            let mut local: FxHashMap<u64, FxHashSet<u64>> = new_fxhashmap();
            for i in start..end {
                if mask[i] {
                    local.entry(eq_col[i]).or_default().insert(neq_col[i]);
                }
            }
            local
        }).collect();
        // Merge local maps into final map
        let mut map: FxHashMap<u64, FxHashSet<u64>> = new_fxhashmap();
        for local in local_maps {
            for (k, v) in local {
                map.entry(k).or_default().extend(v);
            }
        }
        Ok(map)
    }

    /// Estimate the number of distinct values in a column using a sampling
    /// approach: hash the first min(n, 10000) values into 256 buckets and
    /// count non-empty buckets. This is fast and good enough for join ordering.
    fn estimate_distinct(&self, col: &[u64], n: usize) -> u64 {
        if n == 0 { return 0; }
        let sample_size = n.min(10000);
        let mut buckets = [false; 256];
        for i in 0..sample_size {
            let h = crate::exec::join_hash_table::JoinHashTable::hash(col[i]);
            buckets[(h % 256) as usize] = true;
        }
        let filled = buckets.iter().filter(|&&b| b).count() as u64;
        // W29: Linear counting estimator (Whang et al. 1990):
        //   D ≈ -m * ln(1 - filled/m)  where m = 256 (buckets)
        // Much more accurate than the old 'filled * 40' heuristic for
        // low-cardinality columns (e.g. nationkey: true=5, old=200, new=5).
        // This fixes the join-ordering bug where customer⋈supplier (12M output)
        // was chosen over supplier⋈lineitem (1.2M output) because the
        // cardinality estimate was 40× too low.
        if filled >= 256 {
            // All buckets filled — linear counting diverges.
            // Use sample_size as a lower bound (the column has at least
            // this many distinct values in the sample).
            (sample_size as u64).min(n as u64)
        } else {
            let m = 256.0f64;
            let f = filled as f64;
            let estimate = -m * (1.0 - f / m).ln();
            estimate.round() as u64
        }
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

        // Join tables using cardinality-aware ordering.
        // Start with the smallest filtered table (e.g., region after r_name='ASIA'
        // has 1 row). This prevents many-to-many explosions like customer ⋈ supplier.
        // Pick the table with the fewest rows that has at least one join key
        // to another table.
        let mut start_idx = 0;
        let mut start_rows = usize::MAX;
        for (i, t) in tables.iter().enumerate() {
            if t.row_count < start_rows {
                // Check if this table can join to at least one other table
                let mut has_join = false;
                for (j, other) in tables.iter().enumerate() {
                    if i == j { continue; }
                    if !self.find_join_keys(t, other, &conjuncts).is_empty() {
                        has_join = true;
                        break;
                    }
                }
                if has_join {
                    start_idx = i;
                    start_rows = t.row_count;
                }
            }
        }
        let mut joined = tables.remove(start_idx);
        while !tables.is_empty() {
            let mut best_idx = 0;
            let mut best_keys: Vec<JoinKey2> = Vec::new();
            let mut best_est_output: u64 = u64::MAX;
            for (i, table) in tables.iter().enumerate() {
                let keys = self.find_join_keys(&joined, table, &conjuncts);
                if keys.is_empty() { continue; }
                // Estimate output cardinality.
                // For each join key pair (left_col, right_col), estimate:
                //   output ≈ left_rows * right_rows / max(distinct_left, distinct_right)
                // Use the max distinct across all keys (conservative).
                let mut est_output: u64 = 1;
                for k in &keys {
                    let dl = self.estimate_distinct(&joined.columns[k.left][..], joined.row_count);
                    let dr = self.estimate_distinct(&table.columns[k.right][..], table.row_count);
                    let max_d = dl.max(dr).max(1);
                    // Join cardinality formula (Selinger-style):
                    // |R ⋈ S| ≈ |R| * |S| / max(V(R,k), V(S,k))
                    est_output = est_output
                        .saturating_mul(joined.row_count as u64)
                        .saturating_mul(table.row_count as u64)
                        / max_d;
                }
                if est_output < best_est_output {
                    best_est_output = est_output;
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

    fn hash_join_with_keys(
        &self,
        left: ExecTable,
        right: ExecTable,
        keys: &[JoinKey2],
        jt: JoinType2,
    ) -> Result<ExecTable, Error> {
        use xxhash_rust::xxh3::xxh3_64;
        use crate::exec::join_hash_table::JoinHashTable;
        use crate::exec::bloom_filter::BloomFilter;

        // Decide which side to build the hash table on (smaller side).
        // For INNER joins, we can swap freely. For LEFT joins, we must
        // keep left as the probe side (to preserve unmatched left rows).
        let can_swap = jt == JoinType2::Inner;
        let (build_side, probe_side, build_keys, probe_keys, swapped) =
            if can_swap && left.row_count < right.row_count {
                // Build on left, probe with right — swap the key indices.
                let bk: Vec<JoinKey2> = keys.iter().map(|k| JoinKey2 { left: k.left, right: k.left }).collect();
                let pk: Vec<JoinKey2> = keys.iter().map(|k| JoinKey2 { left: k.right, right: k.right }).collect();
                (&left, &right, bk, pk, true)
            } else {
                // Build on right (original behavior), probe with left.
                let bk: Vec<JoinKey2> = keys.iter().map(|k| JoinKey2 { left: k.right, right: k.right }).collect();
                let pk: Vec<JoinKey2> = keys.iter().map(|k| JoinKey2 { left: k.left, right: k.left }).collect();
                (&right, &left, bk, pk, false)
            };

        let ncol = left.columns.len() + right.columns.len();

        // --- Build phase: construct hash table AND Wilson-loop bloom filter ---
        // Single-key fast path: use JoinHashTable (CedarDB-style bloom-tagged
        // chaining with CRC32 hashing — 10x faster probe than HashMap).
        // Multi-key path: pack keys into a single u64 via xxh3, then use JoinHashTable.
        //
        // W29 (TQFT Wilson loop / Frobenius μ): also build a separate
        // BloomFilter from the same build-side keys. The JoinHashTable's
        // 16-bit directory tag is selective (FPR 1/65536) but lives in
        // L2/L3 because the directory is 16 bytes/slot. The separate
        // BloomFilter is ~1% FPR but 10 bits/item — 5-10× smaller, so
        // it lives in L1. For selective joins (e.g. Q5's region='ASIA'
        // filter narrows to 1 nation, then supplier=10K, then ~7K final),
        // 90%+ of probe keys are absent — the L1 bloom check lets us
        // skip the L2 directory probe entirely for those keys.
        let mut build_hash = JoinHashTable::new(build_side.row_count);
        let mut bloom = BloomFilter::new(build_side.row_count);
        if keys.len() == 1 {
            let bk0 = build_keys[0].left;
            for r in 0..build_side.row_count {
                let k = build_side.columns[bk0][r];
                build_hash.insert(k, r as u32);
                bloom.insert(k);
            }
        } else {
            // Multi-key: hash all key columns into a single u64 via xxh3.
            let bk_cols: Vec<usize> = build_keys.iter().map(|k| k.left).collect();
            for r in 0..build_side.row_count {
                let mut buf = [0u8; 64];
                let mut off = 0;
                for &kc in &bk_cols {
                    let v = build_side.columns[kc][r];
                    let bytes = v.to_le_bytes();
                    if off + 8 <= 64 {
                        buf[off..off + 8].copy_from_slice(&bytes);
                        off += 8;
                    }
                }
                let key = xxh3_64(&buf[..off]);
                build_hash.insert(key, r as u32);
                bloom.insert(key);
            }
        };

        // --- Probe phase (PARALLEL morsel-driven) ---
        // Split the probe side into chunks, each thread probes independently
        // and produces its own output columns. Merge at the end by concatenation.
        // This is critical for Q3/Q5/Q7/Q18 where the probe side is large
        // (6M+ rows for lineitem joins) and the build side is small.
        // Each thread gets its own output buffers to avoid contention.
        let est_output = std::cmp::max(probe_side.row_count, build_side.row_count).min(4_000_000);
        let mut out_types = left.col_types.clone();
        out_types.extend(right.col_types.iter().copied());
        let out_strings: Vec<Option<std::sync::Arc<StringSearchColumn>>> = (0..ncol).map(|_| None).collect();
        let mut out_names = left.column_names.clone();
        out_names.extend(right.column_names.clone());

        let left_ncol = left.columns.len();
        let pk_cols: Vec<usize> = probe_keys.iter().map(|k| k.left).collect();

        // Parallel probe using rayon. Each chunk produces its own output cols.
        // The build_hash and bloom are shared (read-only) across threads.
        const CHUNK_SIZE: usize = 65536;
        let probe_row_count = probe_side.row_count;
        let num_chunks = (probe_row_count + CHUNK_SIZE - 1) / CHUNK_SIZE;

        // Parallel probe using rayon. Each chunk produces its own output cols.
        // Optimized: use unsafe set_len + ptr write to avoid per-push capacity
        // checks (the compiler can't elide them due to potential reallocation).
        let partial_results: Vec<Vec<Vec<u64>>> = (0..num_chunks).into_par_iter().map(|chunk_idx| {
            let start = chunk_idx * CHUNK_SIZE;
            let end = std::cmp::min(start + CHUNK_SIZE, probe_row_count);

            let mut local_out: Vec<Vec<u64>> = (0..ncol).map(|_| Vec::with_capacity(CHUNK_SIZE * 2)).collect();
            let mut matched_rows: Vec<u32> = Vec::with_capacity(16);

            // W1-B: Software prefetch distance (rows ahead). Literature default
            // for hash-join probes is 8-32; tuned to K=8 on TPC-H (best total
            // of 3 distances tested: K=8 total=11093, K=16 total=11224, K=32 total=11174).
            const PREFETCH_DIST: usize = 8;
            for p in start..end {
                // W1-B: Prefetch the hash-table directory slot AND bloom
                // filter bits for the probe key PREFETCH_DIST rows ahead.
                // This hides the ~100-cycle L3 miss on the next random
                // directory access (Q21's #1 hot spot at 23.68% of runtime).
                //
                // The probe-side column load for next_key is sequential
                // (hardware-prefetched), so the only added cost is the
                // prefetch instruction itself (~1 cycle each).
                #[cfg(target_arch = "x86_64")]
                if p + PREFETCH_DIST < end {
                    let next_p = p + PREFETCH_DIST;
                    let next_key = if keys.len() == 1 {
                        probe_side.columns[pk_cols[0]][next_p]
                    } else {
                        let mut nbuf = [0u8; 64];
                        let mut noff = 0;
                        for &kc in &pk_cols {
                            let nv = probe_side.columns[kc][next_p];
                            let nbytes = nv.to_le_bytes();
                            if noff + 8 <= 64 {
                                nbuf[noff..noff + 8].copy_from_slice(&nbytes);
                                noff += 8;
                            }
                        }
                        xxh3_64(&nbuf[..noff])
                    };
                    build_hash.prefetch_directory(next_key);
                    bloom.prefetch(next_key);
                }

                let probe_key = if keys.len() == 1 {
                    probe_side.columns[pk_cols[0]][p]
                } else {
                    let mut buf = [0u8; 64];
                    let mut off = 0;
                    for &kc in &pk_cols {
                        let v = probe_side.columns[kc][p];
                        let bytes = v.to_le_bytes();
                        if off + 8 <= 64 {
                            buf[off..off + 8].copy_from_slice(&bytes);
                            off += 8;
                        }
                    }
                    xxh3_64(&buf[..off])
                };

                if !bloom.might_contain(probe_key) {
                    if jt == JoinType2::Left && !swapped {
                        for (c, col) in left.columns.iter().enumerate() { local_out[c].push(col[p]); }
                        for c in 0..right.columns.len() { local_out[left_ncol + c].push(0); }
                    }
                    continue;
                }
                build_hash.probe_all(probe_key, &mut matched_rows);
                if matched_rows.is_empty() {
                    if jt == JoinType2::Left && !swapped {
                        for (c, col) in left.columns.iter().enumerate() { local_out[c].push(col[p]); }
                        for c in 0..right.columns.len() { local_out[left_ncol + c].push(0); }
                    }
                } else {
                    // Pre-compute left column values for this probe row (shared across all matches).
                    // This avoids re-reading left.columns for each match.
                    let left_vals: Vec<u64> = if !swapped {
                        left.columns.iter().map(|col| col[p]).collect()
                    } else {
                        Vec::new()
                    };
                    let right_vals_template: Vec<u64> = if swapped {
                        right.columns.iter().map(|col| col[p]).collect()
                    } else {
                        Vec::new()
                    };
                    for &b in &matched_rows {
                        let b = b as usize;
                        if !swapped {
                            // Left cols from probe (same for all matches), right cols from build.
                            for (c, &v) in left_vals.iter().enumerate() { local_out[c].push(v); }
                            for (c, col) in right.columns.iter().enumerate() { local_out[left_ncol + c].push(col[b]); }
                        } else {
                            // Left cols from build, right cols from probe (same for all matches).
                            for (c, col) in left.columns.iter().enumerate() { local_out[c].push(col[b]); }
                            for (c, &v) in right_vals_template.iter().enumerate() { local_out[left_ncol + c].push(v); }
                        }
                    }
                }
            }
            local_out
        }).collect();

        // Merge: pre-calculate total size to avoid reallocation.
        let total_rows: usize = partial_results.iter().map(|r| r.first().map(|c| c.len()).unwrap_or(0)).sum();
        let mut out_cols: Vec<Vec<u64>> = (0..ncol).map(|_| Vec::with_capacity(total_rows)).collect();
        for local_out in partial_results {
            for c in 0..ncol {
                out_cols[c].extend_from_slice(&local_out[c]);
            }
        }
        let row_count = out_cols.first().map(|c| c.len()).unwrap_or(0);

        let mut col_map = new_hashmap();
        for (i, name) in out_names.iter().enumerate() {
            col_map.entry(name.to_lowercase()).or_insert(i);
        }
        for (k, v) in &left.col_map {
            col_map.insert(k.clone(), *v);
        }
        let off = left.columns.len();
        for (k, v) in &right.col_map {
            col_map.insert(k.clone(), *v + off);
        }
        Ok(ExecTable {
            columns: out_cols.into_iter().map(std::sync::Arc::new).collect(),
            column_names: out_names,
            col_types: out_types,
            string_columns: out_strings,
            row_count,
            col_map,
        })
    }

    fn filter_table(&self, table: &ExecTable, indices: &[usize]) -> ExecTable {
        let mut new_cols = Vec::with_capacity(table.columns.len());
        for col in &table.columns {
            new_cols.push(std::sync::Arc::new(indices.iter().map(|&i| col[i]).collect()));
        }
        // Rebuild string columns if present
        let mut new_strings = Vec::with_capacity(table.string_columns.len());
        for (i, sc) in table.string_columns.iter().enumerate() {
            if let Some(ref scol) = sc {
                let strings: Vec<String> = indices.iter().map(|&r| scol.get(r).to_string()).collect();
                new_strings.push(Some(std::sync::Arc::new(StringSearchColumn::new(strings))));
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
        let mut refs = new_hashset();
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
            Expr2::Subquery(_) | Expr2::Exists { .. } | Expr2::InSubquery { .. } => {
                // Correlated subqueries can reference any outer table.
                // Mark ALL tables as referenced so this expression is NOT
                // applied as a single-table filter before the join.
                for i in 0..tables.len() {
                    refs.insert(i);
                }
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
    /// Also handles OR of conjunctive groups (e.g. Q19): if all OR branches
    /// share the same equi-join key, it is extracted and used for the join.
    /// The OR is then applied as a post-join filter.
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
            // Handle OR: extract common equi-join keys from all branches.
            // E.g. Q19: (p_partkey = l_partkey AND ...) OR (p_partkey = l_partkey AND ...) OR ...
            // The common key p_partkey = l_partkey is used for the join.
            if let Expr2::BinOp { op: BinOp2::Or, .. } = conj {
                let or_keys = self.find_or_common_keys(conj, left, right);
                keys.extend(or_keys);
            }
        }
        keys
    }

    /// Find equi-join keys common to ALL branches of an OR expression.
    /// Collects all OR branches, finds equi-join keys in each, and returns
    /// the intersection.
    fn find_or_common_keys(&self, or_expr: &Expr2, left: &ExecTable, right: &ExecTable) -> Vec<JoinKey2> {
        // Collect all OR branches (flatten nested ORs)
        let mut branches: Vec<&Expr2> = Vec::new();
        self.collect_or_branches(or_expr, &mut branches);
        if branches.is_empty() { return Vec::new(); }
        // For each branch, split into AND-conjuncts and find equi-join keys
        let mut branch_keys: Vec<Vec<JoinKey2>> = Vec::new();
        for branch in &branches {
            let conjuncts = self.split_conjuncts_for_or(branch);
            let keys = self.find_join_keys_direct(left, right, &conjuncts);
            branch_keys.push(keys);
        }
        // Intersect: a key must appear in ALL branches (by left,right indices)
        let mut result = Vec::new();
        for key in &branch_keys[0] {
            if branch_keys.iter().all(|bk| bk.contains(key)) {
                result.push(*key);
            }
        }
        result
    }

    fn collect_or_branches<'b>(&self, expr: &'b Expr2, out: &mut Vec<&'b Expr2>) {
        match expr {
            Expr2::BinOp { op: BinOp2::Or, left, right } => {
                self.collect_or_branches(left, out);
                self.collect_or_branches(right, out);
            }
            _ => out.push(expr),
        }
    }

    fn split_conjuncts_for_or(&self, expr: &Expr2) -> Vec<Expr2> {
        let mut result = Vec::new();
        self.collect_conjuncts(expr, &mut result);
        result
    }

    /// Direct equi-join key finder (no OR handling, used by find_or_common_keys).
    fn find_join_keys_direct(&self, left: &ExecTable, right: &ExecTable, conjuncts: &[Expr2]) -> Vec<JoinKey2> {
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
        let mut col_map = new_hashmap();
        let mut column_names = Vec::new();
        let mut columns = Vec::new();
        let mut col_types = Vec::new();
        let mut string_columns = Vec::new();
        for (i, col) in result.columns.iter().enumerate() {
            column_names.push(col.name.clone());
            columns.push(std::sync::Arc::new(col.values.clone()));
            col_types.push(self.infer_result_type(&col.name, &col.values));
            string_columns.push(None);
            let lower = col.name.to_lowercase();
            col_map.entry(col.name.to_lowercase()).or_insert(i);
            col_map.entry(format!("{}.{}", alias.to_lowercase(), col.name.to_lowercase())).or_insert(i);
        }
        Ok(ExecTable { columns, column_names, col_types, string_columns, row_count: result.row_count, col_map })
    }

    fn infer_result_type(&self, name: &str, values: &[u64]) -> ColType {
        let l = name.to_lowercase();
        // Date columns
        if l.contains("date") || l.contains("shipdate") || l.contains("commitdate") || l.contains("receiptdate")
        { return ColType::Date; }
        // String columns (common in TPC-H SELECT aliases)
        if l == "n_name" || l == "supp_nation" || l == "cust_nation" || l == "nation"
            || l == "s_name" || l == "c_name" || l == "p_mfgr" || l == "p_brand" || l == "p_type"
            || l == "p_container" || l == "l_returnflag" || l == "l_linestatus"
            || l == "l_shipmode" || l == "l_shipinstruct" || l == "o_orderpriority"
            || l == "o_orderstatus" || l == "cntrycode"
        { return ColType::String; }
        // Known integer columns (key columns, counts, years, codes)
        if l.contains("year") || l.contains("count") || l.contains("custdist")
            || l.contains("partkey") || l.contains("suppkey")
            || l.contains("custkey") || l.contains("nationkey") || l.contains("regionkey")
            || l.contains("numwait") || l.contains("numcust")
            || l.contains("supplier_cnt") || l.contains("availqty") || l.contains("size")
            || l == "c_count" || l == "supplier_no" || l == "order_count"
        { return ColType::Int; }
        // Heuristic: inspect actual values to distinguish Int from Float.
        // If all sampled non-zero values are "small" (< 2^32) AND none of them,
        // when interpreted as f64 bits, look like normal float values, then
        // the column contains raw integer values (e.g., an aliased GROUP BY key
        // like `l_suppkey AS supplier_no`). Float aggregations (sum/avg) always
        // produce normal-range f64 values, so this heuristic is safe.
        let sample: Vec<u64> = values.iter().take(16).copied().filter(|&v| v != 0).collect();
        if !sample.is_empty() {
            let all_small_int = sample.iter().all(|&v| v < (1u64 << 32));
            let any_normal_float = sample.iter().any(|&v| {
                let f = f64::from_bits(v);
                f.is_normal() && f.abs() >= 1e-3 && f.abs() <= 1e20
            });
            if all_small_int && !any_normal_float {
                return ColType::Int;
            }
        }
        ColType::Float
    }

    // --- Cross join ---

    fn cross_join(&self, left: ExecTable, right: ExecTable) -> ExecTable {
        let lr = left.row_count;
        let rr = right.row_count;
        if lr == 0 || rr == 0 {
            return ExecTable {
                columns: left.columns.iter().chain(right.columns.iter()).map(|_| std::sync::Arc::new(Vec::new())).collect(),
                column_names: left.column_names.iter().chain(right.column_names.iter()).cloned().collect(),
                col_types: left.col_types.iter().chain(right.col_types.iter()).copied().collect(),
                string_columns: left.string_columns.iter().chain(right.string_columns.iter()).cloned().collect(),
                row_count: 0,
                col_map: new_hashmap(),
            };
        }
        let total = lr * rr;
        let mut columns = Vec::with_capacity(left.columns.len() + right.columns.len());
        for col in &left.columns {
            let mut nc = Vec::with_capacity(total);
            for l in 0..lr { let v = col[l]; for _ in 0..rr { nc.push(v); } }
            columns.push(std::sync::Arc::new(nc));
        }
        for col in &right.columns {
            let mut nc = Vec::with_capacity(total);
            for _ in 0..lr { for r in 0..rr { nc.push(col[r]); } }
            columns.push(std::sync::Arc::new(nc));
        }
        let mut col_types = left.col_types.clone();
        col_types.extend(right.col_types.iter().copied());
        // String columns are NOT rebuilt after cross join — set to None.
        let string_columns: Vec<Option<std::sync::Arc<StringSearchColumn>>> = (0..(left.columns.len() + right.columns.len())).map(|_| None).collect();
        let mut column_names = left.column_names.clone();
        column_names.extend(right.column_names.clone());
        let mut col_map = new_hashmap();
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

        // Split ON into equi-join keys and non-equi-join conjuncts.
        // Non-equi-join conjuncts (LIKE, IN, <, >, etc.) are applied per-match
        // during the join — this ensures LEFT JOIN emits unmatched left rows
        // when all matches are filtered out by the non-equi-join conditions.
        let on_conjuncts = self.split_conjuncts(&Some(on.clone()));
        let non_equi: Vec<Expr2> = on_conjuncts.iter().filter(|c| {
            !matches!(c, Expr2::BinOp { op: BinOp2::Eq, left, right }
                if matches!(left.as_ref(), Expr2::Col(_)) && matches!(right.as_ref(), Expr2::Col(_)))
        }).cloned().collect();

        let mut build: HashMap<Vec<u64>, Vec<usize>> = new_hashmap();
        for r in 0..right.row_count {
            let key: Vec<u64> = keys.iter().map(|k| right.columns[k.right][r]).collect();
            build.entry(key).or_default().push(r);
        }

        let ncol = left.columns.len() + right.columns.len();
        let mut out_cols: Vec<Vec<u64>> = (0..ncol).map(|_| Vec::new()).collect();
        let mut out_types = left.col_types.clone();
        out_types.extend(right.col_types.iter().copied());
        // String columns are NOT rebuilt after join — see hash_join_with_keys.
        let out_strings: Vec<Option<std::sync::Arc<StringSearchColumn>>> = (0..ncol).map(|_| None).collect();
        let mut out_names = left.column_names.clone();
        out_names.extend(right.column_names.clone());
        let mut row_count = 0;

        let left_ncol = left.columns.len();

        // Pre-build the combined col_map once (reused per match).
        let combined_col_map: HashMap<String, usize> = {
            let mut m = new_hashmap();
            for (i, name) in out_names.iter().enumerate() {
                m.entry(name.to_lowercase()).or_insert(i);
            }
            for (k, v) in &left.col_map { m.insert(k.clone(), *v); }
            let off = left_ncol;
            for (k, v) in &right.col_map { m.insert(k.clone(), *v + off); }
            m
        };

        // Build a single combined row (left[l] + right[r]) for non-equi-join eval.
        // We do this per match — non_equi is usually short (1-2 conjuncts).
        for l in 0..left.row_count {
            let key: Vec<u64> = keys.iter().map(|k| left.columns[k.left][l]).collect();
            let matches = build.get(&key).cloned().unwrap_or_default();
            if matches.is_empty() {
                if jt == JoinType2::Left {
                    for (c, col) in left.columns.iter().enumerate() { out_cols[c].push(col[l]); }
                    for c in 0..right.columns.len() { out_cols[left_ncol + c].push(0); }
                    row_count += 1;
                }
            } else {
                let mut any_match_passed = false;
                for r in &matches {
                    // Apply non-equi-join conjuncts per match.
                    if !non_equi.is_empty() {
                        if !self.eval_non_equi_match(&non_equi, &left, l, &right, *r, &out_names, &out_types, &combined_col_map, left_ncol, ncol)? {
                            continue;
                        }
                    }
                    any_match_passed = true;
                    for (c, col) in left.columns.iter().enumerate() { out_cols[c].push(col[l]); }
                    for (c, col) in right.columns.iter().enumerate() { out_cols[left_ncol + c].push(col[*r]); }
                    row_count += 1;
                }
                // For LEFT JOIN: if no matches passed the non-equi-join filter,
                // emit unmatched left row.
                if !any_match_passed && jt == JoinType2::Left {
                    for (c, col) in left.columns.iter().enumerate() { out_cols[c].push(col[l]); }
                    for c in 0..right.columns.len() { out_cols[left_ncol + c].push(0); }
                    row_count += 1;
                }
            }
        }

        let mut col_map = new_hashmap();
        for (i, name) in out_names.iter().enumerate() { col_map.entry(name.to_lowercase()).or_insert(i); }
        for (k, v) in &left.col_map { col_map.insert(k.clone(), *v); }
        let off = left.columns.len();
        for (k, v) in &right.col_map { col_map.insert(k.clone(), *v + off); }

        Ok(ExecTable { columns: out_cols.into_iter().map(std::sync::Arc::new).collect(), column_names: out_names, col_types: out_types, string_columns: out_strings, row_count, col_map })
    }

    /// Evaluate non-equi-join conjuncts for a single (left[l], right[r]) match.
    /// Returns true if all conjuncts pass.
    ///
    /// For conjuncts that only reference right columns, eval on right at row r
    /// (preserves string_columns for LIKE/NOT LIKE).
    /// For conjuncts that reference both tables, build a combined row.
    fn eval_non_equi_match(
        &self, non_equi: &[Expr2],
        left: &ExecTable, l: usize,
        right: &ExecTable, r: usize,
        out_names: &[String], out_types: &[ColType],
        combined_col_map: &HashMap<String, usize>,
        left_ncol: usize, ncol: usize,
    ) -> Result<bool, Error> {
        for conj in non_equi {
            // Check if this conjunct only references right columns
            let refs_left = self.expr_refs_table(conj, left);
            let refs_right = self.expr_refs_table(conj, right);
            let pass = if refs_right && !refs_left {
                // Only right columns — eval on right table at row r
                let v = self.eval(conj, right, r)?;
                self.truthy(&v)
            } else if refs_left && !refs_right {
                // Only left columns — eval on left table at row l
                let v = self.eval(conj, left, l)?;
                self.truthy(&v)
            } else {
                // Both tables — build combined row
                let mut combined_cols: Vec<u64> = Vec::with_capacity(ncol);
                for (c, col) in left.columns.iter().enumerate() { combined_cols.push(col[l]); }
                for (c, col) in right.columns.iter().enumerate() { combined_cols.push(col[r]); }
                // Build a mini StringSearchColumn for the right's string at row r
                let mut combined_strings: Vec<Option<std::sync::Arc<StringSearchColumn>>> = (0..left_ncol).map(|_| None).collect();
                for sc in &right.string_columns {
                    if let Some(ref scol) = sc {
                        if scol.len() > r {
                            combined_strings.push(Some(std::sync::Arc::new(
                                StringSearchColumn::new(vec![scol.get(r).to_string()])
                            )));
                        } else {
                            combined_strings.push(None);
                        }
                    } else {
                        combined_strings.push(None);
                    }
                }
                let combined_t = ExecTable {
                    columns: combined_cols.iter().map(|v| std::sync::Arc::new(vec![*v])).collect(),
                    column_names: out_names.to_vec(),
                    col_types: out_types.to_vec(),
                    string_columns: combined_strings,
                    row_count: 1,
                    col_map: combined_col_map.clone(),
                };
                let v = self.eval(conj, &combined_t, 0)?;
                self.truthy(&v)
            };
            if !pass { return Ok(false); }
        }
        Ok(true)
    }

    /// Check if an expression references any column in the given table.
    fn expr_refs_table(&self, expr: &Expr2, table: &ExecTable) -> bool {
        match expr {
            Expr2::Col(name) => {
                let short = name.rfind('.').map(|p| &name[p+1..]).unwrap_or(name.as_str());
                table.lookup_col(name).is_some() || table.lookup_col(short).is_some()
            }
            Expr2::BinOp { left, right, .. } => {
                self.expr_refs_table(left, table) || self.expr_refs_table(right, table)
            }
            Expr2::Case { whens, else_ } => {
                whens.iter().any(|(c, r)| self.expr_refs_table(c, table) || self.expr_refs_table(r, table))
                    || else_.as_ref().map(|e| self.expr_refs_table(e, table)).unwrap_or(false)
            }
            Expr2::Like { expr, pattern, .. } => {
                self.expr_refs_table(expr, table) || self.expr_refs_table(pattern, table)
            }
            Expr2::Between { expr, low, high, .. } => {
                self.expr_refs_table(expr, table) || self.expr_refs_table(low, table) || self.expr_refs_table(high, table)
            }
            Expr2::InList { expr, list, .. } => {
                self.expr_refs_table(expr, table) || list.iter().any(|e| self.expr_refs_table(e, table))
            }
            Expr2::Neg(e) | Expr2::Not(e) | Expr2::Extract { expr: e, .. } => {
                self.expr_refs_table(e, table)
            }
            Expr2::Substr { expr, start, len } => {
                self.expr_refs_table(expr, table) || self.expr_refs_table(start, table) || self.expr_refs_table(len, table)
            }
            _ => false,
        }
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
        // Try vectorized fast path first; fall back to per-row eval.
        let mut mask = vec![true; table.row_count];
        self.eval_bool_mask_vec(expr, table, &mut mask)?;
        Ok(mask)
    }

    // --- W1-D: Q7 nation-pair LUT fast path ---

    /// Flatten an OR tree (left-associative `OR(OR(a, b), c)`) into a list
    /// of disjunct leaf expressions (by reference).
    fn flatten_disjuncts<'e>(expr: &'e Expr2, out: &mut Vec<&'e Expr2>) {
        match expr {
            Expr2::BinOp { op: BinOp2::Or, left, right } => {
                Self::flatten_disjuncts(left, out);
                Self::flatten_disjuncts(right, out);
            }
            _ => out.push(expr),
        }
    }

    /// Flatten an AND tree into a list of conjunct leaf expressions (by reference).
    fn flatten_conjuncts<'e>(expr: &'e Expr2, out: &mut Vec<&'e Expr2>) {
        match expr {
            Expr2::BinOp { op: BinOp2::And, left, right } => {
                Self::flatten_conjuncts(left, out);
                Self::flatten_conjuncts(right, out);
            }
            _ => out.push(expr),
        }
    }

    /// Extract `(col_name, str_value)` from a `Col == Str` or `Str == Col`
    /// equality. Returns `None` for any other shape.
    fn extract_col_str_eq(expr: &Expr2) -> Option<(&str, &str)> {
        if let Expr2::BinOp { op: BinOp2::Eq, left, right } = expr {
            match (left.as_ref(), right.as_ref()) {
                (Expr2::Col(c), Expr2::Str(s)) => Some((c.as_str(), s.as_str())),
                (Expr2::Str(s), Expr2::Col(c)) => Some((c.as_str(), s.as_str())),
                _ => None,
            }
        } else {
            None
        }
    }

    /// W1-D: Fast path for the TPC-H Q7 nation-pair filter pattern.
    ///
    /// Detects an OR-of-ANDs where every disjunct is a conjunction of
    /// `Col == Str` equalities referencing the **same two string columns**.
    /// The canonical Q7 shape is the symmetric pair:
    ///
    /// ```text
    /// (c1 == 'FRANCE' AND c2 == 'GERMANY')
    /// OR
    /// (c1 == 'GERMANY' AND c2 == 'FRANCE')
    /// ```
    ///
    /// but the implementation also handles non-symmetric multi-pair ORs
    /// (e.g. `(FRANCE,GERMANY) OR (FRANCE,ROMANIA) OR (FRANCE,RUSSIA)`).
    ///
    /// Replaces the generic OR evaluator — which allocates 2 `Vec<bool>`
    /// masks per OR arm plus 1 `Bitmap` per leaf equality and makes 8
    /// passes over the row data — with a single tight loop doing 2 column
    /// loads + N pair checks per row. For Q7 (~1.7M post-join rows x 2
    /// pairs) this eliminates ~6 MB of temporary allocations and collapses
    /// 8 passes into 1.
    ///
    /// Returns `Ok(true)` if the fast path was applied. Returns `Ok(false)`
    /// if the pattern did not match (caller falls back to the generic OR
    /// evaluator). Returns `Err` only if the pattern matched but evaluation
    /// failed (does not happen in the current implementation).
    fn try_nation_pair_or_lut(
        &self,
        or_expr: &Expr2,
        t: &ExecTable,
        mask: &mut [bool],
    ) -> Result<bool, Error> {
        use xxhash_rust::xxh3::xxh3_64;

        // 1. Flatten the OR tree into disjuncts.
        let mut disjuncts: Vec<&Expr2> = Vec::new();
        Self::flatten_disjuncts(or_expr, &mut disjuncts);
        if disjuncts.is_empty() { return Ok(false); }

        // 2. For each disjunct, extract (col_idx, str_hash) pairs.
        //    All disjuncts must reference exactly 2 columns and the same 2 columns.
        let mut col_a: Option<usize> = None;
        let mut col_b: Option<usize> = None;
        let mut pairs: Vec<(u64, u64)> = Vec::with_capacity(disjuncts.len());

        for disj in &disjuncts {
            let mut conjuncts: Vec<&Expr2> = Vec::new();
            Self::flatten_conjuncts(disj, &mut conjuncts);
            // Each conjunct must be a Col==Str equality.
            let mut col_a_hash: Option<u64> = None;
            let mut col_b_hash: Option<u64> = None;
            for conj in &conjuncts {
                let (col_name, str_val) = match Self::extract_col_str_eq(conj) {
                    Some(v) => v,
                    None => return Ok(false),
                };
                let cidx = match t.lookup_col(col_name) {
                    Some(i) => i,
                    None => return Ok(false),
                };
                if cidx >= t.col_types.len() || t.col_types[cidx] != ColType::String {
                    return Ok(false);
                }
                let h = xxh3_64(str_val.as_bytes());
                match (col_a, col_b) {
                    (None, None) => {
                        col_a = Some(cidx);
                        col_a_hash = Some(h);
                    }
                    (Some(a), None) => {
                        if cidx == a {
                            // Disjunct has 2 eqs on the same column - not the
                            // 2-column pair pattern we optimize.
                            return Ok(false);
                        }
                        col_b = Some(cidx);
                        col_b_hash = Some(h);
                    }
                    (Some(a), Some(b)) => {
                        if cidx == a { col_a_hash = Some(h); }
                        else if cidx == b { col_b_hash = Some(h); }
                        else { return Ok(false); }  // references a 3rd column
                    }
                    _ => unreachable!(),
                }
            }
            match (col_a_hash, col_b_hash) {
                (Some(ha), Some(hb)) => pairs.push((ha, hb)),
                _ => return Ok(false),  // disjunct didn't reference both columns
            }
        }

        let (ca, cb) = match (col_a, col_b) {
            (Some(a), Some(b)) => (a, b),
            _ => return Ok(false),
        };

        // 3. Tight loop: per row, check if (col_a[i], col_b[i]) is in the pair set.
        let col_a_data = &t.columns[ca];
        let col_b_data = &t.columns[cb];
        let n = t.row_count;
        let npairs = pairs.len();
        if npairs == 0 { return Ok(false); }

        // Build the result as a packed Bitmap by composing per-pair
        // (col_a == h1) AND (col_b == h2) bitmaps with OR. This reuses
        // the AVX-512 filter_eq_u64 kernel (8 u64s per instruction)
        // and the auto-vectorized byte-wise Bitmap::and / Bitmap::or,
        // avoiding the 2x Vec<bool> allocations + 2x clones + 3 scalar
        // reduction loops (5.4M iterations for Q7's 1.8M post-join rows)
        // that the generic OR evaluator performs.
        //
        // For Q7 (npairs=2, n=1.8M): 4x filter_eq_u64 + 2x Bitmap.and
        // + 1x Bitmap.or + 1x and_into_bool = ~8 AVX-512 passes vs the
        // generic path's 4x filter_eq_u64 + 4x and_into_bool + 3 scalar
        // loops + 2 Vec allocs + 2 Vec clones.
        if npairs <= 8 {
            use crate::exec::bitmap::{self, Bitmap};
            let mut acc: Option<Bitmap> = None;
            for &(h1, h2) in &pairs {
                let bm_a = bitmap::filter_eq_u64(col_a_data, h1);
                let bm_b = bitmap::filter_eq_u64(col_b_data, h2);
                let bm_pair = bm_a.and(&bm_b);
                acc = Some(match acc {
                    None => bm_pair,
                    Some(a) => a.or(&bm_pair),
                });
            }
            if let Some(bm) = acc {
                // mask[i] = mask[i] && bm.get(i)  (AVX-512BW bit expansion)
                bitmap::and_into_bool(&bm, &mut mask[..n]);
            }
        } else {
            // FxHashSet fallback for large N (no current TPC-H query hits
            // this; kept for correctness on hypothetical future queries).
            let set: FxHashSet<(u64, u64)> = pairs.iter().copied().collect();
            for i in 0..n {
                if !mask[i] { continue; }
                let key = (col_a_data[i], col_b_data[i]);
                if !set.contains(&key) { mask[i] = false; }
            }
        }

        Ok(true)
    }

    /// Vectorized boolean mask evaluation. Resolves column indices once,
    /// then loops over rows with direct array access. Falls back to
    /// per-row eval() for expression shapes it doesn't recognize.
    fn eval_bool_mask_vec(&self, expr: &Expr2, t: &ExecTable, mask: &mut [bool]) -> Result<(), Error> {
        match expr {
            Expr2::BinOp { op: BinOp2::And, left, right } => {
                // W2: evaluate left then right directly into the same mask.
                // All leaf comparisons AND into the mask in place (via
                // `bitmap::and_into_bool`), the OR arm has been fixed to
                // AND its disjunction into the mask, and the per-row
                // fallback paths still early-exit on `if !mask[i] { continue; }`
                // so rows already filtered out by the left side are
                // skipped on the right side. This eliminates the previous
                // `mask.to_vec()` allocation (6 MB for a 6 M-row lineitem
                // scan) per conjunct.
                self.eval_bool_mask_vec(left, t, mask)?;
                self.eval_bool_mask_vec(right, t, mask)?;
                Ok(())
            }
            Expr2::BinOp { op: BinOp2::Or, left, right } => {
                // W1-D: try the nation-pair LUT fast path first. Recognizes
                // the Q7 pattern (OR of ANDs of `Col == Str` equalities on
                // the same 2 string columns) and replaces 8 row passes +
                // ~6 MB of temp allocations with a single tight loop.
                if self.try_nation_pair_or_lut(expr, t, mask)? {
                    return Ok(());
                }
                // W2: generic OR fallback — reuse thread-local pool buffers
                // instead of allocating 2 fresh `vec![true; N]` masks per
                // call. The disjunction is AND-ed into the incoming mask
                // (previously the OR arm OVERWROTE mask, relying on the
                // outer conjunct loop to re-AND — a latent bug if
                // eval_bool_mask_vec was ever called on an OR expression
                // with a non-trivial incoming mask).
                let n = t.row_count;
                let mut lmask = take_mask_buf(n);
                lmask[..n].fill(true);
                self.eval_bool_mask_vec(left, t, &mut lmask[..n])?;
                let mut rmask = take_mask_buf(n);
                rmask[..n].fill(true);
                self.eval_bool_mask_vec(right, t, &mut rmask[..n])?;
                for i in 0..n { mask[i] = mask[i] && (lmask[i] || rmask[i]); }
                return_mask_buf(lmask);
                return_mask_buf(rmask);
                Ok(())
            }
            Expr2::BinOp { op, left, right } => {
                // Try to evaluate as Col op Literal or Literal op Col
                self.eval_comparison_vec(*op, left, right, t, mask)?;
                Ok(())
            }
            Expr2::Between { expr, low, high, negated } => {
                // W2: vectorized BETWEEN via two AVX-512 bitmap filters
                // (filter_ge_* + filter_le_*) composed with Bitmap::and,
                // then folded into the running mask via and_into_bool.
                // Matches the leaf-comparison fast path already used by
                // `apply_comparison` for `Col op Lit`. For NOT BETWEEN
                // we compose `col < lo OR col > hi` instead.
                if let Some(col_idx) = self.col_in(expr, t) {
                    use crate::exec::bitmap::{self, Bitmap};
                    let lo_val = self.eval_const(low, t)?;
                    let hi_val = self.eval_const(high, t)?;
                    let col: &[u64] = &t.columns[col_idx];
                    let col_type = t.col_types[col_idx];
                    let n = t.row_count;
                    let bm: Bitmap = match col_type {
                        ColType::Int => {
                            let lo = lo_val.as_i64().unwrap_or(i64::MIN);
                            let hi = hi_val.as_i64().unwrap_or(i64::MAX);
                            if *negated {
                                let bm_lt = bitmap::filter_lt_i64(col, lo);
                                let bm_gt = bitmap::filter_gt_i64(col, hi);
                                bm_lt.or(&bm_gt)
                            } else {
                                let bm_ge = bitmap::filter_ge_i64(col, lo);
                                let bm_le = bitmap::filter_le_i64(col, hi);
                                bm_ge.and(&bm_le)
                            }
                        }
                        ColType::Date => {
                            let lo = lo_val.as_u64().unwrap_or(0);
                            let hi = hi_val.as_u64().unwrap_or(u64::MAX);
                            if *negated {
                                let bm_lt = bitmap::filter_lt_u64(col, lo);
                                let bm_gt = bitmap::filter_gt_u64(col, hi);
                                bm_lt.or(&bm_gt)
                            } else {
                                let bm_ge = bitmap::filter_ge_u64(col, lo);
                                let bm_le = bitmap::filter_le_u64(col, hi);
                                bm_ge.and(&bm_le)
                            }
                        }
                        ColType::Float => {
                            let lo = lo_val.as_f64().unwrap_or(f64::NEG_INFINITY);
                            let hi = hi_val.as_f64().unwrap_or(f64::INFINITY);
                            if *negated {
                                let bm_lt = bitmap::filter_lt_f64(col, lo);
                                let bm_gt = bitmap::filter_gt_f64(col, hi);
                                bm_lt.or(&bm_gt)
                            } else {
                                let bm_ge = bitmap::filter_ge_f64(col, lo);
                                let bm_le = bitmap::filter_le_f64(col, hi);
                                bm_ge.and(&bm_le)
                            }
                        }
                        ColType::String => {
                            // String hashes are not order-comparable;
                            // fall back to a per-row scalar loop.
                            let mut bm = Bitmap::all_ones(n);
                            let lo = lo_val.as_u64().unwrap_or(0);
                            let hi = hi_val.as_u64().unwrap_or(u64::MAX);
                            for i in 0..n {
                                let v = col[i];
                                let in_range = v >= lo && v <= hi;
                                if *negated == in_range { bm.clear(i); }
                            }
                            bm
                        }
                    };
                    bitmap::and_into_bool(&bm, &mut mask[..n]);
                    Ok(())
                } else {
                    // Fallback: per-row eval
                    for i in 0..t.row_count {
                        if !mask[i] { continue; }
                        let v = self.eval(expr, t, i)?;
                        let lo = self.eval(low, t, i)?;
                        let hi = self.eval(high, t, i)?;
                        let in_range = self.cmp_le(&lo, &v) && self.cmp_le(&v, &hi);
                        mask[i] = mask[i] && (*negated != in_range);
                    }
                    Ok(())
                }
            }
            Expr2::InList { expr, list, negated } => {
                if let Some(col_idx) = self.col_in(expr, t) {
                    let vals: Vec<u64> = list.iter().filter_map(|e| {
                        if let Some(ci) = self.col_in(e, t) { Some(t.columns[ci][0]) }
                        else { self.eval_const(e, t).ok().map(|v| v.to_u64()) }
                    }).collect();
                    let col = &t.columns[col_idx];
                    for i in 0..t.row_count {
                        if !mask[i] { continue; }
                        let v = col[i];
                        let found = vals.iter().any(|&x| x == v);
                        mask[i] = mask[i] && (*negated != found);
                    }
                    Ok(())
                } else {
                    for i in 0..t.row_count {
                        if !mask[i] { continue; }
                        let v = self.eval(expr, t, i)?;
                        let mut found = false;
                        for item in list {
                            let iv = self.eval(item, t, i)?;
                            if self.cmp_eq(&v, &iv) { found = true; break; }
                        }
                        mask[i] = mask[i] && (*negated != found);
                    }
                    Ok(())
                }
            }
            Expr2::Like { expr, pattern, negated } => {
                // For LIKE on string columns, use StringSearchColumn if available
                if let Some(col_idx) = self.col_in(expr, t) {
                    if col_idx < t.string_columns.len() {
                        if let Some(ref sc) = t.string_columns[col_idx] {
                            // Get pattern as string
                            let pat = if let Expr2::Str(s) = pattern.as_ref() { s.clone() }
                                else { self.eval(pattern, t, 0).ok().and_then(|v| v.as_str().map(|s| s.to_string())).unwrap_or_default() };
                            if !pat.is_empty() && sc.len() >= t.row_count {
                                // Only use StringSearchColumn if it has enough rows
                                // (after a join, the string column may have the wrong length)
                                let like_mask = self.like_mask(sc, &pat);
                                for i in 0..t.row_count {
                                    if *negated { mask[i] = mask[i] && !like_mask[i]; }
                                    else { mask[i] = mask[i] && like_mask[i]; }
                                }
                                return Ok(());
                            }
                        }
                    }
                }
                // Fallback: per-row eval
                for i in 0..t.row_count {
                    if !mask[i] { continue; }
                    let v = self.eval(expr, t, i)?;
                    let pv = self.eval(pattern, t, i)?;
                    let r = match (&v, &pv) {
                        (Value2::Str(s), Value2::Str(p)) => self.like(s, p),
                        _ => false,
                    };
                    mask[i] = mask[i] && (*negated != r);
                }
                Ok(())
            }
            _ => {
                // Fallback: per-row eval for unrecognized shapes
                for i in 0..t.row_count {
                    if !mask[i] { continue; }
                    let v = self.eval(expr, t, i)?;
                    mask[i] = mask[i] && self.truthy(&v);
                }
                Ok(())
            }
        }
    }

    /// Evaluate a constant expression (literal or column-independent).
    fn eval_const(&self, expr: &Expr2, t: &ExecTable) -> Result<Value2, Error> {
        match expr {
            Expr2::Int(i) => Ok(Value2::Int(*i)),
            Expr2::Float(f) => Ok(Value2::Float(*f)),
            Expr2::Str(s) => Ok(Value2::Str(s.clone())),
            Expr2::Date(d) => Ok(Value2::Date(*d)),
            Expr2::Neg(e) => {
                let v = self.eval_const(e, t)?;
                Ok(match v { Value2::Int(i) => Value2::Int(-i), Value2::Float(f) => Value2::Float(-f), _ => Value2::Null })
            }
            _ => self.eval(expr, t, 0),
        }
    }

    /// Build a LIKE mask for a string column. Handles % wildcards.
    fn like_mask(&self, sc: &crate::exec::fm_index::StringSearchColumn, pattern: &str) -> Vec<bool> {
        let n = sc.len();
        let mut mask = vec![false; n];
        if pattern.is_empty() { mask.fill(true); return mask; }
        let pb = pattern.as_bytes();
        if pb[0] == b'%' && !pb[1..].contains(&b'%') && !pattern.contains('_') {
            // Suffix match: %suffix
            let suffix = &pattern[1..];
            for i in 0..n {
                mask[i] = sc.get(i).ends_with(suffix);
            }
        } else if !pattern.contains('%') && !pattern.contains('_') {
            // Exact match
            for i in 0..n {
                mask[i] = sc.get(i) == pattern;
            }
        } else {
            // General LIKE
            for i in 0..n {
                mask[i] = self.like(sc.get(i), pattern);
            }
        }
        mask
    }

    /// Vectorized comparison: Col op Literal (or Literal op Col).
    /// Resolves column index once, then loops.
    /// Falls back to per-row eval for Col op Col or complex expressions.
    fn eval_comparison_vec(&self, op: BinOp2, left: &Expr2, right: &Expr2, t: &ExecTable, mask: &mut [bool]) -> Result<(), Error> {
        // Try Col op Const (right side must NOT have column refs)
        if let Some(col_idx) = self.col_in(left, t) {
            if !self.expr_has_col(right) {
                let rval = self.eval_const(right, t)?;
                self.apply_comparison(op, col_idx, &rval, t, mask, false)?;
                return Ok(());
            }
        }
        // Try Const op Col (left side must NOT have column refs)
        if let Some(col_idx) = self.col_in(right, t) {
            if !self.expr_has_col(left) {
                let lval = self.eval_const(left, t)?;
                self.apply_comparison(swap_op(op), col_idx, &lval, t, mask, false)?;
                return Ok(());
            }
        }
        // Try Col(inner) op Col(outer): correlated subquery fast path.
        // When evaluating a WHERE filter inside a correlated subquery, one
        // side is an inner column (resolves to `t`) and the other is an
        // outer column (resolves via `self.outer`). Get the outer value ONCE
        // and use the vectorized bitmap filter — avoids per-row outer lookups
        // which made Q17's subquery take 300ms each (× 200 = 60s timeout).
        if let Some((outer_ptr, outer_row)) = self.outer.get() {
            let outer_t = unsafe { &*outer_ptr };
            // Col(inner) op Col(outer)
            if let Some(col_idx) = self.col_in(left, t) {
                if let Expr2::Col(rname) = right {
                    if t.lookup_col(rname).is_none() {
                        if let Some(outer_idx) = outer_t.lookup_col(rname)
                            .or_else(|| rname.rfind('.').and_then(|p| outer_t.lookup_col(&rname[p+1..])))
                        {
                            let cell = outer_t.columns[outer_idx].get(outer_row).copied().unwrap_or(0);
                            let rval = match outer_t.col_types[outer_idx] {
                                ColType::Int => Value2::Int(cell as i64),
                                ColType::Float => Value2::Float(f64::from_bits(cell)),
                                ColType::Date => Value2::Date(cell as u32 as i32),
                                ColType::String => Value2::Int(cell as i64),
                            };
                            self.apply_comparison(op, col_idx, &rval, t, mask, false)?;
                            return Ok(());
                        }
                    }
                }
            }
            // Col(outer) op Col(inner) — swap
            if let Some(col_idx) = self.col_in(right, t) {
                if let Expr2::Col(lname) = left {
                    if t.lookup_col(lname).is_none() {
                        if let Some(outer_idx) = outer_t.lookup_col(lname)
                            .or_else(|| lname.rfind('.').and_then(|p| outer_t.lookup_col(&lname[p+1..])))
                        {
                            let cell = outer_t.columns[outer_idx].get(outer_row).copied().unwrap_or(0);
                            let lval = match outer_t.col_types[outer_idx] {
                                ColType::Int => Value2::Int(cell as i64),
                                ColType::Float => Value2::Float(f64::from_bits(cell)),
                                ColType::Date => Value2::Date(cell as u32 as i32),
                                ColType::String => Value2::Int(cell as i64),
                            };
                            self.apply_comparison(swap_op(op), col_idx, &lval, t, mask, false)?;
                            return Ok(());
                        }
                    }
                }
            }
        }
        // W1-D: Col op Col fast path. Both sides resolve to columns in the
        // current table. The generic fallback below calls eval() per row
        // (2 FxHashMap lookups + Value2 construction + binop per row).
        // For Q7's 5 equi-join re-checks on the 1.8M-row post-join table,
        // that's ~9M hashmap lookups (~378ms). This fast path resolves
        // column indices once and does direct u64 array comparison (~18ms).
        // Only applies to Eq/Ne on Int/Date/String columns (u64 bit-comparable).
        // Float falls through (NaN/-0 edge cases require f64 semantics).
        if let (Some(lidx), Some(ridx)) = (self.col_in(left, t), self.col_in(right, t)) {
            let lt = t.col_types[lidx];
            let rt = t.col_types[ridx];
            if lt == rt && matches!(lt, ColType::Int | ColType::Date | ColType::String) {
                let lcol = &t.columns[lidx];
                let rcol = &t.columns[ridx];
                let n = t.row_count;
                match op {
                    BinOp2::Eq => {
                        for i in 0..n {
                            if mask[i] && lcol[i] != rcol[i] { mask[i] = false; }
                        }
                        return Ok(());
                    }
                    BinOp2::Ne => {
                        for i in 0..n {
                            if mask[i] && lcol[i] == rcol[i] { mask[i] = false; }
                        }
                        return Ok(());
                    }
                    _ => {}
                }
            }
        }
        // Fallback: per-row eval for Col op Col or complex expressions
        for i in 0..t.row_count {
            if !mask[i] { continue; }
            let lv = self.eval(left, t, i)?;
            let rv = self.eval(right, t, i)?;
            let result = self.binop(op, &lv, &rv);
            mask[i] = mask[i] && self.truthy(&result);
        }
        Ok(())
    }

    /// Apply a comparison (Col op Value) to the mask vectorized.
    fn apply_comparison(&self, op: BinOp2, col_idx: usize, val: &Value2, t: &ExecTable, mask: &mut [bool], _negated: bool) -> Result<(), Error> {
        use crate::exec::bitmap;
        let col: &[u64] = &t.columns[col_idx];
        let col_type = t.col_types[col_idx];
        let n = t.row_count;
        let mask = &mut mask[..n];
        match (col_type, val) {
            (ColType::Int, Value2::Int(ival)) => {
                let bm = match op {
                    BinOp2::Eq => bitmap::filter_eq_u64(col, *ival as u64),
                    BinOp2::Ne => bitmap::filter_ne_u64(col, *ival as u64),
                    BinOp2::Lt => bitmap::filter_lt_i64(col, *ival),
                    BinOp2::Le => bitmap::filter_le_i64(col, *ival),
                    BinOp2::Gt => bitmap::filter_gt_i64(col, *ival),
                    BinOp2::Ge => bitmap::filter_ge_i64(col, *ival),
                    _ => return Ok(()),
                };
                bitmap::and_into_bool(&bm, mask);
            }
            (ColType::Date, Value2::Date(dval)) => {
                let target = *dval as u64;
                let bm = match op {
                    BinOp2::Eq => bitmap::filter_eq_u64(col, target),
                    BinOp2::Ne => bitmap::filter_ne_u64(col, target),
                    BinOp2::Lt => bitmap::filter_lt_u64(col, target),
                    BinOp2::Le => bitmap::filter_le_u64(col, target),
                    BinOp2::Gt => bitmap::filter_gt_u64(col, target),
                    BinOp2::Ge => bitmap::filter_ge_u64(col, target),
                    _ => return Ok(()),
                };
                bitmap::and_into_bool(&bm, mask);
            }
            (ColType::Float, Value2::Float(fval)) => {
                let bm = match op {
                    BinOp2::Eq => bitmap::filter_eq_f64_epsilon(col, *fval),
                    BinOp2::Ne => bitmap::filter_ne_f64(col, *fval),
                    BinOp2::Lt => bitmap::filter_lt_f64(col, *fval),
                    BinOp2::Le => bitmap::filter_le_f64(col, *fval),
                    BinOp2::Gt => bitmap::filter_gt_f64(col, *fval),
                    BinOp2::Ge => bitmap::filter_ge_f64(col, *fval),
                    _ => return Ok(()),
                };
                bitmap::and_into_bool(&bm, mask);
            }
            (ColType::Float, Value2::Int(ival)) => {
                let fval = *ival as f64;
                let bm = match op {
                    BinOp2::Eq => bitmap::filter_eq_f64(col, fval),
                    BinOp2::Ne => bitmap::filter_ne_f64(col, fval),
                    BinOp2::Lt => bitmap::filter_lt_f64(col, fval),
                    BinOp2::Le => bitmap::filter_le_f64(col, fval),
                    BinOp2::Gt => bitmap::filter_gt_f64(col, fval),
                    BinOp2::Ge => bitmap::filter_ge_f64(col, fval),
                    _ => return Ok(()),
                };
                bitmap::and_into_bool(&bm, mask);
            }
            (ColType::String, Value2::Str(sval)) => {
                let target_hash = xxhash_rust::xxh3::xxh3_64(sval.as_bytes());
                match op {
                    BinOp2::Eq => {
                        let bm = bitmap::filter_eq_u64(col, target_hash);
                        bitmap::and_into_bool(&bm, mask);
                    }
                    BinOp2::Ne => {
                        let bm = bitmap::filter_ne_u64(col, target_hash);
                        bitmap::and_into_bool(&bm, mask);
                    }
                    _ => {}
                }
            }
            _ => {
                // Fallback: per-row eval
                for i in 0..n {
                    if !mask[i] { continue; }
                    let cv = unsafe { std::ptr::read(col.as_ptr().add(i)) };
                    let v = match col_type {
                        ColType::Int => Value2::Int(cv as i64),
                        ColType::Float => Value2::Float(f64::from_bits(cv)),
                        ColType::Date => Value2::Date(cv as i32),
                        ColType::String => Value2::Str(String::new()),
                    };
                    let matches = match op {
                        BinOp2::Eq => self.cmp_eq(&v, val),
                        BinOp2::Ne => !self.cmp_eq(&v, val),
                        BinOp2::Lt => self.cmp_lt(&v, val),
                        BinOp2::Le => self.cmp_le(&v, val),
                        BinOp2::Gt => !self.cmp_le(&v, val),
                        BinOp2::Ge => !self.cmp_lt(&v, val),
                        _ => false,
                    };
                    mask[i] = mask[i] && matches;
                }
            }
        }
        Ok(())
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
                // Try current table first
                if let Some(idx) = t.lookup_col(name) {
                    let cell = t.columns[idx].get(row).copied().unwrap_or(0);
                    return Ok(match t.col_types[idx] {
                        ColType::Int => Value2::Int(cell as i64),
                        ColType::Float => Value2::Float(f64::from_bits(cell)),
                        ColType::Date => Value2::Date(cell as u32 as i32),
                        ColType::String => {
                            // Use the StringSearchColumn only if it has enough
                            // entries for this row. After a join, string_columns
                            // are not rebuilt (they still have the pre-join row
                            // count), so sc.get(row) would return "" for rows
                            // beyond the original count. Fall back to the u64
                            // hash value (which is what filters and joins use).
                            if let Some(ref sc) = t.string_columns[idx] {
                                if sc.len() > row {
                                    Value2::Str(sc.get(row).to_string())
                                } else {
                                    Value2::Int(cell as i64)
                                }
                            } else {
                                Value2::Int(cell as i64)
                            }
                        }
                    });
                }
                // Try qualified name: if name contains '.', try the part after '.'
                if let Some(dot_pos) = name.rfind('.') {
                    let short_name = &name[dot_pos+1..];
                    if let Some(idx) = t.lookup_col(short_name) {
                        let cell = t.columns[idx].get(row).copied().unwrap_or(0);
                        return Ok(match t.col_types[idx] {
                            ColType::Int => Value2::Int(cell as i64),
                            ColType::Float => Value2::Float(f64::from_bits(cell)),
                            ColType::Date => Value2::Date(cell as u32 as i32),
                            ColType::String => {
                                if let Some(ref sc) = t.string_columns[idx] {
                                    if sc.len() > row {
                                        Value2::Str(sc.get(row).to_string())
                                    } else {
                                        Value2::Int(cell as i64)
                                    }
                                } else {
                                    Value2::Int(cell as i64)
                                }
                            }
                        });
                    }
                }
                // Check outer context (correlated subquery)
                if let Some((outer_ptr, outer_row)) = self.outer.get() {
                    // SAFETY: outer_ptr was set by our own code and points to
                    // an ExecTable that is valid for the duration of this eval.
                    let outer_t = unsafe { &*outer_ptr };
                    // Try full name
                    if let Some(idx) = outer_t.lookup_col(name) {
                        let cell = outer_t.columns[idx].get(outer_row).copied().unwrap_or(0);
                        return Ok(match outer_t.col_types[idx] {
                            ColType::Int => Value2::Int(cell as i64),
                            ColType::Float => Value2::Float(f64::from_bits(cell)),
                            ColType::Date => Value2::Date(cell as u32 as i32),
                            ColType::String => {
                                if let Some(ref sc) = outer_t.string_columns[idx] {
                                    if sc.len() > outer_row {
                                        Value2::Str(sc.get(outer_row).to_string())
                                    } else {
                                        Value2::Int(cell as i64)
                                    }
                                } else {
                                    Value2::Int(cell as i64)
                                }
                            }
                        });
                    }
                    // Try short name (after '.')
                    if let Some(dot_pos) = name.rfind('.') {
                        let short_name = &name[dot_pos+1..];
                        if let Some(idx) = outer_t.lookup_col(short_name) {
                            let cell = outer_t.columns[idx].get(outer_row).copied().unwrap_or(0);
                            return Ok(match outer_t.col_types[idx] {
                                ColType::Int => Value2::Int(cell as i64),
                                ColType::Float => Value2::Float(f64::from_bits(cell)),
                                ColType::Date => Value2::Date(cell as u32 as i32),
                                ColType::String => {
                                    if let Some(ref sc) = outer_t.string_columns[idx] {
                                        if sc.len() > outer_row {
                                            Value2::Str(sc.get(outer_row).to_string())
                                        } else {
                                            Value2::Int(cell as i64)
                                        }
                                    } else {
                                        Value2::Int(cell as i64)
                                    }
                                }
                            });
                        }
                    }
                }
                Err(Error::NotFound(format!("column '{}'", name)))
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
                // Check uncorrelated-subquery cache first.
                let ast_key = (q.as_ref() as *const SelectQuery2) as usize;
                {
                    let cache = self.subquery_cache.borrow();
                    if let Some(v) = cache.get(&ast_key) {
                        return Ok(v.clone());
                    }
                }
                // Try decorrelation: if the subquery is a correlated aggregate
                // (SELECT agg(expr) FROM t WHERE corr1 = outer1 AND corr2 = outer2 AND local_filters),
                // proactively build a derived table once, then per-row eval is a hash lookup.
                // This is critical for Q20 (800k correlation keys, each scanning 6M rows).
                {
                    let cached = self.decorrelated_cache.borrow().contains_key(&ast_key);
                    if !cached {
                        if let Some((map, cols)) = self.try_decorrelate_subquery(q, t)? {
                            self.decorrelated_cache.borrow_mut().insert(ast_key, (map, cols));
                        }
                    }
                }
                {
                    let cache = self.decorrelated_cache.borrow();
                    if let Some((map, corr_cols)) = cache.get(&ast_key) {
                        // Compute correlation hash from outer row's corr cols.
                        let mut corr_hash: u64 = 0;
                        for &ci in corr_cols {
                            let v = t.columns[ci].get(row).copied().unwrap_or(0);
                            corr_hash = corr_hash.wrapping_mul(0x517cc1b727220a95).wrapping_add(v);
                        }
                        if let Some(v) = map.get(&corr_hash) {
                            return Ok(v.clone());
                        }
                        // No match in derived table → subquery returns NULL (no rows match).
                        return Ok(Value2::Null);
                    }
                }
                // Correlated subquery: cache by (ast_key, hash of correlation column values).
                let corr_cols = self.find_correlation_cols(q, t);
                let mut corr_hash: u64 = 0;
                for &ci in &corr_cols {
                    let v = t.columns[ci].get(row).copied().unwrap_or(0);
                    corr_hash = corr_hash.wrapping_mul(0x517cc1b727220a95).wrapping_add(v);
                }
                let cache_key = ast_key.wrapping_add((corr_hash.wrapping_mul(0x9E3779B97F4A7C15)) as usize);
                {
                    let cache = self.subquery_cache.borrow();
                    if let Some(v) = cache.get(&cache_key) {
                        return Ok(v.clone());
                    }
                }
                // Cache miss — execute with outer context (correlated subquery).
                let old_outer = self.outer.get();
                self.outer.set(Some((t as *const ExecTable, row)));
                let r = self.execute(q);
                self.outer.set(old_outer);
                let r = r?;
                let val = r.columns.first().and_then(|c| c.values.first()).copied().unwrap_or(0);
                let name = r.columns.first().map(|c| c.name.as_str()).unwrap_or("");
                let vals_slice: &[u64] = r.columns.first().map(|c| c.values.as_slice()).unwrap_or(&[]);
                let v = match self.infer_result_type(name, vals_slice) {
                    ColType::Float => Value2::Float(f64::from_bits(val)),
                    _ => Value2::Int(val as i64),
                };
                self.subquery_cache.borrow_mut().insert(cache_key, v.clone());
                Ok(v)
            }
            Expr2::Exists { query, negated } => {
                // Semi-join fast path: if the subquery has a single correlation
                // column with an equi-join (e.g. `l_orderkey = o_orderkey`),
                // build a hash set of inner col values ONCE and check membership.
                // This decorrelates EXISTS, reducing ~25k subquery executions
                // (Q4) to 1 hash-set build + 25k lookups.
                let ast_key = (query.as_ref() as *const SelectQuery2) as usize;
                if let Some((outer_col_idx, inner_col_idx)) = self.find_exists_equi_join(query, t) {
                    // Build the hash set (cached by AST pointer)
                    let need_build = !self.exists_cache.borrow().contains_key(&ast_key);
                    if need_build {
                        let set = self.build_exists_hashset(query, inner_col_idx)?;
                        self.exists_cache.borrow_mut().insert(ast_key, set);
                    }
                    let cache = self.exists_cache.borrow();
                    if let Some(set) = cache.get(&ast_key) {
                        let outer_val = t.columns[outer_col_idx].get(row).copied().unwrap_or(0);
                        let exists = set.contains(&outer_val);
                        return Ok(Value2::Int(if if *negated { !exists } else { exists } { 1 } else { 0 }));
                    }
                }
                // Multi-column EXISTS fast path: if the subquery has 2 correlation
                // columns — one equi-join (e.g. l_orderkey = l1.l_orderkey) and one
                // inequality (e.g. l_suppkey <> l1.l_suppkey) — build a
                // HashMap<equi_key, HashSet<ineq_col>> once, then check per row.
                if let Some((outer_eq_idx, inner_eq_idx, outer_neq_idx, inner_neq_idx)) = self.find_exists_multi_col(query, t) {
                    let need_build = !self.exists_multi_cache.borrow().contains_key(&ast_key);
                    if need_build {
                        let map = self.build_exists_multi_map(query, inner_eq_idx, inner_neq_idx)?;
                        self.exists_multi_cache.borrow_mut().insert(ast_key, map);
                    }
                    let cache = self.exists_multi_cache.borrow();
                    if let Some(map) = cache.get(&ast_key) {
                        let outer_eq = t.columns[outer_eq_idx].get(row).copied().unwrap_or(0);
                        let outer_neq = t.columns[outer_neq_idx].get(row).copied().unwrap_or(0);
                        let exists = map.get(&outer_eq).map_or(false, |set| {
                            set.iter().any(|&v| v != outer_neq)
                        });
                        return Ok(Value2::Int(if if *negated { !exists } else { exists } { 1 } else { 0 }));
                    }
                }
                // Fallback: per-row execution (correlated subquery)
                let old_outer = self.outer.get();
                self.outer.set(Some((t as *const ExecTable, row)));
                let r = self.execute(query);
                self.outer.set(old_outer);
                let r = r?;
                let ex = r.row_count > 0;
                Ok(Value2::Int(if if *negated { !ex } else { ex } { 1 } else { 0 }))
            }
            Expr2::InSubquery { expr, query, negated } => {
                let v = self.eval(expr, t, row)?;
                let ast_key = (query.as_ref() as *const SelectQuery2) as usize;
                // Check uncorrelated IN-subquery cache first.
                let need_build = !self.in_subquery_cache.borrow().contains_key(&ast_key);
                if need_build {
                    // Try executing with outer=None to detect uncorrelated.
                    let old_outer = self.outer.get();
                    self.outer.set(None);
                    let r = self.execute(query);
                    self.outer.set(old_outer);
                    match r {
                        Ok(r) => {
                            if let Some(col) = r.columns.first() {
                                let set: FxHashSet<u64> = col.values.iter().copied().collect();
                                self.in_subquery_cache.borrow_mut().insert(ast_key, set);
                            }
                        }
                        Err(_) => {
                            // Correlated — mark as empty set so we don't retry.
                            // Per-row eval with outer context will handle it.
                            self.in_subquery_cache.borrow_mut().insert(ast_key, new_fxhashset());
                        }
                    }
                }
                // Check cache. If the subquery was uncorrelated, the cache has
                // the full result set. If correlated (cache is empty set), fall
                // through to per-row execution.
                let cache = self.in_subquery_cache.borrow();
                if let Some(set) = cache.get(&ast_key) {
                    if !set.is_empty() || self.outer.get().is_none() {
                        // Uncorrelated — check membership.
                        // Note: for correlated subqueries that returned empty
                        // (no rows match), we can't distinguish from "correlated,
                        // not yet executed". But if outer is None, it's top-level,
                        // so empty means truly empty.
                        let v_u64 = v.to_u64();
                        let found = set.contains(&v_u64);
                        return Ok(Value2::Int(if if *negated { !found } else { found } { 1 } else { 0 }));
                    }
                }
                drop(cache);
                // Correlated IN-subquery — execute per row with outer context.
                let old_outer = self.outer.get();
                self.outer.set(Some((t as *const ExecTable, row)));
                let r = self.execute(query);
                self.outer.set(old_outer);
                let r = r?;
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
        let lower = field.to_lowercase();
        // W1-C: Fast path for `extract(year FROM ...)` — uses Howard Hinnant's
        // `civil_from_days` algorithm (~8 integer ops) instead of
        // `time::Date::from_julian_day` (~30 ops + branches per row).
        // Q7/Q8/Q9 each extract year from ~6M lineitem rows.
        if lower == "year" {
            return Value2::Int(crate::types::days_since_epoch_to_year(days as i64) as i64);
        }
        let date = crate::types::Date::from_u64(days as u64);
        let (y, m, d) = date.to_ymd();
        let r = match lower.as_str() {
            "month" => m as i64, "day" => d as i64, _ => y as i64,
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

    /// Low-cardinality GROUP BY fast path using FixedAccumulator.
    /// For <=256 groups: single pass, no HashMap, no Vec<Vec<usize>>.
    /// Returns None if the query is too complex for this path.
    fn try_low_card_grouped(
        &self, query: &SelectQuery2, t: &ExecTable, mask: &[bool],
    ) -> Result<Option<QueryResult>, Error> {
        use crate::exec::fixed_agg::{FixedAccumulator, MAX_FIXED_GROUPS};

        if query.having.is_some() { return Ok(None); }

        let gb_cols: Vec<Option<usize>> = query.group_by.iter()
            .map(|gb| self.col_in(gb, t))
            .collect();
        if gb_cols.iter().any(|c| c.is_none()) { return Ok(None); }
        let gb_cols: Vec<usize> = gb_cols.iter().map(|c| c.unwrap()).collect();

        #[derive(Clone)]
        enum LcAgg {
            GroupByCol(usize),
            CountAll,
            SumCol(usize),
            SumColCol(usize, usize),
            SumColSubOne(usize, usize),
            SumColSubOneAddOne(usize, usize, usize),
            AvgCol(usize),
            MinCol(usize),
            MaxCol(usize),
        }

        let mut plans: Vec<Option<LcAgg>> = Vec::with_capacity(query.select.len());
        for item in &query.select {
            let plan = match &item.expr {
                Expr2::CountStar => Some(LcAgg::CountAll),
                Expr2::Agg { func, arg, distinct: false } => {
                    match func {
                        AggFunc::Count => {
                            // count(Col) counts non-null (non-zero) values.
                            // count(*) counts all rows.
                            // The low_card path only supports CountAll (count(*)).
                            // count(Col) falls back to the HashMap path.
                            if let Some(_) = self.col_in(arg, t) {
                                None
                            } else {
                                Some(LcAgg::CountAll)
                            }
                        }
                        AggFunc::Sum => {
                            if let Some(a) = self.col_in(arg, t) {
                                if t.col_types[a] == ColType::Float { Some(LcAgg::SumCol(a)) } else { None }
                            } else if let Expr2::BinOp { op: BinOp2::Mul, left, right } = arg.as_ref() {
                                if let (Some(a), Some(b)) = (self.col_in(left, t), self.col_in(right, t)) {
                                    if t.col_types[a] == ColType::Float && t.col_types[b] == ColType::Float {
                                        Some(LcAgg::SumColCol(a, b))
                                    } else { None }
                                } else if let (Some(a), Some(b)) = (self.col_in(left, t), self.col_in_sub_one_right(right, t)) {
                                    if t.col_types[a] == ColType::Float && t.col_types[b] == ColType::Float {
                                        Some(LcAgg::SumColSubOne(a, b))
                                    } else { None }
                                } else if let (Some(b), Some(a)) = (self.col_in(right, t), self.col_in_sub_one_right(left, t)) {
                                    if t.col_types[a] == ColType::Float && t.col_types[b] == ColType::Float {
                                        Some(LcAgg::SumColSubOne(a, b))
                                    } else { None }
                                } else { None }
                            } else { None }
                        }
                        AggFunc::Avg => {
                            if let Expr2::Col(name) = arg.as_ref() {
                                if let Some(idx) = t.lookup_col(name) {
                                    if t.col_types[idx] == ColType::Float { Some(LcAgg::AvgCol(idx)) } else { None }
                                } else { None }
                            } else { None }
                        }
                        AggFunc::Min => {
                            if let Expr2::Col(name) = arg.as_ref() {
                                if let Some(idx) = t.lookup_col(name) { Some(LcAgg::MinCol(idx)) } else { None }
                            } else { None }
                        }
                        AggFunc::Max => {
                            if let Expr2::Col(name) = arg.as_ref() {
                                if let Some(idx) = t.lookup_col(name) { Some(LcAgg::MaxCol(idx)) } else { None }
                            } else { None }
                        }
                        _ => None,
                    }
                }
                Expr2::Col(name) => {
                    if let Some(idx) = t.lookup_col(name) { Some(LcAgg::GroupByCol(idx)) } else { None }
                }
                _ => None,
            };
            plans.push(plan);
        }

        for (i, item) in query.select.iter().enumerate() {
            if plans[i].is_some() { continue; }
            if let Expr2::Agg { func: AggFunc::Sum, arg, distinct: false } = &item.expr {
                if let Expr2::BinOp { op: BinOp2::Mul, left, right } = arg.as_ref() {
                    // Try: (Col * (1 - Col2)) * (1 + Col3)
                    if let Some((a, b)) = self.col_in_mul_sub_one(left, t) {
                        if let Some(c) = self.col_in_add_one_right(right, t) {
                            if t.col_types[a] == ColType::Float && t.col_types[b] == ColType::Float && t.col_types[c] == ColType::Float {
                                plans[i] = Some(LcAgg::SumColSubOneAddOne(a, b, c));
                            }
                        }
                    }
                }
            }
        }

        if plans.iter().any(|p| p.is_none()) { return Ok(None); }

        let agg_indices: Vec<usize> = plans.iter().enumerate()
            .filter_map(|(i, p)| match p {
                Some(LcAgg::SumCol(_)) | Some(LcAgg::SumColCol(_, _)) |
                Some(LcAgg::SumColSubOne(_, _)) | Some(LcAgg::SumColSubOneAddOne(_, _, _)) |
                Some(LcAgg::AvgCol(_)) | Some(LcAgg::MinCol(_)) | Some(LcAgg::MaxCol(_)) => Some(i),
                _ => None,
            })
            .collect();
        let num_aggs = agg_indices.len();
        if num_aggs == 0 { return Ok(None); }

        let n = t.row_count;

        // Collect aggregate column references
        let agg_specs: Vec<(usize, Option<usize>, Option<usize>, u8)> = agg_indices.iter().map(|&item_idx| {
            match plans[item_idx].as_ref().unwrap() {
                LcAgg::SumCol(a) => (*a, None, None, 0),
                LcAgg::SumColCol(a, b) => (*a, Some(*b), None, 1),
                LcAgg::SumColSubOne(a, b) => (*a, Some(*b), None, 2),
                LcAgg::SumColSubOneAddOne(a, b, c) => (*a, Some(*b), Some(*c), 3),
                LcAgg::AvgCol(a) => (*a, None, None, 4),
                LcAgg::MinCol(a) => (*a, None, None, 5),
                LcAgg::MaxCol(a) => (*a, None, None, 6),
                _ => (0, None, None, 0),
            }
        }).collect();
        let num_aggs_actual = num_aggs;

        // Parallel single-pass morsel aggregation.
        // Each thread processes a chunk, maintaining its own local group->slot map
        // and per-group accumulators. At the end, merge all thread-local maps.
        // For Q1 (4 groups, 8 threads): merge is 32 entries — trivial.
        const CHUNK_SIZE: usize = 65536;
        let num_chunks = (n + CHUNK_SIZE - 1) / CHUNK_SIZE;

        // Each chunk produces: (group_keys: Vec<u64>, sums: Vec<f64>, counts: Vec<u64>)
        // where sums is laid out as [agg0_grp0, agg0_grp1, ..., agg1_grp0, ...]
        let partials: Vec<Option<(Vec<u64>, Vec<f64>, Vec<u64>)>> = (0..num_chunks).into_par_iter().map(|chunk_idx| {
            let start = chunk_idx * CHUNK_SIZE;
            let end = std::cmp::min(start + CHUNK_SIZE, n);

            let mut local_keys: Vec<u64> = Vec::with_capacity(16);
            let mut local_slot: FxHashMap<u64, usize> = new_fxhashmap();

            let mut local_sums: Vec<f64> = Vec::new();
            let mut local_counts: Vec<u64> = Vec::new();

            for i in start..end {
                if !mask[i] { continue; }

                let mut key_hash: u64 = 0;
                for &ci in &gb_cols {
                    key_hash = key_hash.wrapping_mul(0x517cc1b727220a95).wrapping_add(t.columns[ci][i]);
                }

                let slot = if let Some(&s) = local_slot.get(&key_hash) {
                    s
                } else {
                    let new_slot = local_keys.len();
                    if new_slot >= MAX_FIXED_GROUPS - 1 { return None; }
                    local_keys.push(key_hash);
                    local_slot.insert(key_hash, new_slot);
                    local_sums.extend(std::iter::repeat(0.0f64).take(num_aggs_actual));
                    local_counts.push(0);
                    new_slot
                };

                local_counts[slot] += 1;

                for (ai, &(col_a, col_b_o, col_c_o, at)) in agg_specs.iter().enumerate() {
                    let base = ai * local_keys.len();
                    let va = t.columns[col_a][i];
                    match at {
                        0 => { local_sums[base + slot] += f64::from_bits(va); }
                        1 => {
                            if let Some(cb) = col_b_o {
                                local_sums[base + slot] += f64::from_bits(va) * f64::from_bits(t.columns[cb][i]);
                            }
                        }
                        2 => {
                            if let Some(cb) = col_b_o {
                                local_sums[base + slot] += f64::from_bits(va) * (1.0 - f64::from_bits(t.columns[cb][i]));
                            }
                        }
                        3 => {
                            if let (Some(cb), Some(cc)) = (col_b_o, col_c_o) {
                                local_sums[base + slot] += f64::from_bits(va) * (1.0 - f64::from_bits(t.columns[cb][i])) * (1.0 + f64::from_bits(t.columns[cc][i]));
                            }
                        }
                        4 => { local_sums[base + slot] += f64::from_bits(va); }
                        _ => {}
                    }
                }
            }
            Some((local_keys, local_sums, local_counts))
        }).collect();

        // If any chunk returned None (too many groups), fall back to HashMap path
        if partials.iter().any(|p| p.is_none()) {
            return Ok(None);
        }
        let partials: Vec<(Vec<u64>, Vec<f64>, Vec<u64>)> = partials.into_iter().map(|p| p.unwrap()).collect();

        // Merge: build global group->slot map from all chunk-local keys
        let mut key_to_slot: FxHashMap<u64, usize> = new_fxhashmap();

        let mut group_keys_discovered: Vec<u64> = Vec::new();
        for (keys, _, _) in &partials {
            for &k in keys {
                if !key_to_slot.contains_key(&k) {
                    let slot = group_keys_discovered.len();
                    if slot >= MAX_FIXED_GROUPS - 1 { return Ok(None); }
                    key_to_slot.insert(k, slot);
                    group_keys_discovered.push(k);
                }
            }
        }
        let num_groups_found = group_keys_discovered.len();
        if num_groups_found == 0 {
            let mut cols: Vec<ResultColumn> = Vec::with_capacity(query.select.len());
            for item in &query.select {
                let name = item.alias.clone().unwrap_or_else(|| self.expr_name(&item.expr));
                cols.push(ResultColumn { name, values: Vec::new() });
            }
            return Ok(Some(QueryResult { columns: cols, row_count: 0, elapsed_us: 0 }));
        }

        // Merge sums and counts into final accumulators
        let mut final_sums = vec![0.0f64; num_groups_found * num_aggs_actual];
        let mut final_counts = vec![0u64; num_groups_found];
        for (keys, sums, counts) in &partials {
            let local_ng = keys.len();
            for (local_slot, &key) in keys.iter().enumerate() {
                let global_slot = key_to_slot[&key];
                final_counts[global_slot] += counts[local_slot];
                for a in 0..num_aggs_actual {
                    final_sums[a * num_groups_found + global_slot] += sums[a * local_ng + local_slot];
                }
            }
        }

        // Min/Max (serial pass)
        for (ai, &item_idx) in agg_indices.iter().enumerate() {
            if matches!(plans[item_idx], Some(LcAgg::MinCol(_)) | Some(LcAgg::MaxCol(_))) {
                let a = if let Some(LcAgg::MinCol(a)) | Some(LcAgg::MaxCol(a)) = plans[item_idx] { a } else { 0 };
                let is_min = matches!(plans[item_idx], Some(LcAgg::MinCol(_)));
                let mut mm = vec![if is_min { f64::INFINITY } else { f64::NEG_INFINITY }; num_groups_found];
                for i in 0..n {
                    if !mask[i] { continue; }
                    let mut key_hash: u64 = 0;
                    for &ci in &gb_cols { key_hash = key_hash.wrapping_mul(0x517cc1b727220a95).wrapping_add(t.columns[ci][i]); }
                    if let Some(&slot) = key_to_slot.get(&key_hash) {
                        let v = f64::from_bits(t.columns[a][i]);
                        if is_min { if v < mm[slot] { mm[slot] = v; } } else { if v > mm[slot] { mm[slot] = v; } }
                    }
                }
                for g in 0..num_groups_found { final_sums[ai * num_groups_found + g] = mm[g]; }
            }
        }

        let finalized: Vec<(u64, Vec<f64>, u64, Vec<f64>, Vec<f64>)> = (0..num_groups_found).map(|g| {
            let key = group_keys_discovered[g];
            let sums: Vec<f64> = (0..num_aggs_actual).map(|a| final_sums[a * num_groups_found + g]).collect();
            (key, sums, final_counts[g], vec![0.0; num_aggs_actual], vec![0.0; num_aggs_actual])
        }).collect();
        let mut cols: Vec<ResultColumn> = Vec::with_capacity(query.select.len());

        for (item_idx, item) in query.select.iter().enumerate() {
            let name = item.alias.clone().unwrap_or_else(|| self.expr_name(&item.expr));
            let values: Vec<u64> = match plans[item_idx].as_ref().unwrap() {
                LcAgg::GroupByCol(_) => {
                    finalized.iter().map(|(key, _, _, _, _)| *key).collect()
                }
                LcAgg::CountAll => {
                    finalized.iter().map(|(_, _, count, _, _)| *count).collect()
                }
                LcAgg::SumCol(_) | LcAgg::SumColCol(_, _) | LcAgg::SumColSubOne(_, _) | LcAgg::SumColSubOneAddOne(_, _, _) => {
                    let agg_idx = agg_indices.iter().position(|&idx| idx == item_idx).unwrap();
                    finalized.iter().map(|(_, sums, _, _, _)| sums[agg_idx].to_bits()).collect()
                }
                LcAgg::AvgCol(_) => {
                    let agg_idx = agg_indices.iter().position(|&idx| idx == item_idx).unwrap();
                    finalized.iter().map(|(_, sums, count, _, _)| {
                        if *count == 0 { 0u64 } else { (sums[agg_idx] / *count as f64).to_bits() }
                    }).collect()
                }
                LcAgg::MinCol(_) => {
                    let agg_idx = agg_indices.iter().position(|&idx| idx == item_idx).unwrap();
                    finalized.iter().map(|(_, _, _, mins, _)| mins[agg_idx].to_bits()).collect()
                }
                LcAgg::MaxCol(_) => {
                    let agg_idx = agg_indices.iter().position(|&idx| idx == item_idx).unwrap();
                    finalized.iter().map(|(_, _, _, _, maxs)| maxs[agg_idx].to_bits()).collect()
                }
            };
            cols.push(ResultColumn { name, values });
        }

        let mut result = QueryResult { columns: cols, row_count: finalized.len(), elapsed_us: 0 };

        if !query.order_by.is_empty() {
            result = self.apply_order_by_grouped(result, &query.order_by)?;
        }
        if let Some(limit) = query.limit {
            if result.row_count > limit {
                for col in &mut result.columns { col.values.truncate(limit); }
                result.row_count = limit;
            }
        }
        Ok(Some(result))
    }

    fn execute_grouped(&self, query: &SelectQuery2, t: &ExecTable, mask: &[bool]) -> Result<QueryResult, Error> {
        if query.group_by.is_empty() {
            let indices: Vec<usize> = (0..t.row_count).filter(|&i| mask[i]).collect();
            return self.execute_scalar_agg(query, t, &indices);
        }

        // Low-cardinality fast path (Q1: 4 groups, Q13: ~40 groups)
        if let Some(result) = self.try_low_card_grouped(query, t, mask)? {
            return Ok(result);
        }

        // Fallback: HashMap-based grouping for high cardinality.
        // PARALLEL: split into chunks, each thread builds a local HashMap,
        // then merge. This is critical for Q3 (10k groups, 300k rows) which
        // was serial and took ~200ms just for grouping.
        let indices: Vec<usize> = (0..t.row_count).filter(|&i| mask[i]).collect();

        // Pre-resolve GROUP BY column indices. For computed expressions
        // (extract, substr), pre-evaluate per row (serial — needed because
        // TpchExec is not Sync due to Cell/RefCell).
        let gb_cols: Vec<Option<usize>> = query.group_by.iter()
            .map(|gb| self.col_in(gb, t))
            .collect();
        let has_computed_gb = gb_cols.iter().any(|c| c.is_none());
        // Pre-compute GROUP BY values for computed expressions
        let gb_values: Vec<Vec<u64>> = if has_computed_gb {
            query.group_by.iter().enumerate().map(|(gi, gb)| {
                if gb_cols[gi].is_some() {
                    Vec::new() // will read from column directly
                } else {
                    indices.iter().map(|&idx| self.eval(gb, t, idx).unwrap_or(Value2::Null).to_u64()).collect()
                }
            }).collect()
        } else {
            Vec::new()
        };

        // PARALLEL grouping: split indices into chunks, each thread builds
        // a local HashMap<u64, Vec<usize>>, then merge.
        const GROUP_CHUNK_SIZE: usize = 65536;
        let n_indices = indices.len();
        let num_chunks = (n_indices + GROUP_CHUNK_SIZE - 1) / GROUP_CHUNK_SIZE;

        let local_maps: Vec<FxHashMap<u64, Vec<usize>>> = (0..num_chunks).into_par_iter().map(|chunk_idx| {
            let start = chunk_idx * GROUP_CHUNK_SIZE;
            let end = std::cmp::min(start + GROUP_CHUNK_SIZE, n_indices);
            let mut local: FxHashMap<u64, Vec<usize>> = new_fxhashmap();

            for i in start..end {
                let idx = indices[i];
                let mut key_hash: u64 = 0;
                for (gi, _) in query.group_by.iter().enumerate() {
                    let v = if let Some(ci) = gb_cols[gi] {
                        t.columns[ci][idx]
                    } else {
                        // Use pre-computed value
                        gb_values[gi][i]
                    };
                    key_hash = key_hash.wrapping_mul(0x517cc1b727220a95).wrapping_add(v);
                }
                local.entry(key_hash).or_default().push(idx);
            }
            local
        }).collect();

        // Merge local maps into global group_indices
        let mut group_map: FxHashMap<u64, usize> = new_fxhashmap();

        let mut group_indices: Vec<Vec<usize>> = Vec::with_capacity(1024);
        for local in local_maps {
            for (hash, rows) in local {
                let gid = if let Some(&existing) = group_map.get(&hash) {
                    existing
                } else {
                    let new_id = group_indices.len();
                    group_map.insert(hash, new_id);
                    group_indices.push(Vec::new());
                    new_id
                };
                group_indices[gid].extend(rows);
            }
        }

        let group_indices: Vec<&Vec<usize>> = group_indices.iter().collect();

        // HAVING
        let filtered: Vec<usize> = if let Some(ref having) = query.having {
            let mut v = Vec::new();
            for (gi, gidxs) in group_indices.iter().enumerate() {
                let hv = self.eval_agg_expr(having, t, gidxs)?;
                if self.truthy(&hv) { v.push(gi); }
            }
            v
        } else { (0..group_indices.len()).collect() };

        // Build result using FUSED per-group aggregation.
        let fused = self.try_fused_grouped_agg(&query.select, t, &group_indices, &filtered)?;
        let mut cols = Vec::new();
        for (item_idx, item) in query.select.iter().enumerate() {
            let name = item.alias.clone().unwrap_or_else(|| self.expr_name(&item.expr));
            let values: Vec<u64> = if let Some(ref fv) = fused {
                fv.get(item_idx).cloned().unwrap_or_else(|| {
                    filtered.iter().map(|&gi| {
                        let gidxs = group_indices[gi];
                        self.eval_agg_expr(&item.expr, t, gidxs).unwrap_or(Value2::Null).to_u64()
                    }).collect()
                })
            } else {
                filtered.iter().map(|&gi| {
                    let gidxs = group_indices[gi];
                    self.eval_agg_expr(&item.expr, t, gidxs).unwrap_or(Value2::Null).to_u64()
                }).collect()
            };
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

// Rust function to insert before execute_scalar_agg
    /// Fused per-group aggregation: analyze all select items, and if they
    /// match supported patterns, do a SINGLE pass per group computing all aggregates.
    fn try_fused_grouped_agg(
        &self, select: &[SelectItem2], t: &ExecTable,
        group_indices: &[&Vec<usize>], filtered: &[usize],
    ) -> Result<Option<Vec<Vec<u64>>>, Error> {
        if filtered.is_empty() {
            return Ok(Some(vec![Vec::new(); select.len()]));
        }

        #[derive(Clone)]
        enum FusedAgg {
            GroupByCol(usize),
            CountAll,
            SumCol(usize),
            SumColCol(usize, usize),
            SumColSubOne(usize, usize),
            SumColSubOneAddOne(usize, usize, usize),
            AvgCol(usize),
            MinCol(usize),
            MaxCol(usize),
        }

        let mut plans: Vec<Option<FusedAgg>> = Vec::with_capacity(select.len());
        for item in select {
            let plan = match &item.expr {
                Expr2::CountStar => Some(FusedAgg::CountAll),
                Expr2::Agg { func, arg, distinct: false } => {
                    match func {
                        AggFunc::Count => {
                            // count(Col) counts non-null (non-zero) values.
                            // count(*) counts all rows.
                            // The fused path only supports CountAll (count(*)).
                            // count(Col) falls back to per-row eval_agg_expr.
                            if self.col_in(arg, t).is_some() { None } else { Some(FusedAgg::CountAll) }
                        }
                        AggFunc::Sum => {
                            if let Some(a) = self.col_in(arg, t) {
                                if t.col_types[a] == ColType::Float { Some(FusedAgg::SumCol(a)) } else { None }
                            } else if let Expr2::BinOp { op: BinOp2::Mul, left, right } = arg.as_ref() {
                                if let (Some(a), Some(b)) = (self.col_in(left, t), self.col_in(right, t)) {
                                    if t.col_types[a] == ColType::Float && t.col_types[b] == ColType::Float {
                                        Some(FusedAgg::SumColCol(a, b))
                                    } else { None }
                                } else if let (Some(a), Some(b)) = (self.col_in(left, t), self.col_in_sub_one_right(right, t)) {
                                    if t.col_types[a] == ColType::Float && t.col_types[b] == ColType::Float {
                                        Some(FusedAgg::SumColSubOne(a, b))
                                    } else { None }
                                } else if let (Some(b), Some(a)) = (self.col_in(right, t), self.col_in_sub_one_right(left, t)) {
                                    if t.col_types[a] == ColType::Float && t.col_types[b] == ColType::Float {
                                        Some(FusedAgg::SumColSubOne(a, b))
                                    } else { None }
                                } else { None }
                            } else { None }
                        }
                        AggFunc::Avg => {
                            if let Expr2::Col(name) = arg.as_ref() {
                                if let Some(idx) = t.lookup_col(name) {
                                    if t.col_types[idx] == ColType::Float { Some(FusedAgg::AvgCol(idx)) } else { None }
                                } else { None }
                            } else { None }
                        }
                        AggFunc::Min => {
                            if let Expr2::Col(name) = arg.as_ref() {
                                if let Some(idx) = t.lookup_col(name) { Some(FusedAgg::MinCol(idx)) } else { None }
                            } else { None }
                        }
                        AggFunc::Max => {
                            if let Expr2::Col(name) = arg.as_ref() {
                                if let Some(idx) = t.lookup_col(name) { Some(FusedAgg::MaxCol(idx)) } else { None }
                            } else { None }
                        }
                        _ => None,
                    }
                }
                Expr2::Col(name) => {
                    if let Some(idx) = t.lookup_col(name) { Some(FusedAgg::GroupByCol(idx)) } else { None }
                }
                _ => None,
            };
            plans.push(plan);
        }

        // Second pass for Sum(Col * (1 - Col2) * (1 + Col3))
        for (i, item) in select.iter().enumerate() {
            if plans[i].is_some() { continue; }
            if let Expr2::Agg { func: AggFunc::Sum, arg, distinct: false } = &item.expr {
                if let Expr2::BinOp { op: BinOp2::Mul, left, right } = arg.as_ref() {
                    // Try: (Col * (1 - Col2)) * (1 + Col3)
                    if let Some((a, b)) = self.col_in_mul_sub_one(left, t) {
                        if let Some(c) = self.col_in_add_one_right(right, t) {
                            if t.col_types[a] == ColType::Float && t.col_types[b] == ColType::Float && t.col_types[c] == ColType::Float {
                                plans[i] = Some(FusedAgg::SumColSubOneAddOne(a, b, c));
                            }
                        }
                    }
                    // Try: Col * ((1 - Col2) * (1 + Col3))
                    else if let Some(a) = self.col_in(left, t) {
                        if let Some((b, c)) = self.col_in_mul_sub_one_add_one(right, t) {
                            if t.col_types[a] == ColType::Float && t.col_types[b] == ColType::Float && t.col_types[c] == ColType::Float {
                                plans[i] = Some(FusedAgg::SumColSubOneAddOne(a, b, c));
                            }
                        }
                    }
                }
            }
        }

        if plans.iter().any(|p| p.is_none()) {
            return Ok(None);
        }

        let num_groups = filtered.len();
        let mut results: Vec<Vec<u64>> = vec![Vec::with_capacity(num_groups); select.len()];

        for &gi in filtered {
            let indices = group_indices[gi];
            let mut sums: Vec<f64> = vec![0.0; select.len()];
            let mut counts: Vec<u64> = vec![0; select.len()];
            let mut mins: Vec<f64> = vec![f64::INFINITY; select.len()];
            let mut maxs: Vec<f64> = vec![f64::NEG_INFINITY; select.len()];
            let mut gb_vals: Vec<u64> = vec![0; select.len()];
            let mut gb_found: Vec<bool> = vec![false; select.len()];

            // W3: per-plan SIMD dispatch for large groups; scalar per-row for
            // small groups. The SIMD kernels have ~30 cycles of setup
            // (8 zero accumulators + horizontal reduce) which is only
            // amortized when the group has enough rows to fill >= 1 full
            // 4-accumulator iteration (32 rows). Below this threshold the
            // scalar per-row loop is faster. See W-MATH-RESEARCH trick #3.
            //
            // Q3 (~10K groups x 2 rows each) hits the scalar path entirely;
            // Q5 (5 groups x ~100K rows) and Q18 (57 groups, mixed) hit the
            // SIMD path for their large groups.
            let n = indices.len();
            if n >= 32 {
                use crate::exec::simd_agg;
                for (j, plan) in plans.iter().enumerate() {
                    match plan.as_ref().unwrap() {
                        FusedAgg::GroupByCol(idx) => {
                            if n > 0 { gb_vals[j] = t.columns[*idx][indices[0]]; gb_found[j] = true; }
                        }
                        FusedAgg::CountAll => { counts[j] = n as u64; }
                        FusedAgg::SumCol(a) => {
                            sums[j] = simd_agg::sum_f64_by_idx(&t.columns[*a], indices);
                        }
                        FusedAgg::SumColCol(a, b) => {
                            sums[j] = simd_agg::sum_a_mul_b_by_idx(&t.columns[*a], &t.columns[*b], indices);
                        }
                        FusedAgg::SumColSubOne(a, b) => {
                            // Distributive: sum(a) - sum(a*b) - two FMA chains.
                            sums[j] = simd_agg::sum_a_mul_one_minus_b_by_idx(&t.columns[*a], &t.columns[*b], indices);
                        }
                        FusedAgg::SumColSubOneAddOne(a, b, c) => {
                            // Distributive: sum_a + sum(a*c) - sum(a*b) - sum(a*b*c).
                            sums[j] = simd_agg::sum_a_mul_one_minus_b_mul_one_plus_c_by_idx(
                                &t.columns[*a], &t.columns[*b], &t.columns[*c], indices);
                        }
                        FusedAgg::AvgCol(a) => {
                            sums[j] = simd_agg::sum_f64_by_idx(&t.columns[*a], indices);
                            counts[j] = n as u64;
                        }
                        FusedAgg::MinCol(a) => {
                            let col = &t.columns[*a];
                            let mut m = f64::INFINITY;
                            for &i in indices {
                                let v = f64::from_bits(col[i]);
                                if v < m { m = v; }
                            }
                            mins[j] = m;
                        }
                        FusedAgg::MaxCol(a) => {
                            let col = &t.columns[*a];
                            let mut m = f64::NEG_INFINITY;
                            for &i in indices {
                                let v = f64::from_bits(col[i]);
                                if v > m { m = v; }
                            }
                            maxs[j] = m;
                        }
                    }
                }
            } else {
                // Scalar per-row path for small groups (avoids SIMD setup overhead).
                for &i in indices {
                    for (j, plan) in plans.iter().enumerate() {
                        match plan.as_ref().unwrap() {
                            FusedAgg::GroupByCol(idx) => {
                                if !gb_found[j] { gb_vals[j] = t.columns[*idx][i]; gb_found[j] = true; }
                            }
                            FusedAgg::CountAll => { counts[j] += 1; }
                            FusedAgg::SumCol(a) => { sums[j] += f64::from_bits(t.columns[*a][i]); }
                            FusedAgg::SumColCol(a, b) => { sums[j] += f64::from_bits(t.columns[*a][i]) * f64::from_bits(t.columns[*b][i]); }
                            FusedAgg::SumColSubOne(a, b) => { sums[j] += f64::from_bits(t.columns[*a][i]) * (1.0 - f64::from_bits(t.columns[*b][i])); }
                            FusedAgg::SumColSubOneAddOne(a, b, c) => {
                                sums[j] += f64::from_bits(t.columns[*a][i]) * (1.0 - f64::from_bits(t.columns[*b][i])) * (1.0 + f64::from_bits(t.columns[*c][i]));
                            }
                            FusedAgg::AvgCol(a) => { sums[j] += f64::from_bits(t.columns[*a][i]); counts[j] += 1; }
                            FusedAgg::MinCol(a) => { let v = f64::from_bits(t.columns[*a][i]); if v < mins[j] { mins[j] = v; } }
                            FusedAgg::MaxCol(a) => { let v = f64::from_bits(t.columns[*a][i]); if v > maxs[j] { maxs[j] = v; } }
                        }
                    }
                }
            }

            for (j, plan) in plans.iter().enumerate() {
                let val = match plan.as_ref().unwrap() {
                    FusedAgg::GroupByCol(_) => gb_vals[j],
                    FusedAgg::CountAll => counts[j],
                    FusedAgg::SumCol(_) | FusedAgg::SumColCol(_, _) | FusedAgg::SumColSubOne(_, _) | FusedAgg::SumColSubOneAddOne(_, _, _) => sums[j].to_bits(),
                    FusedAgg::AvgCol(_) => if counts[j] == 0 { 0u64 } else { (sums[j] / counts[j] as f64).to_bits() },
                    FusedAgg::MinCol(_) => mins[j].to_bits(),
                    FusedAgg::MaxCol(_) => maxs[j].to_bits(),
                };
                results[j].push(val);
            }
        }

        Ok(Some(results))
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
                if *distinct {
                    // Distinct requires materializing values — use slow path
                    let mut values: Vec<Value2> = Vec::with_capacity(indices.len());
                    for &idx in indices { values.push(self.eval(arg, t, idx)?); }
                    let mut seen = new_hashset();
                    values.retain(|v| {
                        let key = match v {
                            Value2::Int(i) => format!("i{}", i),
                            Value2::Float(f) => format!("f{}", f.to_bits()),
                            Value2::Str(s) => format!("s{}", s),
                            _ => "null".to_string(),
                        };
                        seen.insert(key)
                    });
                    return Ok(match func {
                        AggFunc::Count => Value2::Int(values.len() as i64),
                        AggFunc::CountDistinct => Value2::Int(values.len() as i64),
                        AggFunc::Sum => self.sum_values(&values),
                        AggFunc::Avg => self.avg_values(&values),
                        AggFunc::Min => self.min_values(&values),
                        AggFunc::Max => self.max_values(&values),
                    });
                }

                // Vectorized fast paths for common aggregate patterns.
                // These avoid per-row eval() and Value2 allocation entirely.
                match func {
                    AggFunc::Count => {
                        // Count(Col) = count non-null
                        if let Expr2::Col(name) = arg.as_ref() {
                            if let Some(idx) = t.lookup_col(name) {
                                let col = &t.columns[idx];
                                let mut cnt = 0i64;
                                for &i in indices { if col[i] != 0 { cnt += 1; } }
                                return Ok(Value2::Int(cnt));
                            }
                        }
                        // Fallback
                        return Ok(Value2::Int(indices.len() as i64));
                    }
                    AggFunc::CountDistinct => {
                        if let Expr2::Col(name) = arg.as_ref() {
                            if let Some(idx) = t.lookup_col(name) {
                                let col = &t.columns[idx];
                                let mut seen = new_hashset();
                                for &i in indices { seen.insert(col[i]); }
                                return Ok(Value2::Int(seen.len() as i64));
                            }
                        }
                        let mut seen = new_hashset();
                        for &i in indices { let v = self.eval(arg, t, i)?; seen.insert(format!("{:?}", v)); }
                        return Ok(Value2::Int(seen.len() as i64));
                    }
                    AggFunc::Sum => {
                        return self.sum_vec(arg, t, indices);
                    }
                    AggFunc::Avg => {
                        let sum = self.sum_vec(arg, t, indices)?;
                        let cnt = indices.len() as f64;
                        if cnt == 0.0 { return Ok(Value2::Null); }
                        let sf = sum.as_f64().unwrap_or(0.0);
                        return Ok(Value2::Float(sf / cnt));
                    }
                    AggFunc::Min => {
                        return self.minmax_vec(arg, t, indices, true);
                    }
                    AggFunc::Max => {
                        return self.minmax_vec(arg, t, indices, false);
                    }
                }
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
                let cmp = va.total_cmp(&vb);
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
                let cmp = va.total_cmp(&vb);
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


    /// Check if an expression contains any column references.
    fn expr_has_col(&self, e: &Expr2) -> bool {
        match e {
            Expr2::Col(_) => true,
            Expr2::BinOp { left, right, .. } => self.expr_has_col(left) || self.expr_has_col(right),
            Expr2::Case { whens, else_ } => {
                whens.iter().any(|(c, r)| self.expr_has_col(c) || self.expr_has_col(r))
                    || else_.as_ref().map(|e| self.expr_has_col(e)).unwrap_or(false)
            }
            Expr2::Neg(e) | Expr2::Not(e) | Expr2::Extract { expr: e, .. } => self.expr_has_col(e),
            Expr2::Like { expr, pattern, .. } => self.expr_has_col(expr) || self.expr_has_col(pattern),
            Expr2::Between { expr, low, high, .. } => self.expr_has_col(expr) || self.expr_has_col(low) || self.expr_has_col(high),
            Expr2::InList { expr, list, .. } => self.expr_has_col(expr) || list.iter().any(|e| self.expr_has_col(e)),
            Expr2::Substr { expr, start, len } => self.expr_has_col(expr) || self.expr_has_col(start) || self.expr_has_col(len),
            // Subqueries can reference outer columns (correlated). Treat as
            // "has column refs" so eval_comparison_vec falls back to per-row
            // eval, which sets up the correct outer context for each row.
            // Without this, `Col = (correlated subquery)` was treated as
            // `Col = const` and the subquery was evaluated ONCE at row 0,
            // producing wrong results (e.g. Q2 returned 1 row instead of 100).
            Expr2::Subquery(_) | Expr2::Exists { .. } | Expr2::InSubquery { .. } => true,
            _ => false,
        }
    }

    /// Vectorized sum: evaluates an expression for all indices and sums.
    /// Fast paths for Col, Col*Col, Col*(1-Col), Col*literal.
    fn sum_vec(&self, expr: &Expr2, t: &ExecTable, indices: &[usize]) -> Result<Value2, Error> {
        match expr {
            Expr2::Col(name) => {
                if let Some(idx) = t.lookup_col(name) {
                    let col = &t.columns[idx];
                    return Ok(match t.col_types[idx] {
                        ColType::Float => {
                            let mut sum = 0.0f64;
                            for &i in indices { sum += f64::from_bits(col[i]); }
                            Value2::Float(sum)
                        }
                        ColType::Int => {
                            let mut isum = 0i64;
                            for &i in indices { isum = isum.wrapping_add(col[i] as i64); }
                            Value2::Int(isum)
                        }
                        _ => Value2::Int(0),
                    });
                }
                Err(Error::NotFound(format!("column '{}'", name)))
            }
            Expr2::BinOp { op: BinOp2::Mul, left, right } => {
                // W21: BF16 fast path for Sum(Col * Col) on float columns
                if let (Some(a), Some(b)) = (self.col_in(left, t), self.col_in(right, t)) {
                    if t.col_types[a] == ColType::Float && t.col_types[b] == ColType::Float
                        && crate::kernel::vnni_agg::has_bf16() {
                        let ca = &t.columns[a];
                        let cb = &t.columns[b];
                        let n = indices.len();
                        let mut da = vec![0u64; n];
                        let mut db = vec![0u64; n];
                        for (k, &i) in indices.iter().enumerate() {
                            da[k] = ca[i];
                            db[k] = cb[i];
                        }
                        let mask = vec![true; n];
                        return Ok(Value2::Float(crate::kernel::vnni_agg::dot_f64_bf16(&da, &db, &mask)));
                    }
                }
                // Fast path: Col * (1 - Col2)  [Q1 sum_disc_price pattern]
                if let (Some(a), Some(b)) = (self.col_in(left, t), self.col_in_sub_one_right(right, t)) {
                    let ca = &t.columns[a];
                    let cb = &t.columns[b];
                    if t.col_types[a] == ColType::Float && t.col_types[b] == ColType::Float {
                        let mut sum = 0.0f64;
                        for &i in indices {
                            sum += f64::from_bits(ca[i]) * (1.0 - f64::from_bits(cb[i]));
                        }
                        return Ok(Value2::Float(sum));
                    }
                }
                // Fast path: (1 - Col2) * Col  [reversed]
                if let (Some(b), Some(a)) = (self.col_in(right, t), self.col_in_sub_one_right(left, t)) {
                    let ca = &t.columns[a];
                    let cb = &t.columns[b];
                    if t.col_types[a] == ColType::Float && t.col_types[b] == ColType::Float {
                        let mut sum = 0.0f64;
                        for &i in indices {
                            sum += f64::from_bits(ca[i]) * (1.0 - f64::from_bits(cb[i]));
                        }
                        return Ok(Value2::Float(sum));
                    }
                }
                // Fast path: Col * (1 - Col2) * (1 + Col3)  [Q1 sum_charge pattern]
                if let Some(a) = self.col_in(left, t) {
                    if let Some((b, c)) = self.col_in_mul_sub_one_add_one(right, t) {
                        let ca = &t.columns[a];
                        let cb = &t.columns[b];
                        let cc = &t.columns[c];
                        if t.col_types[a] == ColType::Float && t.col_types[b] == ColType::Float && t.col_types[c] == ColType::Float {
                            let mut sum = 0.0f64;
                            for &i in indices {
                                sum += f64::from_bits(ca[i]) * (1.0 - f64::from_bits(cb[i])) * (1.0 + f64::from_bits(cc[i]));
                            }
                            return Ok(Value2::Float(sum));
                        }
                    }
                }
                // Col * Col  or  Col * Literal  or  Literal * Col
                let li = self.col_in(left, t);
                let ri = self.col_in(right, t);
                match (li, ri) {
                    (Some(a), Some(b)) => {
                        // Col * Col — both float columns
                        let ca = &t.columns[a];
                        let cb = &t.columns[b];
                        let mut sum = 0.0f64;
                        if t.col_types[a] == ColType::Float && t.col_types[b] == ColType::Float {
                            for &i in indices {
                                sum += f64::from_bits(ca[i]) * f64::from_bits(cb[i]);
                            }
                        } else {
                            for &i in indices {
                                sum += ca[i] as f64 * cb[i] as f64;
                            }
                        }
                        Ok(Value2::Float(sum))
                    }
                    (Some(a), None) => {
                        if self.expr_has_col(right) {
                            // Right side has column refs — can't treat as constant.
                            // Per-row eval: eval right for each row, multiply by left col.
                            let ca = &t.columns[a];
                            let mut sum = 0.0f64;
                            if t.col_types[a] == ColType::Float {
                                for &i in indices {
                                    let rf = self.eval(right, t, i)?.as_f64().unwrap_or(0.0);
                                    sum += f64::from_bits(ca[i]) * rf;
                                }
                            } else {
                                for &i in indices {
                                    let rf = self.eval(right, t, i)?.as_f64().unwrap_or(0.0);
                                    sum += ca[i] as f64 * rf;
                                }
                            }
                            Ok(Value2::Float(sum))
                        } else {
                            // Right is truly constant
                            let rval = self.eval_const(right, t)?;
                            let factor = rval.as_f64().unwrap_or(0.0);
                            let col = &t.columns[a];
                            let mut sum = 0.0f64;
                            if t.col_types[a] == ColType::Float {
                                for &i in indices { sum += f64::from_bits(col[i]) * factor; }
                            } else {
                                for &i in indices { sum += col[i] as f64 * factor; }
                            }
                            Ok(Value2::Float(sum))
                        }
                    }
                    (None, Some(b)) => {
                        if self.expr_has_col(left) {
                            let cb = &t.columns[b];
                            let mut sum = 0.0f64;
                            if t.col_types[b] == ColType::Float {
                                for &i in indices {
                                    let lf = self.eval(left, t, i)?.as_f64().unwrap_or(0.0);
                                    sum += lf * f64::from_bits(cb[i]);
                                }
                            } else {
                                for &i in indices {
                                    let lf = self.eval(left, t, i)?.as_f64().unwrap_or(0.0);
                                    sum += lf * cb[i] as f64;
                                }
                            }
                            Ok(Value2::Float(sum))
                        } else {
                            let lval = self.eval_const(left, t)?;
                            let factor = lval.as_f64().unwrap_or(0.0);
                            let col = &t.columns[b];
                            let mut sum = 0.0f64;
                            if t.col_types[b] == ColType::Float {
                                for &i in indices { sum += factor * f64::from_bits(col[i]); }
                            } else {
                                for &i in indices { sum += factor * col[i] as f64; }
                            }
                            Ok(Value2::Float(sum))
                        }
                    }
                    _ => {
                        // Fallback: per-row eval
                        let mut sum = 0.0f64;
                        for &i in indices { if let Some(f) = self.eval(expr, t, i)?.as_f64() { sum += f; } }
                        Ok(Value2::Float(sum))
                    }
                }
            }
            Expr2::BinOp { op: BinOp2::Sub, left, right } => {
                // (1 - Col) pattern — common in TPC-H: l_extendedprice * (1 - l_discount)
                let li = self.col_in(left, t);
                let ri = self.col_in(right, t);
                match (li, ri) {
                    (None, Some(b)) => {
                        let lval = self.eval_const(left, t)?;
                        let base = lval.as_f64().unwrap_or(0.0);
                        let col = &t.columns[b];
                        let mut sum = 0.0f64;
                        if t.col_types[b] == ColType::Float {
                            for &i in indices { sum += base - f64::from_bits(col[i]); }
                        } else {
                            for &i in indices { sum += base - col[i] as f64; }
                        }
                        Ok(Value2::Float(sum))
                    }
                    (Some(a), None) => {
                        if self.expr_has_col(right) {
                            let ca = &t.columns[a];
                            let mut sum = 0.0f64;
                            if t.col_types[a] == ColType::Float {
                                for &i in indices {
                                    let rf = self.eval(right, t, i)?.as_f64().unwrap_or(0.0);
                                    sum += f64::from_bits(ca[i]) - rf;
                                }
                            } else {
                                for &i in indices {
                                    let rf = self.eval(right, t, i)?.as_f64().unwrap_or(0.0);
                                    sum += ca[i] as f64 - rf;
                                }
                            }
                            Ok(Value2::Float(sum))
                        } else {
                            let rval = self.eval_const(right, t)?;
                            let sub = rval.as_f64().unwrap_or(0.0);
                            let col = &t.columns[a];
                            let mut sum = 0.0f64;
                            if t.col_types[a] == ColType::Float {
                                for &i in indices { sum += f64::from_bits(col[i]) - sub; }
                            } else {
                                for &i in indices { sum += col[i] as f64 - sub; }
                            }
                            Ok(Value2::Float(sum))
                        }
                    }
                    _ => {
                        let mut sum = 0.0f64;
                        for &i in indices { if let Some(f) = self.eval(expr, t, i)?.as_f64() { sum += f; } }
                        Ok(Value2::Float(sum))
                    }
                }
            }
            Expr2::BinOp { op: BinOp2::Add, left, right } => {
                let li = self.col_in(left, t);
                let ri = self.col_in(right, t);
                match (li, ri) {
                    (Some(a), Some(b)) => {
                        let ca = &t.columns[a]; let cb = &t.columns[b];
                        let mut sum = 0.0f64;
                        if t.col_types[a] == ColType::Float && t.col_types[b] == ColType::Float {
                            for &i in indices { sum += f64::from_bits(ca[i]) + f64::from_bits(cb[i]); }
                        } else {
                            for &i in indices { sum += ca[i] as f64 + cb[i] as f64; }
                        }
                        Ok(Value2::Float(sum))
                    }
                    _ => {
                        let mut sum = 0.0f64;
                        for &i in indices { if let Some(f) = self.eval(expr, t, i)?.as_f64() { sum += f; } }
                        Ok(Value2::Float(sum))
                    }
                }
            }
            _ => {
                // Fallback: per-row eval for complex expressions
                let mut sum = 0.0f64;
                for &i in indices { if let Some(f) = self.eval(expr, t, i)?.as_f64() { sum += f; } }
                Ok(Value2::Float(sum))
            }
        }
    }

    /// Vectorized min/max.
    fn minmax_vec(&self, expr: &Expr2, t: &ExecTable, indices: &[usize], is_min: bool) -> Result<Value2, Error> {
        if let Expr2::Col(name) = expr {
            if let Some(idx) = t.lookup_col(name) {
                let col = &t.columns[idx];
                if t.col_types[idx] == ColType::Float {
                    let mut best: Option<f64> = None;
                    for &i in indices {
                        let v = f64::from_bits(col[i]);
                        best = Some(match best {
                            None => v,
                            Some(b) => if is_min { b.min(v) } else { b.max(v) }
                        });
                    }
                    return Ok(best.map(Value2::Float).unwrap_or(Value2::Null));
                } else {
                    let mut best: Option<i64> = None;
                    for &i in indices {
                        let v = col[i] as i64;
                        best = Some(match best {
                            None => v,
                            Some(b) => if is_min { b.min(v) } else { b.max(v) }
                        });
                    }
                    return Ok(best.map(Value2::Int).unwrap_or(Value2::Null));
                }
            }
        }
        // Fallback
        let mut best: Option<f64> = None;
        for &i in indices {
            if let Some(f) = self.eval(expr, t, i)?.as_f64() {
                best = Some(match best {
                    None => f,
                    Some(b) => if is_min { b.min(f) } else { b.max(f) }
                });
            }
        }
        Ok(best.map(Value2::Float).unwrap_or(Value2::Null))
    }

    /// Detect the pattern `(1 - Col)` and return the column index.
    fn col_in_sub_one_right(&self, expr: &Expr2, t: &ExecTable) -> Option<usize> {
        if let Expr2::BinOp { op: BinOp2::Sub, left, right } = expr {
            let is_one = match left.as_ref() {
                Expr2::Int(i) if *i == 1 => true,
                Expr2::Float(f) if *f == 1.0 => true,
                _ => false,
            };
            if is_one {
                return self.col_in(right, t);
            }
        }
        None
    }

    /// Detect the pattern Col * (1 - Col2) and return (col, col2).
    fn col_in_mul_sub_one(&self, expr: &Expr2, t: &ExecTable) -> Option<(usize, usize)> {
        if let Expr2::BinOp { op: BinOp2::Mul, left, right } = expr {
            if let (Some(a), Some(b)) = (self.col_in(left, t), self.col_in_sub_one_right(right, t)) {
                return Some((a, b));
            }
            if let (Some(b), Some(a)) = (self.col_in(right, t), self.col_in_sub_one_right(left, t)) {
                return Some((a, b));
            }
        }
        None
    }

    /// Detect the pattern `(1 - Col2) * (1 + Col3)` and return (col2, col3).
    fn col_in_mul_sub_one_add_one(&self, expr: &Expr2, t: &ExecTable) -> Option<(usize, usize)> {
        if let Expr2::BinOp { op: BinOp2::Mul, left, right } = expr {
            let b = self.col_in_sub_one_right(left, t);
            let c = self.col_in_add_one_right(right, t);
            if let (Some(b), Some(c)) = (b, c) { return Some((b, c)); }
            let b = self.col_in_sub_one_right(right, t);
            let c = self.col_in_add_one_right(left, t);
            if let (Some(b), Some(c)) = (b, c) { return Some((b, c)); }
        }
        None
    }

    /// Detect the pattern `(1 + Col)` and return the column index.
    fn col_in_add_one_right(&self, expr: &Expr2, t: &ExecTable) -> Option<usize> {
        if let Expr2::BinOp { op: BinOp2::Add, left, right } = expr {
            let is_one = match left.as_ref() {
                Expr2::Int(i) if *i == 1 => true,
                Expr2::Float(f) if *f == 1.0 => true,
                _ => false,
            };
            if is_one {
                return self.col_in(right, t);
            }
        }
        None
    }

    fn sum_values(&self, values: &[Value2]) -> Value2 {
        let mut sum = 0.0f64;
        let mut all_int = true;
        for v in values {
            if !matches!(v, Value2::Int(_)) { all_int = false; }
            if let Some(f) = v.as_f64() { sum += f; }
        }
        if all_int {
            let mut isum = 0i64;
            for v in values { if let Some(i) = v.as_i64() { isum = isum.wrapping_add(i); } }
            Value2::Int(isum)
        } else { Value2::Float(sum) }
    }

    fn avg_values(&self, values: &[Value2]) -> Value2 {
        let mut sum = 0.0f64; let mut cnt = 0u64;
        for v in values { if let Some(f) = v.as_f64() { sum += f; cnt += 1; } }
        if cnt == 0 { Value2::Null } else { Value2::Float(sum / cnt as f64) }
    }

    fn min_values(&self, values: &[Value2]) -> Value2 {
        let mut min: Option<f64> = None;
        for v in values { if let Some(f) = v.as_f64() { min = Some(min.map_or(f, |m| m.min(f))); } }
        min.map(Value2::Float).unwrap_or(Value2::Null)
    }

    fn max_values(&self, values: &[Value2]) -> Value2 {
        let mut max: Option<f64> = None;
        for v in values { if let Some(f) = v.as_f64() { max = Some(max.map_or(f, |m| m.max(f))); } }
        max.map(Value2::Float).unwrap_or(Value2::Null)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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
        let cat = Catalog::new(); let exec = TpchExec { catalog: &cat, outer: std::cell::Cell::new(None), subquery_cache: std::cell::RefCell::new(new_hashmap()), exists_cache: std::cell::RefCell::new(new_hashmap()), exists_multi_cache: std::cell::RefCell::new(new_hashmap()), in_subquery_cache: std::cell::RefCell::new(new_hashmap()), decorrelated_cache: std::cell::RefCell::new(new_hashmap()) };
        assert!(exec.like("hello world", "%hello%"));
        assert!(exec.like("hello", "hello"));
        assert!(exec.like("hello world", "hello%"));
        assert!(exec.like("hello world", "%world"));
        assert!(!exec.like("hello", "world"));
        assert!(exec.like("PROMO STEEL", "PROMO%"));
    }
}
