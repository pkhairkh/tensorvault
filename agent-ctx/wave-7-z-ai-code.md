# Wave 7: SQL Parser + Query Language — Work Record

**Task ID:** wave-7
**Agent:** Z.ai Code (single-agent execution)
**Status:** ✅ Complete
**Date:** 2026-07-31

## Summary

Implemented Wave 7 of the turboGP database engine: a hand-written SQL
parser plus the seven turboGP-specific query-language extensions. The wave
introduces a new top-level module `src/sql/` with four sub-modules:

- `lexer.rs` — the tokenizer (keywords, operators, literals, identifiers).
- `parser.rs` — a recursive-descent parser with Pratt-style expression
  precedence for standard `SELECT ... FROM ... [WHERE ...] [GROUP BY ...]
  [ORDER BY ...] [LIMIT n]`.
- `extensions.rs` — a scanner that parses the seven turboGP extensions
  (`APPROXIMATE WITHIN ... CONFIDENCE ...`, `TIER ...`, `SIMILAR TO ...
  WITHIN HAMMING DISTANCE ...`, `CONSISTENCY ...`, `USING ...`,
  `MEMORY BUDGET ...`, `ENERGY BUDGET ... [JOULES]`) from anywhere in the
  token stream, and optionally strips them so the remainder can be parsed
  as standard SQL.
- `plan.rs` — the lowering pass that turns a `(SelectQuery,
  QueryExtensions)` pair into an `executor::plan::LogicalPlan`.

A convenience entry point `turbogp::sql::parse_with_extensions(sql: &str)`
ties the three together: tokenize → parse-and-strip extensions → parse
SELECT.

All DoD gates pass:

- `cargo fmt --check` — clean (only nightly-only config warnings, no diff).
- `cargo clippy --all-targets -- -D warnings` — clean.
- `cargo test` — 248 passed (173 baseline unit + 7 integration + 68 new
  SQL unit tests), debug and release modes both green.

## Files Created / Modified

| File | Change |
|------|--------|
| `src/sql/lexer.rs` | **New file (631 lines).** `pub const KEYWORDS: &[&str]` listing all 40 reserved keywords. `pub enum Token { Keyword(String), Ident(String), Int(i64), Float(f64), String(String), Hex(Vec<u8>), Op(String), LParen, RParen, Comma, Semicolon, EOF }` with `#[derive(Debug, Clone, PartialEq)]`. `pub fn tokenize(input: &str) -> Result<Vec<Token>, String>`. Handles: whitespace; punctuation `() , ;`; operators `= != < > <= >= + - * /`; single-quoted strings with `''` escape; integers; floats (with `.` and `e`/`E` exponent); hex literals `x'...'` / `X'...'` (even digits only); identifiers (alphanumeric + underscore, keywords uppercased case-insensitively, identifiers preserve original case). Floats with no integer part (`.5`) are supported; a lone `.` not followed by a digit is an error. 19 unit tests including error cases (unterminated string/hex, odd hex digits, bad hex chars, lone `!`, unexpected chars). |
| `src/sql/parser.rs` | **New file (656 lines).** `pub struct SelectQuery { select: Vec<SelectItem>, from: String, where_clause: Option<Expr>, group_by: Vec<String>, order_by: Vec<(String, bool)>, limit: Option<usize> }`. `pub enum SelectItem { Column(String), Aggregate { func: String, arg: String, alias: Option<String> }, Star }`. `pub enum Value { Int(i64), Float(f64), String(String), Hex(Vec<u8>) }`. `pub enum Expr { Column(String), Literal(Value), Binary { left: Box<Expr>, op: String, right: Box<Expr> } }`. `pub fn parse(tokens: Vec<Token>) -> Result<SelectQuery, String>`. Internal `Parser` struct with `peek`/`next`/`match_keyword`/`expect_keyword`/`match_op`/`match_ident`. Pratt-style expression precedence: OR < AND < comparison < additive < multiplicative < primary. `LIMIT`, `ASC`, `DESC` are not keywords (matched case-insensitively as idents). Implicit aliases (without `AS`) are not supported to avoid swallowing `LIMIT`/`ASC`/`DESC`. 16 unit tests covering all 9 spec test cases plus precedence, parens, string literals, trailing semicolons, and 5 invalid-SQL error cases. |
| `src/sql/extensions.rs` | **New file (439 lines).** `pub struct QueryExtensions { approximate: Option<(f64, f64)>, tier: Option<String>, similar_to: Option<(String, Vec<u8>, u32)>, consistency: Option<String>, using: Option<String>, memory_budget: Option<u64>, energy_budget: Option<u64> }` with `#[derive(Debug, Clone, Default)]`. `pub fn parse_extensions(tokens: Vec<Token>) -> Result<QueryExtensions, String>` (thin wrapper). `pub fn parse_extensions_and_strip(tokens: Vec<Token>) -> Result<(QueryExtensions, Vec<Token>), String>` — the workhorse: walks the token stream, parses each extension it finds, and collects non-extension tokens into a stripped stream suitable for `parser::parse`. Internal `Cursor<'a>` over `&'a [Token]` with `peek`/`advance`/`match_keyword`/`expect_keyword`/`expect_ident`/`expect_int`/`expect_number`/`take_hex`. `expect_number` accepts both `Int` and `Float` so `WITHIN 1` (int) works alongside `WITHIN 0.01` (float). Approximate semantics per ADR-015: `APPROXIMATE WITHIN ε CONFIDENCE δ` stores `(ε, 1 - δ)` (epsilon, failure probability). `SIMILAR TO` accepts both `SIMILAR TO <hex> ...` (no column, stored as `""`) and `SIMILAR TO <col> <hex> ...`. 20 unit tests covering all 7 extensions, interleaved extensions in a full SELECT, int args, and 6 error cases. |
| `src/sql/plan.rs` | **New file (324 lines).** `pub fn build_plan(query: &SelectQuery, ext: &QueryExtensions) -> LogicalPlan`. Builds a `PlanNode::Scan` leaf and optionally wraps it in `PlanNode::Aggregate`. Scan operator selection: (1) if `ext.similar_to` is set → `SimilarityHamming` with `target_u64` = first 8 bytes of hex (LE-packed, zero-padded) and `max_distance` = distance; (2) else if WHERE is `col = <int>` (or `<int> = col`) → `ScanEqU64` with `target_u64` = int; (3) else → `ScanEqU64` with default params (unfiltered). Aggregate operator selection: `COUNT_DISTINCT` → `AggregateCountDistinct`; all other aggregates (`COUNT`, `SUM`, `AVG`, `MIN`, `MAX`) → `AggregateSumF64` (the engine lacks a dedicated `AggregateCount`). Region ID is a stable hash of the table name (placeholder until a schema catalog arrives). Helpers: `hex_to_target_u64`, `i64_to_u64_target` (clamps negatives to 0 defensively), `table_region_id`, `extract_eq_target`, `pick_aggregate_operator`. 13 unit tests including the spec's `build_plan: simple scan produces a PlanNode::Scan` case, plus equality WHERE, COUNT(*), SUM, AVG+GROUP BY, COUNT_DISTINCT, SIMILAR TO, table-name hashing, non-equality fallback, literal-on-left, and unit tests for the helpers. |
| `src/sql/mod.rs` | **New file (72 lines).** Module doc-comment explaining the four-stage pipeline (tokenize → parse SELECT + parse extensions → build_plan → LogicalPlan) and why no external parser crate is used. Re-exports `tokenize`, `Token`, `KEYWORDS`, `parse`, `Expr`, `SelectItem`, `SelectQuery`, `Value`, `parse_extensions`, `parse_extensions_and_strip`, `QueryExtensions`, `build_plan`. Adds `pub fn parse_with_extensions(sql: &str) -> Result<(SelectQuery, QueryExtensions), String>` — the convenience entry point that ties everything together. |
| `src/lib.rs` | Added `pub mod sql;` to the module list (alphabetically between `schema` and `storage`). Updated the module-list doc-comment to describe `sql` and updated the `schema` description (it no longer owns the SQL parser). |

## Design Decisions

### Task 7-1: Lexer

**Keywords uppercased, identifiers preserve case.** SQL is
case-insensitive for keywords but case-sensitive for identifiers (in most
dialects, modulo unquoted identifier folding). The lexer matches a word
against `KEYWORDS` (case-insensitively) and emits `Token::Keyword(UPPER)`
on match, else `Token::Ident(original_case)`. This lets the parser do
exact `Keyword == "SELECT"` comparisons without re-folding, and lets
column names round-trip with their original case.

**`x'...'` lookahead via `chars.clone()`.** The literal `x` could be the
start of a hex literal (`x'...'`) or an identifier (`xray`, `X1`). The
lexer clones the `Peekable<Chars>` iterator, advances the clone past the
`x`, and checks if the next char is `'`. If so, it's a hex literal; else
it's an identifier. Cloning a `Peekable<Chars>` is cheap (just the
underlying iterator state).

**`.5` (float with no integer part) supported; lone `.` is an error.**
The `.` character dispatches to `read_number` only if the next char is a
digit; otherwise it errors. Qualified names like `table.col` are not yet
supported (the spec defers them).

**`-5` tokenizes as `Op("-") Int(5)`, not `Int(-5)`.** This matches
standard SQL tokenizer behavior (the `-` is an operator) and lets the
parser compose it into a unary or binary expression. The current parser
does not synthesize negative literals from `Op("-") Int(n)`, so `WHERE x
= -5` would fail to parse — documented as a limitation.

### Task 7-2: Parser

**Pratt-style precedence climbing, not a Pratt table.** The parser uses
five nested methods (`parse_or_expr` → `parse_and_expr` →
`parse_comparison_expr` → `parse_additive_expr` →
`parse_multiplicative_expr` → `parse_primary`) instead of a
precedence/-binding-power table. For five precedence levels this is
clearer than a table and equally fast.

**`LIMIT`, `ASC`, `DESC` are identifiers, not keywords.** The spec's
keyword list does not include them. The parser matches them
case-insensitively via `match_ident("LIMIT")` etc. This means a column
named `limit` would be ambiguous — but the parser's select-item parsing
checks for `LParen` after an identifier (to detect aggregates) before
treating it as a column, and the optional-alias parsing only kicks in
after `AS`, so `SELECT limit FROM t` parses as `Column("limit")`
correctly.

**No implicit aliases.** `SELECT col1 col2` (without `AS`) is not parsed
as `col1 AS col2`. This avoids the ambiguity where `SELECT col LIMIT 10`
would swallow `LIMIT` as an alias. The `AS` keyword is required for
aliases. (The spec doesn't require implicit aliases.)

**`parse()` is strict; `parse_with_extensions()` is lenient.** The
public `parse(tokens)` function parses only standard `SELECT` and errors
on any trailing token that isn't `;` or EOF. This is the most predictable
behavior. For SQL with extensions interleaved (e.g. `SELECT AVG(price)
APPROXIMATE WITHIN 0.01 CONFIDENCE 0.99 FROM sales`), users call
`parse_with_extensions(sql)`, which strips extension tokens first and
then calls `parse()` on the stripped stream.

### Task 7-3: Extensions

**`parse_extensions_and_strip` is the workhorse; `parse_extensions` is a
thin wrapper.** The strip variant returns both the parsed
`QueryExtensions` and a `Vec<Token>` with extension tokens removed. This
lets `parse_with_extensions` feed the stripped stream to `parser::parse`
without re-scanning. The non-strip variant discards the stripped stream
for callers who only want the extensions.

**`APPROXIMATE WITHIN ε CONFIDENCE δ` stores `(ε, 1 - δ)`.** The struct
comment says `(epsilon, delta)`. In the empirical-Bernstein literature
(ADR-015), `delta` is the *failure probability* = `1 - confidence`. So
`WITHIN 0.01 CONFIDENCE 0.99` → `(0.01, 0.01)`. This matches the spec's
test #5 expectation exactly.

**`SIMILAR TO` accepts an optional column.** The struct field is
`(String, Vec<u8>, u32)` = `(column, target, max_distance)`. The spec's
test #7 input `SIMILAR TO x'AABB' WITHIN HAMMING DISTANCE 5` has no
column. The parser handles both `SIMILAR TO <hex> ...` (column = `""`)
and `SIMILAR TO <col> <hex> ...` by peeking at the token after `TO`: if
it's `Hex`, no column; if it's `Ident`, it's a column followed by the
hex.

**`expect_number` accepts both `Int` and `Float`.** This lets `WITHIN 1`
(integer) work alongside `WITHIN 0.01` (float). The value is upcast to
`f64` either way. `MEMORY BUDGET` and `ENERGY BUDGET` use `expect_int`
strictly (budgets are byte/joule counts, not floats).

**Named extensions (`tier`, `consistency`, `using`) are uppercased.**
This matches the convention in the struct comments (`"L3"`, `"CXL"`,
`"STRONG"`, `"HYPERLOGLOG"`). The original case is lost — if a user
writes `tier cxl`, the stored value is `"CXL"`.

### Task 7-4: Plan

**`COUNT(*)` lowered to `AggregateSumF64`.** The engine lacks a dedicated
`AggregateCount` operator. The spec's note "(no, just count)" means
"don't use `AggregateCountDistinct` for `COUNT(*)`". The closest existing
operator is `AggregateSumF64` (semantically: sum a column of 1.0s). A
future wave would add `AggregateCount`, or the executor would synthesize
a 1.0s column.

**`COUNT_DISTINCT(x)` lowered to `AggregateCountDistinct`.** The parser
parses `COUNT_DISTINCT(col)` (underscore form) as an aggregate with `func
= "COUNT_DISTINCT"`, which `pick_aggregate_operator` maps to
`Operator::AggregateCountDistinct`. Standard SQL's `COUNT(DISTINCT col)`
syntax is not yet supported (the parser would need to handle the
`DISTINCT` keyword inside the aggregate's argument list).

**Region ID = stable hash of table name.** The parser has no schema
catalog, so it can't resolve `FROM users` to a specific `RegionId`. The
hash (`DefaultHasher`) is deterministic within a process, so the same
table name always maps to the same ID. A future wave will replace this
with a real catalog lookup. The test
`build_plan_table_name_maps_to_stable_region_id` verifies the determinism
and distinctness.

**Only `similar_to` changes the plan structure.** The other six
extensions (`approximate`, `tier`, `consistency`, `using`,
`memory_budget`, `energy_budget`) affect kernel selection, admission
control, and execution semantics, not the plan DAG. They are accepted by
the extension parser and stored in `QueryExtensions`, but `build_plan`
ignores them when constructing the `LogicalPlan`. A future wave could
e.g. use `using` to swap `AggregateCountDistinct` for a HyperLogLog-based
operator.

**Non-equality WHERE falls back to unfiltered scan.** The spec only
requires `col = literal` → `ScanEqU64`. A WHERE clause like `x > 5` or
`x = 5 AND y = 10` is not lowered to a `ScanRangeU64` or
`ScanMultiPredicate`; instead, the scan is unfiltered. This is a
documented simplification — a future wave would add range and
multi-predicate scan lowering.

### Task 7-5: Tests

**All 9 spec test cases are covered**, mapped to specific unit tests:

| Spec test | Unit test |
|-----------|-----------|
| 1. Tokenize `SELECT * FROM users WHERE id = 42` | `sql::lexer::tests::tokenize_simple_select` |
| 2. Tokenize `SELECT AVG(price) APPROXIMATE WITHIN 0.01 CONFIDENCE 0.99 FROM sales` | `sql::lexer::tests::tokenize_approximate_query` |
| 3. Parse `SELECT * FROM t WHERE x = 5` | `sql::parser::tests::parse_select_star_with_where` |
| 4. Parse `SELECT COUNT(*) FROM t` | `sql::parser::tests::parse_count_star` |
| 5. `APPROXIMATE WITHIN 0.01 CONFIDENCE 0.99` → `Some((0.01, 0.01))` | `sql::extensions::tests::parse_approximate_extension` |
| 6. `TIER CXL` → `Some("CXL")` | `sql::extensions::tests::parse_tier_extension` |
| 7. `SIMILAR TO x'AABB' WITHIN HAMMING DISTANCE 5` | `sql::extensions::tests::parse_similar_to_without_column` |
| 8. `SELECT FROM WHERE` → `Err` | `sql::parser::tests::parse_invalid_missing_select_list` |
| 9. `build_plan` simple scan → `PlanNode::Scan` | `sql::plan::tests::build_plan_simple_scan` |

68 new SQL unit tests in total: 19 lexer, 16 parser, 20 extensions, 13
plan.

## Constraints Check

- ✅ `pub mod sql;` registered in `src/lib.rs` (alphabetically between
  `schema` and `storage`).
- ✅ `src/sql/mod.rs` re-exports lexer, parser, extensions, plan.
- ✅ No external parser dependencies (no `sqlparser-rs`). All parsing is
  hand-written.
- ✅ `cargo fmt` clean (only nightly-only config warnings, no diff).
- ✅ `cargo clippy --all-targets -- -D warnings` clean.
- ✅ `cargo test` passes: 248 tests (180 baseline + 68 new), debug and
  release.

## DoD Check

- ✅ `cargo test` passes (180 existing + 68 new = 248 total).
- ✅ `cargo clippy -- -D warnings` passes.
- ✅ Parser handles standard SELECT (SELECT list with `*`/columns/
  aggregates with optional `AS alias`, FROM, WHERE with full expression
  precedence, GROUP BY, ORDER BY with ASC/DESC, LIMIT).
- ✅ Parser handles all 7 extensions (APPROXIMATE, TIER, SIMILAR TO,
  CONSISTENCY, USING, MEMORY BUDGET, ENERGY BUDGET).
- ✅ Invalid SQL returns typed errors (`Err(String)` with human-readable
  messages).

## Future Work (Out of Scope for Wave 7)

- `JOIN` parsing (the spec defers joins).
- `INSERT` / `UPDATE` / `DELETE` parsing.
- `NOT` (the `Expr` enum has no `Unary` variant).
- Subqueries.
- Qualified names (`table.column`).
- `--` line comments and `/* */` block comments.
- Schema catalog to resolve table names to `RegionId`s (replacing the
  hash placeholder in `plan::table_region_id`).
- Range and multi-predicate scan lowering (currently only `col = literal`
  is lowered; other WHERE forms fall back to unfiltered scan).
- Dedicated `AggregateCount` operator for `COUNT(*)`.
- `COUNT(DISTINCT col)` syntax (currently only `COUNT_DISTINCT(col)` is
  recognized).
- Using `ext.using` to select sketch-based aggregate operators.
- Wiring `ext.tier`, `ext.memory_budget`, `ext.energy_budget` into
  admission control / kernel selection.
