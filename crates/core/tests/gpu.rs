//! Port of server/gpu/gpu.test.ts: Use GPU, what CI can check without an NVIDIA GPU. The kernel itself (0.22.0's PTX,
//! unchanged) was checked against the processor's numbers in 0.13.0, and the app checks it on the GPU before each
//! evaluation and one pass in 500 while it runs.

use irelia::gpu::accelerate::{set_similarity_accelerator, start_gpu};
use irelia::gpu::log::read_gpu_log;
use irelia::model::context::Contexts;
use irelia::model::draft::{flat_similarities, row_similarity, similarities};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

fn z_draft() -> Contexts {
    let rows: Vec<Vec<f64>> = (0..300).map(|i| (0..6).map(|c| if (i + c) % 11 == 0 { f64::NAN } else { (i as f64 * 0.7 + c as f64).sin() }).collect()).collect();
    Contexts::from_rows(&rows, None)
}
const ZQ: [f64; 6] = [0.3, f64::NAN, -0.2, 1.1, 0.0, 0.5];

fn bits(v: &[f64]) -> Vec<u64> {
    v.iter().map(|x| x.to_bits()).collect()
}

#[test]
fn gpu() {
    let dir = tempfile::tempdir().unwrap();
    std::env::set_var("IRELIA_CONFIG_DIR", dir.path());
    std::env::set_var("IRELIA_CUDA_LIB", dir.path().join("no-such-driver.so"));

    // ---- without an NVIDIA driver, no session, the reason in the log, and the processor as before
    assert!(start_gpu("thread 1", None).is_none());
    let log = read_gpu_log(400);
    assert!(log.contains("thread 1: No NVIDIA driver: ") && log.contains("could not be loaded") && log.contains("Evaluate builds runs on the processor."), "{log}");
    let z = z_draft();
    let before = similarities(&z, &ZQ, 0.7);
    assert!(before.iter().all(|v| *v > 0.0 && *v <= 1.0));

    // ---- similarities takes the accelerator's numbers, or works them out itself when it declines
    let calls = Arc::new(AtomicUsize::new(0));
    let c2 = calls.clone();
    set_similarity_accelerator(Some(Arc::new(move |stored: &Contexts, q: &[f64], den: f64, out: &mut [f64]| {
        let n = c2.fetch_add(1, Ordering::SeqCst) + 1;
        assert_eq!(stored.rows, 300);
        assert_eq!(q.len(), 6);
        assert_eq!(den, 2.0 * 0.7 * 0.7);
        for (i, o) in out.iter_mut().enumerate().take(stored.rows) {
            *o = row_similarity(&stored.flat, stored.d, q, den, i);
        }
        n == 1
    })));
    let first = similarities(&z, &ZQ, 0.7);
    let second = similarities(&z, &ZQ, 0.7);
    // No bandwidth (h = ∞): every game alike, never the accelerator.
    let _ = similarities(&z, &ZQ, f64::INFINITY);
    set_similarity_accelerator(None);
    assert_eq!(bits(&first), bits(&before), "the accelerator's (game by game, the same numbers)");
    assert_eq!(bits(&second), bits(&before), "declined: the processor's");
    assert_eq!(calls.load(Ordering::SeqCst), 2);

    // ---- one game at a time gives what the loops give, with and without unknown dimensions
    for nan in [false, true] {
        let (d, rows) = (5usize, 200usize);
        let flat: Vec<f64> = (0..rows * d).map(|i| if nan && i % 13 == 0 { f64::NAN } else { (i as f64 * 1.3).cos() }).collect();
        for q in [[0.1, 0.2, -0.3, 0.4, 0.0], [0.1, f64::NAN, -0.3, f64::NAN, 0.0], [f64::NAN; 5]] {
            let mut out = vec![0.0; rows];
            flat_similarities(&flat, nan, rows, d, &q, 0.5, &mut out);
            for (i, o) in out.iter().enumerate() {
                assert_eq!(row_similarity(&flat, d, &q, 0.5, i).to_bits(), o.to_bits());
            }
        }
    }
}
