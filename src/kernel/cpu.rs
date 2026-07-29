//! CPU detection via CPUID.
//!
//! At startup, the engine probes CPUID to determine the running CPU's vendor,
//! generation, and available feature flags. This determines which kernels are
//! selectable.

use raw_cpuid::CpuId;

/// CPU vendor.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CpuVendor {
    /// Intel.
    Intel,
    /// AMD.
    Amd,
    /// Apple Silicon / ARM.
    Apple,
    /// ARM (Neoverse, Graviton, Ampere).
    Arm,
    /// Other / unknown.
    Other,
}

/// CPU target — the combination of vendor + microarchitecture + feature flags
/// that determines which kernel to use.
///
/// This is intentionally coarse: we don't need to distinguish every SKU, just
/// the SIMD capability class.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CpuTarget {
    /// Pure scalar fallback (no SIMD). Always available.
    Scalar,
    /// x86-64 with AVX2 (Haswell+, 2013+).
    X86Avx2,
    /// x86-64 with AVX-512 (Ice Lake+, Zen 4+).
    X86Avx512,
    /// ARM with NEON (all 64-bit ARM).
    ArmNeon,
    /// ARM with SVE / SVE2 (Neoverse V2, Graviton 4).
    ArmSve,
}

impl CpuTarget {
    /// Human-readable name.
    pub fn name(&self) -> &'static str {
        match self {
            Self::Scalar => "scalar",
            Self::X86Avx2 => "x86-avx2",
            Self::X86Avx512 => "x86-avx512",
            Self::ArmNeon => "arm-neon",
            Self::ArmSve => "arm-sve",
        }
    }

    /// Does this target support AVX-512?
    pub fn has_avx512(self) -> bool {
        matches!(self, Self::X86Avx512)
    }

    /// Does this target support AVX2?
    pub fn has_avx2(self) -> bool {
        matches!(self, Self::X86Avx2 | Self::X86Avx512)
    }
}

/// Detect the CPU target for the running machine.
pub fn detect_cpu() -> CpuTarget {
    #[cfg(target_arch = "x86_64")]
    {
        if is_x86_feature_detected!("avx512f") && is_x86_feature_detected!("avx512vpopcntdq") {
            return CpuTarget::X86Avx512;
        }
        if is_x86_feature_detected!("avx2") {
            return CpuTarget::X86Avx2;
        }
    }

    #[cfg(target_arch = "aarch64")]
    {
        use std::arch::is_aarch64_feature_detected;
        if is_aarch64_feature_detected!("sve2") {
            return CpuTarget::ArmSve;
        }
        if is_aarch64_feature_detected!("neon") {
            return CpuTarget::ArmNeon;
        }
    }

    CpuTarget::Scalar
}

/// Detect the CPU vendor (best-effort).
pub fn detect_vendor() -> CpuVendor {
    let cpuid = CpuId::new();
    if let Some(vf) = cpuid.get_vendor_info() {
        let brand = vf.as_str();
        if brand.starts_with("GenuineIntel") {
            return CpuVendor::Intel;
        }
        if brand.starts_with("AuthenticAMD") {
            return CpuVendor::Amd;
        }
    }
    CpuVendor::Other
}

/// Get a human-readable CPU brand string.
pub fn cpu_brand_string() -> String {
    let cpuid = CpuId::new();
    cpuid
        .get_processor_brand_string()
        .map(|b| b.as_str().to_string())
        .unwrap_or_else(|| "unknown".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detect_cpu_returns_something() {
        let cpu = detect_cpu();
        // Should at least return Scalar on any platform.
        let _ = cpu;
    }

    #[test]
    fn cpu_target_name_is_human_readable() {
        assert_eq!(CpuTarget::Scalar.name(), "scalar");
        assert_eq!(CpuTarget::X86Avx512.name(), "x86-avx512");
    }

    #[test]
    fn avx512_implies_avx2() {
        assert!(CpuTarget::X86Avx512.has_avx2());
        assert!(CpuTarget::X86Avx512.has_avx512());
        assert!(CpuTarget::X86Avx2.has_avx2());
        assert!(!CpuTarget::X86Avx2.has_avx512());
    }
}
