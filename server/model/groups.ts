// Grouped drafts (0.20.0, docs/EVAL-SCALING.md, step 3): an evaluation trial, off unless asked for.
//
// Every held-out game of an evaluation is compared with every stored game: its similarity to each (context.ts kernel)
// and, per decision, the weighted sums over them (estimate.ts). That is held-out × stored games, so a full evaluation's
// time grows with the square of the games. Here the stored games' draft contexts are put into M groups once
// (k-means on the standardised draft dimensions, games with the same unknown dimensions only grouped together), and a
// group stands for its games: one similarity per group (its centre's), and per option one set of sums per group
// (Σw, Σw², Σw·WPA, Σw·win: the sums estimate.ts reads, which add up exactly). A query then costs M similarities and
// at most options × M cells instead of a pass over every game. The exact matchup layer stays exact: its games are
// indexed by opponent and summed one by one, with their group's similarity.
//
// It is binned kernel estimation (Silverman 1982, Fan & Marron 1994, Wand 1994): the error comes from treating a
// group's games as if they sat at its centre, shrinking with the groups' spread against the bandwidth. The report
// gives that spread, and `npm run eval:builds -- --compare-grouped` measures the effect on the report itself.
// The app's builds (champ select, the overlay) never use it: only an evaluation's games carry a group.
import { DRAFT_DIMS, kernelAt, scaleOf, standardise, type Contexts } from './context.ts';
import type { PreparedGame } from './dataset.ts';

/** A draft model's groups: each game's (by row) and each group's centre in that model's standardisation. */
export interface DraftGroups {
  of: Int32Array;
  centroids: Contexts;
}

/** What assignDraftGroups did, for the report and the log. */
export interface GroupingStats {
  /** Games grouped and groups made. */
  games: number;
  groups: number;
  /**
   * The games' root-mean-square distance to their group's centre, in standard deviations per known dimension (the
   * kernel's own measure of distance): against the bandwidth (0.7 by default), what the grouping blurs.
   */
  spread: number;
  /** How long the grouping took. */
  ms: number;
}

/**
 * Groups to make for `games` games when none are asked for: about 40 games each, 64 to 1,024. Measured on 150,000
 * synthetic drafts (300 compositions with noise, docs/EVAL-SCALING.md): 1,024 groups took 7.6 s to make and a query
 * 31 times less time than the exact one; 2,048, 4,096 and 8,192 groups agreed on a few more top picks (93 → 95 %)
 * for 27 s to 184 s of grouping and 20 to 5 times less time.
 */
export const autoGroups = (games: number) => Math.max(64, Math.min(1024, Math.round(games / 40)));

/** Deterministic random numbers in [0, 1) (mulberry32). */
function rng(seed: number) {
  let a = seed >>> 0;
  return () => {
    a = (a + 0x6d2b79f5) >>> 0;
    let t = a;
    t = Math.imul(t ^ (t >>> 15), t | 1);
    t ^= t + Math.imul(t ^ (t >>> 7), t | 61);
    return ((t ^ (t >>> 14)) >>> 0) / 4294967296;
  };
}

/** Lloyd's iterations on a sample before every game is assigned. */
const ITERATIONS = 8;
/** The sample k-means is fitted on: this many games per group, at least SAMPLE_MIN (all of them when fewer). */
const SAMPLE_PER_GROUP = 24;
const SAMPLE_MIN = 20_000;

/**
 * k-means on `n` points of `d` dimensions (`x`, row after row; no NaN), `k` centres: fitted on a sample, then every
 * point assigned to its nearest centre. Returns each point's centre (0 to k − 1; some may end up empty).
 */
export function kmeans(x: Float64Array, n: number, d: number, k: number, seed: number): Int32Array {
  const assign = new Int32Array(n);
  if (k <= 1 || n <= 1) return assign;
  k = Math.min(k, n);
  const r = rng(seed);
  // The sample: every point, or an even random draw of them (a partial Fisher–Yates shuffle).
  const size = Math.min(n, Math.max(SAMPLE_MIN, k * SAMPLE_PER_GROUP));
  let sample: Int32Array;
  if (size === n) sample = Int32Array.from({ length: n }, (_, i) => i);
  else {
    const all = Int32Array.from({ length: n }, (_, i) => i);
    for (let i = 0; i < size; i++) { const j = i + Math.floor(r() * (n - i)); const t = all[i]; all[i] = all[j]; all[j] = t; }
    sample = all.slice(0, size);
  }
  // Starting centres by k-means++ (Arthur & Vassilvitskii 2007): each next one drawn with probability proportional to
  // its squared distance from the nearest centre so far, so clusters far apart each get one.
  const c = new Float64Array(k * d);
  const minD = new Float64Array(sample.length).fill(Infinity);
  let pick = sample[Math.floor(r() * sample.length)];
  for (let j = 0; j < k; j++) {
    c.set(x.subarray(pick * d, pick * d + d), j * d);
    let total = 0;
    for (let i = 0; i < sample.length; i++) {
      const at = sample[i] * d;
      let s = 0;
      for (let t = 0; t < d; t++) { const v = x[at + t] - c[j * d + t]; s += v * v; }
      if (s < minD[i]) minD[i] = s;
      total += minD[i];
    }
    if (j + 1 === k) break;
    if (!(total > 0)) { pick = sample[Math.floor(r() * sample.length)]; continue; }
    let u = r() * total, i = 0;
    for (; i < sample.length - 1; i++) { u -= minD[i]; if (u <= 0) break; }
    pick = sample[i];
  }
  const nearest = (at: number) => {
    let best = 0, bestD = Infinity;
    for (let j = 0, cj = 0; j < k; j++, cj += d) {
      let s = 0;
      for (let t = 0; t < d; t++) { const v = x[at + t] - c[cj + t]; s += v * v; if (s >= bestD) break; }
      if (s < bestD) { bestD = s; best = j; }
    }
    return best;
  };
  const sums = new Float64Array(k * d), counts = new Int32Array(k), sampleOf = new Int32Array(sample.length);
  for (let it = 0; it < ITERATIONS; it++) {
    sums.fill(0); counts.fill(0);
    let moved = 0;
    for (let i = 0; i < sample.length; i++) {
      const at = sample[i] * d, j = nearest(at);
      if (it === 0 || sampleOf[i] !== j) moved++;
      sampleOf[i] = j; counts[j]++;
      for (let t = 0; t < d; t++) sums[j * d + t] += x[at + t];
    }
    for (let j = 0; j < k; j++) if (counts[j] > 0) for (let t = 0; t < d; t++) c[j * d + t] = sums[j * d + t] / counts[j];
    if (it > 0 && moved === 0) break;
  }
  for (let i = 0; i < n; i++) assign[i] = nearest(i * d);
  return assign;
}

/**
 * Puts `games` into about `target` groups by their draft context and writes each game's group into `g.dg` (numbered
 * 0 to groups − 1 over all of them). Games are only grouped with games that know the same dimensions, so a group's
 * centre knows what its games know. The same games, target and seed give the same groups.
 */
export function assignDraftGroups(games: PreparedGame[], target: number, seed = 1): GroupingStats {
  const started = performance.now();
  const D = DRAFT_DIMS.length;
  const scale = scaleOf(games.map((g) => g.draft), D);
  const z = games.map((g) => standardise(g.draft, scale));
  // By which dimensions are known.
  const byPattern = new Map<number, number[]>();
  z.forEach((v, i) => {
    let mask = 0;
    for (let t = 0; t < D; t++) if (!Number.isNaN(v[t])) mask |= 1 << t;
    let list = byPattern.get(mask);
    if (!list) { list = []; byPattern.set(mask, list); }
    list.push(i);
  });
  let next = 0, sq = 0, known = 0;
  for (const [mask, members] of [...byPattern].sort((a, b) => a[0] - b[0])) {
    const dims = [...Array(D).keys()].filter((t) => mask & (1 << t));
    const d = dims.length;
    const k = Math.max(1, Math.min(members.length, Math.round((target * members.length) / games.length)));
    if (d === 0 || k === 1) {
      for (const i of members) games[i].dg = next;
      next++;
      // Spread around the group's mean.
      if (d > 0) {
        const m = new Float64Array(d);
        for (const i of members) for (let t = 0; t < d; t++) m[t] += z[i][dims[t]] / members.length;
        for (const i of members) for (let t = 0; t < d; t++) { sq += (z[i][dims[t]] - m[t]) ** 2; known++; }
      }
      continue;
    }
    const x = new Float64Array(members.length * d);
    members.forEach((i, r) => { for (let t = 0; t < d; t++) x[r * d + t] = z[i][dims[t]]; });
    const assign = kmeans(x, members.length, d, k, seed + mask);
    // Numbered in order of first appearance, empty centres dropped.
    const local = new Map<number, number>();
    const sums = new Map<number, { s: Float64Array; n: number }>();
    members.forEach((i, r) => {
      let id = local.get(assign[r]);
      if (id === undefined) { id = next++; local.set(assign[r], id); }
      games[i].dg = id;
      let a = sums.get(id);
      if (!a) { a = { s: new Float64Array(d), n: 0 }; sums.set(id, a); }
      a.n++;
      for (let t = 0; t < d; t++) a.s[t] += x[r * d + t];
    });
    members.forEach((i, r) => {
      const a = sums.get(games[i].dg!)!;
      for (let t = 0; t < d; t++) { sq += (x[r * d + t] - a.s[t] / a.n) ** 2; known++; }
    });
  }
  return { games: games.length, groups: next, spread: known ? Math.sqrt(sq / known) : 0, ms: performance.now() - started };
}

/**
 * The groups of a draft model whose games all carry one (an evaluation's, with grouping on), else null: each game's
 * group renumbered to the ones present, and each group's centre the mean of its games' rows of `zDraft` (that model's
 * standardisation; a dimension none of them knows stays unknown).
 */
export function draftGroupsOf(games: { g: PreparedGame }[], zDraft: Contexts): DraftGroups | null {
  if (!games.length || games.some((s) => s.g.dg === undefined)) return null;
  const local = new Map<number, number>();
  const of = new Int32Array(games.length);
  games.forEach((s, i) => {
    let id = local.get(s.g.dg!);
    if (id === undefined) { id = local.size; local.set(s.g.dg!, id); }
    of[i] = id;
  });
  const m = local.size, d = zDraft.d;
  const sum = new Float64Array(m * d), cnt = new Float64Array(m * d);
  for (let i = 0; i < games.length; i++) {
    const at = i * d, to = of[i] * d;
    for (let t = 0; t < d; t++) { const v = zDraft.flat[at + t]; if (!Number.isNaN(v)) { sum[to + t] += v; cnt[to + t]++; } }
  }
  const flat = new Float64Array(m * d);
  let anyNaN = false;
  for (let i = 0; i < flat.length; i++) { flat[i] = cnt[i] > 0 ? sum[i] / cnt[i] : NaN; if (cnt[i] === 0) anyNaN = true; }
  return { of, centroids: { flat, d, rows: m, anyNaN } };
}

/** Each group's similarity to the draft `zq` at bandwidth `h` (its centre's, by the kernel every game's uses). */
export function groupSimilarities(groups: DraftGroups, zq: ArrayLike<number>, h: number): Float64Array {
  const out = new Float64Array(groups.centroids.rows);
  for (let j = 0; j < out.length; j++) out[j] = kernelAt(groups.centroids, j, zq, h);
  return out;
}
