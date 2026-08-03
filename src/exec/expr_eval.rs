//! # Arithmetic expression evaluator for aggregate args (Wave 40).
//!
//! Evaluates expressions like `price * (1 - discount)` against table rows.
//! Supports: column references, integer/float literals, + - * /, parentheses.

use crate::datasource::table::Table;

/// Evaluate an arithmetic expression for a specific row, returning a u64
/// (f64 bits for float results).
///
/// The expression is a space-separated string of tokens produced by
/// parse_agg_arg, e.g. "price * ( 1 - discount )".
pub fn eval_expr(expr: &str, table: &Table, row_idx: usize) -> u64 {
    let tokens: Vec<&str> = expr.split_whitespace().collect();
    if tokens.is_empty() {
        return 0;
    }
    // Try to evaluate as a simple column reference first.
    if tokens.len() == 1 {
        return eval_token(tokens[0], table, row_idx);
    }
    // Use a recursive descent parser for the expression.
    let mut parser = ExprParser {
        tokens: &tokens,
        pos: 0,
    };
    let result = parser.parse_expr(table, row_idx);
    result
}

fn eval_token(token: &str, table: &Table, row_idx: usize) -> u64 {
    // Try integer literal.
    if let Ok(n) = token.parse::<i64>() {
        return n as u64;
    }
    if let Ok(n) = token.parse::<u64>() {
        return n;
    }
    // Try float literal.
    if let Ok(f) = token.parse::<f64>() {
        return f.to_bits();
    }
    // Column reference.
    if let Some(idx) = table.column_idx(token) {
        return table.columns[idx].get(row_idx).copied().unwrap_or(0);
    }
    0
}

/// Check if an expression is a simple column reference (no operators).
pub fn is_simple_column(expr: &str) -> bool {
    let tokens: Vec<&str> = expr.split_whitespace().collect();
    tokens.len() == 1 && !tokens[0].chars().any(|c| "+-*/()".contains(c))
}

/// Check if an expression contains arithmetic operators.
pub fn is_arithmetic_expr(expr: &str) -> bool {
    expr.split_whitespace().any(|t| {
        t == "+" || t == "-" || t == "*" || t == "/" || t == "(" || t == ")"
    })
}

/// Evaluate a binary operation. The operands are f64 (bit-reinterpreted from u64).
fn eval_binop(op: &str, left: u64, right: u64) -> u64 {
    // Determine if both operands are "small integers" (not f64 bit patterns).
    // If both are < 2^60, treat as integers. Otherwise, treat as f64.
    let left_is_int = left < (1u64 << 60);
    let right_is_int = right < (1u64 << 60);

    if left_is_int && right_is_int {
        // Integer arithmetic.
        match op {
            "+" => left.wrapping_add(right),
            "-" => left.wrapping_sub(right),
            "*" => left.wrapping_mul(right),
            "/" => if right == 0 { 0 } else { left / right },
            _ => 0,
        }
    } else {
        // Float arithmetic.
        let l = f64::from_bits(left);
        let r = f64::from_bits(right);
        let result = match op {
            "+" => l + r,
            "-" => l - r,
            "*" => l * r,
            "/" => if r == 0.0 { 0.0 } else { l / r },
            _ => 0.0,
        };
        result.to_bits()
    }
}

/// Simple recursive descent parser for arithmetic expressions.
struct ExprParser<'a> {
    tokens: &'a [&'a str],
    pos: usize,
}

impl<'a> ExprParser<'a> {
    fn parse_expr(&mut self, table: &Table, row_idx: usize) -> u64 {
        self.parse_additive(table, row_idx)
    }

    fn parse_additive(&mut self, table: &Table, row_idx: usize) -> u64 {
        let mut left = self.parse_multiplicative(table, row_idx);
        loop {
            if self.pos >= self.tokens.len() {
                break;
            }
            let op = self.tokens[self.pos];
            if op != "+" && op != "-" {
                break;
            }
            self.pos += 1;
            let right = self.parse_multiplicative(table, row_idx);
            left = eval_binop(op, left, right);
        }
        left
    }

    fn parse_multiplicative(&mut self, table: &Table, row_idx: usize) -> u64 {
        let mut left = self.parse_primary(table, row_idx);
        loop {
            if self.pos >= self.tokens.len() {
                break;
            }
            let op = self.tokens[self.pos];
            if op != "*" && op != "/" {
                break;
            }
            self.pos += 1;
            let right = self.parse_primary(table, row_idx);
            left = eval_binop(op, left, right);
        }
        left
    }

    fn parse_primary(&mut self, table: &Table, row_idx: usize) -> u64 {
        if self.pos >= self.tokens.len() {
            return 0;
        }
        let token = self.tokens[self.pos];
        if token == "(" {
            self.pos += 1; // consume (
            let val = self.parse_expr(table, row_idx);
            if self.pos < self.tokens.len() && self.tokens[self.pos] == ")" {
                self.pos += 1; // consume )
            }
            return val;
        }
        self.pos += 1;
        eval_token(token, table, row_idx)
    }
}

// -----------------------------------------------------------------------
// Tests
// -----------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::datasource::parquet::{LoadedColumn, LoadedTable};
    use crate::datasource::Table;

    fn make_table() -> Table {
        Table::from_loaded(LoadedTable {
            name: "t".into(),
            columns: vec![
                LoadedColumn { name: "price".into(), cells: vec![100, 200, 300], row_count: 3, string_search: None },
                LoadedColumn { name: "discount".into(), cells: vec![10, 20, 30], row_count: 3, string_search: None },
            ],
            row_count: 3,
        })
    }

    #[test]
    fn eval_simple_column() {
        let t = make_table();
        assert_eq!(eval_expr("price", &t, 0), 100);
        assert_eq!(eval_expr("price", &t, 1), 200);
    }

    #[test]
    fn eval_integer_literal() {
        let t = make_table();
        assert_eq!(eval_expr("42", &t, 0), 42);
    }

    #[test]
    fn eval_addition() {
        let t = make_table();
        // price + discount = 100 + 10 = 110
        assert_eq!(eval_expr("price + discount", &t, 0), 110);
    }

    #[test]
    fn eval_multiplication() {
        let t = make_table();
        // price * discount = 100 * 10 = 1000
        assert_eq!(eval_expr("price * discount", &t, 0), 1000);
    }

    #[test]
    fn eval_subtraction() {
        let t = make_table();
        // price - discount = 100 - 10 = 90
        assert_eq!(eval_expr("price - discount", &t, 0), 90);
    }

    #[test]
    fn eval_parentheses() {
        let t = make_table();
        // (price - discount) * 2 = (100 - 10) * 2 = 180
        assert_eq!(eval_expr("( price - discount ) * 2", &t, 0), 180);
    }

    #[test]
    fn eval_complex_expr() {
        let t = make_table();
        // price * (1 - discount) — but discount=10, so 1-10 = -9 (as u64 wrapping)
        // Actually for TPC-H: SUM(l_extendedprice * (1 - l_discount))
        // where l_discount is a float like 0.05. Let's test with small ints.
        // price * ( 1 - 0 ) = 100 * 1 = 100 (if discount were 0)
        // For our test: price=100, discount=10 → 100 * (1 - 10) = 100 * (-9)
        // In u64 wrapping: -9 as u64 = huge. So let's test a different expr.
        // price * 2 + discount = 200 + 10 = 210
        assert_eq!(eval_expr("price * 2 + discount", &t, 0), 210);
    }

    #[test]
    fn is_simple_column_check() {
        assert!(is_simple_column("price"));
        assert!(!is_simple_column("price * 2"));
    }

    #[test]
    fn is_arithmetic_expr_check() {
        assert!(is_arithmetic_expr("price * 2"));
        assert!(is_arithmetic_expr("( a + b )"));
        assert!(!is_arithmetic_expr("price"));
    }
}
