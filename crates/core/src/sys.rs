//! This computer: cores, memory, processor (model/evalTeam.ts cores / availableMemory / memoryReserve, os.cpus).
//! Read here for the Data tab and the thread plans, never sent anywhere.

use std::sync::LazyLock;
use parking_lot::Mutex;
use sysinfo::{CpuRefreshKind, MemoryRefreshKind, RefreshKind, System};

static SYS: LazyLock<Mutex<System>> = LazyLock::new(|| {
    Mutex::new(System::new_with_specifics(RefreshKind::nothing().with_memory(MemoryRefreshKind::nothing().with_ram()).with_cpu(CpuRefreshKind::nothing())))
});

/// Logical cores (threads the processor runs at once).
pub fn cores() -> usize {
    std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1)
}

pub fn total_memory() -> f64 {
    let mut s = SYS.lock();
    s.refresh_memory_specifics(MemoryRefreshKind::nothing().with_ram());
    s.total_memory() as f64
}

/// Memory the system can give now (bytes).
pub fn available_memory() -> f64 {
    let mut s = SYS.lock();
    s.refresh_memory_specifics(MemoryRefreshKind::nothing().with_ram());
    s.available_memory() as f64
}

/// Left free whatever the evaluation does: the game, Windows and the rest of the app need it.
pub fn memory_reserve(total: f64) -> f64 {
    crate::model::logistic::js_max(1.5 * 1024f64.powi(3), 0.15 * total)
}

pub fn processor() -> Option<String> {
    let mut s = SYS.lock();
    s.refresh_cpu_specifics(CpuRefreshKind::nothing());
    s.cpus().first().map(|c| c.brand().trim().to_string()).filter(|b| !b.is_empty())
}

/// Helpers for a model build: every core but this one, as the free memory allows, at most 6.
pub fn prepare_threads() -> usize {
    const HELPER_HEAP_MB: f64 = 512.0;
    let by_mem = (available_memory() / 3.0 / (HELPER_HEAP_MB * 1024.0 * 1024.0)).floor() as i64;
    (6i64.min(cores() as i64 - 1).min(by_mem)).max(1) as usize
}

/// This process's resident memory (bytes), for logs and measurements.
pub fn own_rss() -> Option<u64> {
    let pid = sysinfo::get_current_pid().ok()?;
    let mut s = System::new();
    s.refresh_processes(sysinfo::ProcessesToUpdate::Some(&[pid]), true);
    s.process(pid).map(|p| p.memory())
}
