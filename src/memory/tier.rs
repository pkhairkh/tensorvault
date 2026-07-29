//! Memory tier definitions.

use serde::{Deserialize, Serialize};

/// A tier of the memory hierarchy.
///
/// Each tier has a characteristic latency, bandwidth, and energy profile.
/// The kernel table has different kernels for different tiers because the
/// optimal prefetch distance, batch size, and SIMD width depend on the tier.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum MemoryTier {
    /// L1/L2 cache (per-core, ~1–4 ns). Auto-managed by hardware.
    L1L2,
    /// L3 / Smart Cache / Infinity Cache (per-socket, ~10–20 ns).
    L3,
    /// Local DDR5 (per-socket, ~80–100 ns).
    Ddr5,
    /// HBM (Xeon Max / MI300A, ~100–150 ns, multi-TB/s).
    Hbm,
    /// CXL.mem expansion (rack-local, ~140–500 ns).
    Cxl,
    /// NVMe SSD (rack-local, ~10–30 µs).
    Nvme,
    /// NVMe-oF / RDMA (cross-rack, ~30–60 µs).
    NvmeOf,
    /// RoCEv2 / InfiniBand (cross-rack, ~1–10 µs RTT).
    Network,
}

impl MemoryTier {
    /// Typical latency in nanoseconds (best case).
    pub fn latency_ns(self) -> f64 {
        match self {
            Self::L1L2 => 4.0,
            Self::L3 => 15.0,
            Self::Ddr5 => 90.0,
            Self::Hbm => 120.0,
            Self::Cxl => 250.0,
            Self::Nvme => 20_000.0,
            Self::NvmeOf => 50_000.0,
            Self::Network => 5_000.0,
        }
    }

    /// Typical bandwidth in GB/s (per link/core).
    pub fn bandwidth_gbps(self) -> f64 {
        match self {
            Self::L1L2 => 2000.0,
            Self::L3 => 300.0,
            Self::Ddr5 => 50.0,
            Self::Hbm => 1600.0,
            Self::Cxl => 64.0,
            Self::Nvme => 14.0,
            Self::NvmeOf => 12.0,
            Self::Network => 50.0,
        }
    }

    /// Energy per 64-byte access in nanojoules.
    pub fn energy_nj(self) -> f64 {
        match self {
            Self::L1L2 => 0.1,
            Self::L3 => 1.5,
            Self::Ddr5 => 2.0,
            Self::Hbm => 2.0,
            Self::Cxl => 7.0,
            Self::Nvme => 1000.0,
            Self::NvmeOf => 1500.0,
            Self::Network => 500.0,
        }
    }

    /// Human-readable name.
    pub fn name(self) -> &'static str {
        match self {
            Self::L1L2 => "L1/L2",
            Self::L3 => "L3",
            Self::Ddr5 => "DDR5",
            Self::Hbm => "HBM",
            Self::Cxl => "CXL",
            Self::Nvme => "NVMe",
            Self::NvmeOf => "NVMe-oF",
            Self::Network => "Network",
        }
    }

    /// Is this a volatile tier (data lost on power-off)?
    pub fn is_volatile(self) -> bool {
        matches!(self, Self::L1L2 | Self::L3 | Self::Ddr5 | Self::Hbm | Self::Cxl)
    }

    /// Is this a local tier (no network hop)?
    pub fn is_local(self) -> bool {
        matches!(self, Self::L1L2 | Self::L3 | Self::Ddr5 | Self::Hbm | Self::Cxl | Self::Nvme)
    }
}

impl Default for MemoryTier {
    fn default() -> Self {
        Self::Ddr5
    }
}

impl std::fmt::Display for MemoryTier {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.name())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tier_latencies_are_monotonic() {
        assert!(MemoryTier::L1L2.latency_ns() < MemoryTier::L3.latency_ns());
        assert!(MemoryTier::L3.latency_ns() < MemoryTier::Ddr5.latency_ns());
        assert!(MemoryTier::Ddr5.latency_ns() < MemoryTier::Cxl.latency_ns());
        assert!(MemoryTier::Cxl.latency_ns() < MemoryTier::Nvme.latency_ns());
    }

    #[test]
    fn tier_energies_are_monotonic() {
        assert!(MemoryTier::L1L2.energy_nj() < MemoryTier::L3.energy_nj());
        assert!(MemoryTier::L3.energy_nj() < MemoryTier::Ddr5.energy_nj());
        assert!(MemoryTier::Ddr5.energy_nj() < MemoryTier::Nvme.energy_nj());
    }

    #[test]
    fn volatile_tiers_excludes_nvme() {
        assert!(MemoryTier::Ddr5.is_volatile());
        assert!(MemoryTier::Cxl.is_volatile());
        assert!(!MemoryTier::Nvme.is_volatile());
    }

    #[test]
    fn local_tiers_excludes_network() {
        assert!(MemoryTier::Ddr5.is_local());
        assert!(MemoryTier::Nvme.is_local());
        assert!(!MemoryTier::Network.is_local());
    }
}
