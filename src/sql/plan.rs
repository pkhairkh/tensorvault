//! Logical-plan construction from a parsed [`SelectQuery`].
//!
//! This module bridges the SQL surface ([`crate::sql::parser`]) and the
//! executor's plan IR ([`crate::executor::plan`]). It takes a parsed
//! `SELECT` statement plus its turboGP extensions and produces a
//! [`LogicalPlan`] rooted at a [`PlanNode`].
//!
//! ## Supported mappings
//!
//! | SQL form | Plan |
//! |----------|------|
//! | `SELECT * FROM t` | `Scan(ScanEqU64, default params)` |
//! | `SELECT * FROM t WHERE col = N` | `Scan(ScanEqU64, target_u64=N)` |
//! | `SELECT COUNT(*) FROM t` | `Aggregate(child=Scan, AggregateSumF64)` |
//! | `SELECT SUM(x) FROM t` | `Aggregate(child=Scan, AggregateSumF64)` |
//! | `SELECT AVG(x) FROM t GROUP BY y` | `Aggregate(child=Scan, AggregateSumF64)` |
//! | `SELECT * FROM t SIMILAR TO x'..' WITHIN HAMMING DISTANCE N` | `Scan(SimilarityHamming, target=hex, max_distance=N)` |
//!
//! ## Simplifications
//!
//! - **No table catalog**: the parser does not yet know how table names map
//!   to region IDs, so [`build_plan`] hashes the table name to a stable
//!   `RegionId`. A future wave will plug in a schema catalog.
//! - **No range / multi-predicate scans**: a WHERE clause that isn't a
//!   simple `col = literal` falls back to an unfiltered scan.
//! - **No joins**: only single-table queries are lowered.
//! - **COUNT(\*) uses `AggregateSumF64`**: the engine lacks a dedicated
//!   `AggregateCount` operator, so `COUNT(*)` is lowered to `AggregateSumF64`
//!   (a future wave would add `AggregateCount`, or the executor would
//!   synthesize a column of 1.0s).
//! - **Extensions other than `similar_to`** do not change the plan
//!   structure: `tier`, `consistency`, `memory_budget`, `energy_budget`,
//!   `approximate`, and `using` affect kernel selection, admission control,
//!   and execution semantics, not the plan DAG.

use crate::executor::plan::{LogicalPlan, PlanNode};
use crate::kernel::{KernelParams, Operator};
use crate::memory::region::RegionId;
use crate::sql::extensions::QueryExtensions;
use crate::sql::parser::{Expr, SelectItem, SelectQuery, Value};
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};

/// Build a [`LogicalPlan`] from a parsed [`SelectQuery`] and its turboGP
/// extensions.
pub fn build_plan(query: &SelectQuery, ext: &QueryExtensions) -> LogicalPlan {
    let region_id = table_region_id(&query.from);
    let scan = build_scan(query, ext, region_id);

    // If the select list contains an aggregate function, wrap the scan in an
    // Aggregate node. GROUP BY also implies aggregation, but for the
    // prototype we only emit an Aggregate node when an aggregate function is
    // explicitly present in the select list.
    let has_aggregate = query.select.iter().any(|i| matches!(i, SelectItem::Aggregate { .. }));
    if has_aggregate {
        let agg_op = pick_aggregate_operator(query);
        LogicalPlan::new(PlanNode::Aggregate { child: Box::new(scan), operator: agg_op })
    } else {
        LogicalPlan::new(scan)
    }
}

/// Build the scan node (the leaf of the plan).
///
/// Operator selection:
/// 1. If `ext.similar_to` is set → `SimilarityHamming`.
/// 2. Else if WHERE is `col = <int>` → `ScanEqU64` with `target_u64 = int`.
/// 3. Else → `ScanEqU64` with default params (no filter).
fn build_scan(query: &SelectQuery, ext: &QueryExtensions, region_id: RegionId) -> PlanNode {
    if let Some((_col, hex, dist)) = &ext.similar_to {
        let target_u64 = hex_to_target_u64(hex);
        return PlanNode::Scan {
            region_id,
            operator: Operator::SimilarityHamming,
            params: KernelParams { target_u64, max_distance: *dist, ..Default::default() },
        };
    }

    if let Some(expr) = &query.where_clause {
        if let Some(target) = extract_eq_target(expr) {
            return PlanNode::Scan {
                region_id,
                operator: Operator::ScanEqU64,
                params: KernelParams { target_u64: target, ..Default::default() },
            };
        }
    }

    // Default: unfiltered scan (no equality predicate).
    PlanNode::Scan { region_id, operator: Operator::ScanEqU64, params: KernelParams::default() }
}

/// Pick the aggregate operator for the query's first aggregate function.
///
/// `COUNT(DISTINCT x)` (parsed as `Aggregate { func: "COUNT_DISTINCT" }`)
/// maps to [`Operator::AggregateCountDistinct`]. All other aggregates
/// (`COUNT`, `SUM`, `AVG`, `MIN`, `MAX`) currently map to
/// [`Operator::AggregateSumF64`].
fn pick_aggregate_operator(query: &SelectQuery) -> Operator {
    for item in &query.select {
        if let SelectItem::Aggregate { func, .. } = item {
            return match func.as_str() {
                "COUNT_DISTINCT" => Operator::AggregateCountDistinct,
                _ => Operator::AggregateSumF64,
            };
        }
    }
    Operator::AggregateSumF64
}

/// Extract the `u64` target from a simple `col = <int>` or `<int> = col`
/// predicate. Returns `None` for any other shape (range, AND, OR, etc.).
fn extract_eq_target(expr: &Expr) -> Option<u64> {
    match expr {
        Expr::Binary { left, op, right } if op == "=" => {
            if let Expr::Literal(Value::Int(n)) = &**right {
                return Some(i64_to_u64_target(*n));
            }
            if let Expr::Literal(Value::Int(n)) = &**left {
                return Some(i64_to_u64_target(*n));
            }
            None
        }
        _ => None,
    }
}

/// Convert an `i64` literal to the `u64` target field of `KernelParams`.
///
/// Negative literals cannot appear directly (the lexer tokenizes `-5` as
/// `Op("-") Int(5)`, and the parser does not synthesize a negative literal),
/// so the `< 0` branch is defensive: it clamps to 0 rather than wrapping.
fn i64_to_u64_target(n: i64) -> u64 {
    if n < 0 {
        0
    } else {
        n as u64
    }
}

/// Pack the first 8 bytes of a hex literal into the `target_u64` field
/// (little-endian). Shorter hex values are zero-padded on the right.
fn hex_to_target_u64(hex: &[u8]) -> u64 {
    let mut padded = [0u8; 8];
    let n = hex.len().min(8);
    padded[..n].copy_from_slice(&hex[..n]);
    u64::from_le_bytes(padded)
}

/// Hash a table name to a stable [`RegionId`].
///
/// The engine has no schema catalog yet, so we hash the table name to derive
/// a deterministic region ID. A future wave will replace this with a real
/// catalog lookup (`table_name → Vec<RegionId>`).
fn table_region_id(table: &str) -> RegionId {
    let mut h = DefaultHasher::new();
    table.hash(&mut h);
    h.finish()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::executor::plan::PlanNode;
    use crate::kernel::Operator;

    /// Helper: parse SQL into (SelectQuery, QueryExtensions), then build the plan.
    fn plan_from_sql(sql: &str) -> LogicalPlan {
        let (q, ext) = crate::sql::parse_with_extensions(sql).expect("parse_with_extensions");
        build_plan(&q, &ext)
    }

    #[test]
    fn build_plan_simple_scan() {
        let plan = plan_from_sql("SELECT * FROM t");
        match &plan.root {
            PlanNode::Scan { operator, .. } => assert_eq!(*operator, Operator::ScanEqU64),
            other => panic!("expected Scan, got {other:?}"),
        }
    }

    #[test]
    fn build_plan_scan_with_equality_where() {
        let plan = plan_from_sql("SELECT * FROM t WHERE x = 5");
        match &plan.root {
            PlanNode::Scan { operator, params, .. } => {
                assert_eq!(*operator, Operator::ScanEqU64);
                assert_eq!(params.target_u64, 5);
            }
            other => panic!("expected Scan, got {other:?}"),
        }
    }

    #[test]
    fn build_plan_count_star_produces_aggregate() {
        let plan = plan_from_sql("SELECT COUNT(*) FROM t");
        match &plan.root {
            PlanNode::Aggregate { child, operator } => {
                assert_eq!(*operator, Operator::AggregateSumF64);
                assert!(matches!(&**child, PlanNode::Scan { .. }), "child should be Scan");
            }
            other => panic!("expected Aggregate, got {other:?}"),
        }
    }

    #[test]
    fn build_plan_sum_produces_aggregate() {
        let plan = plan_from_sql("SELECT SUM(price) FROM sales");
        match &plan.root {
            PlanNode::Aggregate { operator, .. } => {
                assert_eq!(*operator, Operator::AggregateSumF64);
            }
            other => panic!("expected Aggregate, got {other:?}"),
        }
    }

    #[test]
    fn build_plan_avg_with_group_by_produces_aggregate() {
        let plan = plan_from_sql("SELECT AVG(price) FROM sales GROUP BY area");
        match &plan.root {
            PlanNode::Aggregate { operator, child, .. } => {
                assert_eq!(*operator, Operator::AggregateSumF64);
                assert!(matches!(&**child, PlanNode::Scan { .. }));
            }
            other => panic!("expected Aggregate, got {other:?}"),
        }
    }

    #[test]
    fn build_plan_count_distinct_uses_count_distinct_operator() {
        // The parser parses COUNT_DISTINCT(col) as an Aggregate with
        // func = "COUNT_DISTINCT".
        let plan = plan_from_sql("SELECT COUNT_DISTINCT(user_id) FROM events");
        match &plan.root {
            PlanNode::Aggregate { operator, .. } => {
                assert_eq!(*operator, Operator::AggregateCountDistinct);
            }
            other => panic!("expected Aggregate, got {other:?}"),
        }
    }

    #[test]
    fn build_plan_similar_to_uses_hamming_operator() {
        let plan =
            plan_from_sql("SELECT * FROM vectors SIMILAR TO x'AABBCCDD' WITHIN HAMMING DISTANCE 7");
        match &plan.root {
            PlanNode::Scan { operator, params, .. } => {
                assert_eq!(*operator, Operator::SimilarityHamming);
                assert_eq!(params.max_distance, 7);
                // First 8 bytes of hex [0xAA, 0xBB, 0xCC, 0xDD] packed LE.
                let expected = u64::from_le_bytes([0xAA, 0xBB, 0xCC, 0xDD, 0, 0, 0, 0]);
                assert_eq!(params.target_u64, expected);
            }
            other => panic!("expected Scan, got {other:?}"),
        }
    }

    #[test]
    fn build_plan_table_name_maps_to_stable_region_id() {
        let p1 = plan_from_sql("SELECT * FROM users");
        let p2 = plan_from_sql("SELECT * FROM users");
        let p3 = plan_from_sql("SELECT * FROM orders");
        let r1 = match &p1.root {
            PlanNode::Scan { region_id, .. } => *region_id,
            _ => 0,
        };
        let r2 = match &p2.root {
            PlanNode::Scan { region_id, .. } => *region_id,
            _ => 0,
        };
        let r3 = match &p3.root {
            PlanNode::Scan { region_id, .. } => *region_id,
            _ => 0,
        };
        assert_eq!(r1, r2, "same table name → same region id");
        assert_ne!(r1, r3, "different table names → different region ids");
    }

    #[test]
    fn build_plan_non_equality_where_falls_back_to_unfiltered_scan() {
        // `WHERE x > 5` is not a simple equality, so the scan is unfiltered.
        let plan = plan_from_sql("SELECT * FROM t WHERE x > 5");
        match &plan.root {
            PlanNode::Scan { params, .. } => {
                assert_eq!(params.target_u64, 0, "no filter → default target");
            }
            other => panic!("expected Scan, got {other:?}"),
        }
    }

    #[test]
    fn build_plan_literal_on_left_of_equality() {
        // `WHERE 5 = x` — literal on the left, column on the right.
        let plan = plan_from_sql("SELECT * FROM t WHERE 5 = x");
        match &plan.root {
            PlanNode::Scan { params, .. } => assert_eq!(params.target_u64, 5),
            other => panic!("expected Scan, got {other:?}"),
        }
    }

    #[test]
    fn hex_to_target_u64_pads_short_hex() {
        assert_eq!(hex_to_target_u64(&[]), 0);
        assert_eq!(hex_to_target_u64(&[0x42]), 0x42);
        assert_eq!(hex_to_target_u64(&[0x01, 0x02]), 0x0201);
        // 8 bytes — no padding.
        assert_eq!(
            hex_to_target_u64(&[0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08]),
            0x0807060504030201
        );
        // >8 bytes — only first 8 used.
        assert_eq!(
            hex_to_target_u64(&[0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0xFF, 0xFF]),
            0x0807060504030201
        );
    }

    #[test]
    fn i64_to_u64_target_handles_negatives_defensively() {
        assert_eq!(i64_to_u64_target(0), 0);
        assert_eq!(i64_to_u64_target(42), 42);
        assert_eq!(i64_to_u64_target(-1), 0);
    }
}
