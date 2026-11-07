//! CPU Topology — for cache-aware work stealing and NUMA-aware placement.
//!
//! Populated from ACPI MADT + CPUID during boot.
//! Used by work stealing to prefer same-L3 victims over cross-NUMA victims.
//!
//! Reference: "Cache-Aware Scheduling" (Lozi et al., EuroSys 2016).
//! Key finding: stealing from a same-L3 CPU is 3-5x cheaper than cross-NUMA.

use alloc::vec::Vec;

/// Describes the cache/NUMA relationship between two CPUs.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub enum CacheDistance {
    /// Same logical core (hyperthreading sibling). Shares L1/L2.
    HyperThread = 0,
    /// Same physical core on a different HT. Shares L2/L3.
    SameCore = 1,
    /// Different core on same socket/package. Shares L3.
    SameL3 = 2,
    /// Different NUMA node (different socket or memory domain). No shared cache.
    CrossNuma = 3,
}

/// Per-CPU topology entry.
#[derive(Clone, Debug)]
pub struct CpuInfo {
    /// Logical CPU ID (APIC ID on x86).
    pub logical_id: usize,
    /// Physical core ID (same for HT siblings).
    pub core_id: u32,
    /// Socket / package ID.
    pub package_id: u32,
    /// NUMA node ID (may differ from package_id on EPYC/Threadripper).
    pub numa_node: usize,
    /// L3 cache group ID (CPUs sharing an L3 have the same value).
    pub l3_group: u32,
}

impl CpuInfo {
    pub fn unknown(logical_id: usize) -> Self {
        CpuInfo {
            logical_id,
            core_id: logical_id as u32,
            package_id: 0,
            numa_node: 0,
            l3_group: 0,
        }
    }
}

/// The full CPU topology of the system.
pub struct CpuTopology {
    /// Per-CPU information. Index = logical CPU ID.
    cpus: Vec<CpuInfo>,
    /// Precomputed steal order for each CPU (sorted by CacheDistance).
    steal_orders: Vec<Vec<usize>>,
}

impl CpuTopology {
    /// Build topology from ACPI / CPUID data.
    ///
    /// In MaiOS, this is called from `captain` or `multicore_bringup`
    /// after all CPUs have been enumerated.
    pub fn from_cpus(cpus: Vec<CpuInfo>) -> Self {
        let num = cpus.len();
        // Precompute steal orders: for each CPU, sort all other CPUs
        // by cache distance ascending.
        let steal_orders = (0..num)
            .map(|me| {
                let mut others: Vec<(CacheDistance, usize)> = (0..num)
                    .filter(|&other| other != me)
                    .map(|other| (Self::distance(&cpus[me], &cpus[other]), other))
                    .collect();
                others.sort_unstable_by_key(|&(dist, id)| (dist, id));
                others.into_iter().map(|(_, id)| id).collect()
            })
            .collect();

        CpuTopology { cpus, steal_orders }
    }

    /// Build a trivial single-socket topology (fallback if ACPI unavailable).
    pub fn uniform(num_cpus: usize) -> Self {
        let cpus: Vec<CpuInfo> = (0..num_cpus)
            .map(|i| CpuInfo {
                logical_id: i,
                core_id: (i / 2) as u32,   // assume HT pairs
                package_id: 0,
                numa_node: 0,
                l3_group: 0,
            })
            .collect();
        Self::from_cpus(cpus)
    }

    /// Returns the NUMA node for a logical CPU.
    pub fn cpu_to_numa(&self, cpu: usize) -> usize {
        self.cpus.get(cpu).map_or(0, |c| c.numa_node)
    }

    /// Returns the precomputed steal order for `cpu_id`.
    ///
    /// The slice is sorted from closest (HyperThread sibling) to furthest
    /// (cross-NUMA). Work stealing iterates this slice in order.
    pub fn steal_order(&self, cpu_id: usize) -> &[usize] {
        self.steal_orders
            .get(cpu_id)
            .map(|v| v.as_slice())
            .unwrap_or(&[])
    }

    /// Get the CPUs in a given NUMA node.
    pub fn cpus_in_numa(&self, node: usize) -> impl Iterator<Item = usize> + '_ {
        self.cpus.iter().filter(move |c| c.numa_node == node).map(|c| c.logical_id)
    }

    /// Compute the cache distance between two CPUs.
    fn distance(a: &CpuInfo, b: &CpuInfo) -> CacheDistance {
        if a.numa_node != b.numa_node {
            return CacheDistance::CrossNuma;
        }
        if a.l3_group != b.l3_group {
            // Same NUMA but different L3 (e.g., AMD CCD).
            return CacheDistance::CrossNuma;
        }
        if a.core_id != b.core_id {
            return CacheDistance::SameL3;
        }
        if a.logical_id != b.logical_id {
            return CacheDistance::HyperThread;
        }
        CacheDistance::HyperThread
    }
}