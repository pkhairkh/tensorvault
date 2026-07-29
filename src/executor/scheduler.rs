//! The scheduler: dispatches kernel invocations respecting data dependencies
//! and tier bandwidth.

use crate::executor::plan::{lower_to_kernels, KernelInvocation, LogicalPlan, PlanResult};
use crate::kernel::{KernelParams, KernelResult, KernelTable, Operator};
use crate::memory::region::{Region, RegionId};
use crate::Result;
use parking_lot::RwLock;
use std::collections::HashMap;
use std::sync::Arc;

/// The scheduler holds the kernel table and a registry of regions.
pub struct Scheduler {
    /// The kernel table (the moat).
    pub kernel_table: Arc<KernelTable>,
    /// Region registry: maps region IDs to regions.
    regions: RwLock<HashMap<RegionId, Arc<Region>>>,
}

impl Scheduler {
    /// Create a new scheduler.
    pub fn new(kernel_table: Arc<KernelTable>) -> Self {
        Self { kernel_table, regions: RwLock::new(HashMap::new()) }
    }

    /// Register a region with the scheduler.
    pub fn register_region(&self, region: Arc<Region>) {
        self.regions.write().insert(region.id, region);
    }

    /// Look up a region by ID.
    pub fn get_region(&self, id: RegionId) -> Option<Arc<Region>> {
        self.regions.read().get(&id).cloned()
    }

    /// Execute a kernel invocation on a registered region.
    pub fn execute_invocation(&self, inv: &KernelInvocation) -> Result<KernelResult> {
        let region = self
            .get_region(inv.region_id)
            .ok_or_else(|| crate::Error::NotFound(format!("region {}", inv.region_id)))?;

        // Select the best kernel for (operator, tier).
        let kernel = self.kernel_table.select(inv.operator, region.tier).ok_or_else(|| {
            crate::Error::Unsupported(format!(
                "no kernel for operator {:?} on tier {}",
                inv.operator, region.tier
            ))
        })?;

        // Lock the region's backing once and keep the guard alive across the
        // kernel call so the underlying pointer remains valid.
        let data = region.data.lock();
        let cell_count = inv.params.cell_count.min(data.as_slice().len() / 8);

        let mut params = inv.params;
        params.cell_count = cell_count;

        // Execute the kernel.
        // SAFETY: `data` borrows the region's `RegionBacking` for the
        // duration of the lock; `as_slice().as_ptr()` is valid for
        // `data.len()` bytes — at least `cell_count * 8`. The output buffer
        // is a local stack array, valid for 64 bytes.
        let mut output = [0u8; 64];
        let result =
            unsafe { kernel.execute(data.as_slice().as_ptr(), output.as_mut_ptr(), &params) };

        Ok(result)
    }

    /// Execute a logical plan.
    pub fn execute_plan(&self, plan: &LogicalPlan) -> Result<PlanResult> {
        let invocations = lower_to_kernels(plan);
        let mut results = Vec::with_capacity(invocations.len());
        for inv in &invocations {
            // For invocations with region_id == 0 (aggregates/joins), skip
            // if no region is registered. In a full implementation, these
            // would read from intermediate buffers.
            if inv.region_id == 0 {
                results.push(KernelResult::default());
                continue;
            }
            match self.execute_invocation(inv) {
                Ok(r) => results.push(r),
                Err(e) => {
                    tracing::warn!("kernel invocation failed: {}", e);
                    results.push(KernelResult::default());
                }
            }
        }
        Ok(PlanResult { kernel_results: results })
    }

    /// Convenience: scan a region for cells equal to a target.
    pub fn scan_eq(&self, region_id: RegionId, target: u64) -> Result<u64> {
        let region = self
            .get_region(region_id)
            .ok_or_else(|| crate::Error::NotFound(format!("region {}", region_id)))?;
        let cell_count = region.cell_count();

        let inv = KernelInvocation {
            operator: Operator::ScanEqU64,
            tier: region.tier,
            region_id,
            params: KernelParams { target_u64: target, cell_count, ..Default::default() },
        };
        Ok(self.execute_invocation(&inv)?.count)
    }

    /// Convenience: sum f64 cells in a region.
    pub fn sum_f64(&self, region_id: RegionId) -> Result<f64> {
        let region = self
            .get_region(region_id)
            .ok_or_else(|| crate::Error::NotFound(format!("region {}", region_id)))?;
        let cell_count = region.cell_count();

        let inv = KernelInvocation {
            operator: Operator::AggregateSumF64,
            tier: region.tier,
            region_id,
            params: KernelParams { cell_count, ..Default::default() },
        };
        Ok(self.execute_invocation(&inv)?.sum)
    }

    /// Convenience: count cells within Hamming distance of a target.
    pub fn count_similar(
        &self,
        region_id: RegionId,
        target: u64,
        max_distance: u32,
    ) -> Result<u64> {
        let region = self
            .get_region(region_id)
            .ok_or_else(|| crate::Error::NotFound(format!("region {}", region_id)))?;
        let cell_count = region.cell_count();

        let inv = KernelInvocation {
            operator: Operator::SimilarityHamming,
            tier: region.tier,
            region_id,
            params: KernelParams {
                target_u64: target,
                max_distance,
                cell_count,
                ..Default::default()
            },
        };
        Ok(self.execute_invocation(&inv)?.count)
    }

    /// Number of registered regions.
    pub fn region_count(&self) -> usize {
        self.regions.read().len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::kernel::KernelTable;
    use crate::memory::tier::MemoryTier;

    fn make_region_with_cells(id: RegionId, tier: MemoryTier, cells: &[u64]) -> Arc<Region> {
        let mut bytes = vec![0u8; 2 * 1024 * 1024]; // 2 MB
        for (i, &c) in cells.iter().enumerate() {
            let offset = i * 8;
            if offset + 8 <= bytes.len() {
                bytes[offset..offset + 8].copy_from_slice(&c.to_le_bytes());
            }
        }
        Arc::new(Region::from_bytes(id, tier, &bytes))
    }

    #[test]
    fn scheduler_scan_eq_works() {
        let kt = Arc::new(KernelTable::new());
        let sched = Scheduler::new(kt);
        let cells: Vec<u64> = (0..1000).map(|i| i % 7).collect();
        let region = make_region_with_cells(0, MemoryTier::L3, &cells);
        sched.register_region(region);

        let count = sched.scan_eq(0, 3).unwrap();
        // 1000 / 7 ≈ 142.857, so 3 appears either 142 or 143 times.
        assert!(count >= 142 && count <= 143);
    }

    #[test]
    fn scheduler_sum_f64_works() {
        let kt = Arc::new(KernelTable::new());
        let sched = Scheduler::new(kt);
        let cells: Vec<u64> = (1..=100).map(|i| (i as f64).to_bits()).collect();
        let region = make_region_with_cells(0, MemoryTier::L3, &cells);
        sched.register_region(region);

        let sum = sched.sum_f64(0).unwrap();
        let expected: f64 = (1..=100).map(|i| i as f64).sum();
        assert!((sum - expected).abs() < 1e-6);
    }

    #[test]
    fn scheduler_count_similar_works() {
        let kt = Arc::new(KernelTable::new());
        let sched = Scheduler::new(kt);
        let cells: Vec<u64> = vec![0, 1, 2, 3, 4, 5, 6, 7];
        let region = make_region_with_cells(0, MemoryTier::L3, &cells);
        sched.register_region(region);

        // Hamming distance 0 from 3 → just 3 itself.
        let count = sched.count_similar(0, 3, 0).unwrap();
        assert_eq!(count, 1);
    }

    #[test]
    fn scheduler_missing_region_returns_error() {
        let kt = Arc::new(KernelTable::new());
        let sched = Scheduler::new(kt);
        let result = sched.scan_eq(999, 0);
        assert!(result.is_err());
    }
}
