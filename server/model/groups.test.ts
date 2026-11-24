// Grouped drafts (0.20.0, model/groups.ts): the per-group sums are the per-game sums with each game at its group's
// similarity; the grouping is deterministic and keeps games that know different dimensions apart; an evaluation trial
// with grouping on reaches the exact evaluation's verdict on synthetic games, and a team of threads reports what one does.
import assert from 'node:assert/strict';
import { before, test } from 'node:test';
import type { Lane } from '../../shared/types.ts';
import type { StaticData } from '../ddragon.ts';
import { buildExclusivityIndex, type ExclusivityIndex } from '../engine/exclusivity.ts';
import { apAdGames, modelInputs, rng } from '../test/modelFixtures.ts';
import { loadTestStaticData } from '../test/staticData.ts';
import { contextsFrom, DRAFT_DIMS } from './context.ts';
import type { PreparedGame } from './dataset.ts';
import { similarities } from './draft.ts';
import { estimateDecision, type EstimateOptions, type Observation } from './estimate.ts';
import { EVAL_DEFAULTS, evaluate, formatEval, type EvalOptions, type EvalReport } from './evaluate.ts';
import { startTeam } from './runEval.ts';
import { assignDraftGroups, autoGroups, draftGroupsOf, groupSimilarities, kmeans } from './groups.ts';
import { ChampionProfiles } from './profiles.ts';

const D = DRAFT_DIMS.length;

/** `n` games of one lane with random drafts, options and opponents: their observations, sharing one flat context array. */
function sample(seed: number, n: number) {
  const r = rng(seed);
  const z = Array.from({ length: n }, () => Array.from({ length: D }, () => (r() - 0.5) * 4));
  const zs = contextsFrom(z, D);
  const obs: Observation[] = z.map((_, i) => ({
    option: `opt${Math.floor(r() * 6)}`, wpa: r() - 0.5, win: r() < 0.5 ? 1 : 0, w: r() < 0.05 ? 0 : 0.2 + r(),
    lane: (r() < 0.9 ? 'top' : 'mid') as Lane, opponent: r() < 0.1 ? null : 100 + Math.floor(r() * 8), pilot: `p${i % 40}`,
    zs, zRow: i, row: i,
  }));
  const query = () => ({ lane: 'top' as Lane, opponent: 100 + Math.floor(r() * 8), z: Array.from({ length: D }, () => (r() - 0.5) * 4) });
  return { r, zs, obs, query };
}

/** The same estimates to within rounding (the per-group sums add in another order). */
function close(a: ReturnType<typeof estimateDecision>, b: ReturnType<typeof estimateDecision>, what: string) {
  assert.equal(a.options.length, b.options.length, what);
  const near = (x: number, y: number, field: string) => assert.ok(Math.abs(x - y) <= 1e-9 * (1 + Math.abs(y)), `${what}: ${field} ${x} against ${y}`);
  near(a.nEff, b.nEff, 'nEff');
  near(a.sigma, b.sigma, 'sigma');
  const byOption = new Map(b.options.map((o) => [o.option, o]));
  for (const x of a.options) {
    const y = byOption.get(x.option);
    assert.ok(y, `${what}: ${x.option}`);
    for (const f of ['theta', 'low', 'high', 'laneTheta', 'contextTheta', 'nEff', 'winRate'] as const) near(x[f], y[f], `${x.option}.${f}`);
    assert.equal(x.matchupGames, y.matchupGames, `${what}: ${x.option} matchup games`);
    assert.equal(x.games, y.games);
  }
}

test('per-group sums are the per-game sums with each game at its group\'s similarity', () => {
  const { r, zs, obs, query } = sample(3, 2_000);
  const o: EstimateOptions = { h: 0.7, k: 100, minGames: 5, minShare: 0.03 };
  // Random groups and similarities: the sums depend on nothing else.
  const of = Int32Array.from({ length: zs.rows }, () => Math.floor(r() * 37));
  for (let i = 0; i < 8; i++) {
    const q = query();
    const gs = Float64Array.from({ length: 37 }, () => (r() < 0.1 ? 0 : r()));
    const values = Float64Array.from(of, (g) => gs[g]);
    const grouped = estimateDecision(obs, { ...q, sims: { h: o.h, values, groups: { of, sims: gs } } }, o);
    const perGame = estimateDecision(obs, { ...q, sims: { h: o.h, values } }, o);
    close(grouped, perGame, `query ${i}`);
    // No opponent: no matchup layer.
    close(estimateDecision(obs, { ...q, opponent: null, sims: { h: o.h, values, groups: { of, sims: gs } } }, o),
      estimateDecision(obs, { ...q, opponent: null, sims: { h: o.h, values } }, o), `query ${i} without opponent`);
  }
});

test('one game per group is the exact estimate', () => {
  const { zs, obs, query } = sample(5, 600);
  const o: EstimateOptions = { h: 0.7, k: 100, minGames: 5, minShare: 0.03 };
  const games = Array.from({ length: zs.rows }, (_, i) => ({ g: { dg: i } as PreparedGame }));
  const groups = draftGroupsOf(games, zs)!;
  assert.equal(groups.centroids.rows, zs.rows);
  for (let i = 0; i < 5; i++) {
    const q = query();
    const gs = groupSimilarities(groups, q.z, o.h);
    const exact = similarities(zs, q.z, o.h).slice();
    assert.deepEqual(Array.from(gs), Array.from(exact), 'each centre is its game');
    close(estimateDecision(obs, { ...q, sims: { h: o.h, values: exact, groups: { of: groups.of, sims: gs } } }, o),
      estimateDecision(obs, { ...q, sims: { h: o.h, values: exact } }, o), `query ${i}`);
  }
});

test('draft groups: every game gets one, the same each time, and only games knowing the same dimensions share one', () => {
  const r = rng(11);
  const games = Array.from({ length: 3_000 }, (_, i) => ({
    draft: Array.from({ length: D }, (_, t) => (t >= 8 && i % 3 === 0 ? NaN : (r() - 0.5) * 10 + (i % 4) * 5)),
  })) as PreparedGame[];
  const stats = assignDraftGroups(games, 75);
  const first = games.map((g) => g.dg);
  assert.ok(first.every((g) => Number.isInteger(g) && g! >= 0 && g! < stats.groups));
  assert.ok(stats.groups >= 50 && stats.groups <= 75, `${stats.groups} groups for 75 asked`);
  assert.ok(stats.spread > 0 && stats.spread < 1, `spread ${stats.spread}`);
  assignDraftGroups(games, 75);
  assert.deepEqual(games.map((g) => g.dg), first, 'deterministic');
  const pattern = new Map<number, boolean>();
  games.forEach((g, i) => {
    const unknown = i % 3 === 0;
    const seen = pattern.get(g.dg!);
    assert.ok(seen === undefined || seen === unknown, `group ${g.dg} mixes games that know different dimensions`);
    pattern.set(g.dg!, unknown);
  });
  // A group's centre doesn't know what its games don't.
  const zs = contextsFrom(games.map((g) => g.draft), D);
  const groups = draftGroupsOf(games.map((g) => ({ g })), zs)!;
  for (let i = 0; i < games.length; i += 3) assert.ok(Number.isNaN(groups.centroids.flat[groups.of[i] * D + 9]));
  assert.equal(autoGroups(1_000), 64);
  assert.equal(autoGroups(40_000), 1_000);
  assert.equal(autoGroups(1_000_000), 1_024);
});

test('k-means finds well-separated clusters', () => {
  const r = rng(2);
  const n = 900, d = 3, x = new Float64Array(n * d);
  for (let i = 0; i < n; i++) for (let t = 0; t < d; t++) x[i * d + t] = (i % 3) * 100 + r();
  const a = kmeans(x, n, d, 3, 1);
  for (let i = 3; i < n; i++) assert.equal(a[i], a[i % 3], `point ${i}`);
  assert.equal(new Set(a).size, 3);
});

let data: StaticData;
let excl: ExclusivityIndex;
before(() => { data = loadTestStaticData(); excl = buildExclusivityIndex(data); });

const ENGINE = {
  minMatchupGames: 15, generalPriorWeight: 0.15, minLaneGames: 30, otherLaneWeight: 0.15,
  previousPatchWeight: 0.35, minCurrentPatchGames: 40, startingItemWindowMs: 60_000,
};
const OPTS: EvalOptions = {
  ...EVAL_DEFAULTS, engine: ENGINE, lambdas: [1], bandwidths: [0.7, Infinity], ks: [100], bootstrap: 200, live: true, maxLiveGames: 60,
  memory: null, minGames: 100,
};

function run(o: Partial<EvalOptions>, threads = 1) {
  const inputs = modelInputs(apAdGames(800, 40, 21, { effect: true, randomOpponent: true }));
  return evaluate({
    ...inputs, data, excl, profiles: new ChampionProfiles(inputs.totals, [], data), regionPatch: () => undefined, startingItemWindowMs: 60_000,
    ...(threads > 1 ? {
      team: { profileRows: { current: inputs.totals, older: [] }, regions: new Map(), start: (snapshot) => startTeam(snapshot, { threads, perThread: 0, control: null }) },
    } : {}),
  }, { ...OPTS, ...o });
}

test('evaluation trial: grouped drafts reach the exact verdict, say so, and three threads report what one does', async () => {
  const exact = await run({});
  const grouped = await run({ grouping: { groups: 0 } });
  assert.equal(exact.grouping, undefined);
  assert.ok(grouped.grouping && grouped.grouping.groups > 1 && grouped.grouping.games === 800);
  assert.equal(grouped.gate, exact.gate);
  assert.equal(grouped.gate, 'passed');
  const core = (r: EvalReport) => r.decisions.find((d) => d.key === 'core1')!;
  assert.ok(Math.abs(core(grouped).draft - core(exact).draft) < 0.05, `core1 lift ${core(grouped).draft} against ${core(exact).draft}`);
  assert.match(formatEval(grouped), /Grouped drafts \(a trial/);
  assert.doesNotMatch(formatEval(exact), /Grouped drafts/);
  const team = await run({ grouping: { groups: 0 } }, 3);
  const { evaluatedAt: _a, ...x } = grouped;
  const { evaluatedAt: _b, ...y } = team;
  assert.deepEqual({ ...y, grouping: { ...y.grouping!, ms: 0 } }, { ...x, grouping: { ...x.grouping!, ms: 0 } });
});

test('an evaluation reports how long each step took, the reading first', async () => {
  let timings: { label: string; ms: number }[] = [];
  const inputs = modelInputs(apAdGames(300, 30, 4, { effect: true, randomOpponent: true }));
  await evaluate({
    ...inputs, data, excl, profiles: new ChampionProfiles(inputs.totals, [], data), regionPatch: () => undefined, startingItemWindowMs: 60_000,
    onTimings: (t) => { timings = t; },
  }, { ...OPTS, live: false, bandwidths: [0.7], scoring: false, tuning: null, swaps: false, minGames: 50 });
  assert.equal(timings[0].label, 'Reading the stored games');
  assert.ok(timings.length > 5 && timings.every((t) => t.ms >= 0 && t.label));
  assert.ok(timings.some((t) => t.label.startsWith('Comparing builds')));
});
