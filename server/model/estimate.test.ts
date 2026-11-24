// estimateDecision (estimate.ts) indexes the observations once and loops over flat arrays: the same numbers as the
// plain version below (the one it replaced), on random decisions, to the last bit: by its general loop, without context
// (from the index), with the similarities of a draft (option by option) and with sums kept across priors.
import assert from 'node:assert/strict';
import { test } from 'node:test';
import type { Lane } from '../../shared/types.ts';
import { rng } from '../test/modelFixtures.ts';
import { contextsFrom, kernel } from './context.ts';
import { similarities } from './draft.ts';
import { estimateDecision, groupedSums, setSumsAccelerator, type EstimateOptions, type Observation, type Query } from './estimate.ts';

/** The plain version: one pass over every observation, sums in maps. */
function reference(obs: Observation[], q: Query, o: EstimateOptions, only?: Set<string>, dim = -1) {
  class Acc {
    sw = 0; sw2 = 0; swx = 0; swin = 0; n = 0;
    add(w: number, x: number, win: number, k = 1) {
      if (w <= 0 || k <= 0) return;
      const wk = w * k;
      this.sw += wk; this.sw2 += w * wk; this.swx += wk * x; this.swin += wk * win; this.n++;
    }
    get mean() { return this.sw > 0 ? this.swx / this.sw : 0; }
    get nEff() { return this.sw2 > 0 ? (this.sw * this.sw) / this.sw2 : 0; }
  }
  const towards = (a: Acc, prior: number, k: number) => (a.nEff * a.mean + k * prior) / (a.nEff + k);
  const hasContext = Number.isFinite(o.h) && (dim >= 0 ? !Number.isNaN(q.z[dim]) : Array.from(q.z).some((v) => !Number.isNaN(v)));
  const all = new Map<string, Acc>(), lane = new Map<string, Acc>(), ctx = new Map<string, Acc>(), mu = new Map<string, Acc>();
  const pilots = new Map<string, Set<string>>();
  const get = (m: Map<string, Acc>, key: string) => { let a = m.get(key); if (!a) { a = new Acc(); m.set(key, a); } return a; };
  const total = new Acc();
  let laneWeight = 0, sq = 0, sqW = 0;
  for (const ob of obs) {
    if (only && !only.has(ob.option)) continue;
    get(all, ob.option).add(ob.w, ob.wpa, ob.win);
    if (ob.lane !== q.lane) continue;
    get(lane, ob.option).add(ob.w, ob.wpa, ob.win);
    laneWeight += ob.w;
    sq += ob.w * ob.wpa * ob.wpa; sqW += ob.w;
    const sim = hasContext ? kernel(ob.z!, q.z, o.h, dim) : 1;
    get(ctx, ob.option).add(ob.w, ob.wpa, ob.win, sim);
    total.add(ob.w, 0, 0, sim);
    if (q.opponent !== null && ob.opponent === q.opponent) get(mu, ob.option).add(ob.w, ob.wpa, ob.win, sim);
    let ps = pilots.get(ob.option);
    if (!ps) { ps = new Set(); pilots.set(ob.option, ps); }
    ps.add(ob.pilot);
  }
  const sigma = Math.max(0.3, sqW > 0 ? Math.sqrt(sq / sqW) : 0.5);
  const options = [];
  for (const [option, l] of lane) {
    if (l.n < o.minGames || l.sw < o.minShare * laneWeight) continue;
    const thetaAll = towards(all.get(option)!, 0, o.k);
    const laneTheta = towards(l, thetaAll, o.k);
    const c = ctx.get(option)!;
    const contextTheta = hasContext ? towards(c, laneTheta, o.k) : laneTheta;
    const m = mu.get(option);
    const theta = m ? towards(m, contextTheta, o.k) : contextTheta;
    const n = Math.max(hasContext ? c.nEff : l.nEff, m?.nEff ?? 0);
    const half = 1.96 * sigma / Math.sqrt(n + o.k);
    const wr = hasContext ? c : l;
    options.push({
      option, theta, low: theta - half, high: theta + half, laneTheta, contextTheta,
      nEff: hasContext ? c.nEff : l.nEff, matchupGames: m?.n ?? 0, games: l.n, pilots: pilots.get(option)?.size ?? 0,
      winRate: wr.sw > 0 ? wr.swin / wr.sw : 0,
    });
  }
  options.sort((a, b) => b.theta - a.theta || b.games - a.games);
  return { options, nEff: hasContext ? total.nEff : [...lane.values()].reduce((s, a) => s + a.sw, 0) ** 2 / Math.max(1e-9, [...lane.values()].reduce((s, a) => s + a.sw2, 0)), sigma };
}

const DRAFT = 6, LIVE = 3;

/** Random games: draft context (some unknown), per-game live context, options, weights (some zero), opponents. */
function sample(seed: number, games: number) {
  const r = rng(seed);
  const z = (n: number) => Array.from({ length: n }, () => (r() < 0.08 ? NaN : (r() - 0.5) * 4));
  const rows = Array.from({ length: games }, () => ({ draft: z(DRAFT), live: z(LIVE) }));
  const obs = (withLive: boolean): Observation[] => rows.map((g, i) => ({
    option: String(3000 + Math.floor(r() ** 2 * 9)), wpa: (r() - 0.5) * 0.8, win: r() < 0.5 ? 1 : 0,
    w: r() < 0.05 ? 0 : 0.2 + r(), lane: (r() < 0.6 ? 'TOP' : 'MIDDLE') as Lane, opponent: r() < 0.7 ? 1 + Math.floor(r() * 6) : null,
    pilot: `p${Math.floor(r() * games / 4)}`, z: withLive ? [...g.draft, ...g.live] : g.draft, row: i,
  }));
  return { rows, pregame: obs(false), items: obs(true), query: (): Query => ({ lane: r() < 0.5 ? 'TOP' : 'MIDDLE', opponent: r() < 0.7 ? 1 + Math.floor(r() * 6) : null, z: z(DRAFT) }) };
}

test('estimateDecision gives exactly what the plain version gives', () => {
  const settings: EstimateOptions[] = [
    { h: 0.7, k: 100, minGames: 5, minShare: 0.03 }, { h: 0.3, k: 30, minGames: 1, minShare: 0 }, { h: Infinity, k: 300, minGames: 5, minShare: 0.03 },
  ];
  for (const seed of [1, 2, 3]) {
    const { pregame, query } = sample(seed, 600);
    for (const o of settings) {
      for (let i = 0; i < 8; i++) {
        const q = query();
        assert.deepEqual(estimateDecision(pregame, q, o), reference(pregame, q, o), `seed ${seed}, h ${o.h}, query ${i}`);
        const pair = new Set(['3000', '3002']);
        for (const dim of [-1, 2]) assert.deepEqual(estimateDecision(pregame, q, o, pair, dim), reference(pregame, q, o, pair, dim), `only, dim ${dim}`);
      }
    }
  }
});

test('similarities computed once per draft give the same estimates, for pre-game and item decisions', () => {
  const { rows, pregame, items, query } = sample(7, 500);
  const o: EstimateOptions = { h: 0.7, k: 100, minGames: 5, minShare: 0.03 };
  for (let i = 0; i < 10; i++) {
    const q = query();
    // As recommendDraft does: each game's similarity on the draft dimensions; item queries leave the live ones unknown.
    const sims = { h: o.h, values: Float64Array.from(rows, (g) => kernel(g.draft, q.z, o.h)) };
    const itemQuery = { ...q, z: [...Array.from(q.z), ...Array<number>(LIVE).fill(NaN)] };
    assert.deepEqual(estimateDecision(pregame, { ...q, sims }, o), reference(pregame, q, o));
    assert.deepEqual(estimateDecision(items, { ...itemQuery, sims }, o), reference(items, itemQuery, o));
    // Another bandwidth, or the one-dimension reason search, ignores them.
    const other = { ...o, h: 1.5 };
    assert.deepEqual(estimateDecision(pregame, { ...q, sims }, other), reference(pregame, q, other));
    assert.deepEqual(estimateDecision(pregame, { ...q, sims }, o, undefined, 1), reference(pregame, q, o, undefined, 1));
  }
});

test('a grown observation array is indexed again', () => {
  const { pregame, query } = sample(11, 300);
  const o: EstimateOptions = { h: 0.7, k: 100, minGames: 5, minShare: 0.03 };
  const q = query();
  const obs = pregame.slice(0, 200);
  assert.deepEqual(estimateDecision(obs, q, o), reference(obs, q, o));
  obs.push(...pregame.slice(200));
  assert.deepEqual(estimateDecision(obs, q, o), reference(obs, q, o));
});

test('sums kept for a draft (Query.memo) give each prior exactly what a fresh estimate gives', () => {
  const { rows, pregame, items, query } = sample(13, 500);
  for (const h of [0.5, 1, Infinity]) {
    for (let i = 0; i < 6; i++) {
      const q = query();
      const sims = Number.isFinite(h) ? { h, values: Float64Array.from(rows, (g) => kernel(g.draft, q.z, h)) } : undefined;
      const memo = new Map<Observation[], unknown>();
      for (const k of [30, 100, 300]) {
        const o: EstimateOptions = { h, k, minGames: 5, minShare: 0.03 };
        assert.deepEqual(estimateDecision(pregame, { ...q, sims, memo }, o), reference(pregame, q, o), `h ${h}, k ${k}`);
        const itemQuery = { ...q, z: [...Array.from(q.z), ...Array<number>(LIVE).fill(NaN)] };
        assert.deepEqual(estimateDecision(items, { ...itemQuery, sims, memo }, o), reference(items, itemQuery, o));
      }
      assert.equal(memo.size, 2, 'one entry per set of observations');
    }
  }
});

test('similarities (model/draft.ts) are exactly kernel(), with and without games missing a dimension', () => {
  const { rows, query } = sample(17, 400);
  const withGaps = rows.map((g) => g.draft);
  const complete = withGaps.map((z) => z.map((v) => (Number.isNaN(v) ? 0.25 : v)));
  for (const zDraft of [withGaps, complete]) {
    for (const h of [0.5, 1]) {
      for (let i = 0; i < 5; i++) {
        const zq = query().z;
        const got = Array.from(similarities(contextsFrom(zDraft), zq, h));
        assert.deepEqual(got, zDraft.map((z) => kernel(z, zq, h)));
      }
    }
  }
});

test('the weighted sums can come from an accelerator (Use GPU): its numbers are used, a wrong one is caught and replaced', () => {
  const { rows, pregame, query } = sample(19, 500);
  const o: EstimateOptions = { h: 0.7, k: 100, minGames: 5, minShare: 0.03 };
  const draw = () => {
    const q = query();
    return { ...q, sims: { h: o.h, values: Float64Array.from(rows, (g) => kernel(g.draft, q.z, o.h)) } };
  };
  try {
    // Right (the processor's own loop, standing in for the card), declining, and wrong.
    let runs = 0, failures: string[] = [];
    setSumsAccelerator({ run: (g, sims, opponent, out) => { runs++; groupedSums(g, sims, opponent, out); return true; }, failed: (d) => failures.push(d) });
    for (let i = 0; i < 6; i++) { const q = draw(); assert.deepEqual(estimateDecision(pregame, q, o), reference(pregame, q, o)); }
    assert.ok(runs > 0 && failures.length === 0);

    runs = 0;
    setSumsAccelerator({ run: () => { runs++; return false; }, failed: (d) => failures.push(d) });
    for (let i = 0; i < 3; i++) { const q = draw(); assert.deepEqual(estimateDecision(pregame, q, o), reference(pregame, q, o)); }
    assert.ok(runs > 0 && failures.length === 0);

    // One last digit off in one sum: found on the first run (the first of every 500 is checked), the processor's numbers used,
    // and the accelerator not asked again.
    runs = 0; failures = [];
    setSumsAccelerator({
      run: (g, sims, opponent, out) => { runs++; groupedSums(g, sims, opponent, out); out.csw[0] = out.csw[0] + Math.max(Math.abs(out.csw[0]) * 2 ** -52, 2 ** -1070) || 1e-300; return true; },
      failed: (d) => failures.push(d),
    });
    for (let i = 0; i < 4; i++) { const q = draw(); assert.deepEqual(estimateDecision(pregame, q, o), reference(pregame, q, o)); }
    assert.equal(runs, 1);
    assert.equal(failures.length, 1);
    assert.match(failures[0], /^csw\[0\] is /);
  } finally { setSumsAccelerator(null); }
});
