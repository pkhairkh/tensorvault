//! MDL-driven schema selection.
//!
//! Given a column of raw 64-bit cells, choose the type interpretation
//! (f64 vs i32 vs &str vs nullable vs variant) that minimizes description
//! length. This is the principled version of "schema-on-read".

/// Candidate type interpretations for a column.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TypeInterpretation {
    /// All values are real f64.
    F64,
    /// All values are tagged i32.
    I32,
    /// All values are tagged bool.
    Bool,
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
const MODEL_TAG_COST: f64 = 16.0;
const F64_VALUE_BITS: f64 = 64.0;
const I32_VALUE_BITS: f64 = 32.0;
const BOOL_VALUE_BITS: f64 = 8.0;
const PER_VALUE_TAG_COST: f64 = 16.0; // for Variant

/// Compute the description length of a column under interpretation `tau`.
///
/// `f64_count` = number of cells that are valid f64.
/// `i32_count` = number of cells that are valid i32.
/// `bool_count` = number of cells that are valid bool.
/// `null_count` = number of NULL cells.
/// `total` = total cell count.
pub fn description_length(
    total: usize,
    f64_count: usize,
    i32_count: usize,
    bool_count: usize,
    null_count: usize,
    tau: TypeInterpretation,
) -> DescriptionLength {
    let n = total as f64;
    let model_bits = MODEL_TAG_COST;

    let data_bits = match tau {
        TypeInterpretation::F64 => {
            if f64_count + null_count == total {
                n * F64_VALUE_BITS
            } else {
                f64::INFINITY
            }
        }
        TypeInterpretation::I32 => {
            if i32_count + null_count == total {
                n * I32_VALUE_BITS
            } else {
                f64::INFINITY
            }
        }
        TypeInterpretation::Bool => {
            if bool_count + null_count == total {
                n * BOOL_VALUE_BITS
            } else {
                f64::INFINITY
            }
        }
        TypeInterpretation::Null => {
            if null_count == total {
                0.0
            } else {
                f64::INFINITY
            }
        }
        TypeInterpretation::Variant => {
            // Per-value: 16-bit tag + max payload (64 bits) = 80 bits/value.
            n * (PER_VALUE_TAG_COST + F64_VALUE_BITS)
        }
    };

    DescriptionLength {
        model_bits,
        data_bits,
    }
}

/// MDL-optimal schema selection.
pub fn schema_select(
    total: usize,
    f64_count: usize,
    i32_count: usize,
    bool_count: usize,
    null_count: usize,
) -> TypeInterpretation {
    let candidates = [
        TypeInterpretation::Null,
        TypeInterpretation::F64,
        TypeInterpretation::I32,
        TypeInterpretation::Bool,
        TypeInterpretation::Variant,
    ];

    candidates
        .iter()
        .map(|&tau| {
            let dl = description_length(total, f64_count, i32_count, bool_count, null_count, tau);
            (tau, dl.total())
        })
        .min_by(|(_, a), (_, b)| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal))
        .map(|(t, _)| t)
        .unwrap_or(TypeInterpretation::Variant)
}

/// Schema selection with diagnostics.
pub fn schema_select_with_diagnostics(
    total: usize,
    f64_count: usize,
    i32_count: usize,
    bool_count: usize,
    null_count: usize,
) -> (TypeInterpretation, Vec<(TypeInterpretation, DescriptionLength)>) {
    let candidates = [
        TypeInterpretation::Null,
        TypeInterpretation::F64,
        TypeInterpretation::I32,
        TypeInterpretation::Bool,
        TypeInterpretation::Variant,
    ];

    let mut all_costs: Vec<(TypeInterpretation, DescriptionLength)> = candidates
        .iter()
        .map(|&tau| {
            (
                tau,
                description_length(total, f64_count, i32_count, bool_count, null_count, tau),
            )
        })
        .collect();

    all_costs.sort_by(|(_, a), (_, b)| {
        a.total().partial_cmp(&b.total()).unwrap_or(std::cmp::Ordering::Equal)
    });

    let chosen = all_costs
        .first()
        .map(|(t, _)| *t)
        .unwrap_or(TypeInterpretation::Variant);

    (chosen, all_costs)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mdl_picks_f64_for_pure_f64() {
        let chosen = schema_select(1000, 1000, 0, 0, 0);
        assert_eq!(chosen, TypeInterpretation::F64);
    }

    #[test]
    fn mdl_picks_i32_for_pure_i32() {
        let chosen = schema_select(1000, 0, 1000, 0, 0);
        assert_eq!(chosen, TypeInterpretation::I32);
    }

    #[test]
    fn mdl_picks_null_for_all_null() {
        let chosen = schema_select(1000, 0, 0, 0, 1000);
        assert_eq!(chosen, TypeInterpretation::Null);
    }

    #[test]
    fn mdl_picks_variant_for_mixed() {
        let chosen = schema_select(1000, 500, 500, 0, 0);
        assert_eq!(chosen, TypeInterpretation::Variant);
    }

    #[test]
    fn i32_cheaper_than_f64_for_integers() {
        let f64_cost = description_length(1000, 0, 0, 0, 0, TypeInterpretation::F64).total();
        let i32_cost = description_length(1000, 0, 1000, 0, 0, TypeInterpretation::I32).total();
        assert!(i32_cost < f64_cost);
        assert!(f64_cost.is_infinite()); // 0 f64 cells → invalid interpretation
    }

    #[test]
    fn diagnostics_returns_all_costs() {
        let (chosen, costs) = schema_select_with_diagnostics(1000, 1000, 0, 0, 0);
        assert_eq!(chosen, TypeInterpretation::F64);
        assert!(costs.len() >= 5);
    }
}
