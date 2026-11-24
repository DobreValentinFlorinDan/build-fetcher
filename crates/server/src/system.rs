//! This computer (GET /api/system) and the evaluation's thread plan (model/evalTeam.ts planThreads).
//!
//! 1.0's evaluation threads share the games and the models in memory (one process, rayon), where 0.22.0's each held a
//! copy: a thread beyond the first takes a small fraction of what it did, so the cores decide, and the memory only
//! on a very large pool.

use irelia::sys::{available_memory, cores, memory_reserve, processor, total_memory};
use serde_json::{json, Value};

/// Memory one more evaluation thread takes per game read (its per-fold scratch: an estimate, with a margin).
pub const THREAD_BYTES_PER_GAME: f64 = 1_200.0;

pub fn thread_bytes(games: f64) -> f64 {
    THREAD_BYTES_PER_GAME * games
}

/// Threads for an evaluation: every logical core whose memory fits in what is available beyond the reserve.
pub fn plan_threads(per_thread: f64) -> f64 {
    let spare = available_memory() - memory_reserve(total_memory());
    let each = per_thread.max(1.0);
    let by_memory = 1.0 + (spare / each).floor().max(0.0);
    (cores() as f64).min(by_memory).max(1.0)
}

pub fn planned_threads(games: f64) -> f64 {
    plan_threads(thread_bytes(games))
}

/// SystemInfo as JSON.
pub fn system_info(games: f64) -> Value {
    let threads = planned_threads(games);
    let c = cores() as f64;
    json!({
        "processor": processor(),
        "cores": c,
        "memory": { "total": total_memory(), "available": available_memory(), "reserve": memory_reserve(total_memory()) },
        "evaluation": { "threads": threads, "perThread": thread_bytes(games), "measured": false, "limit": if threads >= c { "cores" } else { "memory" } },
    })
}
