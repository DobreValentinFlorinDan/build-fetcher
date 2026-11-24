// Parity harness, step 3: compares two evaluation outputs (parity/ts-eval.ts and the Rust parity_eval). Every verdict
// and every string must be identical; every number within 1e-9 relative (absolute 1e-12 near zero). Time stamps and
// timings are left out. Exit code 1 on any difference.
//
//   node parity/compare.mjs <ts.json> <rs.json>
import fs from 'node:fs';

const [a, b] = process.argv.slice(2).map((f) => JSON.parse(fs.readFileSync(f, 'utf8')));
const REL = 1e-9, ABS = 1e-12;
const IGNORE = new Set(['evaluatedAt', 'ms', 'timings', 'peakRss']);
const diffs = [];
let numbers = 0, exact = 0, worst = 0;
function walk(x, y, at) {
  if (diffs.length > 50) return;
  if (typeof x === 'number' && typeof y === 'number') {
    numbers++;
    if (Object.is(x, y)) { exact++; return; }
    const d = Math.abs(x - y), rel = d / Math.max(Math.abs(x), Math.abs(y));
    if (d > ABS && rel > REL) diffs.push(`${at}: ${x} vs ${y} (rel ${rel.toExponential(2)})`);
    else worst = Math.max(worst, Number.isFinite(rel) ? rel : 0);
    return;
  }
  if (x === null || y === null || typeof x !== 'object' || typeof y !== 'object') {
    if (x !== y) diffs.push(`${at}: ${JSON.stringify(x)?.slice(0, 200)} vs ${JSON.stringify(y)?.slice(0, 200)}`);
    return;
  }
  if (Array.isArray(x) !== Array.isArray(y)) { diffs.push(`${at}: array vs object`); return; }
  if (Array.isArray(x)) {
    if (x.length !== y.length) diffs.push(`${at}: length ${x.length} vs ${y.length}`);
    for (let i = 0; i < Math.min(x.length, y.length); i++) walk(x[i], y[i], `${at}[${i}]`);
    return;
  }
  const keys = new Set([...Object.keys(x), ...Object.keys(y)]);
  for (const k of keys) {
    if (IGNORE.has(k)) continue;
    if (!(k in x)) { diffs.push(`${at}.${k}: missing in the first`); continue; }
    if (!(k in y)) { diffs.push(`${at}.${k}: missing in the second`); continue; }
    walk(x[k], y[k], `${at}.${k}`);
  }
}
walk(a.report, b.report, 'report');
// The text report, but its first line (the time stamp).
const ta = (a.text ?? '').split('\n').slice(1), tb = (b.text ?? '').split('\n').slice(1);
for (let i = 0; i < Math.max(ta.length, tb.length); i++) if (ta[i] !== tb[i]) { diffs.push(`text line ${i + 2}:\n  ${ta[i]}\n  ${tb[i]}`); if (diffs.length > 60) break; }
if (a.known && b.known) walk(a.known, b.known, 'known');
// The key order of the JSON report, as each wrote it.
const order = (o, at = 'report') => (o && typeof o === 'object' ? (Array.isArray(o) ? o.slice(0, 1).flatMap((v, i) => order(v, `${at}[${i}]`)) : [`${at}{${Object.keys(o).filter((k) => !IGNORE.has(k)).join(',')}}`, ...Object.entries(o).flatMap(([k, v]) => order(v, `${at}.${k}`))]) : []);
const oa = order(a.report), ob = order(b.report);
for (let i = 0; i < Math.max(oa.length, ob.length); i++) if (oa[i] !== ob[i]) { diffs.push(`key order: ${oa[i]} vs ${ob[i]}`); break; }
console.log(`${numbers} numbers compared: ${exact} bit for bit, the rest within ${worst.toExponential(2)} relative; ${diffs.length} differences`);
if (a.ms && b.ms) console.log(`time: ${(a.ms / 1000).toFixed(1)} s vs ${(b.ms / 1000).toFixed(1)} s; peak RSS ${(a.peakRss / 2 ** 20).toFixed(0)} MB vs ${(b.peakRss / 2 ** 20).toFixed(0)} MB`);
for (const d of diffs.slice(0, 60)) console.log(`  ${d}`);
process.exit(diffs.length ? 1 : 0);
