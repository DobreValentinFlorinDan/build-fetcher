// Use GPU (0.13.0, Settings → GPU): an evaluation thread's similarities (model/draft.ts) on the graphics card
// (cuda.ts), with the same numbers. Before a thread uses it, the GPU works out test drafts and the processor checks
// every number; while it runs, the processor checks one pass in VERIFY_EVERY. Any difference, error or lost GPU sends
// the thread back to the processor for the rest of the evaluation, and the log (log.ts) says what happened.
import { flatSimilarities, rowSimilarity, setSimilarityAccelerator, type FlatDrafts, type SimilarityAccelerator } from '../model/draft.ts';
import { firstSumsDifference, groupedOut, groupedSums, setSumsAccelerator, type GroupedArrays, type SumsAccelerator } from '../model/estimate.ts';
import { Gpu, MAX_D } from './cuda.ts';
import { gpuLog } from './log.ts';

/** Fewer games than this stay on the processor: copying them there and back costs more than it saves. */
const MIN_ROWS = Number(process.env.IRELIA_GPU_MIN_ROWS ?? 4096);   // the override: tests on small databases
/** Fewer observations in a lane than this stay on the processor (the weighted sums, 0.19.0). */
const MIN_OBSERVATIONS = Number(process.env.IRELIA_GPU_MIN_OBSERVATIONS ?? 20000);
/**
 * The weighted sums on the GPU (0.19.0): off unless IRELIA_GPU_SUMS=1. To stay identical to the last digit, each option's
 * games are added in order by one GPU thread: at 120,000 observations whose largest option holds 25,600, about 9 ms a
 * pass (an estimate) against 1.3 ms on one processor thread (measured), and every evaluation thread waits its turn on
 * the card. On a real laptop card a full evaluation crawled with them on. Kept for a kernel that beats the processor.
 */
const SUMS_ON = process.env.IRELIA_GPU_SUMS === '1';
/** One pass in this many is worked out on the processor too, and compared. */
const VERIFY_EVERY = 500;

const message = (err: unknown) => (err instanceof Error ? err.message : String(err));
const hex = (x: number) => {
  const v = new DataView(new ArrayBuffer(8));
  v.setFloat64(0, x);
  return `${x} (0x${v.getBigUint64(0).toString(16).padStart(16, '0')})`;
};
/** The first game whose numbers differ (bit for bit), or -1. */
function firstDifference(a: Float64Array, b: Float64Array, rows: number) {
  for (let i = 0; i < rows; i++) if (!Object.is(a[i], b[i])) return i;
  return -1;
}

/** Test drafts for the check: draft-sized contexts, unknown dimensions, far-off games and every kind of bandwidth. */
function testCases(): { stored: FlatDrafts; rows: number; d: number; q: Float64Array; den: number }[] {
  let seed = 0x9e3779b9;
  const random = () => {
    seed = (seed + 0x6d2b79f5) | 0;
    let t = Math.imul(seed ^ (seed >>> 15), 1 | seed);
    t = (t + Math.imul(t ^ (t >>> 7), 61 | t)) ^ t;
    return ((t ^ (t >>> 14)) >>> 0) / 2 ** 32;
  };
  const normal = () => Math.sqrt(-2 * Math.log(1 - random())) * Math.cos(2 * Math.PI * random());
  const cases = [];
  for (const [d, nan, h] of [[12, false, 0.7], [12, true, 0.3], [19, true, 1.5], [12, false, 0.05], [19, false, 6], [4, true, 0.12]] as const) {
    const rows = 6000;
    const flat = new Float64Array(rows * d);
    for (let i = 0; i < flat.length; i++) {
      const far = random() < 0.01 ? 40 : 1;
      flat[i] = nan && random() < 0.03 ? NaN : normal() * far;
    }
    const q = Float64Array.from({ length: d }, (_, c) => (nan && c % 5 === 1 ? NaN : normal()));
    cases.push({ stored: { flat, anyNaN: flat.some(Number.isNaN) }, rows, d, q, den: 2 * h * h });
  }
  return cases;
}

/** The GPU against the processor on the test drafts: true when every number is the same. */
function selfCheck(gpu: Gpu, prefix: string): boolean {
  let numbers = 0, back = 0;
  for (const c of testCases()) {
    const out = new Float64Array(c.rows);
    const marked = gpu.similarities(c.stored.flat, c.rows, c.d, c.q, c.den, out);
    for (let x = 0; x < marked.length; x++) out[marked[x]] = rowSimilarity(c.stored.flat, c.d, c.q, c.den, marked[x]);
    const cpu = flatSimilarities(c.stored, c.rows, c.d, c.q, c.den, new Float64Array(c.rows));
    const bad = firstDifference(cpu, out, c.rows);
    if (bad >= 0) {
      gpuLog(`${prefix}check failed: a test game's similarity is ${hex(out[bad])} on the GPU and ${hex(cpu[bad])} on the processor `
        + `(Math.exp of V8 ${process.versions.v8}). The GPU isn't used.`);
      return false;
    }
    numbers += c.rows; back += marked.length;
  }
  gpuLog(`${prefix}check passed: ${numbers.toLocaleString('en-US')} test similarities the same to the last digit (${back} worked out on the processor).`);
  return true;
}

/** A lane for the sums check: options with many, few and no games, zero and negative weights, NaN and zero similarities. */
function sumsCase(): { g: GroupedArrays; sims: Float64Array; opponent: number } {
  let seed = 0x1234abcd;
  const random = () => {
    seed = (seed + 0x6d2b79f5) | 0;
    let t = Math.imul(seed ^ (seed >>> 15), 1 | seed);
    t = (t + Math.imul(t ^ (t >>> 7), 61 | t)) ^ t;
    return ((t ^ (t >>> 14)) >>> 0) / 2 ** 32;
  };
  const K = 211, rows = 5000;
  const start = new Int32Array(K + 1);
  for (let k = 0; k < K; k++) start[k + 1] = start[k] + (k % 17 === 0 ? 0 : Math.floor(random() * random() * 400));
  const N = start[K];
  const gw = new Float64Array(N), gwpa = new Float64Array(N), gwin = new Float64Array(N), gopp = new Int32Array(N), grow = new Int32Array(N);
  for (let i = 0; i < N; i++) {
    const r = random();
    gw[i] = r < 0.03 ? 0 : r < 0.05 ? -0.5 : random() * 3 + 1e-9 * random();
    gwpa[i] = random() * 20 - 3; gwin[i] = random() < 0.5 ? 1 : 0;
    gopp[i] = random() < 0.1 ? -1 : Math.floor(random() * 6);
    grow[i] = Math.floor(random() * rows);
  }
  const sims = new Float64Array(rows);
  for (let i = 0; i < rows; i++) { const r = random(); sims[i] = r < 0.05 ? 0 : r < 0.07 ? NaN : r < 0.08 ? -1 : random() ** 3; }
  return { g: { start, gw, gwpa, gwin, gopp, grow }, sims, opponent: 3 };
}

/** The GPU's sums against the processor's on the test lane: null when every number is the same, else the first difference. */
function sumsCheck(gpu: Gpu): string | null {
  const { g, sims, opponent } = sumsCase();
  const K = g.start.length - 1;
  const gp = groupedOut(K), cpu = groupedOut(K);
  if (!gpu.sums(g, sims, opponent, gp)) return 'the GPU left the test lane to the processor';
  groupedSums(g, sims, opponent, cpu);
  return firstSumsDifference(gp, cpu);
}

/** Per thread, in a GpuSession's `stats`: passes, games, games back to the processor, ms, checks, busy passes, stopped. */
const STATS = 12;
const MAX_THREADS = 64;
export interface GpuSession {
  /** Where the evaluation's other threads count what they did (their startGpu `stats`). */
  stats: SharedArrayBuffer;
  close(): void;
}

/**
 * This thread's similarities on the GPU until close(), or null (no GPU, or its check failed: the processor, as without
 * Use GPU). `busy`: champ select or a game, when the processor takes over so the game has the card. The job thread
 * (rank 0) checks the GPU and, on close, logs every thread's counts and frees what all of them put on the card; the
 * other threads (`rank` ≥ 1, `stats` from its session) skip the check and count into `stats`.
 */
export async function startGpu(o: { thread: string; rank?: number; stats?: SharedArrayBuffer; busy?: () => boolean }): Promise<GpuSession | null> {
  const { thread } = o;
  const rank = o.rank ?? 0;
  let gpu: Gpu;
  try { gpu = await Gpu.open(); } catch (err) {
    gpuLog(`${thread}: ${message(err)}. Evaluate builds runs on the processor.`);
    return null;
  }
  const dev = gpu.device;
  let sumsOff = !gpu.hasSums || !SUMS_ON;
  if (rank === 0) {
    try {
      gpuLog(`${thread}: ${dev.name} (compute ${dev.capability}, ${dev.memoryMb.toLocaleString('en-US')} MB, ${dev.freeMb.toLocaleString('en-US')} MB free; driver ${dev.driver}).`);
      if (!selfCheck(gpu, `${thread}: `)) { gpu.close(); return null; }
      if (!SUMS_ON) gpuLog(`${thread}: the weighted sums stay on the processor (faster there; IRELIA_GPU_SUMS=1 tries the GPU's).`);
      else if (gpu.hasSums) {
        const bad = sumsCheck(gpu);
        if (bad) { sumsOff = true; gpuLog(`${thread}: sums check failed: ${bad}. The weighted sums stay on the processor.`); }
        else gpuLog(`${thread}: sums check passed: the weighted sums of a test lane are the same to the last digit.`);
      } else gpuLog(`${thread}: the sums kernel isn't there (${gpu.sumsReason}). The weighted sums stay on the processor.`);
    } catch (err) {
      gpuLog(`${thread}: ${message(err)} during the check. The GPU isn't used.`);
      gpu.close();
      return null;
    }
  }

  const shared = o.stats ?? new SharedArrayBuffer(8 * STATS * MAX_THREADS);
  const c = new Float64Array(shared, 8 * STATS * Math.min(rank, MAX_THREADS - 1), STATS);
  let scratch = new Float64Array(0);
  const accelerate: SimilarityAccelerator = (stored, n, d, q, den, out) => {
    if (c[6] || n < MIN_ROWS || d > MAX_D) return false;
    if (o.busy?.()) { c[5]++; return false; }
    const t0 = performance.now();
    let marked: ArrayLike<number>;
    try { marked = gpu.similarities(stored.flat, n, d, q, den, out); } catch (err) {
      c[6] = 1;
      gpuLog(`${thread}: ${message(err)}. Back to the processor for the rest of this evaluation (same results).`);
      return false;
    }
    for (let x = 0; x < marked.length; x++) out[marked[x]] = rowSimilarity(stored.flat, d, q, den, marked[x]);
    c[3] += performance.now() - t0;
    c[0]++; c[1] += n; c[2] += marked.length;
    if (c[0] % VERIFY_EVERY === 1) {
      if (scratch.length < n) scratch = new Float64Array(n);
      const cpu = flatSimilarities(stored, n, d, q, den, scratch.subarray(0, n));
      const bad = firstDifference(cpu, out, n);
      if (bad >= 0) {
        c[6] = 1;
        gpuLog(`${thread}: check ${c[4] + 1} failed: a game's similarity is ${hex(out[bad])} on the GPU and ${hex(cpu[bad])} on the processor. `
          + `Back to the processor. The ${c[0] - 1} passes before it ran on the GPU and may differ: run Evaluate builds again with Use GPU off to be sure.`);
        out.set(cpu);
      } else c[4]++;
    }
    return true;
  };
  setSimilarityAccelerator(accelerate);
  // The weighted sums (estimate.ts querySums). Thread 0's slot 11 stops them in every thread.
  const root = new Float64Array(shared, 0, STATS);
  if (rank === 0 && sumsOff) root[11] = 1;
  if (SUMS_ON && (!sumsOff || rank !== 0)) {
    const sums: SumsAccelerator = {
      run(g, sims, opponent, out) {
        if (root[11] || !gpu.hasSums || g.gw.length < MIN_OBSERVATIONS) return false;
        if (o.busy?.()) { c[5]++; return false; }
        const t0 = performance.now();
        try {
          if (!gpu.sums(g, sims, opponent, out)) return false;
        } catch (err) {
          root[11] = 1;
          gpuLog(`${thread}: sums: ${message(err)}. The weighted sums are back on the processor for the rest of this evaluation (same results).`);
          return false;
        }
        c[8]++; c[9] += g.gw.length; c[10] += performance.now() - t0;
        return true;
      },
      failed(detail) {
        root[11] = 1;
        gpuLog(`${thread}: a weighted sum differs: ${detail}. Back to the processor. The sums before it came from the GPU and may differ: run Evaluate builds again with Use GPU off to be sure.`);
      },
    };
    setSumsAccelerator(sums);
  }
  return {
    stats: shared,
    close() {
      setSimilarityAccelerator(null);
      setSumsAccelerator(null);
      const mem = gpu.memory();
      // The other threads are stopped by now (the team closes before the evaluation returns): theirs goes too.
      gpu.close({ reset: rank === 0 });
      if (rank !== 0) return;
      const all = new Float64Array(shared);
      const line = (label: string, from: number, to: number) => {
        const t = new Float64Array(STATS);
        for (let r = from; r < to; r++) for (let k = 0; k < STATS; k++) t[k] = k === 6 || k === 11 ? Math.max(t[k], all[r * STATS + k]) : t[k] + all[r * STATS + k];
        if (!t[0] && !t[5] && !t[6] && !t[8]) return;
        const n = (x: number) => Math.round(x).toLocaleString('en-US');
        gpuLog(`${label}: ${n(t[0])} similarity passes on the GPU (${n(t[1])} games, ${t[0] ? (t[3] / t[0]).toFixed(2) : 0} ms each), `
          + `${n(t[2])} games worked out on the processor, ${n(t[4])} checks passed${t[6] ? ', then back to the processor' : ''}`
          + `${t[5] ? `; ${n(t[5])} passes on the processor in champ select or a game` : ''}.`);
        if (t[8] || t[11]) gpuLog(`${label}: ${n(t[8])} weighted-sum passes on the GPU (${n(t[9])} observations, ${t[8] ? (t[10] / t[8]).toFixed(2) : 0} ms each)${t[11] ? ', then back to the processor' : ''}.`);
      };
      line(`${thread} (${mem.uploadedMb} MB of GPU memory)`, 0, 1);
      line('the other threads', 1, MAX_THREADS);
    },
  };
}

/** Settings → GPU → Test GPU: open it, check it and time it against the processor, all in the log. */
export async function testGpu(): Promise<boolean> {
  gpuLog('Test GPU:');
  let gpu: Gpu;
  try { gpu = await Gpu.open(); } catch (err) { gpuLog(`  ${message(err)}.`); return false; }
  try {
    const dev = gpu.device;
    gpuLog(`  ${dev.name} (compute ${dev.capability}, ${dev.memoryMb.toLocaleString('en-US')} MB, ${dev.freeMb.toLocaleString('en-US')} MB free; driver ${dev.driver}).`);
    if (!selfCheck(gpu, '  ')) return false;
    // 100,000 games of a 12-dimension draft: the size of an evaluation's draft model.
    const rows = 100_000, d = 12;
    const flat = Float64Array.from({ length: rows * d }, (_, i) => Math.sin(i * 12.9898) * 1.7);
    const stored = { flat, anyNaN: false }, q = Float64Array.from({ length: d }, (_, c) => Math.cos(c));
    const out = new Float64Array(rows), cpu = new Float64Array(rows), den = 2 * 0.7 * 0.7;
    const time = (f: () => void) => { f(); const t0 = performance.now(); for (let k = 0; k < 20; k++) f(); return (performance.now() - t0) / 20; };
    const onGpu = time(() => { const m = gpu.similarities(flat, rows, d, q, den, out); for (let x = 0; x < m.length; x++) out[m[x]] = rowSimilarity(flat, d, q, den, m[x]); });
    const onCpu = time(() => flatSimilarities(stored, rows, d, q, den, cpu));
    gpuLog(`  100,000 games' similarities: ${onGpu.toFixed(2)} ms on the GPU, ${onCpu.toFixed(2)} ms on one processor thread (${(onCpu / onGpu).toFixed(1)}×).`);
    return true;
  } catch (err) {
    gpuLog(`  ${message(err)}.`);
    return false;
  } finally { gpu.close(); }
}

/** After a stopped evaluation (service.ts): what its threads left on the card goes. */
export async function resetGpu(): Promise<boolean> {
  try {
    const done = await Gpu.reset();
    if (done) gpuLog('Evaluate builds stopped before its end: the GPU memory its threads took is freed.');
    return done;
  } catch (err) {
    gpuLog(`Freeing the GPU after a stopped evaluation: ${message(err)}.`);
    return false;
  }
}
