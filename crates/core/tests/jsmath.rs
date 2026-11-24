//! exp / log / log1p / hypot to the last bit as Electron 44 (0.22.0's runtime) computes them: the fixtures are
//! Electron's own results, made by parity/math-fixtures.mjs. pow is the app's own, correctly rounded: its fixture's
//! results come from 400-bit arithmetic (parity/pow-fixtures.py). A larger set, when present at $IRELIA_MATH_FIXTURES
//! (the same scripts with more rows), is checked too.
use irelia::jsmath;

fn floats(path: &std::path::Path) -> Vec<f64> {
    let b = std::fs::read(path).unwrap();
    b.chunks(8).map(|c| f64::from_le_bytes(c.try_into().unwrap())).collect()
}

fn same(a: f64, b: f64) -> bool {
    a.to_bits() == b.to_bits() || (a.is_nan() && b.is_nan())
}

fn dirs() -> Vec<std::path::PathBuf> {
    let mut d = vec![std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures")];
    if let Ok(more) = std::env::var("IRELIA_MATH_FIXTURES") {
        d.push(more.into());
    }
    d
}

fn check1(name: &str, f: fn(f64) -> f64) {
    let mut bad = 0;
    let mut n = 0;
    for dir in dirs() {
        for r in floats(&dir.join(name)).chunks(2) {
            n += 1;
            let got = f(r[0]);
            if name == "exp.bin" {
                assert!(same(jsmath::exp_plain_for_tests(r[0]), got), "exp with and without FMA differ at {:e}", r[0]);
            }
            if !same(got, r[1]) {
                bad += 1;
                if bad < 8 {
                    eprintln!("{name} {:e} ({:#x}): {:e} ({:#x}), Electron {:e} ({:#x})", r[0], r[0].to_bits(), got, got.to_bits(), r[1], r[1].to_bits());
                }
            }
        }
    }
    eprintln!("{name}: {n} checked");
    assert_eq!(bad, 0, "{name}: {bad} of {n} differ");
}

#[test]
fn exp_matches_electron() {
    check1("exp.bin", jsmath::exp);
}

#[test]
fn log_matches_electron() {
    check1("log.bin", jsmath::log);
}

#[test]
fn log1p_matches_electron() {
    check1("log1p.bin", jsmath::log1p);
}

/// pow against the correctly rounded power (the same on Windows and Linux), hypot against Electron.
#[test]
fn pow_is_correctly_rounded_and_hypot_matches_electron() {
    let mut bad = 0;
    for dir in dirs() {
        let pow_file = dir.join("pow.bin");
        for r in floats(&pow_file).chunks(3) {
            let got = jsmath::pow(r[0], r[1]);
            if !same(got, r[2]) {
                bad += 1;
                if bad < 8 {
                    eprintln!("{:e} ** {:e} = {:e} ({:#x}), exact {:e} ({:#x})", r[0], r[1], got, got.to_bits(), r[2], r[2].to_bits());
                }
            }
        }
        for r in floats(&dir.join("hypot.bin")).chunks(3) {
            if !same(jsmath::hypot2(r[0], r[1]), r[2]) {
                bad += 1;
            }
        }
    }
    assert_eq!(bad, 0);
}

#[test]
fn the_slow_path_rounds_as_the_fast_one() {
    // Where the double-double paths are sure of the rounding, the 300-bit path must agree with them.
    let mut s: u32 = 7;
    let mut r = move || {
        s = s.wrapping_add(0x6d2b_79f5);
        let mut t = (s ^ (s >> 15)).wrapping_mul(1 | s);
        t = t.wrapping_add((t ^ (t >> 7)).wrapping_mul(61 | t)) ^ t;
        (t ^ (t >> 14)) as f64 / 4_294_967_296.0
    };
    for _ in 0..12 {
        let x = (r() - 0.5) * 1400.0;
        assert!(same(jsmath::exp(x), jsmath::slow_exp_for_tests(x)), "exp {x:e}");
        let y = jsmath::exp((r() - 0.5) * 1400.0);
        assert!(same(jsmath::log(y), jsmath::slow_log_for_tests(y)), "log {y:e}");
        let z = r() * 10.0 - 0.99; // > -1: the 300-bit path is only reached for valid arguments
        assert!(same(jsmath::log1p(z), jsmath::slow_log1p_for_tests(z)), "log1p {z:e}");
    }
    for _ in 0..12 {
        let (x, y) = (r() * 10.0 + 1e-3, (r() - 0.5) * 40.0);
        assert!(same(jsmath::pow(x, y), jsmath::slow_pow_for_tests(x, y)), "pow {x:e} {y:e}");
    }
    // Subnormal results, and the boundaries.
    for x in [-708.5, -720.0, -744.0, -745.13, -745.1332191019411, 709.78, 709.782712893384, -708.3964185322641, -708.39641853226] {
        assert!(same(jsmath::exp(x), jsmath::slow_exp_for_tests(x)), "exp {x:e}");
    }
}

/// Exact results and exact ties (a 54-bit odd integer lies halfway between two doubles: it rounds to the even one).
#[test]
fn pow_exact_results_and_ties() {
    let exact = |x: f64, y: f64, want: u128| {
        let nearest = want as f64; // an integer to f64 conversion rounds to nearest, ties to even
        assert!(same(jsmath::pow(x, y), nearest), "{x} ** {y}: {:e}, exact {want}", jsmath::pow(x, y));
        assert!(same(jsmath::pow(-x, y), if y as i64 % 2 == 1 { -nearest } else { nearest }), "-{x} ** {y}");
    };
    for m in [208_065u128, 208_067, 262_143, 220_001, 250_001, 3, 5, 7, 99_999] {
        exact(m as f64, 3.0, m * m * m); // 54-bit odd cubes from 208_065 up: ties
        exact(m as f64, 4.0, m * m * m * m);
    }
    for m in [208_065u128, 208_067, 262_143, 12_345, 9] {
        let sq = (m * m) as f64; // exact: below 2^53
        assert!(same(jsmath::pow(sq, 1.5), (m * m * m) as f64), "{sq} ** 1.5");
    }
    assert!(same(jsmath::pow(2.0, 10.0), 1024.0));
    assert!(same(jsmath::pow(2.0, -1074.0), 5e-324));
    assert!(same(jsmath::pow(2.0, -1075.0), 0.0)); // a tie between 0 and the smallest double: to even
    assert!(same(jsmath::pow(2.0, 1023.0), 8.98846567431158e307));
    assert!(same(jsmath::pow(2.0, 1024.0), f64::INFINITY));
    assert!(same(jsmath::pow(10.0, 22.0), 1e22));
    assert!(same(jsmath::pow(10.0, -5.0), 1e-5));
    assert!(same(jsmath::pow(0.5, 3.0), 0.125));
    assert!(same(jsmath::pow(-8.0, 1.0 / 3.0), f64::NAN));
    assert!(same(jsmath::pow(-0.0, -3.0), f64::NEG_INFINITY));
    assert!(same(jsmath::pow(f64::NEG_INFINITY, -3.0), -0.0));
    assert!(same(jsmath::pow(f64::NAN, 0.0), 1.0));
    assert!(same(jsmath::pow(1.0, f64::INFINITY), f64::NAN)); // JavaScript's rule
    assert!(same(jsmath::pow(f64::MAX, -0.5), 7.458340731200207e-155));
}
