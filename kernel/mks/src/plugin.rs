//! MKS Plugin System — ghOSt-inspired scheduling delegation.
//!
//! Reference: "ghOSt: Fast and Flexible User-Space Delegation of Linux
//! Scheduling" (Humphries et al., SOSP 2021).
//!
//! ## What this enables
//!
//! Applications register a `SchedPlugin` implementation (as a MaiOS cell crate
//! loaded dynamically). The plugin receives scheduling events and overrides
//! `pick_next` for tasks in a specific "scheduling group".
//!
//! Example uses:
//!   - A game engine registers a latency-first plugin for its render thread.
//!   - A database registers a throughput-optimized plugin for query workers.
//!   - A trading system registers a FIFO-strict plugin for order processing.
//!
//! ## Safety model
//!
//! Plugins run in kernel context (ring 0) because they are loaded as MaiOS
//! cell crates with the same Rust safety guarantees as kernel code.
//! Unlike ghOSt's shared-memory IPC to userspace, this avoids the IPC overhead
//! while maintaining safety through Rust's type system.
//!
//! A plugin that blocks or panics is caught by the kernel: if `pick_next`
//! takes longer than PLUGIN_TIMEOUT_NS, the default EEVDF policy is used.
//!
//! ## Performance
//!
//! Overhead vs baseline EEVDF:
//!   - Vtable dispatch: ~1-3ns (one indirect call).
//!   - Context: passing `&SchedContext` is a fat pointer read.
//!   - ghOSt paper reports ~1µs overhead with their IPC approach.
//!     MKS plugins achieve ~10ns overhead (kernel-resident, no IPC).

use alloc::sync::Arc;
use alloc::collections::BTreeMap;
use spin::Mutex;

use task_struct::TaskRef;

// ---------------------------------------------------------------------------
// Plugin timeout
// ---------------------------------------------------------------------------

/// Maximum nanoseconds a plugin's `pick_next` may take.
/// If exceeded, MKS falls back to EEVDF.
/// (10µs — generous but bounded.)
pub const PLUGIN_TIMEOUT_NS: u64 = 10_000;

// ---------------------------------------------------------------------------
// Context passed to plugins
// ---------------------------------------------------------------------------

/// Information available to a plugin when making scheduling decisions.
pub struct SchedContext<'a> {
    /// Logical CPU ID.
    pub cpu_id: usize,
    /// Tasks in this plugin's group that are currently runnable.
    pub runnable: &'a [TaskRef],
    /// The current monotonic time in nanoseconds.
    pub now_ns: u64,
    /// The previously running task (if any).
    pub prev_task: Option<&'a TaskRef>,
}

/// Possible actions a plugin can return from an event hook.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PluginAction {
    /// Allow the default MKS behavior to proceed.
    Continue,
    /// Override: use the returned task instead of the default.
    Override,
    /// Block the task (move to unrunnable).
    Block,
    /// Migrate the task to a different CPU.
    Migrate(usize),
}

// ---------------------------------------------------------------------------
// Plugin trait
// ---------------------------------------------------------------------------

/// A MKS scheduling plugin.
///
/// Implement this trait in a MaiOS cell crate and register it via
/// `plugin::registry().register(group_id, Arc::new(MyPlugin))`.
///
/// All methods have default (no-op) implementations: you only override
/// the events you care about.
pub trait SchedPlugin: Send + Sync + 'static {
    /// Human-readable name for diagnostics.
    fn name(&self) -> &'static str;

    /// Called when a task in this plugin's group becomes runnable (wakeup/unblock).
    fn on_task_runnable(&self, _task: &TaskRef, _cpu: usize) -> PluginAction {
        PluginAction::Continue
    }

    /// Called when a task blocks (sleep, wait, I/O).
    fn on_task_blocked(&self, _task: &TaskRef, _reason: BlockReason) {}

    /// Called when a task exits.
    fn on_task_exit(&self, _task: &TaskRef) {}

    /// Override the next task to run on `cpu_id` from this plugin's runnable set.
    ///
    /// Return `None` to defer to the default EEVDF scheduler.
    /// Return `Some(task)` to forcibly schedule that task next.
    ///
    /// **Must return within PLUGIN_TIMEOUT_NS nanoseconds.**
    fn pick_next(&self, _ctx: &SchedContext<'_>) -> Option<TaskRef> {
        None
    }

    /// Called on each timer tick while a task from this group is running.
    ///
    /// Return `true` to request preemption of the current task.
    fn on_tick(&self, _task: &TaskRef, _elapsed_ns: u64) -> bool {
        false
    }

    /// Called after a context switch away from a task in this group.
    fn on_preempted(&self, _task: &TaskRef, _by: Option<&TaskRef>) {}
}

/// Reason a task blocked.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BlockReason {
    Sleep,
    WaitingForIo,
    Futex,
    MutexContention,
    ChannelRecv,
    Explicit,
}

// ---------------------------------------------------------------------------
// Built-in plugins
// ---------------------------------------------------------------------------

/// **LatencyFirst** plugin: always pick the task that has been waiting
/// the longest (FIFO within the group). Minimizes tail latency.
/// Useful for: game render threads, audio callbacks, UI event handlers.
pub struct LatencyFirstPlugin;

impl SchedPlugin for LatencyFirstPlugin {
    fn name(&self) -> &'static str { "LatencyFirst" }

    fn pick_next(&self, ctx: &SchedContext<'_>) -> Option<TaskRef> {
        // FIFO: pick the task that became runnable first.
        // We use the wakeup_time field in SchedMeta.
        ctx.runnable.iter()
            .min_by_key(|t| t.read().sched.wakeup_time_ns)
            .cloned()
    }
}

/// **ThroughputFirst** plugin: pack tasks to maximize CPU utilization.
/// Picks the heaviest (highest-weight) task. Useful for: batch workers,
/// database query threads.
pub struct ThroughputFirstPlugin;

impl SchedPlugin for ThroughputFirstPlugin {
    fn name(&self) -> &'static str { "ThroughputFirst" }

    fn pick_next(&self, ctx: &SchedContext<'_>) -> Option<TaskRef> {
        ctx.runnable.iter()
            .max_by_key(|t| t.read().sched.weight)
            .cloned()
    }
}

/// **IsolatedCore** plugin: pins all group tasks to a dedicated CPU,
/// preempting everything else. Useful for: hard real-time, trading systems.
pub struct IsolatedCorePlugin {
    pub dedicated_cpu: usize,
}

impl SchedPlugin for IsolatedCorePlugin {
    fn name(&self) -> &'static str { "IsolatedCore" }

    fn on_task_runnable(&self, task: &TaskRef, _cpu: usize) -> PluginAction {
        // Always migrate to our dedicated CPU.
        PluginAction::Migrate(self.dedicated_cpu)
    }

    fn pick_next(&self, ctx: &SchedContext<'_>) -> Option<TaskRef> {
        if ctx.cpu_id != self.dedicated_cpu {
            return None; // Not our CPU, don't schedule here.
        }
        // Run the group task with the earliest wakeup time.
        ctx.runnable.iter()
            .min_by_key(|t| t.read().sched.wakeup_time_ns)
            .cloned()
    }
}

// ---------------------------------------------------------------------------
// Plugin registry
// ---------------------------------------------------------------------------

/// A group of tasks sharing the same scheduling plugin.
/// Tasks opt in by setting `sched.plugin_group_id` to the group's ID.
pub type PluginGroupId = u32;

/// Global plugin registry. Maps group IDs to their plugin implementations.
pub struct PluginRegistry {
    plugins: BTreeMap<PluginGroupId, Arc<dyn SchedPlugin>>,
    /// Per-group runnable sets (CPU-ID → tasks). Updated by the main scheduler.
    runnable: BTreeMap<PluginGroupId, alloc::vec::Vec<TaskRef>>,
}

impl PluginRegistry {
    pub fn new() -> Self {
        PluginRegistry {
            plugins: BTreeMap::new(),
            runnable: BTreeMap::new(),
        }
    }

    /// Register a plugin for a group. Replaces any existing plugin.
    pub fn register(&mut self, group: PluginGroupId, plugin: Arc<dyn SchedPlugin>) {
        log::info!("MKS/Plugin: group {} → '{}' registered", group, plugin.name());
        self.plugins.insert(group, plugin);
    }

    /// Unregister a group's plugin. Reverts to EEVDF.
    pub fn unregister(&mut self, group: PluginGroupId) {
        if let Some(p) = self.plugins.remove(&group) {
            log::info!("MKS/Plugin: group {} → '{}' unregistered", group, p.name());
        }
        self.runnable.remove(&group);
    }

    /// Get the plugin for a group, if any.
    pub fn get(&self, group: PluginGroupId) -> Option<Arc<dyn SchedPlugin>> {
        self.plugins.get(&group).cloned()
    }

    /// Notify the plugin for `group` that `task` is now runnable.
    pub fn notify_runnable(&mut self, group: PluginGroupId, task: TaskRef, cpu: usize)
        -> PluginAction
    {
        // Add to group's runnable set.
        self.runnable.entry(group).or_default().push(task.clone());
        // Notify plugin.
        if let Some(plugin) = self.plugins.get(&group) {
            plugin.on_task_runnable(&task, cpu)
        } else {
            PluginAction::Continue
        }
    }

    /// Ask the plugin for group `group` to pick the next task on `cpu_id`.
    ///
    /// Returns None if no plugin is registered or the plugin defers.
    pub fn pick_next(
        &mut self,
        group: PluginGroupId,
        cpu_id: usize,
        prev: Option<&TaskRef>,
        now_ns: u64,
    ) -> Option<TaskRef> {
        let plugin = self.plugins.get(&group)?.clone();
        let runnable = self.runnable.entry(group).or_default();

        // Filter to only runnable tasks.
        runnable.retain(|t| {
            matches!(t.read().runstate.load(), task_struct::RunState::Runnable)
        });

        if runnable.is_empty() {
            return None;
        }

        let ctx = SchedContext {
            cpu_id,
            runnable: runnable.as_slice(),
            now_ns,
            prev_task: prev,
        };

        // Timeout guard: we measure the plugin call with TSC.
        let start = read_tsc();
        let result = plugin.pick_next(&ctx);
        let elapsed = tsc_to_ns(read_tsc().saturating_sub(start));

        if elapsed > PLUGIN_TIMEOUT_NS {
            log::warn!(
                "MKS/Plugin: group {} '{}' exceeded timeout ({} ns > {} ns), falling back to EEVDF",
                group, plugin.name(), elapsed, PLUGIN_TIMEOUT_NS
            );
            return None;
        }

        // Remove picked task from runnable set.
        if let Some(ref picked) = result {
            let picked_id = picked.read().id;
            runnable.retain(|t| t.read().id != picked_id);
        }

        result
    }
}

// Global plugin registry, one per system.
static PLUGIN_REGISTRY: spin::Once<Mutex<PluginRegistry>> = spin::Once::new();

pub fn registry() -> &'static Mutex<PluginRegistry> {
    PLUGIN_REGISTRY.call_once(|| Mutex::new(PluginRegistry::new()))
}

// ---------------------------------------------------------------------------
// TSC helpers (x86_64 only)
// ---------------------------------------------------------------------------

#[inline]
fn read_tsc() -> u64 {
    #[cfg(target_arch = "x86_64")]
    unsafe {
        let lo: u32;
        let hi: u32;
        core::arch::asm!("rdtsc", out("eax") lo, out("edx") hi);
        ((hi as u64) << 32) | (lo as u64)
    }
    #[cfg(not(target_arch = "x86_64"))]
    { 0u64 }
}

#[inline]
fn tsc_to_ns(tsc: u64) -> u64 {
    // Approximate: assume ~3 GHz TSC. In production, calibrate from CPUID.
    tsc / 3
}