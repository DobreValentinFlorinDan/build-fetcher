//! Port of `server/gpu/accelerate.ts` (Use GPU, Settings → GPU): an evaluation's similarities (model/draft.rs) on the
//! graphics card (cuda.rs), with the same numbers. Before the evaluation uses it, the GPU works out test drafts and the
//! processor checks every number; while it runs, the processor checks one pass in VERIFY_EVERY. Any difference, error
//! or lost GPU sends the evaluation back to the processor for the rest of it, and the log (log.rs) says what happened.
//!
//! 0.x had one GPU per evaluation thread (each a JavaScript thread of its own); 1.0's threads share one, behind a lock.

use super::cuda::{Gpu, MAX_D};
use super::log::gpu_log;
use crate::model::context::Contexts;
use crate::model::draft::{flat_similarities, row_similarity};
use parking_lot::{Mutex, RwLock};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Instant;

/// One pass in this many is worked out on the processor too, and compared.
const VERIFY_EVERY: u64 = 500;

/// Fewer games than this stay on the processor: copying them there and back costs more than it saves.
fn min_rows() -> usize {
    std::env::var("IRELIA_GPU_MIN_ROWS").ok().and_then(|v| v.parse().ok()).unwrap_or(4096)
}

/// The similarities' stand-in: true when it filled `out` (game by game the processor's numbers), false to leave them
/// to the processor.
pub type Accelerator = Arc<dyn Fn(&Contexts, &[f64], f64, &mut [f64]) -> bool + Send + Sync>;

static ACTIVE: AtomicBool = AtomicBool::new(false);
static ACCELERATOR: RwLock<Option<Accelerator>> = RwLock::new(None);

pub fn set_similarity_accelerator(a: Option<Accelerator>) {
    let on = a.is_some();
    *ACCELERATOR.write() = a;
    ACTIVE.store(on, Ordering::SeqCst);
}

/// draft.rs similarities(): the accelerator's numbers, when there is one and it takes them.
#[inline]
pub fn accelerate(zs: &Contexts, q: &[f64], den: f64, out: &mut [f64]) -> bool {
    if !ACTIVE.load(Ordering::Relaxed) {
        return false;
    }
    let a = ACCELERATOR.read().clone();
    match a {
        Some(a) => a(zs, q, den, out),
        None => false,
    }
}

fn hex(x: f64) -> String {
    format!("{} (0x{:016x})", crate::js::num_to_string(x), x.to_bits())
}

/// The first game whose numbers differ (bit for bit), or None.
fn first_difference(a: &[f64], b: &[f64]) -> Option<usize> {
    a.iter().zip(b).position(|(x, y)| x.to_bits() != y.to_bits())
}

struct Case {
    flat: Vec<f64>,
    any_nan: bool,
    rows: usize,
    d: usize,
    q: Vec<f64>,
    den: f64,
}

/// Test drafts for the check: draft-sized contexts, unknown dimensions, far-off games and every kind of bandwidth.
fn test_cases() -> Vec<Case> {
    let mut seed: u32 = 0x9e37_79b9;
    let mut random = move || {
        seed = seed.wrapping_add(0x6d2b_79f5);
        let mut t = (seed ^ (seed >> 15)).wrapping_mul(1 | seed);
        t = t.wrapping_add((t ^ (t >> 7)).wrapping_mul(61 | t)) ^ t;
        (t ^ (t >> 14)) as f64 / 4_294_967_296.0
    };
    let mut cases = vec![];
    for (d, nan, h) in [(12usize, false, 0.7), (12, true, 0.3), (19, true, 1.5), (12, false, 0.05), (19, false, 6.0), (4, true, 0.12)] {
        let rows = 6000;
        let normal = |r: &mut dyn FnMut() -> f64| (-2.0 * (1.0 - r()).ln()).sqrt() * (2.0 * std::f64::consts::PI * r()).cos();
        let mut flat = vec![0.0; rows * d];
        for x in flat.iter_mut() {
            let far = if random() < 0.01 { 40.0 } else { 1.0 };
            *x = if nan && random() < 0.03 { f64::NAN } else { normal(&mut random) * far };
        }
        let q: Vec<f64> = (0..d).map(|c| if nan && c % 5 == 1 { f64::NAN } else { normal(&mut random) }).collect();
        let any_nan = flat.iter().any(|x| x.is_nan());
        cases.push(Case { flat, any_nan, rows, d, q, den: 2.0 * h * h });
    }
    cases
}

/// The GPU's similarities with the games it marks worked out on the processor; the marked count.
fn on_gpu(gpu: &mut Gpu, id: u64, flat: &[f64], rows: usize, d: usize, q: &[f64], den: f64, out: &mut [f64]) -> Result<usize, String> {
    let marked = gpu.similarities(id, flat, rows, d, q, den, out).map_err(|e| e.to_string())?;
    for &i in &marked {
        out[i] = row_similarity(flat, d, q, den, i);
    }
    Ok(marked.len())
}

/// The GPU against the processor on the test drafts: true when every number is the same.
fn self_check(gpu: &mut Gpu, prefix: &str) -> Result<bool, String> {
    let (mut numbers, mut back) = (0usize, 0usize);
    for c in test_cases() {
        let mut out = vec![0.0; c.rows];
        back += on_gpu(gpu, 0, &c.flat, c.rows, c.d, &c.q, c.den, &mut out)?;
        let mut cpu = vec![0.0; c.rows];
        flat_similarities(&c.flat, c.any_nan, c.rows, c.d, &c.q, c.den, &mut cpu);
        if let Some(bad) = first_difference(&cpu, &out) {
            gpu_log(&format!(
                "{prefix}check failed: a test game's similarity is {} on the GPU and {} on the processor. The GPU isn't used.",
                hex(out[bad]),
                hex(cpu[bad])
            ));
            return Ok(false);
        }
        numbers += c.rows;
    }
    gpu_log(&format!("{prefix}check passed: {} test similarities the same to the last digit ({back} worked out on the processor).", crate::js::thousands(numbers as f64)));
    Ok(true)
}

#[derive(Default)]
struct Stats {
    passes: AtomicU64,
    games: AtomicU64,
    back: AtomicU64,
    micros: AtomicU64,
    checks: AtomicU64,
    busy: AtomicU64,
    stopped: AtomicBool,
}

pub type Busy = Arc<dyn Fn() -> bool + Send + Sync>;

/// An evaluation's similarities on the GPU until it is closed (or dropped).
pub struct GpuSession {
    gpu: Arc<Mutex<Gpu>>,
    stats: Arc<Stats>,
    thread: String,
    closed: bool,
}

/// The evaluation's similarities on the GPU, or None (no GPU, or its check failed: the processor, as without Use
/// GPU). `busy`: champ select or a game, when the processor takes over so the game has the card.
pub fn start_gpu(thread: &str, busy: Option<Busy>) -> Option<GpuSession> {
    let mut gpu = match Gpu::open() {
        Ok(g) => g,
        Err(e) => {
            gpu_log(&format!("{thread}: {e}. Evaluate builds runs on the processor."));
            return None;
        }
    };
    let dev = gpu.device.clone();
    gpu_log(&format!(
        "{thread}: {} (compute {}, {} MB, {} MB free; driver {}).",
        dev.name,
        dev.capability,
        crate::js::thousands(dev.memory_mb),
        crate::js::thousands(dev.free_mb),
        dev.driver
    ));
    match self_check(&mut gpu, &format!("{thread}: ")) {
        Ok(true) => {}
        Ok(false) => {
            gpu.close(false);
            return None;
        }
        Err(e) => {
            gpu_log(&format!("{thread}: {e} during the check. The GPU isn't used."));
            gpu.close(false);
            return None;
        }
    }
    let gpu = Arc::new(Mutex::new(gpu));
    let stats = Arc::new(Stats::default());
    let (g, c, label) = (gpu.clone(), stats.clone(), thread.to_string());
    let min = min_rows();
    let accelerate: Accelerator = Arc::new(move |zs: &Contexts, q: &[f64], den: f64, out: &mut [f64]| -> bool {
        let (n, d) = (zs.rows, zs.d);
        if c.stopped.load(Ordering::SeqCst) || n < min || d > MAX_D {
            return false;
        }
        if busy.as_ref().map(|b| b()).unwrap_or(false) {
            c.busy.fetch_add(1, Ordering::Relaxed);
            return false;
        }
        let t0 = Instant::now();
        let marked = {
            let mut gpu = g.lock();
            match on_gpu(&mut gpu, zs.id, &zs.flat, n, d, q, den, out) {
                Ok(m) => m,
                Err(e) => {
                    c.stopped.store(true, Ordering::SeqCst);
                    gpu_log(&format!("{label}: {e}. Back to the processor for the rest of this evaluation (same results)."));
                    return false;
                }
            }
        };
        c.micros.fetch_add(t0.elapsed().as_micros() as u64, Ordering::Relaxed);
        let passes = c.passes.fetch_add(1, Ordering::Relaxed) + 1;
        c.games.fetch_add(n as u64, Ordering::Relaxed);
        c.back.fetch_add(marked as u64, Ordering::Relaxed);
        if passes % VERIFY_EVERY == 1 {
            let mut cpu = vec![0.0; n];
            flat_similarities(&zs.flat, zs.any_nan, n, d, q, den, &mut cpu);
            if let Some(bad) = first_difference(&cpu, &out[..n]) {
                c.stopped.store(true, Ordering::SeqCst);
                gpu_log(&format!(
                    "{label}: check {} failed: a game's similarity is {} on the GPU and {} on the processor. Back to the processor. The {} passes before it ran on the GPU and may differ: run Evaluate builds again with Use GPU off to be sure.",
                    c.checks.load(Ordering::Relaxed) + 1,
                    hex(out[bad]),
                    hex(cpu[bad]),
                    passes - 1
                ));
                out[..n].copy_from_slice(&cpu);
            } else {
                c.checks.fetch_add(1, Ordering::Relaxed);
            }
        }
        true
    });
    set_similarity_accelerator(Some(accelerate));
    Some(GpuSession { gpu, stats, thread: thread.to_string(), closed: false })
}

impl GpuSession {
    /// Back to the processor; what was put on the card goes, and the log says what the GPU did.
    pub fn close(&mut self) {
        if self.closed {
            return;
        }
        self.closed = true;
        set_similarity_accelerator(None);
        let mb = {
            let mut gpu = self.gpu.lock();
            let mb = gpu.uploaded_mb();
            gpu.close(true);
            mb
        };
        let s = &self.stats;
        let (passes, games, back, micros, checks, busy) = (
            s.passes.load(Ordering::Relaxed),
            s.games.load(Ordering::Relaxed),
            s.back.load(Ordering::Relaxed),
            s.micros.load(Ordering::Relaxed),
            s.checks.load(Ordering::Relaxed),
            s.busy.load(Ordering::Relaxed),
        );
        let stopped = s.stopped.load(Ordering::Relaxed);
        if passes == 0 && busy == 0 && !stopped {
            return;
        }
        let n = |x: u64| crate::js::thousands(x as f64);
        let each = if passes > 0 { crate::js::to_fixed(micros as f64 / 1000.0 / passes as f64, 2) } else { "0".into() };
        gpu_log(&format!(
            "{} ({} MB of GPU memory): {} similarity passes on the GPU ({} games, {each} ms each), {} games worked out on the processor, {} checks passed{}{}.",
            self.thread,
            crate::js::num_to_string(mb),
            n(passes),
            n(games),
            n(back),
            n(checks),
            if stopped { ", then back to the processor" } else { "" },
            if busy > 0 { format!("; {} passes on the processor in champ select or a game", n(busy)) } else { String::new() }
        ));
    }
}

impl Drop for GpuSession {
    fn drop(&mut self) {
        self.close();
    }
}

static TESTING: AtomicBool = AtomicBool::new(false);

pub fn gpu_testing() -> bool {
    TESTING.load(Ordering::SeqCst)
}

/// Settings → GPU → Test GPU: open it, check it and time it against the processor, all in the log.
pub fn test_gpu() -> bool {
    if TESTING.swap(true, Ordering::SeqCst) {
        return false;
    }
    let r = test_gpu_now();
    TESTING.store(false, Ordering::SeqCst);
    r
}

fn test_gpu_now() -> bool {
    gpu_log("Test GPU:");
    let mut gpu = match Gpu::open() {
        Ok(g) => g,
        Err(e) => {
            gpu_log(&format!("  {e}."));
            return false;
        }
    };
    let dev = gpu.device.clone();
    gpu_log(&format!(
        "  {} (compute {}, {} MB, {} MB free; driver {}).",
        dev.name,
        dev.capability,
        crate::js::thousands(dev.memory_mb),
        crate::js::thousands(dev.free_mb),
        dev.driver
    ));
    let r = (|| -> Result<bool, String> {
        if !self_check(&mut gpu, "  ")? {
            return Ok(false);
        }
        // 100,000 games of a 12-dimension draft: the size of an evaluation's draft model.
        let (rows, d) = (100_000usize, 12usize);
        let flat: Vec<f64> = (0..rows * d).map(|i| (i as f64 * 12.9898).sin() * 1.7).collect();
        let q: Vec<f64> = (0..d).map(|c| (c as f64).cos()).collect();
        let den = 2.0 * 0.7 * 0.7;
        let (mut out, mut cpu) = (vec![0.0; rows], vec![0.0; rows]);
        let time = |f: &mut dyn FnMut() -> Result<(), String>| -> Result<f64, String> {
            f()?;
            let t0 = Instant::now();
            for _ in 0..20 {
                f()?;
            }
            Ok(t0.elapsed().as_secs_f64() * 1000.0 / 20.0)
        };
        let on_g = time(&mut || on_gpu(&mut gpu, 1, &flat, rows, d, &q, den, &mut out).map(|_| ()))?;
        let on_c = time(&mut || {
            flat_similarities(&flat, false, rows, d, &q, den, &mut cpu);
            Ok(())
        })?;
        gpu_log(&format!(
            "  100,000 games' similarities: {} ms on the GPU, {} ms on one processor thread ({}×).",
            crate::js::to_fixed(on_g, 2),
            crate::js::to_fixed(on_c, 2),
            crate::js::to_fixed(on_c / on_g, 1)
        ));
        Ok(true)
    })();
    gpu.close(false);
    match r {
        Ok(ok) => ok,
        Err(e) => {
            gpu_log(&format!("  {e}."));
            false
        }
    }
}

/// After an evaluation stopped before its end: what it left on the card goes (a session normally frees it itself).
pub fn reset_gpu() -> bool {
    match Gpu::reset() {
        Ok(done) => {
            if done {
                gpu_log("Evaluate builds stopped before its end: the GPU memory it took is freed.");
            }
            done
        }
        Err(e) => {
            gpu_log(&format!("Freeing the GPU after a stopped evaluation: {e}."));
            false
        }
    }
}
