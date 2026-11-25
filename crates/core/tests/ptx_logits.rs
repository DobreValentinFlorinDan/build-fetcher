//! The hand-written forward-pass kernels (src/gpu/logits.ptx), checked without a GPU: a small interpreter runs the
//! PTX itself, thread by thread, for exactly the instructions the file uses, and the results are compared bit for bit
//! with the processor's sum. That proves the kernels' logic (addresses, loop bounds, the row limit, the scatter);
//! that the NVIDIA driver accepts the file, and the card's speed, can only be checked on a machine with a GPU. Any
//! instruction the interpreter doesn't know fails the test, so the file can't drift away from what is checked here.

use bf_core::gpu::cuda::LOGITS_PTX;
use std::collections::HashMap;

/// One kernel: its parameters (name, is_u64) and its body, as (optional predicate, opcode, operands) or labels.
struct Kernel {
    params: Vec<(String, bool)>,
    body: Vec<Line>,
    labels: HashMap<String, usize>,
}

enum Line {
    Label,
    Op { pred: Option<String>, op: String, args: Vec<String> },
}

fn parse(ptx: &str, name: &str) -> Kernel {
    let start = ptx.find(&format!(".visible .entry {name}(")).unwrap_or_else(|| panic!("no entry {name}"));
    let text = &ptx[start..];
    let open = text.find('(').unwrap();
    let close = text.find(')').unwrap();
    let params = text[open + 1..close]
        .split(',')
        .map(|p| {
            let w: Vec<&str> = p.split_whitespace().collect();
            (w[2].to_string(), w[1] == ".u64")
        })
        .collect();
    let body_start = text.find('{').unwrap();
    let body_end = text.find("\n}").unwrap();
    let mut body = vec![];
    let mut labels = HashMap::new();
    for raw in text[body_start + 1..body_end].lines() {
        let line = raw.split("//").next().unwrap().trim();
        if line.is_empty() || line.starts_with(".reg") {
            continue;
        }
        if let Some(l) = line.strip_suffix(':') {
            labels.insert(l.to_string(), body.len());
            body.push(Line::Label);
            continue;
        }
        let line = line.trim_end_matches(';');
        let (pred, rest) = match line.strip_prefix('@') {
            Some(r) => {
                let (p, rest) = r.split_once(char::is_whitespace).unwrap();
                (Some(p.to_string()), rest.trim())
            }
            None => (None, line),
        };
        let (op, args) = rest.split_once(char::is_whitespace).unwrap_or((rest, ""));
        let args = args.split(',').map(|a| a.trim().to_string()).filter(|a| !a.is_empty()).collect();
        body.push(Line::Op { pred, op: op.to_string(), args });
    }
    Kernel { params, body, labels }
}

/// Global memory: buffers at made-up addresses in one byte array.
struct Memory {
    base: u64,
    bytes: Vec<u8>,
}

impl Memory {
    fn new() -> Memory {
        Memory { base: 0x10_0000, bytes: vec![] }
    }
    fn alloc(&mut self, data: &[u8]) -> u64 {
        while self.bytes.len() % 16 != 0 {
            self.bytes.push(0);
        }
        let at = self.base + self.bytes.len() as u64;
        self.bytes.extend_from_slice(data);
        self.bytes.extend_from_slice(&[0xAB; 64]); // guard: reading past a buffer gives garbage, not a crash
        at
    }
    fn at(&self, addr: u64, n: usize) -> &[u8] {
        let o = (addr - self.base) as usize;
        assert!(o + n <= self.bytes.len(), "out of bounds read at {addr:#x}");
        &self.bytes[o..o + n]
    }
    fn write(&mut self, addr: u64, data: &[u8]) {
        let o = (addr - self.base) as usize;
        assert!(o + data.len() <= self.bytes.len(), "out of bounds write at {addr:#x}");
        self.bytes[o..o + data.len()].copy_from_slice(data);
    }
}

fn run_thread(k: &Kernel, params: &[u64], mem: &mut Memory, ctaid: u32, ntid: u32, tid: u32) {
    let mut reg: HashMap<String, u64> = HashMap::new();
    let pv: HashMap<&str, u64> = k.params.iter().zip(params).map(|((n, _), v)| (n.as_str(), *v)).collect();
    let val = |reg: &HashMap<String, u64>, a: &str| -> u64 {
        if a.starts_with('%') {
            *reg.get(a).unwrap_or_else(|| panic!("register {a} read before it was written"))
        } else {
            a.parse().unwrap_or_else(|_| panic!("not a number: {a}"))
        }
    };
    let addr = |reg: &HashMap<String, u64>, a: &str| -> u64 {
        let inner = a.trim_start_matches('[').trim_end_matches(']');
        match inner.split_once('+') {
            Some((r, off)) => val(reg, r) + off.parse::<u64>().unwrap(),
            None => val(reg, inner),
        }
    };
    let mut pc = 0;
    let mut steps = 0;
    while pc < k.body.len() {
        steps += 1;
        assert!(steps < 1_000_000, "runaway loop");
        let Line::Op { pred, op, args } = &k.body[pc] else {
            pc += 1;
            continue;
        };
        pc += 1;
        if let Some(p) = pred {
            if reg.get(p).copied().unwrap_or(0) == 0 {
                continue;
            }
        }
        let a = |i: usize| args[i].as_str();
        let f = |x: u64| f64::from_bits(x);
        match op.as_str() {
            "ld.param.u64" | "ld.param.u32" => {
                let name = a(1).trim_start_matches('[').trim_end_matches(']');
                reg.insert(a(0).into(), pv[name]);
            }
            "mov.u32" => {
                let v = match a(1) {
                    "%ctaid.x" => ctaid,
                    "%ntid.x" => ntid,
                    "%tid.x" => tid,
                    x => panic!("mov.u32 {x}"),
                };
                reg.insert(a(0).into(), v as u64);
            }
            "mov.f64" => {
                let hex = a(1).strip_prefix("0d").expect("f64 immediate");
                reg.insert(a(0).into(), u64::from_str_radix(hex, 16).unwrap());
            }
            "mad.lo.s32" => {
                let v = (val(&reg, a(1)) as u32).wrapping_mul(val(&reg, a(2)) as u32).wrapping_add(val(&reg, a(3)) as u32);
                reg.insert(a(0).into(), v as u64);
            }
            "setp.ge.u32" => {
                let r = (val(&reg, a(1)) as u32) >= (val(&reg, a(2)) as u32);
                reg.insert(a(0).into(), r as u64);
            }
            "setp.ge.u64" => {
                let r = val(&reg, a(1)) >= val(&reg, a(2));
                reg.insert(a(0).into(), r as u64);
            }
            "setp.lt.u64" => {
                let r = val(&reg, a(1)) < val(&reg, a(2));
                reg.insert(a(0).into(), r as u64);
            }
            "bra" => pc = *k.labels.get(a(0)).unwrap_or_else(|| panic!("no label {}", a(0))),
            "cvta.to.global.u64" => {
                let v = val(&reg, a(1));
                reg.insert(a(0).into(), v);
            }
            "mul.wide.u32" => {
                let v = (val(&reg, a(1)) as u32 as u64) * (val(&reg, a(2)) as u32 as u64);
                reg.insert(a(0).into(), v);
            }
            "add.s64" => {
                let v = val(&reg, a(1)).wrapping_add(val(&reg, a(2)));
                reg.insert(a(0).into(), v);
            }
            "shl.b64" => {
                let v = val(&reg, a(1)) << val(&reg, a(2));
                reg.insert(a(0).into(), v);
            }
            "ld.global.u64" | "ld.global.f64" => {
                let at = addr(&reg, a(1));
                let v = u64::from_le_bytes(mem.at(at, 8).try_into().unwrap());
                reg.insert(a(0).into(), v);
            }
            "ld.global.u32" => {
                let at = addr(&reg, a(1));
                let v = u32::from_le_bytes(mem.at(at, 4).try_into().unwrap());
                reg.insert(a(0).into(), v as u64);
            }
            // Round to nearest, each on its own: Rust's f64 * and + (never fused).
            "mul.rn.f64" => {
                let v = f(val(&reg, a(1))) * f(val(&reg, a(2)));
                reg.insert(a(0).into(), v.to_bits());
            }
            "add.rn.f64" => {
                let v = f(val(&reg, a(1))) + f(val(&reg, a(2)));
                reg.insert(a(0).into(), v.to_bits());
            }
            "st.global.f64" => {
                let at = addr(&reg, a(0));
                let v = val(&reg, a(1));
                mem.write(at, &v.to_le_bytes());
            }
            "ret" => return,
            other => panic!("the interpreter does not know `{other}`: extend it before using that instruction"),
        }
    }
}

fn launch(k: &Kernel, params: &[u64], mem: &mut Memory, n: usize) {
    const BLOCK: u32 = 256;
    let blocks = n.div_ceil(BLOCK as usize).max(1) as u32;
    for b in 0..blocks {
        for t in 0..BLOCK {
            run_thread(k, params, mem, b, BLOCK, t);
        }
    }
}

fn bytes_u64(v: &[u64]) -> Vec<u8> {
    v.iter().flat_map(|x| x.to_le_bytes()).collect()
}
fn bytes_u32(v: &[u32]) -> Vec<u8> {
    v.iter().flat_map(|x| x.to_le_bytes()).collect()
}
fn bytes_f64(v: &[f64]) -> Vec<u8> {
    v.iter().flat_map(|x| x.to_le_bytes()).collect()
}

/// xorshift, for reproducible cases.
struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
    fn f(&mut self) -> f64 {
        (self.next() >> 11) as f64 / (1u64 << 53) as f64 * 6.0 - 3.0
    }
}

#[test]
fn no_fused_or_unrounded_float_arithmetic() {
    for line in LOGITS_PTX.lines().map(|l| l.split("//").next().unwrap()) {
        assert!(!line.contains("fma") && !line.contains("mad.f64") && !line.contains("mad.rn"), "{line}");
        if line.contains(".f64") && (line.contains("mul.") || line.contains("add.")) {
            assert!(line.contains(".rn."), "float arithmetic without an explicit rounding mode may be fused: {line}");
        }
    }
}

#[test]
fn logits_kernel_matches_the_processor_bit_for_bit() {
    let k = parse(LOGITS_PTX, "logits_csr");
    assert_eq!(k.params.len(), 6);
    let mut r = Rng(0x5eed_1234_5678_9abc);
    // Row counts around the block size (256), including an empty batch row and odd values.
    for (rows, dim, max_nnz) in [(1usize, 16usize, 4usize), (255, 1000, 40), (257, 5000, 96), (600, 2000, 120)] {
        let w: Vec<f64> = (0..dim)
            .map(|i| match i % 97 {
                0 => -0.0,
                1 => 1e-300,
                2 => 1e300,
                _ => r.f(),
            })
            .collect();
        let mut ptr = vec![0u64];
        let (mut idx, mut val) = (vec![], vec![]);
        for row in 0..rows {
            let n = if row % 50 == 7 { 0 } else { r.below(max_nnz as u64 + 1) as usize };
            for _ in 0..n {
                idx.push(r.below(dim as u64) as u32);
                val.push(if r.below(5) == 0 { r.f() * 10.0 } else { 1.0 });
            }
            ptr.push(idx.len() as u64);
        }
        // The processor's sum (Csr::row_dot's order).
        let want: Vec<f64> = (0..rows)
            .map(|row| {
                let mut s = 0.0;
                for j in ptr[row] as usize..ptr[row + 1] as usize {
                    s += w[idx[j] as usize] * val[j];
                }
                s
            })
            .collect();
        let mut mem = Memory::new();
        let p_ptr = mem.alloc(&bytes_u64(&ptr));
        let p_idx = mem.alloc(&bytes_u32(&idx));
        let p_val = mem.alloc(&bytes_f64(&val));
        let p_w = mem.alloc(&bytes_f64(&w));
        let sentinel = vec![f64::from_bits(0x7ff8_dead_beef_0000); rows + 300];
        let p_out = mem.alloc(&bytes_f64(&sentinel));
        launch(&k, &[p_ptr, p_idx, p_val, p_w, p_out, rows as u64], &mut mem, rows);
        let got: Vec<f64> = mem.at(p_out, (rows + 300) * 8).chunks(8).map(|c| f64::from_le_bytes(c.try_into().unwrap())).collect();
        for row in 0..rows {
            assert_eq!(got[row].to_bits(), want[row].to_bits(), "rows {rows}, row {row}: {} vs {}", got[row], want[row]);
        }
        // Threads past the last row write nothing.
        for (i, g) in got.iter().enumerate().skip(rows) {
            assert_eq!(g.to_bits(), sentinel[0].to_bits(), "thread {i} wrote past the rows");
        }
    }
}

#[test]
fn scatter_writes_only_the_given_weights() {
    let k = parse(LOGITS_PTX, "scatter");
    assert_eq!(k.params.len(), 4);
    let dim = 3000;
    let before: Vec<f64> = (0..dim).map(|i| i as f64 * 0.5).collect();
    let idx: Vec<u32> = (0..300).map(|i| (i * 7 + 3) as u32 % dim as u32).collect();
    let vals: Vec<f64> = (0..300).map(|i| -(i as f64) - 0.25).collect();
    let mut mem = Memory::new();
    let p_idx = mem.alloc(&bytes_u32(&idx));
    let p_val = mem.alloc(&bytes_f64(&vals));
    let p_w = mem.alloc(&bytes_f64(&before));
    launch(&k, &[p_idx, p_val, p_w, idx.len() as u64], &mut mem, idx.len());
    let after: Vec<f64> = mem.at(p_w, dim * 8).chunks(8).map(|c| f64::from_le_bytes(c.try_into().unwrap())).collect();
    let mut want = before.clone();
    for (j, v) in idx.iter().zip(&vals) {
        want[*j as usize] = *v;
    }
    assert_eq!(after, want);
}
