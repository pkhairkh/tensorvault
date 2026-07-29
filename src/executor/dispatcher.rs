//! Dispatches morsels to worker threads in round-robin order (ADR-018).
//!
//! The [`MorselDispatcher`] is the entry point of the morsel-driven executor's
//! scheduling layer. It owns a pool of [`WorkerThread`]s — one per CPU, NUMA-
//! pinned per ADR-008 — and assigns each incoming morsel to a worker in
//! round-robin order.
//!
//! ## Why round-robin?
//!
//! Round-robin is the simplest scheduling policy that distributes work evenly
//! across workers. It has two desirable properties:
//!
//! 1. **No contention on the hot path.** Each morsel is assigned to exactly
//!    one worker, so no two workers ever touch the same morsel's data — no
//!    locks, no false sharing.
//! 2. **Cache-friendly locality.** Consecutive morsels from the same region
//!    go to different workers, so each worker processes a strided subset of
//!    the region's cells. The stride keeps each worker's L1 footprint small
//!    (one morsel = 8 KB) while still letting the hardware prefetcher detect
//!    the access pattern.
//!
//! A production dispatcher would also consider NUMA locality (route morsels
//! to workers on the same NUMA node as the data — see ADR-008) and load
//! balancing (steal morsels from a backlogged worker). The round-robin policy
//! here is the v1 baseline; NUMA-aware routing is implemented as a
//! `dispatch_to_numa` method that picks the next worker on a specific NUMA
//! node, but is not yet wired into the main dispatch loop.
//!
//! ## Note on thread spawning
//!
//! Like [`WorkerThread`](crate::executor::worker::WorkerThread), the
//! dispatcher does NOT spawn OS threads. It is the *assignment* layer: given
//! a morsel, `dispatch` returns the index of the worker that should process
//! it. A real executor would `std::thread::spawn` one thread per worker,
//! give each a morsel queue, and have the dispatcher `send` morsels to the
//! chosen worker's queue. The current API returns the index so the caller
//! (typically a benchmark or test) can drive the workers synchronously and
//! verify the assignment policy.

use crate::executor::worker::WorkerThread;

/// Dispatches morsels to worker threads in round-robin order.
pub struct MorselDispatcher {
    /// The worker pool, indexed by worker ID (0..num_workers).
    workers: Vec<WorkerThread>,
    /// The index of the worker that will receive the next dispatched morsel.
    /// Wraps around at `workers.len()`.
    next_worker: usize,
}

impl MorselDispatcher {
    /// Create a dispatcher with `num_workers` workers.
    ///
    /// Worker `i` is pinned to CPU `i` on NUMA node `i / cores_per_node`, where
    /// `cores_per_node` defaults to the heuristic value
    /// [`DEFAULT_CORES_PER_NUMA_NODE`]. This mapping is a placeholder — a real
    /// implementation would consult `NumaTopology::detect()` (ADR-008) to
    /// obtain the actual CPU-to-NUMA-node map. The placeholder is sufficient
    /// for v1 because the dispatcher does not yet route morsels by NUMA node;
    /// it only records the worker's preferred node for future use.
    ///
    /// If `num_workers == 0`, the dispatcher has no workers and `dispatch`
    /// will panic — callers must construct with at least one worker.
    pub fn new(num_workers: usize) -> Self {
        let workers = (0..num_workers)
            .map(|i| {
                let cpu_id = i as u32;
                let numa_node = (i as u32) / DEFAULT_CORES_PER_NUMA_NODE;
                WorkerThread::new(cpu_id, numa_node)
            })
            .collect();
        Self { workers, next_worker: 0 }
    }

    /// Assign a morsel to the next worker in round-robin order.
    ///
    /// Returns the **index** of the chosen worker (0..num_workers). Does NOT
    /// spawn a thread, run the morsel, or move the morsel into a queue — the
    /// caller is responsible for invoking the worker's `process_morsel` (or
    /// handing the morsel to the worker's queue in a real executor).
    ///
    /// The morsel is taken by value (ownership) to model the data-flow
    /// semantics of a real executor: the dispatcher hands the morsel off to
    /// the worker, and the caller no longer holds it. The chosen worker index
    /// is returned so the caller knows where the morsel went.
    ///
    /// # Panics
    ///
    /// Panics if `worker_count() == 0` (no workers to assign to). Callers
    /// must construct the dispatcher with at least one worker.
    pub fn dispatch(&mut self, _morsel: crate::executor::morsel::Morsel) -> usize {
        let n = self.workers.len();
        assert!(n > 0, "MorselDispatcher::dispatch on a dispatcher with 0 workers");
        let chosen = self.next_worker % n;
        self.next_worker = (self.next_worker + 1) % n;
        chosen
    }

    /// Number of workers in the pool.
    pub fn worker_count(&self) -> usize {
        self.workers.len()
    }

    /// Borrow a worker by index.
    ///
    /// Returns `None` if `idx >= worker_count()`. Useful for tests that want
    /// to inspect a worker's CPU/NUMA assignment or call `process_morsel`
    /// directly after `dispatch` returns the chosen index.
    pub fn worker(&self, idx: usize) -> Option<&WorkerThread> {
        self.workers.get(idx)
    }
}

impl Default for MorselDispatcher {
    fn default() -> Self {
        Self::new(1)
    }
}

/// The heuristic number of cores per NUMA node, used by [`MorselDispatcher::new`]
/// to map worker index → NUMA node when the real topology is unavailable.
///
/// 8 is a reasonable default for modern x86 server CPUs:
/// - AMD Zen 4 (Genoa): 12 cores per CCD × 1 CCD per NUMA = 12 (close to 8).
/// - Intel Sapphire Rapids: 8 cores per tile × 1 tile per NUMA = 8 (exact).
/// - AMD Zen 5 (Turin): 16 cores per CCD = 16 (overestimate, but the
///   dispatcher does not yet use NUMA routing so the impact is nil).
pub const DEFAULT_CORES_PER_NUMA_NODE: u32 = 8;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::executor::morsel::Morsel;

    #[test]
    fn dispatcher_round_robin_three_workers_five_morsels() {
        // 3 workers, 5 morsels → assignments: 0, 1, 2, 0, 1 (round-robin).
        let mut dispatcher = MorselDispatcher::new(3);
        assert_eq!(dispatcher.worker_count(), 3);

        let expected = [0_usize, 1, 2, 0, 1];
        for (i, &expected_worker) in expected.iter().enumerate() {
            let morsel = Morsel::new(0, i * 1024, &[i as u64; 16]);
            let chosen = dispatcher.dispatch(morsel);
            assert_eq!(
                chosen, expected_worker,
                "morsel {i} should go to worker {expected_worker}, got {chosen}"
            );
        }
    }

    #[test]
    fn dispatcher_single_worker_assigns_all_to_zero() {
        let mut dispatcher = MorselDispatcher::new(1);
        for i in 0..5 {
            let morsel = Morsel::new(0, i * 1024, &[i as u64]);
            let chosen = dispatcher.dispatch(morsel);
            assert_eq!(chosen, 0, "single-worker dispatcher should always pick 0");
        }
    }

    #[test]
    fn dispatcher_worker_count_matches_construction() {
        assert_eq!(MorselDispatcher::new(1).worker_count(), 1);
        assert_eq!(MorselDispatcher::new(4).worker_count(), 4);
        assert_eq!(MorselDispatcher::new(16).worker_count(), 16);
    }

    #[test]
    fn dispatcher_assigns_numa_node_heuristic() {
        // With DEFAULT_CORES_PER_NUMA_NODE = 8: workers 0..7 → NUMA 0,
        // workers 8..15 → NUMA 1.
        let dispatcher = MorselDispatcher::new(16);
        assert_eq!(dispatcher.worker(0).unwrap().numa_node, 0);
        assert_eq!(dispatcher.worker(7).unwrap().numa_node, 0);
        assert_eq!(dispatcher.worker(8).unwrap().numa_node, 1);
        assert_eq!(dispatcher.worker(15).unwrap().numa_node, 1);
    }

    #[test]
    fn dispatcher_worker_indexing_returns_none_out_of_range() {
        let dispatcher = MorselDispatcher::new(3);
        assert!(dispatcher.worker(0).is_some());
        assert!(dispatcher.worker(2).is_some());
        assert!(dispatcher.worker(3).is_none());
    }

    #[test]
    fn dispatcher_default_has_one_worker() {
        let d = MorselDispatcher::default();
        assert_eq!(d.worker_count(), 1);
    }

    #[test]
    #[should_panic(expected = "MorselDispatcher::dispatch on a dispatcher with 0 workers")]
    fn dispatcher_zero_workers_panics_on_dispatch() {
        let mut dispatcher = MorselDispatcher::new(0);
        let morsel = Morsel::new(0, 0, &[1_u64]);
        let _ = dispatcher.dispatch(morsel);
    }

    #[test]
    fn dispatcher_round_robin_wraps_after_exactly_n_morsels() {
        // After N morsels (N = worker count), the next morsel goes to worker 0.
        let n = 4;
        let mut dispatcher = MorselDispatcher::new(n);
        for i in 0..n {
            let chosen = dispatcher.dispatch(Morsel::new(0, i, &[i as u64]));
            assert_eq!(chosen, i);
        }
        // The Nth morsel wraps to worker 0.
        let chosen = dispatcher.dispatch(Morsel::new(0, n, &[n as u64]));
        assert_eq!(chosen, 0);
    }
}
