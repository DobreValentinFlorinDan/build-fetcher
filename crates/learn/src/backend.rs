//! Where the heavy arithmetic runs. The processor (`CpuBackend`) is the reference: every row's sum runs in a fixed
//! order and rows are independent, so the result doesn't depend on how many threads rayon uses. An accelerator (the
//! NVIDIA GPU through `bf_core::gpu::cuda`, or a stand-in in tests) goes through `CheckedBackend`, which checks it
//! against the processor before using it and then one call in `verify_every`, and falls back to the processor for
//! good on any difference beyond `TOLERANCE`, any error, or no device.
//!
//! What the GPU does in 1.0: `similarities` (the "contexts like the user's" weights the recommender uses), through the
//! PTX kernel bf-core ships and irelia-companion proved bit-identical to the processor. `logits` (the training and
//! evaluation forward pass) has no kernel in bf-core yet, so on CUDA it stays on the processor; the trait and the
//! checks are in place for one, and tests drive them with a stand-in accelerator. Real-GPU timing was not measured.

use parking_lot::Mutex;
use rayon::prelude::*;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

/// The largest difference allowed between an accelerator and the processor: |a − c| ≤ TOLERANCE · (1 + |c|). Loose
/// enough for a reordered sum of ~100 doubles, tight enough that any real bug (a wrong index, f32 arithmetic) fails.
pub const TOLERANCE: f64 = 1e-9;

/// A batch of sparse rows (compressed sparse rows).
#[derive(Clone, Debug, Default)]
pub struct Csr {
    /// Row r is `idx[ptr[r]..ptr[r + 1]]` and the matching `val`.
    pub ptr: Vec<usize>,
    pub idx: Vec<u32>,
    pub val: Vec<f64>,
}

impl Csr {
    pub fn new() -> Csr {
        Csr { ptr: vec![0], idx: vec![], val: vec![] }
    }
    pub fn rows(&self) -> usize {
        self.ptr.len() - 1
    }
    pub fn push_row(&mut self, feats: &[(u32, f64)]) {
        for &(i, v) in feats {
            self.idx.push(i);
            self.val.push(v);
        }
        self.ptr.push(self.idx.len());
    }
    /// Row r's dot product with `w`, in index order.
    #[inline]
    pub fn row_dot(&self, r: usize, w: &[f64]) -> f64 {
        let mut s = 0.0;
        for k in self.ptr[r]..self.ptr[r + 1] {
            s += w[self.idx[k] as usize] * self.val[k];
        }
        s
    }
    pub fn bytes(&self) -> usize {
        self.ptr.len() * 8 + self.idx.len() * 4 + self.val.len() * 8
    }
}

/// The operations the learner and the recommender hand to a backend.
pub trait Backend: Send + Sync {
    /// "cpu" or "gpu: <device>".
    fn name(&self) -> String;
    /// out[r] = Σ w[idx] · val over row r.
    fn logits(&self, rows: &Csr, w: &[f64], out: &mut [f64]);
    /// out[i] = exp(−mean over known dimensions of (flat[i·d + c] − q[c])² / den); NaN marks a dimension unknown; a
    /// row with no known dimension is 1. The same definition as bf-core's similarities kernel.
    fn similarities(&self, flat: &[f64], rows: usize, d: usize, q: &[f64], den: f64, out: &mut [f64]);
    /// Lines for the learning log (checks, fallbacks), drained.
    fn take_events(&self) -> Vec<String> {
        vec![]
    }
}

/// Rows per rayon task: fixed, so the split never depends on the thread count (each row is independent anyway).
const ROWS_PER_TASK: usize = 256;

pub struct CpuBackend;

impl Backend for CpuBackend {
    fn name(&self) -> String {
        "cpu".into()
    }
    fn logits(&self, rows: &Csr, w: &[f64], out: &mut [f64]) {
        out[..rows.rows()].par_chunks_mut(ROWS_PER_TASK).enumerate().for_each(|(c, o)| {
            for (k, x) in o.iter_mut().enumerate() {
                *x = rows.row_dot(c * ROWS_PER_TASK + k, w);
            }
        });
    }
    fn similarities(&self, flat: &[f64], rows: usize, d: usize, q: &[f64], den: f64, out: &mut [f64]) {
        out[..rows].par_chunks_mut(ROWS_PER_TASK).enumerate().for_each(|(c, o)| {
            for (k, x) in o.iter_mut().enumerate() {
                *x = row_similarity(flat, d, q, den, c * ROWS_PER_TASK + k);
            }
        });
    }
}

/// One row's similarity, in the kernel's order (similarities.cu): the same subtractions, products and sums, each
/// rounded on its own, then a correctly rounded exp.
pub fn row_similarity(flat: &[f64], d: usize, q: &[f64], den: f64, i: usize) -> f64 {
    let mut d2 = 0.0;
    let mut n = 0.0;
    for c in 0..d {
        let (qc, g) = (q[c], flat[i * d + c]);
        if qc.is_nan() || g.is_nan() {
            continue;
        }
        let t = g - qc;
        d2 += t * t;
        n += 1.0;
    }
    if n != 0.0 {
        bf_core::jsmath::exp(-(d2 / n) / den)
    } else {
        1.0
    }
}

/// A device that may do some of a backend's work. `None`: it doesn't offer this operation (the processor does it);
/// `Some(Err)`: it failed (the processor takes over for good).
pub trait Accelerator: Send {
    fn name(&self) -> String;
    fn logits(&mut self, rows: &Csr, w: &[f64], out: &mut [f64]) -> Option<Result<(), String>>;
    fn similarities(&mut self, flat: &[f64], rows: usize, d: usize, q: &[f64], den: f64, out: &mut [f64]) -> Option<Result<(), String>>;
}

/// An accelerator checked against the processor (see the module notes).
pub struct CheckedBackend {
    accel: Mutex<Option<Box<dyn Accelerator>>>,
    name: String,
    cpu: CpuBackend,
    verify_every: u64,
    calls: AtomicU64,
    stopped: AtomicBool,
    events: Mutex<Vec<String>>,
    /// Calls the accelerator did / that were verified / that fell to the processor.
    pub on_device: AtomicU64,
    pub verified: AtomicU64,
}

fn agree(a: &[f64], c: &[f64]) -> Option<usize> {
    a.iter().zip(c).position(|(x, y)| !((x - y).abs() <= TOLERANCE * (1.0 + y.abs())))
}

impl CheckedBackend {
    /// Checks `accel` on test cases first: None (with the reason in `log`) when it disagrees or fails, so the caller
    /// uses the processor.
    pub fn new(mut accel: Box<dyn Accelerator>, verify_every: u64, log: &mut Vec<String>) -> Option<CheckedBackend> {
        let name = accel.name();
        let cpu = CpuBackend;
        for (k, (rows, w)) in logit_cases().iter().enumerate() {
            let mut a = vec![0.0; rows.rows()];
            let mut c = vec![0.0; rows.rows()];
            cpu.logits(rows, w, &mut c);
            match accel.logits(rows, w, &mut a) {
                None => break,
                Some(Err(e)) => {
                    log.push(format!("{name}: {e} during the check; the processor is used"));
                    return None;
                }
                Some(Ok(())) => {
                    if let Some(i) = agree(&a, &c) {
                        log.push(format!("{name}: check failed (logits case {k}, row {i}: {} vs {} on the processor); the processor is used", a[i], c[i]));
                        return None;
                    }
                }
            }
        }
        for (k, (flat, rows, d, q, den)) in similarity_cases().iter().enumerate() {
            let mut a = vec![0.0; *rows];
            let mut c = vec![0.0; *rows];
            cpu.similarities(flat, *rows, *d, q, *den, &mut c);
            match accel.similarities(flat, *rows, *d, q, *den, &mut a) {
                None => break,
                Some(Err(e)) => {
                    log.push(format!("{name}: {e} during the check; the processor is used"));
                    return None;
                }
                Some(Ok(())) => {
                    if let Some(i) = agree(&a, &c) {
                        log.push(format!("{name}: check failed (similarities case {k}, row {i}: {} vs {}); the processor is used", a[i], c[i]));
                        return None;
                    }
                }
            }
        }
        log.push(format!("{name}: check passed (tolerance {TOLERANCE:e} relative); verifying one call in {verify_every}"));
        Some(CheckedBackend {
            accel: Mutex::new(Some(accel)),
            name,
            cpu,
            verify_every: verify_every.max(1),
            calls: AtomicU64::new(0),
            stopped: AtomicBool::new(false),
            events: Mutex::new(vec![]),
            on_device: AtomicU64::new(0),
            verified: AtomicU64::new(0),
        })
    }

    /// True once the accelerator was dropped (a mismatch or an error).
    pub fn fell_back(&self) -> bool {
        self.stopped.load(Ordering::SeqCst)
    }

    fn stop(&self, why: String) {
        self.stopped.store(true, Ordering::SeqCst);
        *self.accel.lock() = None;
        self.events.lock().push(format!("{}: {why}; back to the processor for good (same results)", self.name));
    }

    /// Runs `op` on the accelerator when it offers it; verifies one call in N against `cpu`. False: the caller computes
    /// on the processor (out is then overwritten).
    fn run(&self, out: &mut [f64], op: &mut dyn FnMut(&mut dyn Accelerator, &mut [f64]) -> Option<Result<(), String>>, cpu: &dyn Fn(&mut [f64])) -> bool {
        if self.stopped.load(Ordering::SeqCst) {
            return false;
        }
        let n = self.calls.fetch_add(1, Ordering::SeqCst);
        let r = {
            let mut g = self.accel.lock();
            let Some(a) = g.as_mut() else { return false };
            op(a.as_mut(), out)
        };
        match r {
            None => false,
            Some(Err(e)) => {
                self.stop(e);
                false
            }
            Some(Ok(())) => {
                if n % self.verify_every == 0 {
                    let mut c = vec![0.0; out.len()];
                    cpu(&mut c);
                    self.verified.fetch_add(1, Ordering::Relaxed);
                    if let Some(i) = agree(out, &c) {
                        self.stop(format!("verification failed (row {i}: {} vs {})", out[i], c[i]));
                        out.copy_from_slice(&c);
                        return true;
                    }
                }
                self.on_device.fetch_add(1, Ordering::Relaxed);
                true
            }
        }
    }
}

impl Backend for CheckedBackend {
    fn name(&self) -> String {
        if self.fell_back() {
            format!("cpu (after {})", self.name)
        } else {
            self.name.clone()
        }
    }
    fn logits(&self, rows: &Csr, w: &[f64], out: &mut [f64]) {
        let n = rows.rows();
        let done = self.run(&mut out[..n], &mut |a, o| a.logits(rows, w, o), &|c| self.cpu.logits(rows, w, c));
        if !done {
            self.cpu.logits(rows, w, out);
        }
    }
    fn similarities(&self, flat: &[f64], rows: usize, d: usize, q: &[f64], den: f64, out: &mut [f64]) {
        let done = self.run(&mut out[..rows], &mut |a, o| a.similarities(flat, rows, d, q, den, o), &|c| self.cpu.similarities(flat, rows, d, q, den, c));
        if !done {
            self.cpu.similarities(flat, rows, d, q, den, out);
        }
    }
    fn take_events(&self) -> Vec<String> {
        std::mem::take(&mut *self.events.lock())
    }
}

/// Test rows for the logits check: training-sized rows, repeated indices, large and tiny values.
fn logit_cases() -> Vec<(Csr, Vec<f64>)> {
    let mut r = crate::features::Rng(0x5eed_0001);
    let mut out = vec![];
    for (rows, dim, nnz) in [(2000usize, 4096usize, 60usize), (500, 1 << 16, 150)] {
        let w: Vec<f64> = (0..dim).map(|_| r.normal() * if r.uniform() < 0.01 { 50.0 } else { 0.3 }).collect();
        let mut csr = Csr::new();
        for _ in 0..rows {
            let feats: Vec<(u32, f64)> = (0..nnz).map(|_| (r.below(dim) as u32, if r.uniform() < 0.8 { 1.0 } else { r.normal() * 3.0 })).collect();
            csr.push_row(&feats);
        }
        out.push((csr, w));
    }
    out
}

fn similarity_cases() -> Vec<(Vec<f64>, usize, usize, Vec<f64>, f64)> {
    let mut r = crate::features::Rng(0x5eed_0002);
    let mut out = vec![];
    for (d, nan, h) in [(8usize, false, 0.3), (8, true, 0.7), (12, true, 1.5)] {
        let rows = 3000;
        let flat: Vec<f64> = (0..rows * d).map(|_| if nan && r.uniform() < 0.05 { f64::NAN } else { r.normal() }).collect();
        let q: Vec<f64> = (0..d).map(|c| if nan && c % 4 == 1 { f64::NAN } else { r.normal() }).collect();
        out.push((flat, rows, d, q, 2.0 * h * h));
    }
    out
}

// ------------------------------------------------------------------------------------------------ CUDA

/// The NVIDIA GPU through bf-core's run-time-loaded driver interface: similarities only (see the module notes).
pub struct CudaAccelerator {
    gpu: bf_core::gpu::cuda::Gpu,
    name: String,
}

impl CudaAccelerator {
    pub fn open() -> Result<CudaAccelerator, String> {
        let gpu = bf_core::gpu::cuda::Gpu::open().map_err(|e| e.to_string())?;
        let name = format!("gpu: {}", gpu.device.name);
        Ok(CudaAccelerator { gpu, name })
    }
}

impl Accelerator for CudaAccelerator {
    fn name(&self) -> String {
        self.name.clone()
    }
    fn logits(&mut self, _rows: &Csr, _w: &[f64], _out: &mut [f64]) -> Option<Result<(), String>> {
        None
    }
    fn similarities(&mut self, flat: &[f64], rows: usize, d: usize, q: &[f64], den: f64, out: &mut [f64]) -> Option<Result<(), String>> {
        if d > bf_core::gpu::cuda::MAX_D {
            return None;
        }
        Some(match self.gpu.similarities(0, flat, rows, d, q, den, out) {
            Ok(marked) => {
                // The kernel leaves rows whose exp it can't round for sure to the processor.
                for i in marked {
                    out[i] = row_similarity(flat, d, q, den, i);
                }
                Ok(())
            }
            Err(e) => Err(e.to_string()),
        })
    }
}

/// The backend to use: the GPU when asked for and it passes its check, else the processor. `log` gets why.
pub fn select_backend(use_gpu: bool, verify_every: u64, log: &mut Vec<String>) -> std::sync::Arc<dyn Backend> {
    if use_gpu {
        match CudaAccelerator::open() {
            Ok(a) => {
                if let Some(b) = CheckedBackend::new(Box::new(a), verify_every, log) {
                    return std::sync::Arc::new(b);
                }
            }
            Err(e) => log.push(format!("GPU not used: {e}; the processor is used (same results)")),
        }
    }
    std::sync::Arc::new(CpuBackend)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cpu_logits_do_not_depend_on_threads() {
        let (rows, w) = logit_cases().remove(0);
        let mut a = vec![0.0; rows.rows()];
        let mut b = vec![0.0; rows.rows()];
        rayon::ThreadPoolBuilder::new().num_threads(1).build().unwrap().install(|| CpuBackend.logits(&rows, &w, &mut a));
        rayon::ThreadPoolBuilder::new().num_threads(4).build().unwrap().install(|| CpuBackend.logits(&rows, &w, &mut b));
        assert!(a.iter().zip(&b).all(|(x, y)| x.to_bits() == y.to_bits()));
    }

    #[test]
    fn no_gpu_here_means_the_processor() {
        let mut log = vec![];
        let b = select_backend(true, 10, &mut log);
        // This machine has no NVIDIA driver; on one that has, the check decides.
        if CudaAccelerator::open().is_err() {
            assert_eq!(b.name(), "cpu");
            assert!(log[0].contains("GPU not used"), "{log:?}");
        }
    }
}
