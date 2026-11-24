//! Timings of the similarities kernel and exp, not a test of results (ignored by default):
//!   cargo test --release -p irelia --test bench_kernel -- --ignored --nocapture
use irelia::jsmath;
use irelia::model::draft::flat_similarities;
use std::time::Instant;

fn best(mut f: impl FnMut()) -> f64 {
    (0..15).map(|_| { let t = Instant::now(); f(); t.elapsed().as_secs_f64() }).fold(f64::INFINITY, f64::min)
}

#[test]
#[ignore]
fn kernel_timings() {
    let (rows, d) = (20_000usize, 24usize);
    let mut s: u32 = 9;
    let mut r = move || { s = s.wrapping_mul(1664525).wrapping_add(1013904223); (s >> 8) as f64 / 16_777_216.0 };
    let flat: Vec<f64> = (0..rows * d).map(|_| r() * 4.0 - 2.0).collect();
    let q: Vec<f64> = (0..d).map(|_| r() * 4.0 - 2.0).collect();
    let mut out = vec![0.0; rows];
    let mut plain = vec![0.0; rows];
    let old = best(|| {
        for i in 0..rows {
            let mut d2 = 0.0;
            for j in 0..d { let t = flat[i * d + j] - q[j]; d2 += t * t; }
            plain[i] = jsmath::exp_plain_for_tests(-(d2 / d as f64) / 2.0);
        }
    });
    let new = best(|| flat_similarities(&flat, false, rows, d, &q, 2.0, &mut out));
    assert!(out.iter().zip(&plain).all(|(a, b)| a.to_bits() == b.to_bits()));
    let xs: Vec<f64> = (0..rows).map(|_| -r() * 30.0).collect();
    let e_old = best(|| { for (o, x) in plain.iter_mut().zip(&xs) { *o = jsmath::exp_plain_for_tests(*x); } });
    let e_new = best(|| { for (o, x) in out.iter_mut().zip(&xs) { *o = jsmath::exp(*x); } });
    assert!(out.iter().zip(&plain).all(|(a, b)| a.to_bits() == b.to_bits()));
    let ns = |t: f64| t * 1e9 / rows as f64;
    eprintln!("similarities, {d} dimensions: {:.1} ns a row before, {:.1} ns now", ns(old), ns(new));
    eprintln!("exp: {:.1} ns before, {:.1} ns now", ns(e_old), ns(e_new));
}
