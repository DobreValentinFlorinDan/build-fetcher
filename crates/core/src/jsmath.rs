//! `Math.exp`, `Math.log`, `Math.log1p`, `x ** y` and `Math.hypot` for the model. The same doubles on every system,
//! processor and graphics card, whatever C library or JavaScript engine is around:
//! - exp, log and log1p are correctly rounded: a double-double evaluation (exp: the GPU kernel's, similarities.cu; log
//!   and log1p: one Newton step on it) that returns only when every value within its error bound rounds the same,
//!   else 300-bit arithmetic (dashu-float). 0.22.0's Electron 44 (V8 15.2, LLVM libc) rounds them correctly too, so
//!   these are 0.22.0's doubles.
//! - pow is correctly rounded as well, built on those two. 0.22.0 took the C library's pow (V8's use_std_math_pow),
//!   which is within one unit in the last place but differs between Windows and Linux: here a result can differ from
//!   0.22.0's in its last digit (about 3 in 10,000 against glibc).
//! - hypot: V8's builtin (math.tq).
//!
//! Checked in `tests/jsmath.rs`: exp, log, log1p and hypot against Electron 44 (parity/math-fixtures.mjs), pow against
//! 400-bit arithmetic (parity/pow-fixtures.py).

#[inline]
fn hi(x: f64) -> u32 {
    (x.to_bits() >> 32) as u32
}
#[inline]
fn lo(x: f64) -> u32 {
    x.to_bits() as u32
}
#[inline]
fn from_words(hi: u32, lo: u32) -> f64 {
    f64::from_bits(((hi as u64) << 32) | lo as u64)
}
#[inline]
fn with_hi(x: f64, h: u32) -> f64 {
    from_words(h, lo(x))
}

const LN2_HI: f64 = 6.93147180369123816490e-01;
const LN2_LO: f64 = 1.90821492927058770002e-10;
const TWO54: f64 = 1.80143985094819840000e+16;
const LG1: f64 = 6.666666666666735130e-01;
const LG2: f64 = 3.999999999940941908e-01;
const LG3: f64 = 2.857142874366239149e-01;
const LG4: f64 = 2.222219843214978396e-01;
const LG5: f64 = 1.818357216161805012e-01;
const LG6: f64 = 1.531383769920937332e-01;
const LG7: f64 = 1.479819860511658591e-01;

/// fdlibm's log (Node 22's V8): within an ulp, the starting point of `log`.
fn log_fdlibm(x: f64) -> f64 {
    let mut x = x;
    let mut hx = hi(x) as i32;
    let lx = lo(x);
    let mut k: i32 = 0;
    if hx < 0x00100000 {
        if ((hx & 0x7fffffff) as u32 | lx) == 0 {
            return f64::NEG_INFINITY;
        }
        if hx < 0 {
            return f64::NAN;
        }
        k -= 54;
        x *= TWO54;
        hx = hi(x) as i32;
    }
    if hx >= 0x7ff00000 {
        return x + x;
    }
    k += (hx >> 20) - 1023;
    hx &= 0x000fffff;
    let i = (hx + 0x95f64) & 0x100000;
    x = with_hi(x, (hx | (i ^ 0x3ff00000)) as u32);
    k += i >> 20;
    let f = x - 1.0;
    if (0x000fffff & (2 + hx)) < 3 {
        if f == 0.0 {
            if k == 0 {
                return 0.0;
            }
            let dk = k as f64;
            return dk * LN2_HI + dk * LN2_LO;
        }
        let r = f * f * (0.5 - 0.33333333333333333 * f);
        if k == 0 {
            return f - r;
        }
        let dk = k as f64;
        return dk * LN2_HI - ((r - dk * LN2_LO) - f);
    }
    let s = f / (2.0 + f);
    let dk = k as f64;
    let z = s * s;
    let mut i = hx - 0x6147a;
    let w = z * z;
    let j = 0x6b851 - hx;
    let t1 = w * (LG2 + w * (LG4 + w * LG6));
    let t2 = z * (LG1 + w * (LG3 + w * (LG5 + w * LG7)));
    i |= j;
    let r = t2 + t1;
    if i > 0 {
        let hfsq = 0.5 * f * f;
        if k == 0 {
            f - (hfsq - s * (hfsq + r))
        } else {
            dk * LN2_HI - ((hfsq - (s * (hfsq + r) + dk * LN2_LO)) - f)
        }
    } else if k == 0 {
        f - s * (f - r)
    } else {
        dk * LN2_HI - ((s * (f - r) - dk * LN2_LO) - f)
    }
}

/// `Math.hypot(...values)` (V8: the largest magnitude scales a Kahan sum of squares).
pub fn hypot(values: &[f64]) -> f64 {
    if values.is_empty() {
        return 0.0;
    }
    let mut abs = Vec::with_capacity(values.len());
    let mut nan = false;
    let mut max: f64 = 0.0;
    for &v in values {
        if v.is_nan() {
            nan = true;
            abs.push(0.0);
        } else {
            let a = v.abs();
            abs.push(a);
            if a > max {
                max = a;
            }
        }
    }
    if max == f64::INFINITY {
        return f64::INFINITY;
    }
    if nan {
        return f64::NAN;
    }
    if max == 0.0 {
        return 0.0;
    }
    let mut sum = 0.0;
    let mut comp = 0.0;
    for a in abs {
        let n = a / max;
        let summand = n * n - comp;
        let pre = sum + summand;
        comp = (pre - sum) - summand;
        sum = pre;
    }
    sum.sqrt() * max
}

pub fn hypot2(a: f64, b: f64) -> f64 {
    hypot(&[a, b])
}

/// `Math.imul`.
pub fn imul(a: i32, b: i32) -> i32 {
    a.wrapping_mul(b)
}

// ------------------------------------------------------------------------------------------- exact arithmetic

/// a + b = s + e exactly (Knuth's TwoSum, any magnitudes).
#[inline]
fn two_sum(a: f64, b: f64) -> (f64, f64) {
    let s = a + b;
    let bb = s - a;
    (s, (a - (s - bb)) + (b - bb))
}

/// a + b = s + e exactly when |a| >= |b| (LLVM's exact_add<FAST2SUM>: hi, then b - (hi - a)).
#[inline]
fn fast_two_sum(a: f64, b: f64) -> (f64, f64) {
    let hi = a + b;
    let t = hi - a;
    (hi, b - t)
}

/// Veltkamp's split at bit `n` (LLVM's fputil::split<double, n>).
#[inline]
fn split(a: f64, n: u32) -> (f64, f64) {
    let c = ((1u64 << n) + 1) as f64;
    let t1 = c * a;
    let t2 = a - t1;
    let hi = t1 + t2;
    (hi, a - hi)
}

/// a * b = hi + lo exactly (Dekker's product without FMA: LLVM's exact_mult<double, split_b>(a, b), a split at 27).
#[inline]
fn exact_mult(a: f64, b: f64, split_b: u32) -> (f64, f64) {
    let (ah, al) = split(a, 27);
    let (bh, bl) = split(b, split_b);
    let hi = a * b;
    let t1 = ah * bh - hi;
    let t2 = ah * bl + t1;
    let t3 = al * bh + t2;
    (hi, al * bl + t3)
}

// ------------------------------------------------------------------------------------------- correctly rounded

use crate::jsmath_tables::{TH, TL};

const INV_L: f64 = 184.6649652337873; // 0x1.71547652b82fep+7: 128 / ln 2
const L1: f64 = 0.005415212348452769; // 0x1.62e42ff000000p-8: L1 + L2 + L3 = ln 2 / 128
const L2: f64 = -3.281964900695705e-13; // -0x1.718432a200000p-42
const L3: f64 = 1.636073772867244e-23; // 0x1.3c7673007e5edp-76
const ERR1: f64 = 8.470329472543003e-22; // 2^-70: exp_core's bound, relative
const ERR2: f64 = 3.1554436208840472e-30; // 2^-98: exp_core2's bound, relative

/// 2^k for -1022 <= k <= 1023.
#[inline]
fn pow2i(k: i32) -> f64 {
    f64::from_bits(((k + 1023) as u64) << 52)
}

/// x = n ln2/128 + rh + rl, |rh| <= ln2/256, the remainder to about 2^-110, for |x| < 746: x - n L1 and n L2 are
/// exact (the GPU kernel's reduction); the sum's error is kept (TwoSum).
#[inline]
fn reduce(x: f64) -> (i32, f64, f64) {
    let n = (x * INV_L).round_ties_even();
    let a = x - n * L1;
    let b = n * L2;
    let rh = a - b;
    let bb = rh - a;
    let e = (a - (rh - bb)) + (-b - bb);
    (n as i32, rh, e - n * L3)
}

/// a * b = hi + lo exactly: with FMA the processor's fused multiply-add, else Dekker's product. Both are exact for the
/// operands exp_core gives them (no overflow, the low part far above the subnormals), so the same two doubles.
#[inline(always)]
fn product<const FMA: bool>(a: f64, b: f64) -> (f64, f64) {
    if FMA {
        let hi = a * b;
        (hi, a.mul_add(b, -hi))
    } else {
        exact_mult(a, b, 27)
    }
}

/// 2^(n/128) exp(rh + rl) as (hi, lo, k): (hi + lo) 2^k within 2^-70 hi, for |rh| <= ln2/256. The GPU kernel's crExp
/// (similarities.cu), with Dekker's products for its two fused multiply-adds (both exact, so the same doubles).
#[inline(always)]
fn exp_core_with<const FMA: bool>(ni: i32, rh: f64, rl: f64) -> (f64, f64, i32) {
    let (s, sl) = product::<FMA>(rh, rh);
    let cross = rh * rl;
    let mut q = 1.0 / 40320.0 + rh * (1.0 / 362880.0);
    q = 1.0 / 5040.0 + rh * q;
    q = 1.0 / 720.0 + rh * q;
    q = 1.0 / 120.0 + rh * q;
    q = 1.0 / 24.0 + rh * q;
    q = 1.0 / 6.0 + rh * q;
    q = (s * rh) * q;
    let h1 = 1.0 + rh;
    let l1 = rh - (h1 - 1.0);
    let half = s * 0.5;
    let h2 = h1 + half;
    let z = h2 - h1;
    let l2 = (h1 - (h2 - z)) + (half - z);
    let mut lo = sl * 0.5 + cross;
    lo += rl;
    lo += l1 + l2;
    lo += q;
    let j = (ni & 127) as usize;
    let k = (ni - j as i32) / 128;
    let (th, tl) = (TH[j], TL[j]);
    let (ph, pl) = product::<FMA>(th, h2);
    (ph, pl + (th * lo + tl * h2), k)
}

fn exp_core(ni: i32, rh: f64, rl: f64) -> (f64, f64, i32) {
    exp_core_with::<false>(ni, rh, rl)
}

#[inline]
fn dd_mul(a: (f64, f64), b: (f64, f64)) -> (f64, f64) {
    let (ph, pl) = exact_mult(a.0, b.0, 27);
    fast_two_sum(ph, pl + (a.0 * b.1 + a.1 * b.0))
}

#[inline]
fn dd_add(a: (f64, f64), b: (f64, f64)) -> (f64, f64) {
    let (s, e) = two_sum(a.0, b.0);
    fast_two_sum(s, e + (a.1 + b.1))
}

/// 1/k! for k = 0..11 as double-double (hi, lo).
const INV_FACT: [(f64, f64); 12] = [
    (1.0, 0.0),
    (1.0, 0.0),
    (0.5, 0.0),
    (0.16666666666666666, 9.25185853854297e-18),
    (0.041666666666666664, 2.3129646346357427e-18),
    (0.008333333333333333, 1.1564823173178714e-19),
    (0.001388888888888889, -5.300543954373577e-20),
    (0.0001984126984126984, 1.7209558293420705e-22),
    (2.48015873015873e-05, 2.1511947866775882e-23),
    (2.7557319223985893e-06, -1.858393274046472e-22),
    (2.755731922398589e-07, 2.3767714622250297e-23),
    (2.505210838544172e-08, -1.448814070935912e-24),
];

/// The same as exp_core to about 2^-102: the series in double-double (the r^12 term is below 2^-130), times the table's
/// 106-bit 2^(j/128). Only for the rare values exp_core can't round for sure.
fn exp_core2(ni: i32, rh: f64, rl: f64) -> (f64, f64, i32) {
    let r = fast_two_sum(rh, rl);
    let mut p = INV_FACT[11];
    for c in INV_FACT[..11].iter().rev() {
        p = dd_add(dd_mul(p, r), *c);
    }
    let j = (ni & 127) as usize;
    let k = (ni - j as i32) / 128;
    let (hi, lo) = dd_mul((TH[j], TL[j]), p);
    (hi, lo, k)
}

/// hi + lo rounded, when every value within `err` of it rounds the same.
#[inline]
fn rounded(hi: f64, lo: f64, err: f64) -> Option<f64> {
    let a = hi + (lo - err);
    let b = hi + (lo + err);
    (a == b).then_some(a)
}

/// (hi + lo) 2^k rounded (subnormal results included), when every value within err 2^k of it rounds the same.
fn finish(hi: f64, lo: f64, err: f64, k: i32) -> Option<f64> {
    if k > 1023 {
        // exp just below the overflow threshold: k = 1024, hi < 1.
        return rounded(hi, lo, err).map(|v| v * pow2i(1023) * pow2i(k - 1023));
    }
    if k > -1022 {
        return rounded(hi, lo, err).map(|v| v * pow2i(k)); // exact: a normal result
    }
    // Subnormal: in units of 2^-1022, round w = (hi + lo) 2^(k+1022) < 2 to a multiple of 2^-52, exactly: 1 + wh rounds
    // there, and what it leaves (rem, exact up to wl's rounding) says whether that was right.
    let s = pow2i(k + 1022);
    let (wh, wl) = two_sum(hi * s, lo * s);
    let u = 1.0 + wh;
    let base = u - 1.0;
    let rem = (wh - base) + wl;
    let es = err * s;
    const HALF: f64 = 1.1102230246251565e-16; // 2^-53
    const ULP: f64 = 2.220446049250313e-16; // 2^-52
    let tiny = pow2i(-1022);
    if rem.abs() + es < HALF {
        Some(base * tiny)
    } else if rem - es > HALF {
        Some((base + ULP) * tiny)
    } else if rem + es < -HALF {
        Some((base - ULP) * tiny)
    } else {
        None
    }
}

/// Slow path: f in 300-bit arithmetic, rounded to the nearest double (subnormals included). Reached about once in
/// 2^45 calls: when both double-double stages are unsure.
mod slow {
    use dashu_base::BitTest;
    use dashu_float::round::mode::HalfEven;
    use dashu_float::FBig;
    type F = FBig<HalfEven, 2>;
    const PREC: usize = 300;

    fn big(x: f64) -> F {
        F::try_from(x).expect("finite").with_precision(PREC).value()
    }

    /// The nearest double to v (ties to even), by integer arithmetic on its significand.
    fn nearest(v: &F) -> f64 {
        let repr = v.repr();
        let m = repr.significand();
        if m.is_zero() {
            return 0.0;
        }
        let neg = m < &dashu_int::IBig::ZERO;
        let mag: dashu_int::UBig = dashu_int::ops::UnsignedAbs::unsigned_abs(m.clone());
        let bits = mag.bit_len() as isize;
        let e = repr.exponent(); // value = mag * 2^e
        let top = e + bits - 1;
        if top > 1023 {
            return if neg { f64::NEG_INFINITY } else { f64::INFINITY };
        }
        let keep_lsb = (top - 52).max(-1074);
        let shift = keep_lsb - e;
        let (mut q, round_up) = if shift <= 0 {
            (mag << ((-shift) as usize), false)
        } else {
            let s = shift as usize;
            let q = &mag >> s;
            let rem = &mag - (&q << s);
            let half = dashu_int::UBig::ONE << (s - 1);
            let up = rem > half || (rem == half && q.bit(0));
            (q, up)
        };
        if round_up {
            q += dashu_int::UBig::ONE;
        }
        let qf: f64 = q.to_f64().value(); // at most 54 bits: exact
        let p2 = |k: isize| f64::from_bits(((k + 1023) as u64) << 52); // 2^k, -1022 <= k <= 1023
        let r = qf * p2(keep_lsb.max(-1022)) * p2((keep_lsb + 1022).min(0));
        if neg { -r } else { r }
    }

    pub fn exp(x: f64) -> f64 {
        if x.is_nan() {
            return x;
        }
        if x == f64::INFINITY {
            return x;
        }
        if x == f64::NEG_INFINITY {
            return 0.0;
        }
        nearest(&big(x).exp())
    }
    pub fn log(x: f64) -> f64 {
        if x.is_nan() || x < 0.0 {
            return f64::NAN;
        }
        if x == 0.0 {
            return f64::NEG_INFINITY;
        }
        if x == f64::INFINITY {
            return x;
        }
        if x == 1.0 {
            return 0.0;
        }
        nearest(&big(x).ln())
    }
    /// x^y for a positive finite x other than 1 and a finite y. A result within 2^-200 of a 55-bit number is that
    /// number (an exact power or an exact tie: x^y can't come that close otherwise), so ties round to even.
    pub fn pow(x: f64, y: f64) -> f64 {
        let v = (big(y) * big(x).ln()).exp();
        let snapped = v.clone().with_precision(55).value().with_precision(PREC).value();
        let d = &v - &snapped;
        let tiny = v.clone() >> 200isize;
        let near = if d < F::ZERO { -d < tiny } else { d < tiny };
        nearest(if near { &snapped } else { &v })
    }
    pub fn log1p(x: f64) -> f64 {
        if x.is_nan() || x < -1.0 {
            return f64::NAN;
        }
        if x == -1.0 {
            return f64::NEG_INFINITY;
        }
        if x == f64::INFINITY {
            return x;
        }
        nearest(&(big(x) + big(1.0)).ln())
    }
}

#[inline(always)]
fn exp_with<const FMA: bool>(x: f64) -> f64 {
    if x.is_nan() {
        return x;
    }
    if x > 709.782712893384 {
        return f64::INFINITY; // above 0x1.62e42fefa39efp+9 the result rounds to infinity
    }
    if x < -745.1332191019412 {
        return 0.0; // below -0x1.74910d52d3052p+9 it rounds to zero
    }
    if x.abs() < 1.1102230246251565e-16 {
        return 1.0 + x; // |x| < 2^-53
    }
    let (n, rh, rl) = reduce(x);
    let (hi, lo, k) = exp_core_with::<FMA>(n, rh, rl);
    if let Some(y) = finish(hi, lo, hi * ERR1, k) {
        return y;
    }
    let (hi, lo, k) = exp_core2(n, rh, rl);
    if let Some(y) = finish(hi, lo, hi * ERR2, k) {
        return y;
    }
    slow::exp(x)
}

/// exp_with on a processor with FMA: the two exact products take one instruction each.
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "fma")]
unsafe fn exp_fma(x: f64) -> f64 {
    exp_with::<true>(x)
}

/// `Math.exp`: correctly rounded (Electron 44's, LLVM libc). The same doubles with or without FMA: the result is
/// only returned when it is the one correct rounding.
pub fn exp(x: f64) -> f64 {
    #[cfg(target_arch = "x86_64")]
    if std::is_x86_feature_detected!("fma") {
        // SAFETY: the processor has FMA.
        return unsafe { exp_fma(x) };
    }
    exp_with::<false>(x)
}

/// exp without FMA, whatever the processor has (the tests compare the two).
#[doc(hidden)]
pub fn exp_plain_for_tests(x: f64) -> f64 {
    exp_with::<false>(x)
}

/// log(x) as (hi, lo) and an absolute error bound, for a positive finite x: fdlibm's log of x's mantissa, one Newton
/// step on exp (stage 1: exp_core, within 2^-69; stage 2: exp_core2, within 2^-96), and e ln2 to about 2^-112.
fn log_dd(x: f64, stage2: bool) -> (f64, f64, f64) {
    // A subnormal x: times 2^64 (exact), its exponent 64 less.
    let (x, sub) = if x < f64::MIN_POSITIVE { (x * 1.8446744073709552e19, 64) } else { (x, 0) };
    let bits = x.to_bits();
    let mut e = ((bits >> 52) & 0x7ff) as i64 - 1023 - sub;
    let mut m = f64::from_bits((bits & 0x000f_ffff_ffff_ffff) | 0x3ff0_0000_0000_0000);
    if m > std::f64::consts::SQRT_2 {
        m *= 0.5;
        e += 1;
    }
    let y0 = log_fdlibm(m);
    let (n, rh, rl) = reduce(y0);
    let (eh, el, ke) = if stage2 { exp_core2(n, rh, rl) } else { exp_core(n, rh, rl) };
    let (eh, el) = (eh * pow2i(ke), el * pow2i(ke)); // ke is 0 or -1 here: exact
    // log(m) = y0 + log(m / e^y0); m - eh is exact (Sterbenz).
    let d = (m - eh) - el;
    let c = if stage2 {
        let t = d / eh;
        t - 0.5 * t * t
    } else {
        d / m
    };
    let k = (128 * e) as f64; // e ln2 = 128 e (L1 + L2 + L3): the first two products are exact
    let (s1, e1) = two_sum(k * L1, y0);
    let (s2, e2) = two_sum(s1, k * L2);
    let lo = ((e1 + e2) + k * L3) + c;
    let err = if stage2 { 1.3e-29 + s2.abs() * 8e-31 } else { 1.7e-21 + s2.abs() * 1e-30 };
    (s2, lo, err)
}

/// `Math.log`: correctly rounded (Electron 44's, LLVM libc).
pub fn log(x: f64) -> f64 {
    if x.is_nan() || x < 0.0 {
        return f64::NAN;
    }
    if x == 0.0 {
        return f64::NEG_INFINITY;
    }
    if x == f64::INFINITY {
        return x;
    }
    if x == 1.0 {
        return 0.0;
    }
    for stage2 in [false, true] {
        let (hi, lo, err) = log_dd(x, stage2);
        if let Some(y) = rounded(hi, lo, err) {
            return y;
        }
    }
    slow::log(x)
}

/// (-1)^(k+1) / k for k = 1..13 as double-double (hi, lo): log1p(x) = x sum c_k x^(k-1).
const LOG1P_COEFFS: [(f64, f64); 13] = [
    (1.0, 0.0),
    (-0.5, 0.0),
    (0.3333333333333333, 1.850371707708594e-17),
    (-0.25, 0.0),
    (0.2, -1.1102230246251566e-17),
    (-0.16666666666666666, -9.25185853854297e-18),
    (0.14285714285714285, 7.93016446160826e-18),
    (-0.125, 0.0),
    (0.1111111111111111, 6.1679056923619804e-18),
    (-0.1, 5.551115123125783e-18),
    (0.09090909090909091, -2.523234146875356e-18),
    (-0.08333333333333333, -4.625929269271485e-18),
    (0.07692307692307693, -4.270088556250602e-18),
];

/// `Math.log1p`: correctly rounded (Electron 44's, LLVM libc).
pub fn log1p(x: f64) -> f64 {
    if x.is_nan() || x < -1.0 {
        return f64::NAN;
    }
    if x == -1.0 {
        return f64::NEG_INFINITY;
    }
    if x == f64::INFINITY {
        return x;
    }
    if x.abs() < 1.1102230246251565e-16 {
        return x; // |x| < 2^-53: x - x^2/2 + ... rounds to x (and keeps -0)
    }
    if x.abs() < 0.0009765625 {
        // |x| < 2^-10: the series in double-double (its x^14 term is below 2^-140 |x|), within 2^-100 of the result.
        let mut p = LOG1P_COEFFS[12];
        let xd = (x, 0.0);
        for c in LOG1P_COEFFS[..12].iter().rev() {
            p = dd_add(dd_mul(p, xd), *c);
        }
        let (hi, lo) = dd_mul(p, xd);
        if let Some(y) = rounded(hi, lo, hi.abs() * ERR2) {
            return y;
        }
        return slow::log1p(x);
    }
    // 1 + x = s + t exactly; log(s + t) = log(s) + t/s - (t/s)^2/2 ..., the square below 2^-106.
    let (s, t) = two_sum(1.0, x);
    for stage2 in [false, true] {
        let (hi, lo, err) = log_dd(s, stage2);
        if let Some(y) = rounded(hi, lo + t / s, err) {
            return y;
        }
    }
    slow::log1p(x)
}

/// `x ** y` / `Math.pow(x, y)`: JavaScript's special cases, then the correctly rounded power, the same on every
/// system (0.22.0 took the C library's pow, which differs between Windows and Linux in the last digit of some results).
/// exp(y log x) in double-double, returned only when every value within the error bound rounds the same; a second,
/// more accurate stage; then 300-bit arithmetic (where exact results and exact ties are recognised).
pub fn pow(x: f64, y: f64) -> f64 {
    if y.is_nan() {
        return f64::NAN;
    }
    if y == 0.0 {
        return 1.0;
    }
    if x.is_nan() {
        return f64::NAN;
    }
    if y.is_infinite() {
        let ax = x.abs();
        if ax == 1.0 {
            return f64::NAN; // JavaScript's rule (C gives 1)
        }
        return if (ax > 1.0) == (y > 0.0) { f64::INFINITY } else { 0.0 };
    }
    if y == 2.0 {
        return x * x;
    }
    if y == 0.5 {
        return if x.is_infinite() { f64::INFINITY } else { (x + 0.0).sqrt() };
    }
    if y == 1.0 {
        return x;
    }
    let integer = y == y.trunc();
    let odd = integer && y.abs() < 9007199254740992.0 && (y as i64) & 1 == 1;
    let sign = if odd && x.is_sign_negative() { -1.0 } else { 1.0 };
    if x == 0.0 {
        return if y < 0.0 { sign * f64::INFINITY } else { sign * 0.0 };
    }
    if x.is_infinite() {
        return if y > 0.0 { sign * f64::INFINITY } else { sign * 0.0 };
    }
    if x < 0.0 && !integer {
        return f64::NAN;
    }
    let ax = x.abs();
    if ax == 1.0 {
        return sign;
    }
    if y.abs() >= 18446744073709551616.0 {
        // |log ax| >= 2^-53, so |y log ax| >= 2^11: over the top or under the bottom.
        return if (ax > 1.0) == (y > 0.0) { f64::INFINITY } else { 0.0 };
    }
    for stage2 in [false, true] {
        let (lh, ll, lerr) = log_dd(ax, stage2);
        let (ph, pl) = exact_mult(y, lh, 27);
        let pl = pl + y * ll;
        if ph > 709.79 {
            return sign * f64::INFINITY;
        }
        if ph < -745.14 {
            return sign * 0.0;
        }
        // y log ax = ph + pl within perr: the logarithm's bound times |y|, and the product's rounding.
        let perr = y.abs() * lerr + ph.abs() * 1e-30;
        let (n, rh, rl) = reduce(ph);
        let rl = rl + pl;
        let (hi, lo, k) = if stage2 { exp_core2(n, rh, rl) } else { exp_core(n, rh, rl) };
        let rel = if stage2 { 2.0 * ERR2 } else { 2.0 * ERR1 } + 1.01 * perr;
        if let Some(v) = finish(hi, lo, hi * rel, k) {
            return sign * v;
        }
    }
    sign * slow::pow(ax, y)
}

#[doc(hidden)]
pub fn slow_exp_for_tests(x: f64) -> f64 {
    slow::exp(x)
}
#[doc(hidden)]
pub fn slow_log_for_tests(x: f64) -> f64 {
    slow::log(x)
}
#[doc(hidden)]
pub fn slow_pow_for_tests(x: f64, y: f64) -> f64 {
    slow::pow(x, y)
}
#[doc(hidden)]
pub fn slow_log1p_for_tests(x: f64) -> f64 {
    slow::log1p(x)
}
