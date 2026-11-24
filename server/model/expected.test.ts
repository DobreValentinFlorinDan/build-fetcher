// Scoring pre-game decisions against each game's expected win rate before it started (model/expected.ts, 0.9.0):
// the fit is a plain ridge logistic regression with the lane opponent as a shrunk effect, no game's result reaches its
// own expected win rate, and a summoner spell taken mostly by strong pilots stops looking better than it is.
import assert from 'node:assert/strict';
import { before, test } from 'node:test';
import type { StaticData } from '../ddragon.ts';
import { buildExclusivityIndex, type ExclusivityIndex } from '../engine/exclusivity.ts';
import { apAdGames, modelInputs, pilotSkillGames, rng, type SynthGame } from '../test/modelFixtures.ts';
import { loadTestStaticData } from '../test/staticData.ts';
import { frameSlabs, prepareGames, scoreGames, type PreparedGame } from './dataset.ts';
import { EVAL_DEFAULTS, evaluate, formatEval } from './evaluate.ts';
import { crossFitted, EXPECTED_DEFAULTS, fitGrouped } from './expected.ts';
import { fitLogistic } from './logistic.ts';
import { ChampionProfiles } from './profiles.ts';

let data: StaticData;
let excl: ExclusivityIndex;
before(() => { data = loadTestStaticData(); excl = buildExclusivityIndex(data); });

const ageOf = () => 0;
/** One penalty: with several, a game's result takes part in choosing it (as any cross-validated choice). */
const ONE = { ...EXPECTED_DEFAULTS, penalties: [{ lambda: 10, opponentLambda: 30 }] };

function prepared(games: SynthGame[]): PreparedGame[] {
  const inputs = modelInputs(games);
  return prepareGames(inputs.rows, inputs.participants, data, new ChampionProfiles(inputs.totals, [], data), { startingItemWindowMs: 60_000 });
}

test('expected win rate: the grouped fit is a ridge logistic regression with one column per group', () => {
  const r = rng(3);
  const n = 400, d = 4, groups = 5;
  const X = new Float64Array(n * d), group = new Int32Array(n), y = new Float64Array(n);
  const rows: number[][] = [];
  for (let i = 0; i < n; i++) {
    const x = [1, r() * 2 - 1, r() * 2 - 1, r() < 0.3 ? 1 : 0];
    x.forEach((v, j) => { X[i * d + j] = v; });
    group[i] = i % 7 === 0 ? -1 : Math.floor(r() * groups);
    y[i] = r() < 1 / (1 + Math.exp(-(0.3 + x[1] - 0.5 * x[2] + (group[i] >= 0 ? group[i] * 0.2 - 0.4 : 0)))) ? 1 : 0;
    rows.push([...x, ...Array.from({ length: groups }, (_, k) => (group[i] === k ? 1 : 0))]);
  }
  const grouped = fitGrouped(X, d, group, groups, y, 1, 1);
  const plain = fitLogistic(rows, y, { lambda: 1, maxIterations: 50, tolerance: 1e-12 });
  assert.ok(grouped.converged);
  for (let j = 0; j < d; j++) assert.ok(Math.abs(grouped.beta[j] - plain.beta[j]) < 1e-7, `β${j}: ${grouped.beta[j]} vs ${plain.beta[j]}`);
  for (let k = 0; k < groups; k++) assert.ok(Math.abs(grouped.u[k] - plain.beta[d + k]) < 1e-7, `group ${k}: ${grouped.u[k]} vs ${plain.beta[d + k]}`);
});

test("expected win rate: cross-fitted, so a game's own result never moves it; the user's own games get none", () => {
  const games = prepared(pilotSkillGames(600, 30));
  const before = crossFitted(games, ageOf).values;
  assert.ok(before.every((p) => p > 0 && p < 1), 'every game has one');
  // Flipping a game's result changes the models of the other folds, never its own fold's.
  const flipped = games.map((g, i) => (i === 0 ? { ...g, win: (1 - g.win) as 0 | 1 } : g));
  assert.equal(crossFitted(flipped, ageOf, ONE).values[0], crossFitted(games, ageOf, ONE).values[0]);
  const withMine = games.map((g, i) => (i === 1 ? { ...g, source: 'me' as const } : g));
  assert.ok(Number.isNaN(crossFitted(withMine, ageOf).values[1]));
  // Too few games: none (scored against the overall win rate, as before 0.9.0).
  assert.ok(crossFitted(games.slice(0, 100), ageOf).values.every(Number.isNaN));
});

test('expected win rate: a summoner spell taken mostly by strong pilots stops looking better than it is', () => {
  const games = prepared(pilotSkillGames(1600, 40));
  const meanBy = (scored: ReturnType<typeof scoreGames>) => {
    const sums = new Map<string, { s: number; n: number }>();
    for (const x of scored) {
      const e = sums.get(x.g.summoners) ?? { s: 0, n: 0 };
      e.s += x.wpa0; e.n++;
      sums.set(x.g.summoners, e);
    }
    const of = (k: string) => sums.get(k)!.s / sums.get(k)!.n;
    return of('4,14') - of('4,12');   // Flash + Ignite minus Flash + Teleport
  };
  const overall = meanBy(scoreGames(games, null));
  const fair = meanBy(scoreGames(games, null, crossFitted(games, ageOf).values));
  assert.ok(overall > 0.02, `against the overall win rate Ignite looks ${(overall * 100).toFixed(1)} pts better`);
  assert.ok(fair < -0.03 && fair > -0.14, `against the expected win rate it costs ${(fair * 100).toFixed(1)} pts (truly 8)`);
});

const ENGINE = {
  minMatchupGames: 15, generalPriorWeight: 0.15, minLaneGames: 30, otherLaneWeight: 0.15,
  previousPatchWeight: 0.35, minCurrentPatchGames: 40, startingItemWindowMs: 60_000,
};

function run(games: SynthGame[]) {
  const inputs = modelInputs(games);
  return evaluate({
    ...inputs, data, excl, profiles: new ChampionProfiles(inputs.totals, [], data), regionPatch: () => undefined, startingItemWindowMs: 60_000,
  }, { ...EVAL_DEFAULTS, engine: ENGINE, lambdas: [1], bandwidths: [0.7], ks: [100], bootstrap: 300, live: false, memory: null });
}

test('eval: scoring against the draft passes when pilot strength hides what a summoner spell costs', async () => {
  const r = await run(pilotSkillGames(1600, 40));
  const c = r.scoring!;
  assert.ok(c.expected && c.expected.logLoss < c.expected.baseLogLoss, 'the expected win rate predicts held-out results');
  const summoners = c.decisions.find((d) => d.key === 'summoners')!;
  assert.ok(summoners.changed > 0.5, `the summoner pick changes in most games (${summoners.changed})`);
  assert.ok(summoners.fair > summoners.classic, `${summoners.fair} > ${summoners.classic}`);
  assert.ok(c.adjusted.low > 0, `CI ${c.adjusted.low}..${c.adjusted.high}`);
  assert.ok(c.brier.fair <= c.brier.classic);
  assert.equal(c.gate, 'passed', c.gateReason);
  assert.match(formatEval(r), /Runes and summoners against the draft: PASSED/);
});

test("eval: scoring against the draft stays off when the draft and the pilot don't predict who wins", async () => {
  const r = await run(apAdGames(800, 40, 21, { effect: true, randomOpponent: true }));
  assert.equal(r.scoring!.gate, 'failed');
  assert.match(r.scoring!.gateReason, /no better than the overall win rate/);
});

test('frames from shared blocks (0.9.1, the model build) are the same as frames of their own', () => {
  const inputs = modelInputs(apAdGames(60, 10, 3));
  const profiles = new ChampionProfiles(inputs.totals, [], data);
  const own = prepareGames(inputs.rows, inputs.participants, data, profiles, { startingItemWindowMs: 60_000 });
  // Small blocks, so the games' frames span several of them.
  const shared = prepareGames(inputs.rows, inputs.participants, data, profiles, { startingItemWindowMs: 60_000, frames: frameSlabs(5000) });
  assert.ok(own.every((g) => g.frames && g.frames.length > 0));
  assert.deepEqual(shared.map((g) => Array.from(g.frames!)), own.map((g) => Array.from(g.frames!)));
  assert.ok(new Set(shared.map((g) => g.frames!.buffer)).size > 1 && new Set(shared.map((g) => g.frames!.buffer)).size < shared.length, 'several games per block');
  // An array larger than a block gets one of its own.
  const big = frameSlabs(10)(25);
  assert.equal(big.length, 25);
  assert.equal(big.buffer.byteLength, 25 * 4);
});
