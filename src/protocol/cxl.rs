//! CXL single-rack coordinator (stub).
//!
//! Within a CXL 3.0 fabric, coherence is hardware-managed. Transactions
//! commit via a fence + CXL cache flush, ~250 ns latency. No consensus
//! protocol is needed — the fabric provides visibility.

use crate::memory::tier::MemoryTier;

/// A CXL coordinator for single-rack transactions.
pub struct CxlCoordinator {
    /// Whether CXL is available on this system.
    available: bool,
}

impl CxlCoordinator {
    /// Create a new CXL coordinator.
    pub fn new() -> Self {
        Self { available: crate::memory::numa::cxl_available() }
    }

    /// Is CXL available?
    pub fn is_available(&self) -> bool {
        self.available
    }

    /// Commit a transaction via CXL fence.
    ///
    /// In a real implementation, this would:
    /// 1. Issue a CXL.cache flush for the modified cache lines
    /// 2. Issue a memory fence (MFENCE)
    /// 3. Publish the commit by writing to a shared CXL-resident commit record
    ///
    /// Returns the commit latency in nanoseconds.
    pub fn commit(&self, _txn_id: u64) -> u64 {
        if !self.available {
            return 0; // No CXL — caller should use a different coordinator.
        }
        // Simulated commit latency: ~250 ns typical, ~500 ns contended.
        250
    }

    /// The tier this coordinator writes commit records to.
    pub fn commit_tier(&self) -> MemoryTier {
        MemoryTier::Cxl
    }
}

impl Default for CxlCoordinator {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cxl_coordinator_doesnt_crash() {
        let c = CxlCoordinator::new();
        let _ = c.commit(0);
    }
}
