// Parity harness: lane builds and draft recommendations must be identical (numbers within 1e-9 relative).
//   node parity/compare-drafts.mjs <ts.json> <rs.json>
import fs from 'node:fs';
const [a, b] = process.argv.slice(2).map((f) => JSON.parse(fs.readFileSync(f, 'utf8')));
const diffs = [];
let numbers = 0, exact = 0;
function walk(x, y, at) {
  if (diffs.length > 40) return;
  if (typeof x === 'number' && typeof y === 'number') {
    numbers++;
    if (Object.is(x, y)) { exact++; return; }
    const d = Math.abs(x - y), rel = d / Math.max(Math.abs(x), Math.abs(y));
    if (d > 1e-12 && rel > 1e-9) diffs.push(`${at}: ${x} vs ${y}`);
    return;
  }
  if (x === null || y === null || typeof x !== 'object' || typeof y !== 'object') { if (x !== y) diffs.push(`${at}: ${JSON.stringify(x)} vs ${JSON.stringify(y)}`); return; }
  if (Array.isArray(x)) {
    if (!Array.isArray(y) || x.length !== y.length) { diffs.push(`${at}: length ${x.length} vs ${y?.length}`); return; }
    x.forEach((v, i) => walk(v, y[i], `${at}[${i}]`));
    return;
  }
  const kx = Object.keys(x), ky = Object.keys(y);
  if (kx.join() !== ky.join()) diffs.push(`${at}: keys ${kx.join()} vs ${ky.join()}`);
  for (const k of new Set([...kx, ...ky])) walk(x[k], y[k], `${at}.${k}`);
}
if (a.results.length !== b.results.length) diffs.push(`drafts: ${a.results.length} vs ${b.results.length}`);
a.results.forEach((r, i) => walk(r, b.results[i], `draft ${i}`));
console.log(`${a.results.length} drafts, ${numbers} numbers: ${exact} bit for bit; ${diffs.length} differences`);
for (const d of diffs) console.log(`  ${d}`);
process.exit(diffs.length ? 1 : 0);
