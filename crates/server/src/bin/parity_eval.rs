//! Parity harness, step 2 (1.0's side): the evaluation of a data folder, as parity/ts-eval.ts runs 0.22.0's.
//!
//!   parity_eval <data-folder> <out.json> [--quick] [--threads N] [--candidates]
//!
//! 2.2.0's candidates (5-minute state sample, effective window) are off unless --candidates: 0.22.0 has neither, so
//! the comparison with it runs without them; the app's own full evaluation has them on.

use irelia::db::{Db, OpenOptions};
use irelia::model::run_eval::{format_eval, measure, RunOptions};
use irelia::testkit::load_test_static_data;
use std::time::Instant;

fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let dir = std::path::PathBuf::from(&args[1]);
    let out = std::path::PathBuf::from(&args[2]);
    let quick = args.iter().any(|a| a == "--quick");
    let candidates = args.iter().any(|a| a == "--candidates");
    let threads: usize = args.iter().position(|a| a == "--threads").and_then(|i| args.get(i + 1)).and_then(|s| s.parse().ok()).unwrap_or(1);
    let db = Db::open(&dir.join("irelia.db"), OpenOptions { migrate: false, busy_timeout_ms: 5000 })?;
    let data = load_test_static_data();
    let started = Instant::now();
    let peak = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(rss()));
    let p2 = peak.clone();
    let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let s2 = stop.clone();
    let sampler = std::thread::spawn(move || {
        while !s2.load(std::sync::atomic::Ordering::Relaxed) {
            p2.fetch_max(rss(), std::sync::atomic::Ordering::Relaxed);
            std::thread::sleep(std::time::Duration::from_millis(200));
        }
    });
    let opts = if candidates {
        None
    } else {
        let mut o = irelia::model::run_eval::eval_options(&irelia::model::run_eval::eval_reads(&db, &data, quick)?.scope, quick);
        o.state_sample = false;
        o.effective_window = false;
        Some(o)
    };
    let output = measure(
        &db,
        data,
        RunOptions {
            quick,
            threads,
            log: Box::new(|l| eprintln!("[rs] {l}")),
            progress: Box::new(|p| {
                if p.part.is_none() {
                    eprintln!("[rs] step {}/{}: {}", p.step, p.steps, p.label)
                }
            }),
            opts,
            salt: Some("parity".into()),
            base: None,
            control: None,
        },
    )?;
    stop.store(true, std::sync::atomic::Ordering::Relaxed);
    let _ = sampler.join();
    let ms = started.elapsed().as_secs_f64() * 1000.0;
    let peak = peak.load(std::sync::atomic::Ordering::Relaxed);
    let v = serde_json::json!({
        "report": serde_json::to_value(&output.report)?,
        "text": format_eval(&output.report),
        "ms": ms,
        "peakRss": peak,
        "timings": serde_json::to_value(&output.timings)?,
        "known": output.next.map(|n| n.known).unwrap_or_default(),
    });
    std::fs::write(&out, irelia::js::stringify_pretty(&v))?;
    eprintln!("[rs] done in {:.1} s, peak RSS {} MB", ms / 1000.0, peak / (1 << 20));
    Ok(())
}

/// The process's resident memory (Linux /proc; elsewhere 0).
fn rss() -> u64 {
    std::fs::read_to_string("/proc/self/status")
        .ok()
        .and_then(|s| s.lines().find(|l| l.starts_with("VmRSS:")).and_then(|l| l.split_whitespace().nth(1)).and_then(|k| k.parse::<u64>().ok()))
        .map(|k| k * 1024)
        .unwrap_or(0)
}
