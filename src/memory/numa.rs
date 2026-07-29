//! NUMA topology and CXL device discovery.

use crate::memory::tier::MemoryTier;
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
}
