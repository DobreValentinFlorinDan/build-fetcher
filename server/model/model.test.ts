// Stage B models on synthetic games: champion profiles (B1), the game-state model and its offline/live parity (B2),
// and the draft-aware recommender (B3), with its weights. Everything runs offline on the Data Dragon snapshot.
import assert from 'node:assert/strict';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { after, before, test } from 'node:test';
import { captureTimeline, participantRows } from '../capture.ts';
import { openDb } from '../db.ts';
import type { StaticData } from '../ddragon.ts';
import type { AllGameData, LiveApiPlayer } from '../engine/adaptive.ts';
import { buildExclusivityIndex, type ExclusivityIndex } from '../engine/exclusivity.ts';
import { recommend } from '../engine/recommend.ts';
import { nameIndex } from '../lcu/liveClient.ts';
import { IRELIA, makeMatch, makeTimeline } from '../test/fixtures.ts';
import { AD_TEAM, ALLIES, AP_TEAM, apAdGames, modelInputs, rng, type SynthGame } from '../test/modelFixtures.ts';
import { loadTestStaticData } from '../test/staticData.ts';
import { DRAFT_DIMS } from './context.ts';
import { prepareGames, scoreGames, trainState, type PreparedGame } from './dataset.ts';
import { buildDraftModel, recommendDraft, type DraftModel } from './draft.ts';
import { calibration, fitLogistic, scoreProbabilities } from './logistic.ts';
import { ChampionProfiles, teamMagicShare } from './profiles.ts';
import { stateFeatures, statesAt, stateFromLive, trainStateModel, winProbability, type GameStateInput } from './state.ts';
import { patchWeigher, pilotCapFactors, pilotWeight, regionWeight, WEIGHT_DEFAULTS } from './weights.ts';

let data: StaticData;
let excl: ExclusivityIndex;
before(() => { data = loadTestStaticData(); excl = buildExclusivityIndex(data); });

const root = fs.mkdtempSync(path.join(os.tmpdir(), 'irelia-model-'));
const dbs: { close(): void }[] = [];
after(() => { for (const d of dbs) d.close(); fs.rmSync(root, { recursive: true, force: true, maxRetries: 5 }); });

const SETTINGS = {
  minMatchupGames: 15, generalPriorWeight: 0.15, minLaneGames: 30, otherLaneWeight: 0.15,
  previousPatchWeight: 0.35, minCurrentPatchGames: 40, startingItemWindowMs: 60_000,
};

function model(games: SynthGame[]): { m: DraftModel; prepared: PreparedGame[]; inputs: ReturnType<typeof modelInputs> } {
  const inputs = modelInputs(games);
  const profiles = new ChampionProfiles(inputs.totals, [], data);
  const prepared = prepareGames(inputs.rows, inputs.participants, data, profiles, { startingItemWindowMs: 60_000 });
  const state = trainState(prepared);
  return { m: buildDraftModel(scoreGames(prepared, state), state, profiles), prepared, inputs };
}

// ------------------------------------------------------------------------------------------- B1 profiles

test('profiles: damage mix, healing and class shrinkage from the participants table', () => {
  const db = openDb(path.join(root, 'profiles.db'));
  dbs.push(db);
  const games = apAdGames(40, 10);
  for (const g of modelInputs(games).rows) {
    db.insertMatch({ id: g.match_id, platform: 'KR', patch: g.patch, version: g.game_version!, queue: 420, creation: g.created_at, duration: g.game_duration, hasIrelia: true });
  }
  // Participants as the capture stores them.
  for (const s of games) {
    const m = makeMatch({ id: s.id, champions: [...ALLIES, ...s.enemies] });
    for (const p of m.info.participants) {
      if (AP_TEAM.includes(p.championId)) { p.magicDamageDealtToChampions = 25_000; p.physicalDamageDealtToChampions = 1000; }
    }
    db.insertParticipants(s.id, participantRows(m));
  }
  const profiles = new ChampionProfiles(db.championTotals([data.patch]), db.olderChampionTotals([data.patch]), data);
  const syndra = profiles.get(134);
  assert.equal(syndra.games, 20);
  assert.ok(syndra.magicShare > 0.75, `Syndra deals mostly magic damage (${syndra.magicShare})`);
  assert.ok(syndra.ranged && !profiles.get(IRELIA).ranged);
  // Leona heals her allies in the fixture (4000 per game): the healing goes to allyHealPerMin.
  assert.ok(profiles.get(89).allyHealPerMin > 5 * profiles.get(134).allyHealPerMin);
  // A mage nobody played takes the mages' average; a champion with few games sits between its own data and its class.
  const brand = profiles.get(63);
  assert.equal(brand.games, 0);
  assert.ok(brand.magicShare > 0.5 && brand.magicShare < syndra.magicShare);
  assert.ok(teamMagicShare(AP_TEAM.map((id) => profiles.get(id))) > 0.7);
  assert.ok(teamMagicShare(AD_TEAM.map((id) => profiles.get(id))) < 0.5);
});

// ------------------------------------------------------------------------------------------- B2 state model

test('logistic regression recovers a known effect and is calibrated', () => {
  const r = rng(3);
  const X: number[][] = [], y: number[] = [];
  for (let i = 0; i < 4000; i++) {
    const x = r() * 4 - 2;
    X.push([1, x]);
    y.push(r() < 1 / (1 + Math.exp(-(0.3 + 1.2 * x))) ? 1 : 0);
  }
  const fit = fitLogistic(X, y, { lambda: 0.1 });
  assert.ok(fit.converged);
  assert.ok(Math.abs(fit.beta[1] - 1.2) < 0.15, `slope ${fit.beta[1]}`);
  assert.ok(Math.abs(fit.beta[0] - 0.3) < 0.15, `intercept ${fit.beta[0]}`);
  const p = X.map((x) => 1 / (1 + Math.exp(-(fit.beta[0] + fit.beta[1] * x[1]))));
  const s = scoreProbabilities(p, y);
  assert.ok(s.brier < 0.22 && s.logLoss < 0.63);
  for (const b of calibration(p, y).filter((b) => b.n > 200)) assert.ok(Math.abs(b.predicted - b.observed) < 0.07);
});

test('state model: more gold means a higher win chance', () => {
  const r = rng(5);
  const team = (inv: number) => ({ inventory: inv, kills: 0, cs: 0, level: 9, towers: 0, inhibitors: 0, dragons: 0, elders: 0, barons: 0, grubsHeralds: 0 });
  const rows = Array.from({ length: 3000 }, () => {
    const lead = (r() - 0.5) * 8000;
    const s: GameStateInput = { t: 900 + r() * 900, us: team(15_000 + lead), them: team(15_000), me: null, opponent: null };
    return { x: stateFeatures(s), win: r() < 1 / (1 + Math.exp(-lead / 2000)) ? 1 : 0 };
  });
  const m = trainStateModel(rows, 300);
  const at = (lead: number) => winProbability(m, { t: 1200, us: team(15_000 + lead), them: team(15_000), me: null, opponent: null });
  assert.ok(at(4000) > 0.8 && at(-4000) < 0.2 && Math.abs(at(0) - 0.5) < 0.08, `${at(4000)} ${at(0)} ${at(-4000)}`);
});

test('the same moment gives the same state from a stored timeline and from the Live Client', () => {
  const match = makeMatch({ id: 'KR_1', minutes: 20 });
  const cap = captureTimeline(makeTimeline(match));
  const price = (id: number) => data.items.get(id)?.goldTotal ?? 0;
  const [offline] = statesAt({ cap, irelia: 1, opponent: 6, teamOf: (p) => (p <= 5 ? 100 : 200), priceOf: price }, [13 * 60_000]);
  // By 13:00 in the fixture: Irelia killed participant 6, team 200 took a dragon, team 100 a top tower; Irelia owns
  // Doran's Blade and a potion.
  assert.equal(offline.us.kills, 1);
  assert.equal(offline.them.dragons, 1);
  assert.equal(offline.us.towers, 1);
  assert.equal(offline.us.inventory, 500);
  assert.equal(offline.me!.inventory - offline.opponent!.inventory, 500);

  const frame = (pid: number) => cap.frames[String(pid)][13];
  const players: LiveApiPlayer[] = match.info.participants.map((p) => ({
    championName: data.champions.get(p.championId)!.key, riotId: `${p.puuid}#KR`, riotIdGameName: p.puuid,
    team: p.teamId === 100 ? 'ORDER' : 'CHAOS', position: p.teamPosition, level: frame(p.participantId)[3],
    items: p.participantId === 1 ? [{ itemID: 1055 }, { itemID: 2003 }] : [],
    scores: { kills: p.participantId === 1 ? 1 : 0, deaths: p.participantId === 6 ? 1 : 0, assists: 0, creepScore: frame(p.participantId)[4] },
  }));
  const raw: AllGameData = {
    activePlayer: { riotId: players[0].riotId, currentGold: 0 }, allPlayers: players, gameData: { gameTime: 780 },
    events: { Events: [
      { EventID: 1, EventName: 'ChampionKill', EventTime: 360, KillerName: players[0].riotIdGameName, VictimName: players[5].riotIdGameName },
      { EventID: 2, EventName: 'DragonKill', EventTime: 540, KillerName: players[6].riotIdGameName, DragonType: 'Fire' },
      { EventID: 3, EventName: 'TurretKilled', EventTime: 720, KillerName: players[0].riotIdGameName, TurretKilled: 'Turret_T2_L_03_A' },
    ] },
  };
  const index = nameIndex(players);
  const live = stateFromLive(raw, players[0], players[5], price, (n) => { const p = index(n); return p ? (p.team === 'ORDER' ? 100 : 200) : null; });
  assert.deepEqual(stateFeatures(live), stateFeatures(offline));
});

// ------------------------------------------------------------------------------------------- B3 draft model

test('weights: pilot class and tier, region, the 5 % pilot cap', () => {
  const g = (o: Partial<PreparedGame>) => ({ source: 'otp', tier: 'CHALLENGER', lp: 1000, mastery: 500_000, platform: 'KR', ...o }) as PreparedGame;
  assert.equal(pilotWeight(g({}), WEIGHT_DEFAULTS), 1);
  assert.ok(Math.abs(pilotWeight(g({ source: 'main' }), WEIGHT_DEFAULTS) - 0.8) < 1e-9);
  assert.ok(Math.abs(pilotWeight(g({ source: 'harvested', tier: 'UNKNOWN', lp: null }), WEIGHT_DEFAULTS) - 0.3) < 1e-9);
  assert.equal(pilotWeight(g({ source: 'me' }), WEIGHT_DEFAULTS), 0);
  assert.equal(regionWeight(g({ platform: 'EUW1' }), { ...WEIGHT_DEFAULTS, ownPlatform: 'EUW1' }), 1.5);
  assert.equal(regionWeight(g({ platform: 'NA1' }), { ...WEIGHT_DEFAULTS, regions: ['KR'] }), 0);

  // One pilot with half of 40 pilots' games is cut to 5 % of the lane.
  const games = [...Array.from({ length: 100 }, () => ({ pilot: 'whale', lane: 'TOP' as const, w: 1 })),
    ...Array.from({ length: 100 }, (_, i) => ({ pilot: `p${i % 40}`, lane: 'TOP' as const, w: 1 }))];
  const f = pilotCapFactors(games, 0.05);
  const total = games.reduce((s, x) => s + x.w * f.get(`TOP:${x.pilot}`)!, 0);
  assert.ok(Math.abs((100 * f.get('TOP:whale')!) / total - 0.05) < 1e-6);
  // With fewer than 20 pilots the cap rises to an equal share.
  const few = pilotCapFactors([{ pilot: 'a', lane: 'MIDDLE', w: 9 }, { pilot: 'b', lane: 'MIDDLE', w: 1 }], 0.05);
  assert.ok(Math.abs(9 * few.get('MIDDLE:a')! - 1) < 1e-6);
});

test('patch weights follow the Data Dragon diff, manual overrides and hotfixes', () => {
  const pg = (patch: string, created = 0) => ({ patch, platform: 'KR', created }) as PreparedGame;
  const fp = data.fingerprints;
  const same = { ...data, previous: { version: '16.18.1', items: new Map(fp.items), champion: fp.champion } };
  const w = patchWeigher(same, () => undefined, WEIGHT_DEFAULTS);
  assert.equal(w(pg('16.19')), 1);
  assert.equal(w(pg('16.18'), [3153]), 0.8);
  assert.equal(w(pg('16.17')), 0);
  const changedItems = new Map(fp.items);
  changedItems.set(3153, 'nerfed');
  const itemChanged = patchWeigher({ ...data, previous: { version: '16.18.1', items: changedItems, champion: fp.champion } }, () => undefined, WEIGHT_DEFAULTS);
  assert.equal(itemChanged(pg('16.18'), [3153]), 0.2);
  assert.equal(itemChanged(pg('16.18'), [3078]), 0.8);
  const ireliaChanged = patchWeigher({ ...data, previous: { version: '16.18.1', items: fp.items, champion: 'old' } }, () => undefined, WEIGHT_DEFAULTS);
  assert.equal(ireliaChanged(pg('16.18')), 0.2);
  assert.equal(patchWeigher(data, () => undefined, WEIGHT_DEFAULTS)(pg('16.18')), 0.35);   // no previous files: unknown
  const opts = { ...WEIGHT_DEFAULTS, patchWeights: { '16.18': 0.6 }, hotfixes: [{ patch: '16.19', date: '2026-09-26T00:00:00Z' }] };
  const manual = patchWeigher(same, () => undefined, opts);
  assert.equal(manual(pg('16.18'), [3153]), 0.6);
  assert.equal(manual(pg('16.19', Date.UTC(2026, 8, 25))), 0.5);
  assert.equal(manual(pg('16.19', Date.UTC(2026, 8, 27))), 1);
});

test('draft model: the item that wins against the enemy damage type is picked, with the reason', () => {
  const games = apAdGames(800, 40);
  const { m, inputs } = model(games);
  assert.ok(m.state, 'state model trained');
  const baseline = recommend(inputs.rows, data, excl, null, SETTINGS, 'TOP');
  const vsAp = recommendDraft(m, { lane: 'TOP', opponent: AP_TEAM[0], enemies: AP_TEAM, allies: ALLIES.slice(1) }, data, excl, () => undefined, baseline);
  const vsAd = recommendDraft(m, { lane: 'TOP', opponent: AD_TEAM[0], enemies: AD_TEAM, allies: ALLIES.slice(1) }, data, excl, () => undefined, baseline);
  const first = (r: typeof vsAp) => r.picks.find((p) => p.key === 'core1')!;
  assert.equal(first(vsAp).value, '3053', JSON.stringify(first(vsAp)));
  assert.equal(first(vsAd).value, '3153', JSON.stringify(first(vsAd)));
  // Whichever the lane-level pick is, the other draft moved away from it, and says why.
  const moved = [first(vsAp), first(vsAd)].find((p) => p.reason);
  assert.ok(moved, 'one of the two picks differs from the lane-level pick');
  assert.match(moved!.reason!, /magic damage/);
  // Each pick names the lane build's choice it replaces, with that choice's lift in this draft.
  assert.equal(first(vsAp).baseline?.value, String(baseline.coreBuild[0].value[0]));
  assert.ok(first(vsAp).baseline?.lift !== undefined);
  assert.ok(first(vsAp).stats!.lift > 0 && first(vsAp).stats!.low < first(vsAp).stats!.lift);
  // The assembled build is in the baseline's shape and never holds two exclusive items.
  assert.equal(vsAp.build.coreBuild[0].value[0], 3053);
  assert.equal(excl.conflicts([...vsAp.build.coreBuild[0].value, vsAp.build.boots[0].value]).length, 0);
  assert.equal(vsAp.build.runes!.value.perks.length, 6);
  assert.equal(vsAp.build.skills!.value.path.length, 18);
  assert.ok(vsAp.context.some((c) => c.key === 'eMagic' && c.z > 0.5));
  assert.equal(DRAFT_DIMS.length, m.zDraft.d);
});

test('draft model: too few similar games keep the lane baseline, flagged', () => {
  const games = apAdGames(24, 12);
  const { m, inputs } = model(games);
  const baseline = recommend(inputs.rows, data, excl, null, SETTINGS, 'TOP');
  const r = recommendDraft(m, { lane: 'TOP', opponent: null, enemies: AP_TEAM, allies: [] }, data, excl, () => undefined, baseline);
  const core1 = r.picks.find((p) => p.key === 'core1')!;
  assert.ok(core1.fallback);
  assert.equal(core1.value, String(baseline.coreBuild[0].value[0]));
  assert.equal(core1.changed, false);
});

test('the user\'s own games never feed the draft model', () => {
  const games = apAdGames(200, 20);
  const { m } = model(games);
  const own = m.scored.map((s) => ({ ...s, g: { ...s.g, source: 'me' as const } }));
  const mOwn = { ...m, scored: own };
  const baseline = recommend(modelInputs(games.slice(0, 50)).rows, data, excl, null, SETTINGS, 'TOP');
  const r = recommendDraft(mOwn, { lane: 'TOP', opponent: null, enemies: AP_TEAM, allies: [] }, data, excl, () => undefined, baseline);
  assert.equal(r.sample.games, 0);
  assert.ok(r.picks.every((p) => p.stats === null || p.fallback));
});

// ------------------------------------------------------------------------------------------- fine-tuning (0.15.0)

test('fine-tuning: pilot standing\'s strength, the fade within a patch, and the item decisions\' own prior', async () => {
  const { pilotWeight, recencyWeigher, WEIGHT_DEFAULTS } = await import('./weights.ts');
  const { DRAFT_DEFAULTS } = await import('./draft.ts');
  const g = (o: Partial<PreparedGame>) => ({ source: 'otp', tier: 'DIAMOND', lp: null, mastery: 100_000, created: 0, patch: '16.21', platform: 'KR', ...o }) as PreparedGame;
  const w1 = pilotWeight(g({}), WEIGHT_DEFAULTS);
  assert.ok(w1 > 0 && w1 < 1);
  assert.equal(pilotWeight(g({}), { ...WEIGHT_DEFAULTS, pilotStrength: 1 }), w1, '1: as before');
  assert.equal(pilotWeight(g({}), { ...WEIGHT_DEFAULTS, pilotStrength: 0 }), 1, '0: every pilot alike');
  assert.ok(Math.abs(pilotWeight(g({}), { ...WEIGHT_DEFAULTS, pilotStrength: 2 }) - w1 * w1) < 1e-12);
  assert.equal(pilotWeight(g({ source: 'me' }), { ...WEIGHT_DEFAULTS, pilotStrength: 0 }), 0, 'your own games never count');

  const DAY = 86_400_000;
  const fade = recencyWeigher(30 * DAY, { recencyDays: 7 }, (x) => (x.patch === '16.21' ? 0 : 1));
  assert.equal(fade(g({ created: 30 * DAY })), 1);
  assert.ok(Math.abs(fade(g({ created: 23 * DAY })) - 0.5) < 1e-12, 'half after 7 days');
  assert.equal(fade(g({ created: 2 * DAY, patch: '16.20' })), 1, 'older patches: patch memory fades them');
  assert.equal(recencyWeigher(30 * DAY, { recencyDays: null }, () => 0)(g({ created: 0 })), 1, 'none by default');

  // The item decisions' prior shrinks the item estimates alone.
  const games = apAdGames(400, 40);
  const { m, inputs } = model(games);
  const baseline = recommend(inputs.rows, data, excl, null, SETTINGS, 'TOP');
  const q = { lane: 'TOP' as const, opponent: null, enemies: AP_TEAM, allies: [] };
  const est = (itemK?: number) => recommendDraft(m, q, data, excl, () => undefined, baseline,
    { ...DRAFT_DEFAULTS, ...(itemK !== undefined ? { itemK } : {}) }, { reasons: false, detail: true }).estimates!;
  const plain = est(), tight = est(100_000);
  assert.deepEqual(tight.keystone, plain.keystone, 'pre-game decisions keep the shared prior');
  const spread = (e: Record<string, number>) => Math.max(...Object.values(e)) - Math.min(...Object.values(e));
  assert.ok(spread(tight.core1) < spread(plain.core1), 'a much stronger item prior pulls the item options together');
});
