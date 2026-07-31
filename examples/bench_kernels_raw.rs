//! Wave 12 — raw kernel throughput benchmark.
//!
//! Drives every AVX-512 (and scalar/AVX2 baseline) kernel directly with a
//! 1 M-row synthetic `Vec<u64>` column and reports `cells/sec`, taking the
//! best-of-5 iterations to filter out scheduler / DVFS noise.
//!
//! ## What this measures
//!
//! Each kernel struct is invoked through its `Kernel::execute` trait method
//! with a pre-built input slice — **no** `KernelTable`, **no** `Scheduler`,
//! **no** `Region` lock. This is the raw compute throughput of the SIMD
//! inner loop.
//!
//! ## Peak reference
//!
//! The task defines the theoretical peak as **8 lanes × 2.0 GHz =
//! 16 G cells/sec** for u64 scans and 16 G for f64 with FMA. Real silicon
//! (AMD EPYC-Turin) sustains higher clocks, but this is a stable
//! cross-platform reference point so "% of peak" is comparable across runs.
//!
//! ## Usage
//!
//! ```sh
//! cargo run --release --example bench_kernels_raw
//! ```

#![cfg(target_arch = "x86_64")]

use std::hint::black_box;
use std::time::Instant;

use turbogp::kernel::aggregate::{CountDistinctScalar, SumF64Avx2, SumF64Avx512, SumF64Scalar};
use turbogp::kernel::cpu::CpuTarget;
use turbogp::kernel::hash::{
    HashBuildScalar, HashProbeAvx512, HashProbeScalar, HashTable,
};
use turbogp::kernel::leapfrog::LeapfrogScalar;
use turbogp::kernel::scan::{
    ScanEqAvx2, ScanEqAvx512Cxl, ScanEqAvx512Ddr5, ScanEqAvx512L3, ScanEqScalar,
    ScanMultiPredicateAvx512, ScanMultiPredicateScalar, ScanRangeAvx512L3, ScanRangeScalar,
};
use turbogp::kernel::similarity::{HammingAvx512, HammingScalar};
use turbogp::kernel::{Kernel, KernelParams, PredicateOp};
use turbogp::memory::tier::MemoryTier;

/// Cells per kernel invocation (1 M).
const N: usize = 1_000_000;

/// Theoretical peak for u64 SIMD scans: 8 lanes × 2.0 GHz = 16 G cells/sec.
const PEAK_U64_CPS: f64 = 16.0e9;
/// Theoretical peak for f64 SIMD aggregates: 8 lanes × 2.0 GHz = 16 G cells/sec
/// (FMA adds two flops per lane but one cell per lane, so cells/sec stays 16 G).
const PEAK_F64_CPS: f64 = 16.0e9;

/// A single row in the printed throughput table.
struct Row {
    name: &'static str,
    cpu: CpuTarget,
    tier: MemoryTier,
    cps: f64,
    peak: f64,
    note: &'static str,
}

impl Row {
    fn pct(&self) -> f64 {
        100.0 * self.cps / self.peak
    }
}

/// Build a deterministic 1 M-cell u64 column. `i % 100` so equality and
/// range predicates have known, low hit rates (no branch prediction surprise).
fn make_u64_cells() -> Vec<u64> {
    (0..N).map(|i| (i % 100) as u64).collect()
}

/// Build a deterministic 1 M-cell f64 column stored as u64 bits.
fn make_f64_cells() -> Vec<u64> {
    (0..N).map(|i| ((i as f64) + 1.0).to_bits()).collect()
}

/// Build a sorted u64 column for leapfrog (strictly ascending, no dups).
fn make_sorted(n: usize, stride: u64) -> Vec<u64> {
    (0..n).map(|i| (i as u64) * stride).collect()
}

/// Best-of-5 timing. Returns throughput in cells/sec.
fn bench<F: FnMut()>(cells: usize, mut f: F) -> f64 {
    let mut best = f64::INFINITY;
    for _ in 0..5 {
        let t = Instant::now();
        f();
        let elapsed = t.elapsed().as_secs_f64();
        if elapsed < best {
            best = elapsed;
        }
    }
    (cells as f64) / best
}

// ---------------------------------------------------------------------------
// Per-kernel benchmark wrappers. Each is `unsafe` because `Kernel::execute`
// is unsafe (raw pointer contract).
// ---------------------------------------------------------------------------

unsafe fn bench_scan_eq(kernel: &dyn Kernel, cells: &[u64]) -> f64 {
    let params =
        KernelParams { target_u64: 42, cell_count: cells.len(), ..Default::default() };
    let mut output = [0u8; 64];
    bench(cells.len(), || {
        black_box(kernel.execute(cells.as_ptr() as *const u8, output.as_mut_ptr(), &params));
    })
}

unsafe fn bench_scan_range(kernel: &dyn Kernel, cells: &[u64]) -> f64 {
    let params = KernelParams {
        low_u64: 10,
        high_u64: 20,
        cell_count: cells.len(),
        ..Default::default()
    };
    let mut output = [0u8; 64];
    bench(cells.len(), || {
        black_box(kernel.execute(cells.as_ptr() as *const u8, output.as_mut_ptr(), &params));
    })
}

unsafe fn bench_scan_multi(kernel: &dyn Kernel, cells: &[u64]) -> f64 {
    // 3 predicates: cell == 5 AND cell > 2 AND cell < 10 → only 5 matches.
    let params = KernelParams {
        target_u64: 5,
        pred1_op: PredicateOp::Eq,
        target2_u64: 2,
        pred2_op: PredicateOp::Gt,
        target3_u64: 10,
        pred3_op: PredicateOp::Lt,
        predicate_count: 3,
        cell_count: cells.len(),
        ..Default::default()
    };
    let mut output = [0u8; 64];
    bench(cells.len(), || {
        black_box(kernel.execute(cells.as_ptr() as *const u8, output.as_mut_ptr(), &params));
    })
}

unsafe fn bench_sum_f64(kernel: &dyn Kernel, cells: &[u64]) -> f64 {
    let params = KernelParams { cell_count: cells.len(), ..Default::default() };
    let mut output = [0u8; 64];
    bench(cells.len(), || {
        black_box(kernel.execute(cells.as_ptr() as *const u8, output.as_mut_ptr(), &params));
    })
}

unsafe fn bench_hamming(kernel: &dyn Kernel, cells: &[u64]) -> f64 {
    let params = KernelParams {
        target_u64: 0,
        max_distance: 3,
        cell_count: cells.len(),
        ..Default::default()
    };
    let mut output = [0u8; 64];
    bench(cells.len(), || {
        black_box(kernel.execute(cells.as_ptr() as *const u8, output.as_mut_ptr(), &params));
    })
}

unsafe fn bench_count_distinct(kernel: &dyn Kernel, cells: &[u64]) -> f64 {
    let params = KernelParams { cell_count: cells.len(), ..Default::default() };
    let mut output = [0u8; 64];
    bench(cells.len(), || {
        black_box(kernel.execute(cells.as_ptr() as *const u8, output.as_mut_ptr(), &params));
    })
}

/// Hash-build: every call leaks a `Box<HashTable>` because the kernel calls
/// `Box::into_raw`. To avoid leaking 5 MB per iteration we recover the box
/// inside the closure and free it. Throughput = keys/sec.
unsafe fn bench_hash_build(kernel: &dyn Kernel, keys: &[u64]) -> f64 {
    let params = KernelParams { cell_count: keys.len(), ..Default::default() };
    let mut output = [0u8; 64];
    bench(keys.len(), || {
        kernel.execute(keys.as_ptr() as *const u8, output.as_mut_ptr(), &params);
        // Recover the Box to avoid leaking 24 bytes × distinct_keys per call.
        let ptr = *(output.as_ptr() as *const *mut HashTable);
        if !ptr.is_null() {
            drop(Box::from_raw(ptr));
        }
    })
}

/// Hash-probe: build the table once outside the timed loop, then time only
/// the probe kernel. Throughput = probe_keys/sec.
unsafe fn bench_hash_probe(kernel: &dyn Kernel, table_ptr: *const HashTable, probe_keys: &[u64]) -> f64 {
    // Probe input layout: 8-byte table pointer prefix + probe_keys × 8 bytes.
    let mut probe_input = vec![0u8; 8 + probe_keys.len() * 8];
    probe_input[..8].copy_from_slice(&(table_ptr as u64).to_le_bytes());
    for (i, &k) in probe_keys.iter().enumerate() {
        probe_input[8 + i * 8..8 + (i + 1) * 8].copy_from_slice(&k.to_le_bytes());
    }
    let params = KernelParams { cell_count: probe_keys.len(), ..Default::default() };
    let mut output = [0u8; 64];
    bench(probe_keys.len(), || {
        black_box(kernel.execute(
            probe_input.as_ptr(),
            output.as_mut_ptr(),
            &params,
        ));
    })
}

/// Leapfrog scalar kernel: two sorted slices of length `n/2` each,
/// concatenated into one input buffer. Throughput = total cells / sec.
unsafe fn bench_leapfrog(kernel: &dyn Kernel, left: &[u64], right: &[u64]) -> f64 {
    let total = left.len() + right.len();
    let mut buf: Vec<u8> = Vec::with_capacity(total * 8);
    for &k in left {
        buf.extend_from_slice(&k.to_le_bytes());
    }
    for &k in right {
        buf.extend_from_slice(&k.to_le_bytes());
    }
    let params = KernelParams {
        cell_count: left.len(),
        target_u64: right.len() as u64,
        ..Default::default()
    };
    let mut output = [0u8; 64];
    bench(total, || {
        black_box(kernel.execute(buf.as_ptr(), output.as_mut_ptr(), &params));
    })
}

fn main() {
    // Detect CPU features up-front so we can skip AVX-512 kernels on
    // non-AVX-512 hosts (the kernel structs are still defined on x86_64,
    // but calling them without the feature would SIGILL).
    let has_avx2 = std::arch::is_x86_feature_detected!("avx2");
    let has_avx512f = std::arch::is_x86_feature_detected!("avx512f");
    let has_avx512vpopcntdq = std::arch::is_x86_feature_detected!("avx512vpopcntdq");

    println!("=== Wave 12 Raw Kernel Benchmark ===");
    println!(
        "CPU features: avx2={has_avx2}  avx512f={has_avx512f}  avx512vpopcntdq={has_avx512vpopcntdq}"
    );
    println!("Cells per kernel invocation: {N}");
    println!();

    let cells = make_u64_cells();
    let f64_cells = make_f64_cells();
    let mut rows: Vec<Row> = Vec::new();

    // ---- scan_eq ----
    unsafe {
        rows.push(Row {
            name: ScanEqScalar.name(),
            cpu: ScanEqScalar.cpu(),
            tier: ScanEqScalar.tier(),
            cps: bench_scan_eq(&ScanEqScalar, &cells),
            peak: PEAK_U64_CPS,
            note: "",
        });
        if has_avx2 {
            rows.push(Row {
                name: ScanEqAvx2.name(),
                cpu: ScanEqAvx2.cpu(),
                tier: ScanEqAvx2.tier(),
                cps: bench_scan_eq(&ScanEqAvx2, &cells),
                peak: PEAK_U64_CPS,
                note: "",
            });
        }
        if has_avx512f {
            rows.push(Row {
                name: ScanEqAvx512L3.name(),
                cpu: ScanEqAvx512L3.cpu(),
                tier: ScanEqAvx512L3.tier(),
                cps: bench_scan_eq(&ScanEqAvx512L3, &cells),
                peak: PEAK_U64_CPS,
                note: "",
            });
            rows.push(Row {
                name: ScanEqAvx512Ddr5.name(),
                cpu: ScanEqAvx512Ddr5.cpu(),
                tier: ScanEqAvx512Ddr5.tier(),
                cps: bench_scan_eq(&ScanEqAvx512Ddr5, &cells),
                peak: PEAK_U64_CPS,
                note: "4-page SW prefetch",
            });
            rows.push(Row {
                name: ScanEqAvx512Cxl.name(),
                cpu: ScanEqAvx512Cxl.cpu(),
                tier: ScanEqAvx512Cxl.tier(),
                cps: bench_scan_eq(&ScanEqAvx512Cxl, &cells),
                peak: PEAK_U64_CPS,
                note: "8-page SW prefetch",
            });
        }
    }

    // ---- scan_range ----
    unsafe {
        rows.push(Row {
            name: ScanRangeScalar.name(),
            cpu: ScanRangeScalar.cpu(),
            tier: ScanRangeScalar.tier(),
            cps: bench_scan_range(&ScanRangeScalar, &cells),
            peak: PEAK_U64_CPS,
            note: "",
        });
        if has_avx512f {
            rows.push(Row {
                name: ScanRangeAvx512L3.name(),
                cpu: ScanRangeAvx512L3.cpu(),
                tier: ScanRangeAvx512L3.tier(),
                cps: bench_scan_range(&ScanRangeAvx512L3, &cells),
                peak: PEAK_U64_CPS,
                note: "",
            });
        }
    }

    // ---- scan_multi_predicate ----
    unsafe {
        rows.push(Row {
            name: ScanMultiPredicateScalar.name(),
            cpu: ScanMultiPredicateScalar.cpu(),
            tier: ScanMultiPredicateScalar.tier(),
            cps: bench_scan_multi(&ScanMultiPredicateScalar, &cells),
            peak: PEAK_U64_CPS,
            note: "",
        });
        if has_avx512f {
            rows.push(Row {
                name: ScanMultiPredicateAvx512.name(),
                cpu: ScanMultiPredicateAvx512.cpu(),
                tier: ScanMultiPredicateAvx512.tier(),
                cps: bench_scan_multi(&ScanMultiPredicateAvx512, &cells),
                peak: PEAK_U64_CPS,
                note: "VPTERNLOGQ fusion",
            });
        }
    }

    // ---- sum_f64 ----
    unsafe {
        rows.push(Row {
            name: SumF64Scalar.name(),
            cpu: SumF64Scalar.cpu(),
            tier: SumF64Scalar.tier(),
            cps: bench_sum_f64(&SumF64Scalar, &f64_cells),
            peak: PEAK_F64_CPS,
            note: "",
        });
        if has_avx2 {
            rows.push(Row {
                name: SumF64Avx2.name(),
                cpu: SumF64Avx2.cpu(),
                tier: SumF64Avx2.tier(),
                cps: bench_sum_f64(&SumF64Avx2, &f64_cells),
                peak: PEAK_F64_CPS,
                note: "",
            });
        }
        if has_avx512f {
            rows.push(Row {
                name: SumF64Avx512.name(),
                cpu: SumF64Avx512.cpu(),
                tier: SumF64Avx512.tier(),
                cps: bench_sum_f64(&SumF64Avx512, &f64_cells),
                peak: PEAK_F64_CPS,
                note: "",
            });
        }
    }

    // ---- hamming similarity ----
    unsafe {
        rows.push(Row {
            name: HammingScalar.name(),
            cpu: HammingScalar.cpu(),
            tier: HammingScalar.tier(),
            cps: bench_hamming(&HammingScalar, &cells),
            peak: PEAK_U64_CPS,
            note: "",
        });
        if has_avx512f {
            rows.push(Row {
                name: HammingAvx512.name(),
                cpu: HammingAvx512.cpu(),
                tier: HammingAvx512.tier(),
                cps: bench_hamming(&HammingAvx512, &cells),
                peak: PEAK_U64_CPS,
                note: if has_avx512vpopcntdq { "VPOPCNTDQ" } else { "scalar fallback" },
            });
        }
    }

    // ---- hash_build / hash_probe ----
    unsafe {
        rows.push(Row {
            name: HashBuildScalar.name(),
            cpu: HashBuildScalar.cpu(),
            tier: HashBuildScalar.tier(),
            cps: bench_hash_build(&HashBuildScalar, &cells),
            peak: PEAK_U64_CPS,
            note: "std::HashMap (not SIMD)",
        });

        // Build the table once for probe benchmarking.
        let table = HashTable::build(&cells);
        let table_ptr: *const HashTable = &table as *const HashTable;
        rows.push(Row {
            name: HashProbeScalar.name(),
            cpu: HashProbeScalar.cpu(),
            tier: HashProbeScalar.tier(),
            cps: bench_hash_probe(&HashProbeScalar, table_ptr, &cells),
            peak: PEAK_U64_CPS,
            note: "std::HashMap lookup",
        });
        if has_avx512f {
            rows.push(Row {
                name: HashProbeAvx512.name(),
                cpu: HashProbeAvx512.cpu(),
                tier: HashProbeAvx512.tier(),
                cps: bench_hash_probe(&HashProbeAvx512, table_ptr, &cells),
                peak: PEAK_U64_CPS,
                note: "delegates to scalar",
            });
        }
    }

    // ---- count_distinct ----
    unsafe {
        rows.push(Row {
            name: CountDistinctScalar.name(),
            cpu: CountDistinctScalar.cpu(),
            tier: CountDistinctScalar.tier(),
            cps: bench_count_distinct(&CountDistinctScalar, &cells),
            peak: PEAK_U64_CPS,
            note: "HashSet (not SIMD)",
        });
    }

    // ---- leapfrog ----
    unsafe {
        // Two sorted slices with stride 2 and 3 → intersection size ~N/6.
        let half = N / 2;
        let left = make_sorted(half, 2);
        let right = make_sorted(half, 3);
        rows.push(Row {
            name: LeapfrogScalar.name(),
            cpu: LeapfrogScalar.cpu(),
            tier: LeapfrogScalar.tier(),
            cps: bench_leapfrog(&LeapfrogScalar, &left, &right),
            peak: PEAK_U64_CPS,
            note: "2-way intersection",
        });
    }

    // ---- print table ----
    println!();
    println!(
        "{:<32} {:<10} {:<6} {:>14} {:>14} {:>8}  {}",
        "KERNEL", "TARGET", "TIER", "CELLS/SEC", "PEAK", "%PEAK", "NOTES"
    );
    println!("{}", "-".repeat(110));
    for r in &rows {
        println!(
            "{:<32} {:<10} {:<6} {:>14.3e} {:>14.3e} {:>7.1}%  {}",
            r.name,
            r.cpu.name(),
            format!("{:?}", r.tier),
            r.cps,
            r.peak,
            r.pct(),
            r.note,
        );
    }
    println!();
    println!(
        "Peak reference: 8 lanes × 2.0 GHz = 16.0 G cells/sec (u64 & f64)."
    );
    println!(
        "{} kernels benchmarked. (cells/sec = best-of-5 iterations.)",
        rows.len()
    );
}
