// Quick updates (0.16.0, docs/EVAL-UPDATE.md): what a quick update adds to its base is what a full evaluation measures
// for the new games, and with no new game it is the full evaluation again.
import assert from 'node:assert/strict';
import { before, test } from 'node:test';
import type { StaticData } from '../ddragon.ts';
import { buildExclusivityIndex, type ExclusivityIndex } from '../engine/exclusivity.ts';
import { apAdGames, modelInputs, type SynthGame } from '../test/modelFixtures.ts';
import { loadTestStaticData } from '../test/staticData.ts';
import { keepVerdicts, planEvaluation, UPDATE_RULES, updateFrom, verdictShift, type BaseHeader } from './evalBase.ts';
import type { Db } from '../db.ts';
import type { EvalSummary } from './service.ts';
import {
  EVAL_DEFAULTS, evalSteps, evaluate, groupOutcomes, mergeOutcomes, withinPilot, withinPilotOf, type EvalBaseData, type EvalOptions, type EvalProgress, type EvalReport,
} from './evaluate.ts';
import { ChampionProfiles } from './profiles.ts';

let data: StaticData;
let excl: ExclusivityIndex;
before(() => { data = loadTestStaticData(); excl = buildExclusivityIndex(data); });

const ENGINE = {
  minMatchupGames: 15, generalPriorWeight: 0.15, minLaneGames: 30, otherLaneWeight: 0.15,
  previousPatchWeight: 0.35, minCurrentPatchGames: 40, startingItemWindowMs: 60_000,
};
const OPTS: EvalOptions = { ...EVAL_DEFAULTS, engine: ENGINE, lambdas: [1], bandwidths: [0.5, 0.7], ks: [30, 100], bootstrap: 300, live: false };

async function run(games: SynthGame[], o: { base?: EvalBaseData; progress?: (p: EvalProgress) => void } = {}) {
  const inputs = modelInputs(games);
  let next: EvalBaseData | null = null;
  const header: BaseHeader = { ...HEADER, fullHeldOut: o.base?.known.length ?? 0, known: o.base?.known.length ?? 0 };
  const opts = o.base ? { ...OPTS, update: updateFrom(o.base, header, OPTS.maxGridGames) } : OPTS;
  const report = await evaluate({
    ...inputs, data, excl, profiles: new ChampionProfiles(inputs.totals, [], data), regionPatch: () => undefined, startingItemWindowMs: 60_000,
    base: o.base, salt: o.base?.salt ?? 'test-salt', onBase: (b) => { next = b; }, progress: o.progress,
  }, opts);
  return { report, base: next! as EvalBaseData, opts };
}

/** A report without what differs between two runs on the same data by design. */
const comparable = (r: EvalReport) => { const { evaluatedAt: _, update: _u, ...rest } = r; return JSON.parse(JSON.stringify(rest)); };

const GAMES = apAdGames(800, 40, 21, { effect: true, randomOpponent: true });

const HEADER: BaseHeader = {
  logic: 1, fullAt: 0, fullMark: { row: 0, games: 0, patch: null }, fullHeldOut: 0, fullPatch: null, fullCurrent: 0,
  lastAt: 0, lastMark: { row: 0, games: 0, patch: null }, known: 0, patches: [], regions: [], key: '',
};

test('quick update: with no new game it is the full evaluation again, number for number', async () => {
  const full = await run(GAMES);
  assert.equal(full.report.gate, 'passed', full.report.gateReason);
  assert.equal(full.base.known.length, 800, 'every held-out game measured, by its hash');
  assert.ok(full.base.final.g.pilots.every((p) => !p.startsWith('pilot')), 'the base names pilots by a hash only');
  const again = await run(GAMES, { base: full.base });
  assert.deepEqual(again.report.update?.newGames, 0);
  assert.deepEqual(comparable(again.report), comparable(full.report));
  assert.deepEqual(verdictShift(full.report, again.report), { decisive: null, close: [] });
  assert.equal(again.base.known.length, 800);
});

test('quick update: the new games are added to the base, every gate decided on all of them', async () => {
  const older = GAMES.filter((_, i) => i % 5 !== 0);
  const first = await run(older);
  const steps: EvalProgress[] = [];
  const quick = await run(GAMES, { base: first.base, progress: (p) => { if (p.part === undefined) steps.push(p); } });
  const full = await run(GAMES);
  assert.equal(quick.report.update?.newGames, 160);
  assert.equal(quick.report.update?.missing, 0);
  assert.equal(quick.base.known.length, 800);
  // Every game's outcomes once: the decisions' counts are the full evaluation's.
  assert.deepEqual(quick.report.decisions.map((d) => d.n), full.report.decisions.map((d) => d.n));
  assert.equal(quick.report.brier.n, full.report.brier.n);
  assert.equal(quick.report.pilots, full.report.pilots);
  // The base's grid and choice stand; the verdicts here are the full evaluation's.
  assert.deepEqual(quick.report.grid, first.report.grid);
  assert.deepEqual(quick.report.chosen, first.report.chosen);
  assert.deepEqual(verdictShift(full.report, quick.report), { decisive: null, close: [] });
  assert.ok(Math.abs(quick.report.lift.delta - full.report.lift.delta) < 0.02, `${quick.report.lift.delta} against ${full.report.lift.delta}`);
  // Its steps, as evalSteps counts them, in order.
  assert.equal(steps.length, evalSteps(quick.opts));
  assert.deepEqual(steps.map((s) => s.step), steps.map((_, i) => i + 1));
  assert.ok(steps.some((s) => /160 new held-out games/.test(s.label)), steps.map((s) => s.label).join(' | '));
});

test('quick update: games the base measured that are gone still count, and are reported', async () => {
  const first = await run(GAMES);
  const fewer = await run(GAMES.slice(0, 760), { base: first.base });
  assert.equal(fewer.report.update?.newGames, 0);
  assert.equal(fewer.report.update?.missing, 40);
  assert.deepEqual(fewer.report.decisions.map((d) => d.n), first.report.decisions.map((d) => d.n));
});

test('merged outcomes are what grouping them all at once gives, and the within-pilot lift reads them alike', () => {
  const o = (pilot: string, key: 'keystone' | 'core1', wpa: number, d: boolean, b: boolean) => ({ pilot, key, wpa, followedDraft: d, followedBaseline: b });
  const a = [o('x', 'keystone', 0.1, true, false), o('y', 'core1', -0.2, false, true), o('x', 'core1', 0.3, true, true)];
  const b = [o('z', 'keystone', 0.05, false, false), o('x', 'keystone', -0.1, true, true), o('z', 'core1', 0.2, true, false)];
  const merged = mergeOutcomes(groupOutcomes(a), groupOutcomes(b), (p) => p);
  assert.deepEqual(merged, groupOutcomes([...a, ...b]));
  const many = Array.from({ length: 400 }, (_, i) => o(`p${i % 23}`, i % 3 ? 'keystone' : 'core1', Math.sin(i), i % 4 === 0, i % 7 < 3));
  assert.deepEqual(withinPilotOf(groupOutcomes(many)), withinPilot(many));
});

test('a verdict the new games only move onto the line stands; one they move clearly across goes to a full evaluation', () => {
  const report = (low: number, high: number, gate: 'passed' | 'failed') => ({ gate, gateReason: 'x', lift: { draft: 0, baseline: 0, delta: (low + high) / 2, low, high } }) as unknown as EvalReport;
  const passed = report(0.001, 0.009, 'passed');
  // Half-width 0.004: the margin is a quarter of it, 0.001.
  assert.deepEqual(verdictShift(passed, report(-0.0005, 0.0075, 'failed')), { decisive: null, close: ['the draft-adjusted builds'] });
  assert.deepEqual(verdictShift(passed, report(-0.004, 0.004, 'failed')), { decisive: 'the draft-adjusted builds', close: [] });
  assert.deepEqual(verdictShift(passed, report(0.002, 0.01, 'passed')), { decisive: null, close: [] });
  const kept = keepVerdicts(passed, report(-0.0005, 0.0075, 'failed'), ['the draft-adjusted builds']);
  assert.equal(kept.gate, 'passed');
  assert.match(kept.gateReason, /stands/);
  assert.equal(UPDATE_RULES.margin, 0.25);
});

test('planEvaluation: quick while the full evaluation still describes the games, full on a new patch, a region moving, growth or age', () => {
  const day = 86_400_000, now = 100 * day;
  let rows = 1000, current = 100;
  const db = {
    gameMark: () => ({ row: rows, games: rows }), gamesAfterRow: (row: number) => Math.max(0, rows - row), gameVersions: () => [],
    modelGameCount: () => current,
  } as unknown as Db;
  const mark = { row: 1000, games: 1000, patch: data.patch };
  const header: BaseHeader = {
    ...HEADER, fullAt: now - day, fullMark: mark, fullHeldOut: 1000, fullPatch: data.patch, fullCurrent: 100, lastAt: now - day, lastMark: mark,
    known: 1000, patches: planReads(), regions: [], key: 'k',
  };
  const summary = { evaluatedAt: now - day, mark } as EvalSummary;
  const plan = (o: Partial<{ header: BaseHeader; key: string; at: number }> = {}) => planEvaluation(db, data, o.at ?? now, { header: o.header ?? header, summary, key: o.key ?? 'k' });
  assert.equal(plan().path, 'none');
  rows = 1050; current = 110;
  assert.deepEqual([plan().path, plan().newGames], ['update', 50]);
  assert.equal(plan({ key: 'other' }).path, 'full');
  assert.match(plan({ at: now + 7 * day }).reason, /over 7 days old/);
  assert.match(plan({ header: { ...header, patches: header.patches.slice(1) } }).reason, /new patch|patches/);
  assert.match(plan({ header: { ...header, regions: [['KR', { patch: '1.1', previousPatch: '1.0' }]] } }).reason, /regions/);
  // The current patch: over 30 % more than the full evaluation had.
  current = 131;
  assert.match(plan().reason, /31 more games of patch/);
  current = 110; rows = 1201;
  assert.match(plan().reason, /over 20 % more/);
  assert.equal(planEvaluation(db, data, now, { header: null, summary, key: 'k' }).path, 'full');
});

/** The patches an evaluation reads on the test data, with no games stored. */
function planReads() {
  return [...new Set([data.patch, data.previousPatch])];
}
