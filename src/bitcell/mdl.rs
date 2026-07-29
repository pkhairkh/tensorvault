//! MDL-driven schema selection (skeleton).
//!
//! This is the formal contribution described in `mdl_sketch.tex`. Given a
//! column of raw 64-bit cells, choose the type interpretation that minimizes
//! description length.
//!
//! The full algorithm is in the LaTeX doc; here we implement a runnable
//! skeleton that demonstrates the idea.

use crate::bitcell::cell::Cell;
use crate::bitcell::column::CellColumn;

/// Candidate type interpretations for a column.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TypeInterpretation {
    /// All values are real f64.
    F64,
    /// All values are tagged i32.
    I32,
    /// All values are tagged bool.
    Bool,
    /// All values are short strings.
    ShortStr,
    /// All values are NULL.
    Null,
    /// Mixed — pay per-value tag cost.
    Variant,
}

impl TypeInterpretation {
    /// Human-readable name.
    pub fn name(&self) -> &'static str {
        match self {
            Self::F64 => "f64",
            Self::I32 => "i32",
            Self::Bool => "bool",
            Self::ShortStr => "short_str",
            Self::Null => "null",
            Self::Variant => "variant",
        }
    }
}

/// Description length of a column under an interpretation, in bits.
#[derive(Debug, Clone, Copy)]
pub struct DescriptionLength {
    /// Cost of the model (type tag + metadata).
    pub model_bits: f64,
    /// Cost of the data given the model.
    pub data_bits: f64,
}

impl DescriptionLength {
    /// Total description length.
    pub fn total(&self) -> f64 {
        self.model_bits + self.data_bits
    }
}

/// MDL constants (in bits).
const MODEL_TAG_COST: f64 = 16.0; // 16 bits to identify the type
const PER_VALUE_TAG_COST: f64 = 16.0; // 16 bits per value for variant tag
const F64_VALUE_BITS: f64 = 64.0;
const I32_VALUE_BITS: f64 = 32.0;
const BOOL_VALUE_BITS: f64 = 8.0;
const SHORT_STR_VALUE_BITS: f64 = 48.0; // up to 6 bytes

/// Compute the description length of `column` under interpretation `tau`.
pub fn description_length(column: &CellColumn, tau: TypeInterpretation) -> DescriptionLength {
    let n = column.len() as f64;
    let model_bits = MODEL_TAG_COST;

    let data_bits = match tau {
        TypeInterpretation::F64 => n * F64_VALUE_BITS,
        TypeInterpretation::I32 => n * I32_VALUE_BITS,
        TypeInterpretation::Bool => n * BOOL_VALUE_BITS,
        TypeInterpretation::ShortStr => n * SHORT_STR_VALUE_BITS,
        TypeInterpretation::Null => 0.0, // no data cost, all values are NULL
        TypeInterpretation::Variant => {
            // Per-value: 16-bit tag + max payload (64 bits) = 80 bits/value.
            n * (PER_VALUE_TAG_COST + F64_VALUE_BITS)
        }
    };

    // Penalty for cells that don't match the interpretation.
    // If we interpret the column as F64 but a cell is an i32, we have to
    // re-encode it (pay a conversion cost) or fall back to Variant.
    let mismatch_penalty = mismatch_penalty(column, tau);

    DescriptionLength {
        model_bits,
        data_bits: data_bits + mismatch_penalty,
    }
}

/// Compute the penalty (in bits) for cells that don't fit the interpretation.
/// If the column has any mismatch, the penalty is infinite (forcing fallback
/// to Variant), unless the interpretation is Variant itself.
fn mismatch_penalty(column: &CellColumn, tau: TypeInterpretation) -> f64 {
    if tau == TypeInterpretation::Variant {
        return 0.0;
    }

    let mut mismatches = 0usize;
    for c in &column.cells {
        let matches = match tau {
            TypeInterpretation::F64 => c.is_f64(),
            TypeInterpretation::I32 => c.is_i32(),
            TypeInterpretation::Bool => (c.to_bits() >> 48) == 0xFFF1,
            TypeInterpretation::ShortStr => c.is_short_str(),
            TypeInterpretation::Null => c.is_null(),
            TypeInterpretation::Variant => true,
        };
        if !matches {
            mismatches += 1;
        }
    }

    if mismatches == 0 {
        0.0
    } else {
        // Infinite penalty — interpretation is invalid.
        f64::INFINITY
    }
}

/// Detect the dominant interpretation of a column by scanning its cells.
pub fn detect_dominant(column: &CellColumn) -> TypeInterpretation {
    if column.is_empty() {
        return TypeInterpretation::Null;
    }

    let mut counts = std::collections::HashMap::new();
    for c in &column.cells {
        let tau = if c.is_null() {
            TypeInterpretation::Null
        } else if c.is_f64() {
            TypeInterpretation::F64
        } else if c.is_i32() {
            TypeInterpretation::I32
        } else if (c.to_bits() >> 48) == 0xFFF1 {
            TypeInterpretation::Bool
        } else if c.is_short_str() {
            TypeInterpretation::ShortStr
        } else {
            TypeInterpretation::Variant
        };
        *counts.entry(tau).or_insert(0u32) += 1;
    }

    counts
        .into_iter()
        .max_by_key(|(_, c)| *c)
        .map(|(t, _)| t)
        .unwrap_or(TypeInterpretation::Variant)
}

/// MDL-optimal schema selection.
///
/// Computes the description length under each candidate interpretation and
/// returns the one with minimum total cost. If no single-type interpretation
/// fits (all have infinite penalty), returns Variant.
pub fn schema_select(column: &CellColumn) -> TypeInterpretation {
    let candidates = [
        TypeInterpretation::Null,
        TypeInterpretation::F64,
        TypeInterpretation::I32,
        TypeInterpretation::Bool,
        TypeInterpretation::ShortStr,
        TypeInterpretation::Variant,
    ];

    candidates
        .iter()
        .map(|&tau| {
            let dl = description_length(column, tau);
            (tau, dl.total())
        })
        .min_by(|(_, a), (_, b)| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal))
        .map(|(t, _)| t)
        .unwrap_or(TypeInterpretation::Variant)
}

/// Schema selection result with diagnostic info.
#[derive(Debug, Clone)]
pub struct SchemaSelectionResult {
    pub chosen: TypeInterpretation,
    pub all_costs: Vec<(TypeInterpretation, DescriptionLength)>,
}

/// Schema selection with diagnostic info — returns costs for all candidates.
pub fn schema_select_with_diagnostics(column: &CellColumn) -> SchemaSelectionResult {
    let candidates = [
        TypeInterpretation::Null,
        TypeInterpretation::F64,
        TypeInterpretation::I32,
        TypeInterpretation::Bool,
        TypeInterpretation::ShortStr,
        TypeInterpretation::Variant,
    ];

    let mut all_costs: Vec<(TypeInterpretation, DescriptionLength)> = candidates
        .iter()
        .map(|&tau| (tau, description_length(column, tau)))
        .collect();

    all_costs.sort_by(|(_, a), (_, b)| {
        a.total().partial_cmp(&b.total()).unwrap_or(std::cmp::Ordering::Equal)
    });

    let chosen = all_costs
        .first()
        .map(|(t, _)| *t)
        .unwrap_or(TypeInterpretation::Variant);

    SchemaSelectionResult { chosen, all_costs }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mdl_picks_f64_for_pure_f64_column() {
        // Use non-zero f64 values — 0.0's bit pattern is all-zeros which is our NULL tag.
        let col: CellColumn = (0..1000).map(|i| Cell::from_f64((i as f64) + 1.0)).collect();
        let result = schema_select(&col);
        assert_eq!(result, TypeInterpretation::F64);
    }

    #[test]
    fn mdl_picks_i32_for_pure_i32_column() {
        let col: CellColumn = (0..1000).map(|i| Cell::from_i32(i)).collect();
        let result = schema_select(&col);
        assert_eq!(result, TypeInterpretation::I32);
    }

    #[test]
    fn mdl_picks_null_for_all_null_column() {
        let col: CellColumn = (0..1000).map(|_| Cell::null()).collect();
        let result = schema_select(&col);
        assert_eq!(result, TypeInterpretation::Null);
    }

    #[test]
    fn mdl_picks_variant_for_mixed_column() {
        let mut col = CellColumn::new();
        for i in 0..500 {
            col.push_i32(i);
        }
        for i in 0..500 {
            col.push_f64(i as f64);
        }
        let result = schema_select(&col);
        assert_eq!(result, TypeInterpretation::Variant);
    }

    #[test]
    fn mdl_diagnostics_show_all_costs() {
        let col: CellColumn = (0..100).map(|i| Cell::from_f64((i as f64) + 1.0)).collect();
        let result = schema_select_with_diagnostics(&col);
        assert_eq!(result.chosen, TypeInterpretation::F64);
        assert!(result.all_costs.len() >= 5);
        // The chosen one should have the lowest cost.
        let chosen_cost = result
            .all_costs
            .iter()
            .find(|(t, _)| *t == result.chosen)
            .map(|(_, dl)| dl.total())
            .unwrap();
        for (_, dl) in &result.all_costs {
            assert!(chosen_cost <= dl.total());
        }
    }

    #[test]
    fn detect_dominant_works() {
        let col: CellColumn = (0..100).map(|i| Cell::from_i32(i)).collect();
        assert_eq!(detect_dominant(&col), TypeInterpretation::I32);
    }

    #[test]
    fn i32_is_cheaper_than_f64_for_integers() {
        let col: CellColumn = (0..1000).map(|i| Cell::from_i32(i)).collect();
        let i32_cost = description_length(&col, TypeInterpretation::I32).total();
        let f64_cost = description_length(&col, TypeInterpretation::F64).total();
        // f64 interpretation should be infinite (i32 cells aren't f64).
        assert!(f64_cost.is_infinite());
        assert!(i32_cost < f64_cost);
    }
}
