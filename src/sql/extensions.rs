//! turboGP SQL extensions parser.
//!
//! turboGP extends standard SQL with seven query-language extensions that
//! expose the engine's hardware-aware features directly to the user:
//!
//! | Extension | Syntax | Field |
//! |-----------|--------|-------|
//! | Approximate aggregation | `APPROXIMATE WITHIN <eps> CONFIDENCE <conf>` | [`QueryExtensions::approximate`] |
//! | Memory-tier pinning | `TIER <name>` | [`QueryExtensions::tier`] |
//! | Vector similarity | `SIMILAR TO [<col>] <hex> WITHIN HAMMING DISTANCE <n>` | [`QueryExtensions::similar_to`] |
//! | Consistency level | `CONSISTENCY <level>` | [`QueryExtensions::consistency`] |
//! | Sketch method | `USING <method>` | [`QueryExtensions::using`] |
//! | Memory budget | `MEMORY BUDGET <bytes>` | [`QueryExtensions::memory_budget`] |
//! | Energy budget | `ENERGY BUDGET <joules> [JOULES]` | [`QueryExtensions::energy_budget`] |
//!
//! The parser is a *scanner*: it walks the token stream looking for the
//! extension keywords above and parses each extension it finds, leaving
//! unrelated tokens untouched. This means the same function can parse a
//! standalone extension string (e.g. `"TIER CXL"`) or a full SQL query with
//! extensions interleaved (e.g. `"SELECT AVG(price) APPROXIMATE WITHIN 0.01
//! CONFIDENCE 0.99 FROM sales"`).
//!
//! ## Approximate semantics
//!
//! `APPROXIMATE WITHIN ε CONFIDENCE δ` is interpreted per ADR-015 (empirical
//! Bernstein bounds): the answer is guaranteed to be within `ε` of the true
//! value with probability at least `δ`. Internally we store `(ε, 1 - δ)` —
//! i.e. `(epsilon, failure_probability)` — because that is the form the
//! Bernstein bound consumes. A user-facing API would convert back.

use crate::sql::lexer::Token;

/// Parsed turboGP query extensions.
///
/// Each field corresponds to one of the seven extensions; `None` means the
/// extension was not present in the parsed token stream.
#[derive(Debug, Clone, Default)]
pub struct QueryExtensions {
    /// `APPROXIMATE WITHIN <eps> CONFIDENCE <conf>`: stored as
    /// `(epsilon, failure_probability)` where `failure_probability = 1 -
    /// confidence`. The error is at most `epsilon` with probability at
    /// least `1 - failure_probability`.
    pub approximate: Option<(f64, f64)>,
    /// `TIER <name>`: pin the query's working set to the named memory tier
    /// (e.g. `"L3"`, `"CXL"`, `"DDR5"`). Stored uppercased.
    pub tier: Option<String>,
    /// `SIMILAR TO [<col>] <hex> WITHIN HAMMING DISTANCE <n>`: vector
    /// similarity search. Stored as `(column, target_bytes, max_distance)`.
    /// `column` is the empty string if the user did not name a column.
    pub similar_to: Option<(String, Vec<u8>, u32)>,
    /// `CONSISTENCY <level>`: stored uppercased (e.g. `"STRONG"`,
    /// `"READ_COMMITTED"`, `"EVENTUAL"`).
    pub consistency: Option<String>,
    /// `USING <method>`: select a sketch / approximate-aggregate method
    /// (e.g. `"HYPERLOGLOG"`, `"COUNT_MIN"`). Stored uppercased.
    pub using: Option<String>,
    /// `MEMORY BUDGET <bytes>`: soft cap on bytes the query may touch.
    pub memory_budget: Option<u64>,
    /// `ENERGY BUDGET <joules> [JOULES]`: soft cap on joules the query may
    /// spend (RAPL-measured, ADR-022).
    pub energy_budget: Option<u64>,
}

/// Scan a token stream for turboGP extensions and return the parsed
/// [`QueryExtensions`].
///
/// Non-extension tokens (e.g. `SELECT`, `FROM`, column names) are skipped
/// silently. This lets the same function parse a standalone extension
/// string (`"TIER CXL"`) or a full SQL query with extensions interleaved.
///
/// # Errors
///
/// Returns `Err(String)` if an extension keyword is followed by something
/// other than its expected arguments (e.g. `APPROXIMATE` without `WITHIN`,
/// or `MEMORY BUDGET <negative>`). A parse error aborts the whole scan.
pub fn parse_extensions(tokens: Vec<Token>) -> Result<QueryExtensions, String> {
    let (ext, _stripped) = parse_extensions_and_strip(tokens)?;
    Ok(ext)
}

/// Scan a token stream for turboGP extensions, returning the parsed
/// [`QueryExtensions`] and a *stripped* token stream with all extension
/// tokens removed.
///
/// The stripped stream is suitable for feeding to
/// [`crate::sql::parser::parse`]: e.g. the token stream for
/// `"SELECT AVG(price) APPROXIMATE WITHIN 0.01 CONFIDENCE 0.99 FROM sales"`
/// produces `approximate = Some((0.01, 0.01))` and a stripped stream
/// equivalent to `"SELECT AVG(price) FROM sales"`.
///
/// # Errors
///
/// Same error conditions as [`parse_extensions`].
pub fn parse_extensions_and_strip(
    tokens: Vec<Token>,
) -> Result<(QueryExtensions, Vec<Token>), String> {
    let mut ext = QueryExtensions::default();
    let mut stripped = Vec::with_capacity(tokens.len());
    let mut cur = Cursor::new(&tokens);

    while let Some(token) = cur.peek() {
        let kw = match token {
            Token::Keyword(k) => k.as_str(),
            _ => {
                stripped.push(token.clone());
                cur.advance();
                continue;
            }
        };
        match kw {
            "APPROXIMATE" => {
                cur.advance(); // consume APPROXIMATE
                cur.expect_keyword("WITHIN")?;
                let epsilon = cur.expect_number()?;
                cur.expect_keyword("CONFIDENCE")?;
                let confidence = cur.expect_number()?;
                let failure_prob = 1.0 - confidence;
                ext.approximate = Some((epsilon, failure_prob));
            }
            "TIER" => {
                cur.advance();
                let name = cur.expect_ident()?;
                ext.tier = Some(name.to_uppercase());
            }
            "SIMILAR" => {
                cur.advance();
                cur.expect_keyword("TO")?;
                let (col, hex) = parse_similar_target(&mut cur)?;
                cur.expect_keyword("WITHIN")?;
                cur.expect_keyword("HAMMING")?;
                cur.expect_keyword("DISTANCE")?;
                let distance = cur.expect_int()?;
                if distance < 0 {
                    return Err(format!("HAMMING DISTANCE must be non-negative, got {distance}"));
                }
                ext.similar_to = Some((col, hex, distance as u32));
            }
            "CONSISTENCY" => {
                cur.advance();
                let level = cur.expect_ident()?;
                ext.consistency = Some(level.to_uppercase());
            }
            "USING" => {
                cur.advance();
                let method = cur.expect_ident()?;
                ext.using = Some(method.to_uppercase());
            }
            "MEMORY" => {
                cur.advance();
                cur.expect_keyword("BUDGET")?;
                let bytes = cur.expect_int()?;
                if bytes < 0 {
                    return Err(format!("MEMORY BUDGET must be non-negative, got {bytes}"));
                }
                ext.memory_budget = Some(bytes as u64);
            }
            "ENERGY" => {
                cur.advance();
                cur.expect_keyword("BUDGET")?;
                let joules = cur.expect_int()?;
                if joules < 0 {
                    return Err(format!("ENERGY BUDGET must be non-negative, got {joules}"));
                }
                let _ = cur.match_keyword("JOULES"); // optional unit
                ext.energy_budget = Some(joules as u64);
            }
            _ => {
                stripped.push(token.clone());
                cur.advance();
            }
        }
    }

    Ok((ext, stripped))
}

/// Parse the target of a `SIMILAR TO` clause: either `[col] <hex>` (with a
/// column name) or just `<hex>` (without).
fn parse_similar_target(cur: &mut Cursor<'_>) -> Result<(String, Vec<u8>), String> {
    match cur.peek() {
        Some(Token::Hex(_)) => {
            let hex = cur.take_hex()?;
            Ok((String::new(), hex))
        }
        Some(Token::Ident(_)) => {
            let col = cur.expect_ident()?;
            let hex = match cur.peek() {
                Some(Token::Hex(_)) => cur.take_hex()?,
                other => {
                    return Err(format!(
                        "expected hex literal after column in SIMILAR TO, got {other:?}"
                    ));
                }
            };
            Ok((col, hex))
        }
        other => Err(format!("expected column name or hex literal in SIMILAR TO, got {other:?}")),
    }
}

/// A read-only cursor over a token slice.
struct Cursor<'a> {
    tokens: &'a [Token],
    pos: usize,
}

impl<'a> Cursor<'a> {
    fn new(tokens: &'a [Token]) -> Self {
        Self { tokens, pos: 0 }
    }

    fn peek(&self) -> Option<&Token> {
        self.tokens.get(self.pos)
    }

    fn advance(&mut self) {
        if self.pos < self.tokens.len() {
            self.pos += 1;
        }
    }

    fn match_keyword(&mut self, kw: &str) -> bool {
        if let Some(Token::Keyword(k)) = self.peek() {
            if k == kw {
                self.pos += 1;
                return true;
            }
        }
        false
    }

    fn expect_keyword(&mut self, kw: &str) -> Result<(), String> {
        if self.match_keyword(kw) {
            return Ok(());
        }
        Err(format!("expected keyword {kw}, got {:?}", self.peek()))
    }

    fn expect_ident(&mut self) -> Result<String, String> {
        if let Some(Token::Ident(s)) = self.peek() {
            let s = s.clone();
            self.pos += 1;
            return Ok(s);
        }
        Err(format!("expected identifier, got {:?}", self.peek()))
    }

    fn expect_int(&mut self) -> Result<i64, String> {
        if let Some(Token::Int(i)) = self.peek() {
            let i = *i;
            self.pos += 1;
            return Ok(i);
        }
        Err(format!("expected integer, got {:?}", self.peek()))
    }

    /// Accept either an integer or a float as an `f64`. Used for `WITHIN`
    /// and `CONFIDENCE` so that `WITHIN 1` (int) is accepted alongside
    /// `WITHIN 0.01` (float).
    fn expect_number(&mut self) -> Result<f64, String> {
        match self.peek() {
            Some(Token::Float(f)) => {
                let f = *f;
                self.pos += 1;
                Ok(f)
            }
            Some(Token::Int(i)) => {
                let f = *i as f64;
                self.pos += 1;
                Ok(f)
            }
            other => Err(format!("expected number, got {other:?}")),
        }
    }

    fn take_hex(&mut self) -> Result<Vec<u8>, String> {
        if let Some(Token::Hex(h)) = self.peek() {
            let h = h.clone();
            self.pos += 1;
            return Ok(h);
        }
        Err(format!("expected hex literal, got {:?}", self.peek()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sql::lexer::tokenize;

    fn parse_ext(s: &str) -> QueryExtensions {
        let toks = tokenize(s).expect("tokenize failed");
        parse_extensions(toks).expect("parse_extensions failed")
    }

    #[test]
    fn parse_approximate_extension() {
        // APPROXIMATE WITHIN 0.01 CONFIDENCE 0.99
        // → (epsilon=0.01, failure_probability = 1 - 0.99 = 0.01)
        let ext = parse_ext("APPROXIMATE WITHIN 0.01 CONFIDENCE 0.99");
        let (eps, delta) = ext.approximate.expect("approximate should be set");
        assert!((eps - 0.01).abs() < 1e-12, "epsilon {eps}");
        assert!((delta - 0.01).abs() < 1e-12, "delta {delta}");
    }

    #[test]
    fn parse_tier_extension() {
        let ext = parse_ext("TIER CXL");
        assert_eq!(ext.tier.as_deref(), Some("CXL"));
    }

    #[test]
    fn parse_tier_extension_lowercases_to_upper() {
        let ext = parse_ext("TIER cxl");
        assert_eq!(ext.tier.as_deref(), Some("CXL"));
    }

    #[test]
    fn parse_similar_to_without_column() {
        let ext = parse_ext("SIMILAR TO x'AABB' WITHIN HAMMING DISTANCE 5");
        let (col, hex, dist) = ext.similar_to.expect("similar_to should be set");
        assert_eq!(col, "");
        assert_eq!(hex, vec![0xAA, 0xBB]);
        assert_eq!(dist, 5);
    }

    #[test]
    fn parse_similar_to_with_column() {
        let ext = parse_ext("SIMILAR TO embedding x'AABBCCDD' WITHIN HAMMING DISTANCE 12");
        let (col, hex, dist) = ext.similar_to.expect("similar_to should be set");
        assert_eq!(col, "embedding");
        assert_eq!(hex, vec![0xAA, 0xBB, 0xCC, 0xDD]);
        assert_eq!(dist, 12);
    }

    #[test]
    fn parse_consistency_extension() {
        let ext = parse_ext("CONSISTENCY STRONG");
        assert_eq!(ext.consistency.as_deref(), Some("STRONG"));
    }

    #[test]
    fn parse_using_extension() {
        let ext = parse_ext("USING HYPERLOGLOG");
        assert_eq!(ext.using.as_deref(), Some("HYPERLOGLOG"));
    }

    #[test]
    fn parse_memory_budget_extension() {
        let ext = parse_ext("MEMORY BUDGET 1073741824");
        assert_eq!(ext.memory_budget, Some(1_073_741_824));
    }

    #[test]
    fn parse_energy_budget_extension_with_joules() {
        let ext = parse_ext("ENERGY BUDGET 500 JOULES");
        assert_eq!(ext.energy_budget, Some(500));
    }

    #[test]
    fn parse_energy_budget_extension_without_joules() {
        let ext = parse_ext("ENERGY BUDGET 500");
        assert_eq!(ext.energy_budget, Some(500));
    }

    #[test]
    fn parse_all_extensions_interleaved_in_select() {
        let ext = parse_ext(
            "SELECT AVG(price) APPROXIMATE WITHIN 0.01 CONFIDENCE 0.99 \
             TIER CXL USING HYPERLOGLOG MEMORY BUDGET 1048576 ENERGY BUDGET 100 JOULES \
             FROM sales",
        );
        assert!(ext.approximate.is_some());
        assert_eq!(ext.tier.as_deref(), Some("CXL"));
        assert_eq!(ext.using.as_deref(), Some("HYPERLOGLOG"));
        assert_eq!(ext.memory_budget, Some(1_048_576));
        assert_eq!(ext.energy_budget, Some(100));
    }

    #[test]
    fn parse_empty_input_returns_default() {
        let ext = parse_ext("");
        assert!(ext.approximate.is_none());
        assert!(ext.tier.is_none());
    }

    #[test]
    fn parse_input_with_no_extensions_returns_default() {
        let ext = parse_ext("SELECT * FROM t WHERE x = 5");
        assert!(ext.approximate.is_none());
    }

    #[test]
    fn parse_approximate_with_int_args() {
        // WITHIN accepts an integer literal (1) alongside floats.
        let ext = parse_ext("APPROXIMATE WITHIN 1 CONFIDENCE 0.5");
        let (eps, delta) = ext.approximate.unwrap();
        assert!((eps - 1.0).abs() < 1e-12);
        assert!((delta - 0.5).abs() < 1e-12, "delta {delta}");
    }

    #[test]
    fn err_on_approximate_without_within() {
        let toks = tokenize("APPROXIMATE 0.01").unwrap();
        assert!(parse_extensions(toks).is_err());
    }

    #[test]
    fn err_on_tier_without_name() {
        let toks = tokenize("TIER").unwrap();
        assert!(parse_extensions(toks).is_err());
    }

    #[test]
    fn err_on_similar_to_without_hex() {
        let toks = tokenize("SIMILAR TO WITHIN HAMMING DISTANCE 5").unwrap();
        assert!(parse_extensions(toks).is_err());
    }

    #[test]
    fn err_on_similar_to_missing_distance() {
        let toks = tokenize("SIMILAR TO x'AABB' WITHIN HAMMING").unwrap();
        assert!(parse_extensions(toks).is_err());
    }

    #[test]
    fn err_on_memory_budget_negative() {
        // The lexer tokenizes `-5` as `Op("-") Int(5)`. The extension parser
        // hits `expect_int` on `Op("-")` and errors.
        let toks = tokenize("MEMORY BUDGET -5").unwrap();
        assert!(parse_extensions(toks).is_err());
    }

    #[test]
    fn err_on_energy_budget_missing_value() {
        let toks = tokenize("ENERGY BUDGET").unwrap();
        assert!(parse_extensions(toks).is_err());
    }
}
