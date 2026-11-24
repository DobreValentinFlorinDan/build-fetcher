// Late-game swaps (0.17.0, docs/SWAPS.md): what a shop visit with every slot full decides, the advice the draft model
// gives from such decisions, and the Evaluate builds gate on them.
import assert from 'node:assert/strict';
import { before, test } from 'node:test';
import { ITEM_EVENT, type TimelineCapture } from '../capture.ts';
import type { StaticData } from '../ddragon.ts';
import { buildExclusivityIndex, type ExclusivityIndex } from '../engine/exclusivity.ts';
import { AP_TEAM, apAdGames, modelInputs, rng } from '../test/modelFixtures.ts';
import { loadTestStaticData } from '../test/staticData.ts';
import { prepareGames, scoreGames, type PreparedGame } from './dataset.ts';
import { adviseSwap, buildDraftModel, itemQueryOf, weighGames } from './draft.ts';
import { ESTIMATE_DEFAULTS } from './estimate.ts';
import { updateFrom, type BaseHeader } from './evalBase.ts';
import { EVAL_DEFAULTS, evaluate, formatEval, type EvalBaseData, type EvalOptions } from './evaluate.ts';
import { ChampionProfiles } from './profiles.ts';
import { KEEP, SELL, swapDecisions } from './swaps.ts';
import { WEIGHT_DEFAULTS } from './weights.ts';

let data: StaticData;
let excl: ExclusivityIndex;
before(() => { data = loadTestStaticData(); excl = buildExclusivityIndex(data); });

const MIN = 60_000;
const BOOTS = 3047, BORK = 3153, TRINITY = 3078, STERAK = 3053, DANCE = 6333, CLEAVER = 3071, GA = 3026, VISAGE = 3065, PICKAXE = 1037;
const { BUY, SELL: SOLD, DESTROY, UNDO } = ITEM_EVENT;

test('swaps: a full inventory\'s shop visits from 20:00, what became of each item held, an undo and components taken into account', () => {
  const at = (minute: number, kind: number, item: number, after = 0) => [minute * MIN, 1, kind, item, after];
  const cap = {
    items: [
      at(10, BUY, BOOTS), at(12, BUY, BORK), at(15, BUY, TRINITY), at(18, BUY, STERAK), at(21, BUY, DANCE), at(24, BUY, CLEAVER),
      [24.5 * MIN, 2, BUY, GA, 0],                                   // someone else's purchase: not hers
      at(26, BUY, 2003), [26 * MIN + 10_000, 1, DESTROY, 2003, 0],   // a visit with every slot full: everything kept
      at(30, SOLD, BOOTS), [30 * MIN + 5_000, 1, BUY, PICKAXE, 0],   // boots sold for a component...
      at(33, BUY, GA), [33 * MIN + 1, 1, DESTROY, PICKAXE, 0],      // ...and Guardian Angel finished: not full before
      at(36, SOLD, CLEAVER), [36 * MIN + 3_000, 1, UNDO, 0, CLEAVER], // sold and taken back: kept
      at(38, SOLD, BORK),                                            // sold, nothing finished after
    ],
  } as unknown as TimelineCapture;
  const d = swapDecisions(cap, 1, (id) => data.items.get(id));
  const points = [...new Set(d.map((x) => x.t / MIN))];
  assert.deepEqual(points, [26, 30, 36, 38], 'the visits with every slot full, from 20:00');
  assert.ok(d.every((x) => x.w === 1 / 4), 'the game\'s points share one game\'s weight');
  const of = (minute: number) => Object.fromEntries(d.filter((x) => x.t === minute * MIN).map((x) => [x.held, x.option]));
  assert.deepEqual(of(26), { [BOOTS]: KEEP, [BORK]: KEEP, [TRINITY]: KEEP, [STERAK]: KEEP, [DANCE]: KEEP, [CLEAVER]: KEEP });
  assert.deepEqual(of(30), { [BOOTS]: GA, [BORK]: KEEP, [TRINITY]: KEEP, [STERAK]: KEEP, [DANCE]: KEEP, [CLEAVER]: KEEP }, 'boots for the next legendary');
  assert.deepEqual(of(36), { [BORK]: KEEP, [TRINITY]: KEEP, [STERAK]: KEEP, [DANCE]: KEEP, [CLEAVER]: KEEP, [GA]: KEEP }, 'the boots are gone; the sell undone');
  assert.equal(of(38)[BORK], SELL, 'nothing finished within 10 minutes');
  assert.deepEqual(swapDecisions(cap, 1, (id) => data.items.get(id), { fromMinute: 31, visitGapMs: 30_000, replaceWithinMs: 10 * MIN, fullSlots: 6 }).map((x) => x.t / MIN)
    .filter((t, i, a) => a.indexOf(t) === i), [36, 38], 'from the minute set');
});

/**
 * Synthetic games with one late decision each about their boots: against the AP team (`effect`), swapping them for
 * Spirit Visage goes with winning; against the AD team, and without `effect`, it says nothing.
 */
function withSwaps(effect: boolean, seed: number) {
  const r = rng(seed);
  return (games: PreparedGame[]) => {
    for (const g of games) {
      const ap = g.enemies.some((id) => AP_TEAM.slice(1).includes(id));
      const p = effect && ap ? (g.win ? 0.75 : 0.25) : 0.5;
      g.swaps = [{ held: BOOTS, item: r() < p ? VISAGE : KEEP, t: 30 * MIN, w: 1, x: null, live: [NaN, NaN, NaN, NaN, NaN, NaN, NaN, 30] }];
    }
    return games;
  };
}

test('swaps: the advice is the option that did better in drafts like this one, else keep', () => {
  const inputs = modelInputs(apAdGames(800, 40, 21, { effect: true, randomOpponent: true }));
  const profiles = new ChampionProfiles(inputs.totals, [], data);
  const games = withSwaps(true, 3)(prepareGames(inputs.rows, inputs.participants, data, profiles, { startingItemWindowMs: 60_000 }));
  const model = buildDraftModel(scoreGames(games, null), null, profiles);
  const weighted = weighGames(model, data, () => undefined, WEIGHT_DEFAULTS);
  const o = { ...ESTIMATE_DEFAULTS, h: 0.7, k: 100 };
  const advice = (enemies: number[]) => adviseSwap(model, weighted, itemQueryOf(model, { lane: 'TOP', opponent: null, enemies, allies: [] }, o.h), BOOTS, o);
  const ap = advice(AP_TEAM)!;
  assert.deepEqual(ap.pick, { kind: 'swap', item: VISAGE });
  assert.ok(ap.lift! > 0.1 && ap.low! < ap.lift! && ap.high! > ap.lift!, `lift ${ap.lift} (${ap.low} to ${ap.high})`);
  assert.equal(ap.minute, 30);
  const ad = advice(games.find((g) => !g.enemies.some((id) => AP_TEAM.slice(1).includes(id)))!.enemies)!;
  assert.ok(ad.pick.kind === 'keep' || (ad.lift ?? 0) < ap.lift!, 'against the AD team, no such lead');
  assert.equal(adviseSwap(model, weighted, itemQueryOf(model, { lane: 'TOP', opponent: null, enemies: AP_TEAM, allies: [] }, o.h), GA, o), null, 'no game held it');
});

const ENGINE = {
  minMatchupGames: 15, generalPriorWeight: 0.15, minLaneGames: 30, otherLaneWeight: 0.15,
  previousPatchWeight: 0.35, minCurrentPatchGames: 40, startingItemWindowMs: 60_000,
};
const OPTS: EvalOptions = { ...EVAL_DEFAULTS, engine: ENGINE, lambdas: [1], bandwidths: [0.7], ks: [100], bootstrap: 300, live: false, memory: null, scoring: false, tuning: null };

async function gate(effect: boolean) {
  const inputs = modelInputs(apAdGames(800, 40, 21, { effect: true, randomOpponent: true }));
  const profiles = new ChampionProfiles(inputs.totals, [], data);
  const inject = withSwaps(effect, 5);
  return evaluate({
    ...inputs, data, excl, profiles, regionPatch: () => undefined, startingItemWindowMs: 60_000,
    prepare: async (rows, participants, frames) => inject(prepareGames(rows, participants, data, profiles, { startingItemWindowMs: 60_000, frames })),
  }, OPTS);
}

test('swaps: Evaluate builds passes the advice when following it does better than keeping, and fails it when it says nothing', async () => {
  const r = await gate(true);
  assert.ok(r.swaps, 'reported');
  assert.equal(r.swaps!.situations, 800);
  assert.equal(r.swaps!.games, 800);
  assert.ok(r.swaps!.advised > 0.2, `advised to sell in ${r.swaps!.advised}`);
  assert.equal(r.swaps!.gate, 'passed', r.swaps!.gateReason);
  assert.match(formatEval(r), /Late-game swaps: PASSED/);
  const none = await gate(false);
  assert.equal(none.swaps!.gate, 'failed', none.swaps!.gateReason);
});

test('swaps: a quick update adds the new games\' situations to its base\'s, and with none new it is the full evaluation\'s', async () => {
  const all = apAdGames(800, 40, 21, { effect: true, randomOpponent: true });
  const run = async (games: typeof all, base?: EvalBaseData) => {
    const inputs = modelInputs(games);
    const profiles = new ChampionProfiles(inputs.totals, [], data);
    const inject = withSwaps(true, 5);
    let next: EvalBaseData | null = null;
    const header = { fullHeldOut: base?.known.length ?? 0 } as BaseHeader;
    const report = await evaluate({
      ...inputs, data, excl, profiles, regionPatch: () => undefined, startingItemWindowMs: 60_000, salt: 's', onBase: (b) => { next = b; },
      ...(base ? { base } : {}),
      prepare: async (rows, participants, frames) => inject(prepareGames(rows, participants, data, profiles, { startingItemWindowMs: 60_000, frames })),
    }, base ? { ...OPTS, update: updateFrom(base, header, OPTS.maxGridGames) } : OPTS);
    return { report, base: next! as EvalBaseData };
  };
  const full = await run(all);
  const again = await run(all, full.base);
  assert.deepEqual(again.report.swaps, full.report.swaps);
  const first = await run(all.filter((_, i) => i % 5 !== 0));
  const quick = await run(all, first.base);
  assert.deepEqual([quick.report.swaps!.situations, quick.report.swaps!.games], [800, 800], 'every game\'s situations once');
});
