//! Tier-aware memory manager.
//!
//! Every piece of data lives in a specific tier of the memory hierarchy,
//! chosen by access pattern. The memory manager migrates whole 2 MB regions
//! between tiers based on telemetry.
//!
//! ## Tiers
//!
//! | Tier | Latency | What lives here |
//! |------|---------|-----------------|
//! | L1/L2 | 1–4 ns | Current 4 KB working batch (auto-managed by HW) |
//! | L3 | 10–20 ns | Hot indexes, hash tables < 32 MB, bloom filters |
//! | DDR5 | 80–100 ns | Hot working set, large hash tables |
//! | HBM | 100–150 ns | Scan-heavy analytics (Xeon Max, MI300A) |
//! | CXL | 140–500 ns | Buffer pool extension, cold-ish indexes |
//! | NVMe | 10–30 µs | WAL, LSM compaction, cold data |

pub mod numa;
pub mod region;
pub mod tier;

pub use numa::{NumaTopology, NumaNode};
pub use region::{Region, RegionId, RegionStats};
pub use tier::MemoryTier;
