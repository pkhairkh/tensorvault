//! NUMA topology and CXL device discovery.

use crate::memory::tier::MemoryTier;
use crate::Result;
use std::fs;
use std::path::Path;

/// A NUMA node in the system.
#[derive(Debug, Clone)]
pub struct NumaNode {
    /// OS NUMA node ID (0-indexed).
    pub id: u32,
    /// Number of CPUs in this node.
    pub cpus: Vec<u32>,
    /// Memory tier associated with this node.
    pub tier: MemoryTier,
    /// Total memory in bytes.
    pub memory_bytes: u64,
}

impl NumaNode {
    /// Human-readable summary.
    pub fn summary(&self) -> String {
        format!(
            "NUMA {}: {} CPUs, {:.1} GB, tier={}",
            self.id,
            self.cpus.len(),
            self.memory_bytes as f64 / 1_073_741_824.0,
            self.tier.name(),
        )
    }
}

/// The NUMA topology of the running system.
#[derive(Debug, Clone, Default)]
pub struct NumaTopology {
    /// All NUMA nodes detected.
    pub nodes: Vec<NumaNode>,
}

impl NumaTopology {
    /// Detect the NUMA topology by reading `/sys/devices/system/node/`.
    ///
    /// On non-Linux systems, returns an empty topology.
    pub fn detect() -> Self {
        let mut nodes = Vec::new();

        if let Ok(entries) = fs::read_dir("/sys/devices/system/node/") {
            for entry in entries.flatten() {
                let name = entry.file_name();
                let name = name.to_string_lossy();
                if !name.starts_with("node") {
                    continue;
                }
                let id_str = &name[4..];
                let id: u32 = match id_str.parse() {
                    Ok(v) => v,
                    Err(_) => continue,
                };

                let cpus = read_cpu_list(&format!("/sys/devices/system/node/node{}/cpulist", id));
                let memory_bytes =
                    fs::read_to_string(format!("/sys/devices/system/node/node{}/meminfo", id))
                        .ok()
                        .and_then(|s| {
                            s.lines()
                                .find(|l| l.contains("MemTotal"))
                                .and_then(|l| l.split_whitespace().last())
                                .and_then(|v| v.parse::<u64>().ok())
                                .map(|v| v * 1024)
                        })
                        .unwrap_or(0);

                let tier = classify_node(id, &cpus);

                nodes.push(NumaNode { id, cpus, tier, memory_bytes });
            }
        }

        nodes.sort_by_key(|n| n.id);
        Self { nodes }
    }

    /// Find nodes of a specific tier.
    pub fn nodes_of_tier(&self, tier: MemoryTier) -> Vec<&NumaNode> {
        self.nodes.iter().filter(|n| n.tier == tier).collect()
    }

    /// Number of NUMA nodes.
    pub fn node_count(&self) -> usize {
        self.nodes.len()
    }

    /// Pretty-print the topology.
    pub fn dump(&self) -> String {
        if self.nodes.is_empty() {
            return "NUMA topology: not available (non-Linux or single-node)\n".to_string();
        }
        let mut out = String::from("NUMA topology:\n");
        for n in &self.nodes {
            out.push_str(&format!("  {}\n", n.summary()));
        }
        out
    }
}

/// Classify a NUMA node by tier based on heuristics.
fn classify_node(id: u32, _cpus: &[u32]) -> MemoryTier {
    // Heuristic: nodes with CPUs are DDR5; nodes without CPUs are likely CXL.
    // In production, we'd check /sys/bus/cxl/ for CXL devices.
    let cpus_present = !_cpus.is_empty();

    // Check if this node is a CXL device.
    let cxl_path = format!("/sys/devices/system/node/node{}/class", id);
    if let Ok(class) = fs::read_to_string(&cxl_path) {
        if class.trim() == "cxl" || class.trim().contains("cxl") {
            return MemoryTier::Cxl;
        }
    }

    if cpus_present {
        MemoryTier::Ddr5
    } else {
        // Memory-only node — could be CXL or HBM.
        MemoryTier::Cxl
    }
}

/// Read a CPU list from a sysfs file (e.g., "0-7,16-23").
fn read_cpu_list(path: &str) -> Vec<u32> {
    let content = match fs::read_to_string(path) {
        Ok(c) => c,
        Err(_) => return Vec::new(),
    };
    let content = content.trim();
    if content.is_empty() {
        return Vec::new();
    }
    let mut cpus = Vec::new();
    for range in content.split(',') {
        if let Some((start, end)) = range.split_once('-') {
            if let (Ok(s), Ok(e)) = (start.parse::<u32>(), end.parse::<u32>()) {
                for c in s..=e {
                    cpus.push(c);
                }
            }
        } else if let Ok(c) = range.parse::<u32>() {
            cpus.push(c);
        }
    }
    cpus
}

/// Check if CXL is available on this system.
pub fn cxl_available() -> bool {
    Path::new("/sys/bus/cxl").exists()
}

// ---------------------------------------------------------------------------
// CPU affinity (ADR-008)
// ---------------------------------------------------------------------------

/// Pin the calling thread to a specific CPU.
///
/// On Linux, this calls `sched_setaffinity(2)` with a CPU set containing
/// only `cpu_id`. After this call returns successfully, the OS scheduler
/// will only run the calling thread on `cpu_id` — eliminating cross-NUMA
/// memory access on the hot path (ADR-008).
///
/// On non-Linux targets, this is a no-op that returns `Ok(())` — there is
/// no portable affinity API (macOS uses `thread_policy_set`, Windows uses
/// `SetThreadAffinityMask`, neither is wired in here).
///
/// # Errors
///
/// Returns [`crate::Error::Unsupported`] if `cpu_id` exceeds the static
/// `cpu_set_t` capacity (1023 on glibc), or [`crate::Error::Io`] if the
/// underlying `sched_setaffinity` call fails (e.g. `EPERM` in a restricted
/// container, or `EINVAL` if `cpu_id` is not in the calling thread's
/// `cgroup` cpuset).
pub fn pin_thread_to_cpu(cpu_id: u32) -> Result<()> {
    #[cfg(target_os = "linux")]
    {
        pin_thread_to_cpu_linux(cpu_id)
    }
    #[cfg(not(target_os = "linux"))]
    {
        // No-op on non-Linux: there is no portable affinity API.
        let _ = cpu_id;
        Ok(())
    }
}

/// Return the CPU index that the calling thread is currently running on.
///
/// On Linux, this calls `sched_getcpu(3)`, which is a fast vDSO call (no
/// syscall). On non-Linux targets, returns `0`.
///
/// Returns `0` if the underlying call fails (which should not happen on a
/// functioning Linux system).
pub fn get_current_cpu() -> u32 {
    #[cfg(target_os = "linux")]
    {
        // SAFETY: `sched_getcpu` is a safe FFI function — it takes no
        // pointer arguments, performs no I/O, and simply reads the CPU
        // index from the thread-local vDSO. The libc crate exposes it as
        // `unsafe extern "C" fn` (like all extern "C" functions), but the
        // function itself has no safety preconditions.
        let cpu = unsafe { libc::sched_getcpu() };
        if cpu < 0 {
            0
        } else {
            cpu as u32
        }
    }
    #[cfg(not(target_os = "linux"))]
    {
        0
    }
}

/// Linux implementation of [`pin_thread_to_cpu`].
#[cfg(target_os = "linux")]
fn pin_thread_to_cpu_linux(cpu_id: u32) -> Result<()> {
    // The libc crate's `cpu_set_t` is a fixed-size struct (sized for
    // `CPU_SETSIZE = 1024` on glibc). `CPU_SET` with an index ≥ the bit
    // count of the set would be an out-of-bounds write — reject those up
    // front.
    let cpu_max = 8 * std::mem::size_of::<libc::cpu_set_t>();
    let cpu_idx = cpu_id as usize;
    if cpu_idx >= cpu_max {
        return Err(crate::Error::Unsupported(format!(
            "cpu_id {cpu_id} exceeds the static cpu_set_t capacity ({cpu_max} CPUs)"
        )));
    }

    // SAFETY: `mem::zeroed()` on `cpu_set_t` is sound because the type is a
    // `#[repr(C)]` struct of `c_ulong` words whose all-zero bit pattern is
    // the documented empty-set state (the same state `CPU_ZERO` writes).
    let mut cpuset: libc::cpu_set_t = unsafe { std::mem::zeroed() };

    // SAFETY: `CPU_ZERO` and `CPU_SET` take `&mut cpu_set_t` and only write
    // to the struct's bitset words. `cpuset` is a local variable, so the
    // reference is valid for the duration of the call. `cpu_idx` has been
    // bounds-checked above to be `< 8 * size_of::<cpu_set_t>()`, so the
    // `CPU_SET` write stays in bounds.
    unsafe {
        libc::CPU_ZERO(&mut cpuset);
        libc::CPU_SET(cpu_idx, &mut cpuset);
    }

    // SAFETY: `sched_setaffinity(0, ...)` operates on the calling thread
    // (`pid = 0`). The `cpuset` pointer is valid for
    // `size_of::<cpu_set_t>()` bytes and has been fully initialized by
    // `CPU_ZERO` + `CPU_SET` above. The kernel reads the bitset and
    // updates the calling thread's CPU affinity mask.
    let rc = unsafe {
        libc::sched_setaffinity(
            0,
            std::mem::size_of::<libc::cpu_set_t>(),
            &cpuset as *const libc::cpu_set_t,
        )
    };
    if rc != 0 {
        let err = std::io::Error::last_os_error();
        return Err(crate::Error::Io(err));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn topology_detect_doesnt_crash() {
        let topo = NumaTopology::detect();
        // Should not crash; may be empty on non-Linux.
        let _ = topo.dump();
    }

    #[test]
    fn nodes_of_tier_filters() {
        let topo = NumaTopology {
            nodes: vec![
                NumaNode {
                    id: 0,
                    cpus: vec![0, 1],
                    tier: MemoryTier::Ddr5,
                    memory_bytes: 16_000_000_000,
                },
                NumaNode {
                    id: 1,
                    cpus: vec![],
                    tier: MemoryTier::Cxl,
                    memory_bytes: 32_000_000_000,
                },
            ],
        };
        assert_eq!(topo.nodes_of_tier(MemoryTier::Ddr5).len(), 1);
        assert_eq!(topo.nodes_of_tier(MemoryTier::Cxl).len(), 1);
        assert_eq!(topo.nodes_of_tier(MemoryTier::Hbm).len(), 0);
    }

    #[test]
    fn node_summary_is_human_readable() {
        let n = NumaNode {
            id: 0,
            cpus: vec![0, 1, 2, 3],
            tier: MemoryTier::Ddr5,
            memory_bytes: 16_000_000_000,
        };
        let s = n.summary();
        assert!(s.contains("NUMA 0"));
        assert!(s.contains("4 CPUs"));
        assert!(s.contains("DDR5"));
    }

    /// Test: `pin_thread_to_cpu` does not crash. On Linux, pinning to the
    /// current CPU should succeed (we're already allowed to run on it); on
    /// other OS it's a no-op returning `Ok(())`.
    #[test]
    fn pin_thread_to_cpu_does_not_crash() {
        let cpu = get_current_cpu();
        let result = pin_thread_to_cpu(cpu);
        // Accept either Ok or Err — the DoD requirement is "doesn't crash".
        // On Linux this should normally be Ok, but in heavily restricted
        // containers `sched_setaffinity` can fail with EPERM; we don't want
        // to fail the test in that case.
        if let Err(e) = &result {
            eprintln!("note: pin_thread_to_cpu({cpu}) returned {e:?}");
        }
    }

    /// Test: `get_current_cpu` returns a sensible value. On Linux, the
    /// result should be a non-negative CPU index (we map the -1 error
    /// sentinel to 0). On non-Linux, it always returns 0.
    #[test]
    fn get_current_cpu_returns_valid() {
        let cpu = get_current_cpu();
        // The valid CPU index range on any reasonable Linux system is
        // [0, 8192). Anything outside that would indicate a corrupted
        // return value.
        assert!(cpu < 8192, "unreasonable CPU index: {cpu}");
    }
}
