// The math fixtures of crates/core/tests/jsmath.rs: Math.exp / log / log1p / pow / hypot as 0.22.0 computed them,
// that is in Electron 44 (V8 15.2, whose exp/log/log1p/pow come from LLVM's libc). Run it with Electron as Node:
//
//   ELECTRON_RUN_AS_NODE=1 node_modules/electron/dist/electron parity/math-fixtures.mjs crates/core/tests/fixtures [rows]
//
// Inputs cover the ranges the model feeds these functions (similarities, logistic losses, weights), wide random
// ranges and the special values. Each file is little-endian doubles, a row per call.
import fs from 'node:fs';
import path from 'node:path';

const out = process.argv[2];
const ROWS = Number(process.argv[3] ?? 20000);
if (!process.versions.electron) console.warn('Not running in Electron: these are this V8\'s numbers, not 0.22.0\'s.');
console.log(`V8 ${process.versions.v8}${process.versions.electron ? `, Electron ${process.versions.electron}` : ''}`);

let s = 20261108;
const r = () => { s = (s + 0x6d2b79f5) | 0; let t = Math.imul(s ^ (s >>> 15), 1 | s); t = (t + Math.imul(t ^ (t >>> 7), 61 | t)) ^ t; return ((t ^ (t >>> 14)) >>> 0) / 2 ** 32; };
const bits = () => { const b = new Float64Array(1); new Uint32Array(b.buffer).set([(r() * 2 ** 32) >>> 0, (r() * 2 ** 32) >>> 0]); return b[0]; };
const SPECIAL = [0, -0, 1, -1, 0.5, 2, Infinity, -Infinity, NaN, Number.MIN_VALUE, -Number.MIN_VALUE, Number.MAX_VALUE, 2 ** -1022, 1e-300, 709.782712893384, 709.79, -745.1332191019411, -745.14, -708.4, 1e-320];

function write(name, rows) {
  const b = new Float64Array(rows.flat());
  fs.writeFileSync(path.join(out, name), Buffer.from(b.buffer));
  console.log(`${name}: ${rows.length} rows`);
}

// exp: similarities' -(d2/n)/den (0 down to the thousands), the logistic link, wide and random bit patterns.
const expIn = () => {
  const k = r();
  if (k < 0.3) return -r() * 30;
  if (k < 0.45) return -(r() ** 2) * 2000;
  if (k < 0.65) return (r() - 0.5) * 40;
  if (k < 0.8) return r() * 1455 - 745;
  if (k < 0.9) return (r() - 0.5) * 2 ** -(r() * 60);
  return bits();
};
// log: probabilities and their complements, counts, wide.
const logIn = () => {
  const k = r();
  if (k < 0.35) return r();
  if (k < 0.5) return 1 - r() * 1e-3;
  if (k < 0.7) return r() * 1e6;
  if (k < 0.9) return 2 ** ((r() - 0.5) * 2100);
  return Math.abs(bits());
};
// log1p: League points and mastery (integers), small and negative arguments, wide.
const log1pIn = () => {
  const k = r();
  if (k < 0.25) return Math.floor(r() * 3000);
  if (k < 0.4) return Math.floor(r() * 2e6);
  if (k < 0.65) return r() * 2 - 1;
  if (k < 0.8) return (r() - 0.5) * 2 ** -(r() * 60);
  if (k < 0.9) return 2 ** (r() * 1024);
  return bits();
};
// pow: weights (w ** strength, 0.5 ** (age / days), decay ** (age - 1)), timings, live gold, wide and special.
const powIn = () => {
  const k = r();
  if (k < 0.15) return [r() * 3, [0, 0.5, 2, 1][Math.floor(r() * 4)]];
  if (k < 0.3) return [0.5, r() * 20];
  if (k < 0.45) return [[0.5, 0.75, 0.9][Math.floor(r() * 3)], Math.floor(r() * 8) - 1];
  if (k < 0.55) return [r() * 50, 1.3];
  if (k < 0.75) return [r() * 10, (r() - 0.5) * 40];
  if (k < 0.85) return [-(r() * 10), Math.round((r() - 0.5) * 40)];
  if (k < 0.92) return [bits(), bits()];
  return [SPECIAL[Math.floor(r() * SPECIAL.length)], [0, 0.5, -0.5, 1, -1, 2, 3, -3, 0.25, Infinity, -Infinity, NaN, 1e300, -1e300][Math.floor(r() * 14)]];
};

const rows = (n, f) => Array.from({ length: n }, f);
write('exp.bin', [...SPECIAL.map((x) => [x, Math.exp(x)]), ...rows(ROWS, () => { const x = expIn(); return [x, Math.exp(x)]; })]);
write('log.bin', [...SPECIAL.map((x) => [x, Math.log(x)]), ...rows(ROWS, () => { const x = logIn(); return [x, Math.log(x)]; })]);
write('log1p.bin', [...SPECIAL.map((x) => [x, Math.log1p(x)]), ...rows(ROWS, () => { const x = log1pIn(); return [x, Math.log1p(x)]; })]);
write('pow.bin', rows(ROWS, () => { const [x, y] = powIn(); return [x, y, x ** y]; }));
write('hypot.bin', rows(ROWS, () => { const x = (r() - 0.5) * 2 ** ((r() - 0.5) * 40), y = (r() - 0.5) * 2 ** ((r() - 0.5) * 40); return [x, y, Math.hypot(x, y)]; }));
