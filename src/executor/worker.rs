//! A NUMA-pinned worker thread that processes morsels through a pipeline.
//!
//! Each [`WorkerThread`] is bound to a specific CPU and NUMA node at
//! construction. When a worker is spawned (outside this module — the worker
//! here is just the *bookkeeping* for the thread's affinity), it calls
//! [`WorkerThread::pin`] to call `sched_setaffinity(2)` so the OS scheduler
//! will only run the worker on its assigned CPU (ADR-008). This eliminates
//! cross-NUMA memory access on the hot path: a worker pinned on NUMA node 0
//! only ever touches local DDR5, never the remote node's memory.
//!
//! ## Why pin?
//!
//! Without pinning, the Linux scheduler may migrate a worker between CPUs at
//! any time — including across NUMA nodes. A migration to a remote NUMA node
//! invalidates the worker's L1/L2/L3 cache state, so the next morsel it
//! processes pays a ~100 ns remote-DRAM penalty per cache line. With pinning,
//! the worker stays on one CPU, its L1/L2 state survives across morsels, and
//! the per-morsel pipeline runs entirely on local memory.
//!
//! ## Note on thread spawning
//!
//! This module does NOT spawn OS threads — that's the responsibility of the
//! dispatcher (or a higher-level orchestrator). The [`WorkerThread`] struct
//! is the *bookkeeping* for a worker: its CPU, its NUMA node, and a
//! [`process_morsel`](WorkerThread::process_morsel) method that runs a kernel
//! on a morsel. A real executor would `std::thread::spawn` one thread per
//! `WorkerThread`, call `pin()` inside the spawned closure, and then loop on
//! a morsel queue calling `process_morsel` for each dequeued morsel.

use crate::executor::morsel::Morsel;
use crate::kernel::{Kernel, KernelParams, KernelResult};
use crate::Result;

/// A NUMA-pinned worker thread that processes morsels through a pipeline.
///
/// `cpu_id` and `numa_node` are set at construction and never change; pinning
/// is a runtime operation that calls `sched_setaffinity` (Linux) or is a
/// no-op (non-Linux).
#[derive(Debug, Clone, Copy)]
pub struct WorkerThread {
    /// The CPU/core this worker is pinned to.
    pub cpu_id: u32,
    /// The NUMA node this worker prefers.
    pub numa_node: u32,
}

impl WorkerThread {
    /// Create a new worker bound to `cpu_id` on `numa_node`.
    pub fn new(cpu_id: u32, numa_node: u32) -> Self {
        Self { cpu_id, numa_node }
    }

    /// Pin the calling thread to this worker's CPU.
    ///
    /// Must be called from the thread that will run the worker (typically the
    /// first thing inside a `std::thread::spawn` closure). On Linux this calls
    /// `sched_setaffinity(2)` with a 1-CPU set; on non-Linux it is a no-op.
    ///
    /// # Errors
    ///
    /// Returns [`crate::Error::Unsupported`] if `cpu_id` exceeds the static
    /// `cpu_set_t` capacity (1023 on glibc), or [`crate::Error::Io`] if the
    /// underlying `sched_setaffinity` call fails (e.g. `EPERM` in a
    /// restricted container). See `src/memory/numa.rs` for details.
    pub fn pin(&self) -> Result<()> {
        crate::memory::numa::pin_thread_to_cpu(self.cpu_id)
    }

    /// Execute a kernel on the morsel's data.
    ///
    /// Copies `params` and overrides `cell_count` with the morsel's actual
    /// valid length (so callers can pass a `KernelParams` with a placeholder
    /// `cell_count` of 0 and let the worker fill it in). The kernel's `execute`
    /// is `unsafe` because it dereferences raw pointers — we uphold the
    /// safety contract by passing `morsel.as_slice().as_ptr()` (valid for
    /// `morsel.len() * 8` bytes) and a 64-byte stack output buffer.
    pub fn process_morsel(
        &self,
        morsel: &Morsel,
        kernel: &dyn Kernel,
        params: &KernelParams,
    ) -> KernelResult {
        let mut params = *params;
        params.cell_count = morsel.len;
        // The output buffer is large enough for any KernelResult layout (16
        // bytes count + 8 bytes sum + 8 bytes mask = 32 bytes; we round up to
        // 64 for alignment headroom).
        let mut output = [0u8; 64];
        // SAFETY: `morsel.as_slice()` borrows the morsel's `Vec<u64>` for the
        // duration of the call; `as_ptr()` is valid for `morsel.len() * 8`
        // readable bytes (the Vec owns exactly that many). `output` is a
        // 64-byte stack array, valid for 64 writable bytes — more than
        // `size_of::<KernelResult>()`. The kernel was selected from the
        // kernel table, which only registers kernels whose CPU feature flags
        // are present on this machine (ADR-003).
        unsafe {
            kernel.execute(morsel.as_slice().as_ptr() as *const u8, output.as_mut_ptr(), &params)
        }
    }
}

impl Default for WorkerThread {
    fn default() -> Self {
        Self::new(0, 0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::kernel::{KernelParams, KernelTable, Operator};

    #[test]
    fn worker_pin_does_not_crash() {
        // The DoD is "pin doesn't crash" — we accept either Ok or Err.
        // On Linux this normally succeeds; in heavily restricted containers
        // `sched_setaffinity` can fail with EPERM. Either way, no panic.
        let worker = WorkerThread::new(0, 0);
        let _ = worker.pin();
    }

    #[test]
    fn worker_process_morsel_runs_scan_eq() {
        let kt = KernelTable::new();
        let kernel = kt
            .select(Operator::ScanEqU64, crate::memory::tier::MemoryTier::L3)
            .expect("scan_eq kernel should be registered for L3");
        let worker = WorkerThread::new(0, 0);

        // 10 cells, three of which equal 7.
        let cells: Vec<u64> = vec![1, 7, 2, 7, 3, 7, 4, 5, 6, 7];
        let morsel = Morsel::new(0, 0, &cells);

        let params = KernelParams { target_u64: 7, ..Default::default() };
        let result = worker.process_morsel(&morsel, kernel.as_ref(), &params);
        assert_eq!(result.count, 4, "expected 4 sevens in the morsel");
    }

    #[test]
    fn worker_process_morsel_runs_sum_f64() {
        let kt = KernelTable::new();
        let kernel = kt
            .select(Operator::AggregateSumF64, crate::memory::tier::MemoryTier::L3)
            .expect("sum_f64 kernel should be registered for L3");
        let worker = WorkerThread::new(1, 0);

        let values = [1.0_f64, 2.0, 3.0, 4.0, 5.0];
        let cells: Vec<u64> = values.iter().map(|v| v.to_bits()).collect();
        let morsel = Morsel::new(0, 0, &cells);

        let params = KernelParams::default();
        let result = worker.process_morsel(&morsel, kernel.as_ref(), &params);
        assert!((result.sum - 15.0).abs() < 1e-9, "sum should be 15.0, got {}", result.sum);
        assert_eq!(result.count, 5);
    }

    #[test]
    fn worker_default_is_cpu_zero_numa_zero() {
        let w = WorkerThread::default();
        assert_eq!(w.cpu_id, 0);
        assert_eq!(w.numa_node, 0);
    }

    #[test]
    fn worker_clone_copy_preserves_fields() {
        let w1 = WorkerThread::new(7, 2);
        let w2 = w1;
        assert_eq!(w1.cpu_id, w2.cpu_id);
        assert_eq!(w1.numa_node, w2.numa_node);
    }
}
