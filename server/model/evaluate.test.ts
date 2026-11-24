// npm run eval:builds on synthetic games (B6): the gate passes when the draft carries real information the lane
// baseline can't see, and fails when it carries none.
import assert from 'node:assert/strict';
import { before, test } from 'node:test';
import type { StaticData } from '../ddragon.ts';
import { buildExclusivityIndex, type ExclusivityIndex } from '../engine/exclusivity.ts';
import { apAdGames, modelInputs, type SynthGame } from '../test/modelFixtures.ts';
import { loadTestStaticData } from '../test/staticData.ts';
import { prepareGames, scoreGames } from './dataset.ts';
import { EVAL_DEFAULTS, evalSteps, evaluate, formatEval, purchaseChances, withinPilot, type EvalOptions, type EvalProgress } from './evaluate.ts';
import { ChampionProfiles } from './profiles.ts';
import { STATE_FEATURES, type StateModel } from './state.ts';

let data: StaticData;
let excl: ExclusivityIndex;
before(() => { data = loadTestStaticData(); excl = buildExclusivityIndex(data); });

const ENGINE = {
  minMatchupGames: 15, generalPriorWeight: 0.15, minLaneGames: 30, otherLaneWeight: 0.15,
  previousPatchWeight: 0.35, minCurrentPatchGames: 40, startingItemWindowMs: 60_000,
};
const OPTS: EvalOptions = { ...EVAL_DEFAULTS, engine: ENGINE, lambdas: [1], bandwidths: [0.7], ks: [100], bootstrap: 300, live: true };

function run(effect: boolean, live: boolean, o: Partial<EvalOptions> = {}, progress?: (p: EvalProgress) => void) {
  const inputs = modelInputs(apAdGames(800, 40, 21, { effect, randomOpponent: true }));
  return evaluate({
    ...inputs, data, excl, profiles: new ChampionProfiles(inputs.totals, [], data), regionPatch: () => undefined, startingItemWindowMs: 60_000,
    progress,
  }, { ...OPTS, live, ...o });
}

test('eval: the draft model passes the gate when the enemy comp decides the best first item', async () => {
  const r = await run(true, true);
  assert.equal(r.games, 800);
  assert.equal(r.pilots, 40);
  const core1 = r.decisions.find((d) => d.key === 'core1')!;
  assert.ok(core1.draft > 0.15, `draft lift on the first item ${core1.draft}`);
  assert.ok(Math.abs(core1.baseline) < 0.1, `the baseline can't see the comp (${core1.baseline})`);
  assert.ok(r.lift.low > 0, `CI ${r.lift.low}..${r.lift.high}`);
  assert.ok(r.brier.draft <= r.brier.baseline);
  assert.equal(r.gate, 'passed', r.gateReason);
  assert.ok(r.stability && r.stability.games > 0 && r.stability.flipsPerGame < 1);
  assert.match(formatEval(r), /Gate: PASSED/);
});

test('eval: with no draft effect the gate fails', async () => {
  const r = await run(false, false);
  assert.equal(r.gate, 'failed');
  assert.match(r.gateReason, /interval/);
});

test('eval: reports every step in order, and replays at most maxLiveGames held-out games', async () => {
  const seen: EvalProgress[] = [];
  const opts = { lambdas: [0.3, 1], bandwidths: [0.7, Infinity], ks: [100], maxLiveGames: 120 };
  const r = await run(true, true, opts, (p) => seen.push(p));
  const steps = evalSteps({ ...OPTS, ...opts, live: true });
  assert.equal(steps, 2 + OPTS.folds + 2 + OPTS.folds + 2 + 2 + OPTS.folds + 1 + 1 + 1,
    'two penalties, one step per fold, the final comparison, the interval, patch memory (per fold, all games, early), scoring against the draft (expected win rate, comparison), fine-tuning (per fold, all games), late-game swaps, the replay');
  // Each step starts once, in order; between them (0.10.0), how far into the step, never back.
  const starts = seen.filter((p) => p.part === undefined);
  assert.deepEqual(starts.map((p) => p.step), Array.from({ length: steps }, (_, i) => i + 1));
  assert.ok(seen.every((p) => p.steps === steps && p.label.length > 0));
  assert.match(starts.at(-1)!.label, /^Replaying \d+ held-out games/);
  let at = { step: 0, part: 0 };
  for (const p of seen) {
    if (p.part === undefined) { at = { step: p.step, part: 0 }; continue; }
    assert.ok(p.step === at.step && p.part >= at.part && p.part <= 1, `step ${p.step} at ${p.part} after step ${at.step} at ${at.part}`);
    at.part = p.part;
  }
  // An even subsample: every fold rounds up, so up to one game more per fold.
  assert.ok(r.stability && r.stability.games > 0 && r.stability.games <= 120 + OPTS.folds, `replayed ${r.stability?.games}`);
});

test('eval: keeps only the timelines the live replay reads (they are most of the memory)', async () => {
  const quick = { lambdas: [1], bandwidths: [Infinity], ks: [100], bootstrap: 10 };
  const inputs = modelInputs(apAdGames(800, 40, 21, { effect: true, randomOpponent: true }));
  assert.ok(inputs.rows.filter((r) => r.timeline).length >= 790);
  const r = await evaluate({
    ...inputs, data, excl, profiles: new ChampionProfiles(inputs.totals, [], data), regionPatch: () => undefined, startingItemWindowMs: 60_000,
  }, { ...OPTS, ...quick, live: true, maxLiveGames: 100 });
  const kept = inputs.rows.filter((row) => row.timeline).length;
  assert.ok(kept <= 100 + OPTS.folds && kept >= r.stability!.games && r.stability!.games > 50, `kept ${kept}, replayed ${r.stability?.games}`);

  const off = modelInputs(apAdGames(800, 40, 21, { effect: true, randomOpponent: true }));
  await evaluate({ ...off, data, excl, profiles: new ChampionProfiles(off.totals, [], data), regionPatch: () => undefined, startingItemWindowMs: 60_000 },
    { ...OPTS, ...quick, live: false });
  assert.equal(off.rows.filter((row) => row.timeline).length, 0, 'no replay: none kept');
});

test('eval: read one game at a time, it keeps no timeline, reads the replay\'s again, and reports the same', async () => {
  const quick = { lambdas: [1], bandwidths: [0.7], ks: [100], bootstrap: 50, live: true, maxLiveGames: 100, memory: null };
  const games = apAdGames(800, 40, 21, { effect: true, randomOpponent: true });
  const make = () => modelInputs(games);
  const kept = make();
  const before = await evaluate({ ...kept, data, excl, profiles: new ChampionProfiles(kept.totals, [], data), regionPatch: () => undefined, startingItemWindowMs: 60_000 },
    { ...OPTS, ...quick });
  const streamed = make();
  const raw = new Map(streamed.rows.map((r) => [r.match_id, r.timeline!]));
  const asked: string[] = [];
  let read = 0;
  function* rows() { for (const r of streamed.rows) { read++; yield r; } }
  const input = {
    ...streamed, rows: rows(), data, excl, profiles: new ChampionProfiles(streamed.totals, [], data), regionPatch: () => undefined, startingItemWindowMs: 60_000,
    timelinesOf: (ids: string[]) => { asked.push(...ids); return new Map(ids.map((id) => [id, raw.get(id)!])); },
  };
  const after = await evaluate(input, { ...OPTS, ...quick });
  assert.equal(read, 800);
  assert.equal(streamed.rows.filter((r) => r.timeline).length, 0, 'every raw timeline dropped');
  assert.ok(asked.length > 50 && asked.length <= 100 + OPTS.folds, `the replay asked for ${asked.length}`);
  assert.equal(input.participants.length, 0, 'the participants are let go once read');
  const { evaluatedAt: _a, ...x } = before;
  const { evaluatedAt: _b, ...y } = after;
  assert.deepEqual(y, x);
});

// Patch memory (model/weights.ts) against the two-patch model: 300 games of today's and the previous patch, 900 of the
// three patches before, from the same pilots.
function runMemory(older: (g: SynthGame) => SynthGame) {
  const recent = apAdGames(300, 40, 31, { effect: true, randomOpponent: true })
    .map((g, i) => ({ ...g, version: i % 3 === 2 ? '16.18.1.1' : '16.19.1.1' }));
  const past = apAdGames(900, 40, 32, { effect: true, randomOpponent: true })
    .map((g, i) => older({ ...g, id: `KR_${30_000 + i}`, version: `16.${17 - (i % 3)}.1.1` }));
  const inputs = modelInputs([...recent, ...past]);
  return evaluate({
    ...inputs, classicPatches: ['16.19', '16.18'], data, excl, profiles: new ChampionProfiles(inputs.totals, [], data),
    regionPatch: () => undefined, startingItemWindowMs: 60_000,
  }, { ...OPTS, live: false, minGames: 100 });
}

test('eval: patch memory passes when older patches still hold, and leaves the two-patch evaluation as it was', async () => {
  const r = await runMemory((g) => g);
  assert.equal(r.games, 300, 'everything else is evaluated on the two-patch games');
  const m = r.memory!;
  assert.equal(m.olderGames, 900);
  assert.equal(m.grid.length, OPTS.memory!.decays.length);
  assert.ok(m.early.delta > 0 && m.early.low > 0, `early in a patch ${m.early.delta} (${m.early.low}..${m.early.high})`);
  assert.ok(m.full.delta >= 0, `all games ${m.full.delta}`);
  assert.equal(m.gate, 'passed', m.gateReason);
  assert.match(formatEval(r), /Patch memory: PASSED/);
});

test('eval: patch memory fails when older patches no longer hold', async () => {
  // On the older patches Sterak's Gage and BotRK swapped roles: what won against AP lost against it.
  const r = await runMemory((g) => ({ ...g, items: [g.items[0] === 3053 ? 3153 : 3053, ...g.items.slice(1)] }));
  const m = r.memory!;
  // Early in a patch the two-patch model mostly keeps the lane baseline's pick, so either can come out ahead there by
  // chance; with all games the older patches only blur what today's games show, and the outcome predictions get worse.
  assert.ok(m.full.memory < m.full.classic, `all games: ${m.full.memory} vs ${m.full.classic}`);
  assert.ok(m.full.brier > m.full.classicBrier);
  assert.equal(m.gate, 'failed');
});

test('eval: patch memory needs games from older patches to be judged', async () => {
  const r = await run(true, false);
  assert.equal(r.memory!.olderGames, 0);
  assert.equal(r.memory!.gate, 'failed');
  assert.match(r.memory!.gateReason, /older patches/);
  assert.equal((await run(true, false, { memory: null })).memory, null);
});

test('eval: the purchases\' chances, computed once, are what scoring a game reads of them (0.12.0)', () => {
  const inputs = modelInputs(apAdGames(200, 20, 5, { effect: true, randomOpponent: true }));
  const games = prepareGames(inputs.rows, inputs.participants, data, new ChampionProfiles(inputs.totals, [], data), { startingItemWindowMs: 60_000 });
  assert.ok(games.some((g) => g.items.some((c) => c.x)) && games.some((g) => g.boots?.x), 'purchases with their state');
  // A fold with a state model and one without (its purchases score against the base rate).
  const model = { version: 1, features: STATE_FEATURES, beta: STATE_FEATURES.map((_, i) => ((i % 5) - 2) * 0.013), lambda: 1, rows: 0, games: 0, base: 0.53 } as unknown as StateModel;
  const models = [model, null];
  const chances = purchaseChances(games, models);
  models.forEach((m, f) => scoreGames(games, m).forEach((s, k) => {
    const at = (k * models.length + f) * 7;
    s.itemWpa.forEach((wpa, slot) => assert.equal(wpa, s.g.win - chances[at + slot]));
    if (s.bootsWpa !== null) assert.equal(s.bootsWpa, s.g.win - chances[at + 6]);
  }));
});

test('eval: fine-tuning tries each setting against the chosen model and reports them (0.15.0)', async () => {
  const r = await run(true, false, { tuning: { pilotStrengths: [0, 2], recencyDays: [14] }, ks: [30, 100], memory: null, scoring: false });
  assert.ok(r.tuning, 'reported');
  assert.deepEqual(r.tuning.grid.map((g) => `${g.setting} ${g.value}`), ['pilotStrength 0', 'pilotStrength 2', `itemK ${r.chosen.k === 30 ? 100 : 30}`, 'recencyDays 14']);
  assert.ok(['passed', 'failed'].includes(r.tuning.gate) && r.tuning.gateReason.length > 0);
  if (r.tuning.gate === 'passed') assert.ok(r.tuning.full && r.tuning.full.low > 0);
  assert.match(formatEval(r), /\nFine-tuning\n-----------\n/);
  assert.ok(r.lift.withinPilot && Number.isFinite(r.lift.withinPilot.delta));
  const off = await run(true, false, { tuning: null, memory: null, scoring: false });
  assert.equal(off.tuning, null);
  assert.equal(off.lift.delta, r.lift.delta, 'the rest of the report as without it');
});

test('eval: the within-pilot lift leaves out who follows: better pilots following say nothing (0.15.0)', () => {
  // Pilot A always follows the draft pick and wins more; pilot B never does. Across pilots following looks good; within
  // each pilot there is nothing to compare. Pilot C follows half the time, and following does better by 0.2.
  const o = (pilot: string, followed: boolean, wpa: number) => ({ pilot, key: 'core1' as const, wpa, followedDraft: followed, followedBaseline: !followed });
  const out = [
    ...Array.from({ length: 20 }, () => o('A', true, 0.3)), ...Array.from({ length: 20 }, () => o('B', false, -0.3)),
    ...Array.from({ length: 10 }, () => o('C', true, 0.1)), ...Array.from({ length: 10 }, () => o('C', false, -0.1)),
  ];
  const w = withinPilot(out);
  assert.ok(Math.abs(w.draft - 0.2) < 1e-9, `within C only: ${w.draft}`);
  assert.ok(Math.abs(w.baseline + 0.2) < 1e-9);
});
