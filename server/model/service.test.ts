// The draft model inside the app (model/service.ts) on a real database: built from SQL, the lane baseline by default
// until an evaluation passes, the user's switch winning over both, and the user's own games kept out of the model.
import assert from 'node:assert/strict';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { after, test } from 'node:test';

// Settings and evaluation results go to a temporary folder: appConfig.ts reads this when it loads.
const root = fs.mkdtempSync(path.join(os.tmpdir(), 'irelia-service-'));
process.env.IRELIA_CONFIG_DIR = path.join(root, 'config');

const { captureTimeline, participantRows, CAPTURE_VERSION } = await import('../capture.ts');
const { openDb } = await import('../db.ts');
const { buildExclusivityIndex } = await import('../engine/exclusivity.ts');
const { recommend } = await import('../engine/recommend.ts');
const { captureGame } = await import('../scraper/ingest.ts');
const { DEFAULT_SCOPE } = await import('../scrapeScope.ts');
const { AP_TEAM, apAdGames, synthMatch } = await import('../test/modelFixtures.ts');
const { loadTestStaticData } = await import('../test/staticData.ts');
const { beforeWindow, evalDir, ModelService, modelPatches } = await import('./service.ts');
const { updateAppConfig } = await import('../appConfig.ts');
const { prepareGames, scoreGames, trainState } = await import('./dataset.ts');
const { buildDraftModel, recommendDraft } = await import('./draft.ts');
const { BuyIndex } = await import('./buy.ts');
const { ChampionProfiles } = await import('./profiles.ts');

const db = openDb(path.join(root, 'irelia.db'));
after(() => { db.close(); fs.rmSync(root, { recursive: true, force: true, maxRetries: 5 }); });

const SETTINGS = {
  minMatchupGames: 15, generalPriorWeight: 0.15, minLaneGames: 30, otherLaneWeight: 0.15,
  previousPatchWeight: 0.35, minCurrentPatchGames: 40, startingItemWindowMs: 60_000,
};

test('service: builds from the database, keeps the baseline until an evaluation passes, and the switch wins', () => {
  const data = loadTestStaticData();
  const excl = buildExclusivityIndex(data);
  const games = [...apAdGames(300, 30), ...apAdGames(4, 1, 9).map((g, i) => ({ ...g, id: `KR_${90_000 + i}`, pilot: 'ME' }))];
  db.tx(() => {
    for (const s of games) {
      const { match, timeline } = synthMatch(s);
      const cap = captureTimeline(timeline);
      const { row } = captureGame(match, timeline, match.info.participants[0], s.pilot === 'ME' ? 'me' : 'otp', undefined);
      db.insertMatch({ id: s.id, platform: 'KR', patch: data.patch, version: match.info.gameVersion, queue: 420, creation: match.info.gameCreation, duration: match.info.gameDuration, hasIrelia: true });
      db.insertGame(row);
      db.insertParticipants(s.id, participantRows(match));
      db.insertTimeline(s.id, cap.interval, JSON.stringify(cap));
      db.setQuality(s.id, false, 'GameComplete', CAPTURE_VERSION);
    }
  });
  const service = new ModelService({
    db, data: () => data, excl: () => excl, scope: () => DEFAULT_SCOPE, regionPatch: () => () => undefined,
    ownPlatform: () => 'KR', busy: () => false, startingItemWindowMs: 60_000,
  });
  const baseline = recommend(db.gamesForPatches([data.patch]), data, excl, null, SETTINGS, 'TOP');
  assert.equal(service.draft({ lane: 'TOP', opponent: null, enemies: AP_TEAM, allies: [], regions: null }, baseline).draft, null,
    'before the first build there is only the baseline: a request never waits on a build');
  assert.equal(service.needsBuild(), true);
  service.rebuild();
  assert.equal(service.needsBuild(), false, 'nothing changed since');
  const r = service.draft({ lane: 'TOP', opponent: null, enemies: AP_TEAM, allies: [], regions: null }, baseline);
  assert.ok(r.draft, 'a draft-adjusted build');
  assert.equal(r.draft!.sample.games, 300, 'your own four games are not in it');
  assert.equal(r.mode, 'baseline');
  assert.equal(r.model?.gate, 'not evaluated');
  assert.equal(r.model?.games, 300);
  assert.equal(service.current()!.prepared.filter((g) => g.source === 'me').length, 4, 'the personal layer still sees them');

  // Only the build reads each purchase's state features and raw live context: the kept model drops them, and gives the
  // same draft-adjusted build as a model that keeps them.
  const kept = service.current()!;
  assert.ok(kept.prepared.every((g) => [...g.items, ...(g.boots ? [g.boots] : []), ...(g.swaps ?? [])].every((c) => c.x === null && c.live.length === 0)));
  const patches = modelPatches(db, data);
  const profiles = new ChampionProfiles(db.championTotals(patches), db.olderChampionTotals(patches), data);
  const prepared = prepareGames(db.modelGames(patches, [...DEFAULT_SCOPE.draftSources, 'me']), db.modelParticipants(patches, [...DEFAULT_SCOPE.draftSources, 'me']), data, profiles, { startingItemWindowMs: 60_000 });
  const state = trainState(prepared);
  const full = buildDraftModel(scoreGames(prepared, state), state, profiles);
  assert.ok(prepared.some((g) => g.items.some((c) => c.x !== null)), 'this one keeps them');
  const q = { lane: 'TOP' as const, opponent: null, enemies: AP_TEAM, allies: [] };
  assert.deepEqual(r.draft, recommendDraft(full, q, data, excl, () => undefined, baseline, service.settings()));
  // The state frames go too; the best buy for your gold reads the win probability at each one instead, with the same answers.
  assert.ok(kept.prepared.every((g) => g.frames === null) && kept.prepared.some((g) => (g.pAt?.length ?? 0) > 10));
  for (const ask of [{ target: 3078, gold: 1300 }, { target: 3053, gold: 900 }, { target: 3153, gold: 2000 }]) {
    const query = { lane: 'TOP' as const, opponent: null, owned: [], ...ask };
    const best = new BuyIndex(kept.model.scored, kept.model.state, data).best(query);
    assert.ok(best && best.items.length > 0, `a purchase toward ${ask.target}`);
    assert.deepEqual(best, new BuyIndex(scoreGames(prepared, state), state, data).best(query));
  }

  fs.mkdirSync(evalDir(), { recursive: true });
  fs.writeFileSync(path.join(evalDir(), 'latest.json'), JSON.stringify({ evaluatedAt: Date.now(), gate: 'passed', bandwidth: 0.5, k: 30, lambda: 1, dataVersion: '' }));
  service.evaluated();
  assert.equal(service.mode(), 'draft');
  assert.deepEqual([service.settings().estimate.h, service.settings().estimate.k], [0.5, 30]);
  service.setMode('baseline');
  assert.equal(service.mode(), 'baseline');
  service.setMode(null);
  assert.equal(service.mode(), 'draft');
});

test('service: patch memory follows the evaluation unless switched, reads the older patches, and keeps the state model on two', () => {
  const data = loadTestStaticData();
  const excl = buildExclusivityIndex(data);
  // 200 games from three patches back (16.16), after the first test's 300 of today's patch.
  db.tx(() => {
    for (const s of apAdGames(200, 20, 5).map((g, i) => ({ ...g, id: `KR_${50_000 + i}`, version: '16.16.1.1' }))) {
      const { match, timeline } = synthMatch(s);
      const cap = captureTimeline(timeline);
      const { row } = captureGame(match, timeline, match.info.participants[0], 'otp', undefined);
      db.insertMatch({ id: s.id, platform: 'KR', patch: '16.16', version: match.info.gameVersion, queue: 420, creation: match.info.gameCreation, duration: match.info.gameDuration, hasIrelia: true });
      db.insertGame(row);
      db.insertParticipants(s.id, participantRows(match));
      db.insertTimeline(s.id, cap.interval, JSON.stringify(cap));
      db.setQuality(s.id, false, 'GameComplete', CAPTURE_VERSION);
    }
  });
  assert.ok(!modelPatches(db, data).includes('16.16'), 'two patches: not read');
  assert.ok(modelPatches(db, data, 6).includes('16.16'), 'within the window');
  const old = beforeWindow(db, data);
  assert.deepEqual([old('16.19'), old('16.16'), old('16.14'), old('16.13'), old('15.20')], [false, false, false, true, null]);

  const service = new ModelService({
    db, data: () => data, excl: () => excl, scope: () => DEFAULT_SCOPE, regionPatch: () => () => undefined,
    ownPlatform: () => 'KR', busy: () => false, startingItemWindowMs: 60_000,
  });
  service.rebuild();
  const off = service.info()!;
  assert.deepEqual([off.memory.active, off.memory.choice, off.memory.gate, off.games, off.memory.olderGames], [false, 'auto', 'not evaluated', 300, 0]);

  // The switch (saved by the main thread): on at once; the model holds it once rebuilt.
  updateAppConfig({ patchMemory: 'on' });
  service.optionsChanged();
  assert.equal(service.settings().weights.memory?.patches, 6);
  assert.deepEqual([service.info()!.memory.active, service.info()!.memory.inModel, service.needsBuild()], [true, false, true]);
  service.rebuild();
  const on = service.info()!;
  assert.deepEqual([on.memory.active, on.memory.choice, on.games, on.memory.olderGames, on.memory.decay, on.memory.inModel], [true, 'on', 500, 200, 0.75, true]);
  assert.equal(on.stateModel!.games, 300, "the game-state model learns from today's and the previous patch only");

  // An evaluation that found it better turns it on by itself, with its fade; Off wins over it.
  fs.writeFileSync(path.join(evalDir(), 'latest.json'), JSON.stringify({
    evaluatedAt: Date.now(), gate: 'failed', bandwidth: 0.7, k: 100, lambda: 1, dataVersion: '', memory: { gate: 'passed', decay: 0.9 },
  }));
  updateAppConfig({ patchMemory: undefined });
  service.evaluated();
  assert.deepEqual([service.info()!.memory.choice, service.settings().weights.memory?.decay], ['auto', 0.9]);
  updateAppConfig({ patchMemory: 'off' });
  service.optionsChanged();
  assert.equal(service.settings().weights.memory, null);
  updateAppConfig({ patchMemory: undefined });
  service.optionsChanged();
});

test('service: runes against the draft follow the evaluation unless switched, and change the pre-game scores only', () => {
  const data = loadTestStaticData();
  const excl = buildExclusivityIndex(data);
  const service = new ModelService({
    db, data: () => data, excl: () => excl, scope: () => DEFAULT_SCOPE, regionPatch: () => () => undefined,
    ownPlatform: () => 'KR', busy: () => false, startingItemWindowMs: 60_000,
  });
  const latest = (scoring: object) => fs.writeFileSync(path.join(evalDir(), 'latest.json'), JSON.stringify({
    evaluatedAt: Date.now(), gate: 'failed', bandwidth: 0.7, k: 100, lambda: 1, dataVersion: '', scoring,
  }));
  // Following the evaluation, which found no gain: off, every pre-game choice against the overall win rate.
  latest({ gate: 'failed', penalty: null });
  service.evaluated();
  service.rebuild();
  assert.deepEqual(service.info()!.scoring, { active: false, gate: 'failed', choice: 'auto', inModel: false });
  const overall = service.current()!.model.scored;
  assert.equal(new Set(overall.map((x) => x.wpa0 + (1 - x.g.win) * 2)).size, 2, 'one rate for every game: two values (won, lost)');

  // An evaluation that found it better turns it on, with the penalties it chose: only pre-game scores change.
  latest({ gate: 'passed', penalty: { lambda: 100, opponentLambda: 100 } });
  service.evaluated();
  service.rebuild();
  assert.deepEqual(service.info()!.scoring, { active: true, gate: 'passed', choice: 'auto', inModel: true });
  const fair = service.current()!.model.scored;
  const byId = new Map(overall.map((x) => [x.g.matchId, x]));
  assert.ok(fair.some((x) => Math.abs(x.wpa0 - byId.get(x.g.matchId)!.wpa0) > 1e-6), 'pre-game scores move');
  assert.ok(fair.every((x) => JSON.stringify(x.itemWpa) === JSON.stringify(byId.get(x.g.matchId)!.itemWpa)), 'item scores stay');

  // Your switch (saved by the main thread, which also stops following the evaluation): Off wins over the verdict.
  updateAppConfig({ pregameScoring: 'off', patchMemory: 'off' });
  service.optionsChanged();
  assert.deepEqual(service.info()!.scoring, { active: false, gate: 'passed', choice: 'off', inModel: true });
  updateAppConfig({ pregameScoring: undefined, patchMemory: undefined });
});

test('service: a switch moved during a build is what the next build is for', () => {
  const data = loadTestStaticData();
  const excl = buildExclusivityIndex(data);
  // The build reads the scope as it starts: the switch moves right then (saved by the main thread, which the model
  // thread hears of when the build is over; a long build also lets the settings' few-second cache lapse).
  let during: (() => void) | null = null;
  const service = new ModelService({
    db, data: () => data, excl: () => excl, scope: () => { const f = during; during = null; f?.(); return DEFAULT_SCOPE; },
    regionPatch: () => () => undefined, ownPlatform: () => 'KR', busy: () => false, startingItemWindowMs: 60_000,
  });
  service.rebuild();
  service.settings();
  during = () => { updateAppConfig({ patchMemory: 'on' }); service.optionsChanged(); };
  service.rebuild();
  const mid = service.info()!.memory;
  assert.deepEqual([mid.active, mid.inModel, service.needsBuild()], [true, false, true], 'built without it, so another build is due');
  service.rebuild();
  const done = service.info()!.memory;
  assert.deepEqual([done.active, done.inModel, service.needsBuild()], [true, true, false]);
  updateAppConfig({ patchMemory: undefined });
  service.optionsChanged();
});

test('service: champ-select drafts reuse the weights and observations, with the same answers, and let them go when idle', () => {
  const data = loadTestStaticData();
  const excl = buildExclusivityIndex(data);
  const service = new ModelService({
    db, data: () => data, excl: () => excl, scope: () => DEFAULT_SCOPE, regionPatch: () => () => undefined,
    ownPlatform: () => 'KR', busy: () => false, startingItemWindowMs: 60_000,
  });
  service.rebuild();
  const b = service.current()!;
  const baseline = recommend([], data, excl, null, SETTINGS, 'TOP', () => undefined);
  const drafts = [
    { lane: 'TOP' as const, opponent: 122, enemies: [122, ...AP_TEAM.slice(1)], allies: [], regions: null },
    { lane: 'TOP' as const, opponent: null, enemies: AP_TEAM.slice(0, 3), allies: [], regions: null },
    { lane: 'MIDDLE' as const, opponent: 134, enemies: AP_TEAM, allies: [], regions: null },
  ];
  for (const q of drafts) {
    const kept = service.draft(q, baseline).draft;
    const fresh = recommendDraft(b.model, q, data, excl, () => undefined, baseline, service.settings(q.regions));
    assert.deepEqual(kept, fresh);
  }
  const cache = () => (service as unknown as { draftCache: { cache: Map<string, unknown> } | null }).draftCache;
  assert.ok(cache() && cache()!.cache.size > 0, 'kept between drafts');
  assert.equal(service.trimCaches(), false, 'still kept a moment after the last draft');
  assert.equal(service.trimCaches(0), true);
  assert.equal(cache(), null, 'let go once idle');
  // Outside champ select: the same answer, nothing kept (and what the last champ select kept goes).
  service.draft(drafts[0], baseline);
  const outside = service.draft(drafts[1], baseline, false).draft;
  assert.deepEqual(outside, recommendDraft(b.model, drafts[1], data, excl, () => undefined, baseline, service.settings(null)));
  assert.equal(service.keepsDrafts(), false);
});

test('new data is due for a build at 1 % of the games (at least 50) after half an hour, or after six hours (0.15.0)', async () => {
  const { dataDue } = await import('./service.ts');
  const min = 60_000;
  assert.equal(dataDue('118000:1:90000:118000', '118000:1:90000:118000', 7 * 60 * min), false, 'nothing changed');
  assert.equal(dataDue('118000:1:90000:118000', '118080:2:90080:118080', 45 * min), false, '80 games: under 1 %');
  assert.equal(dataDue('118000:1:90000:118000', '119180:2:91180:119180', 45 * min), true, '1,180 games');
  assert.equal(dataDue('118000:1:90000:118000', '119180:2:91180:119180', 10 * min), false, 'but not before half an hour');
  assert.equal(dataDue('118000:1:90000:118000', '118001:2:90001:118001', 6 * 60 * min), true, 'six hours: any change');
  assert.equal(dataDue('2000:1:1500:2000', '2049:2:1549:2049', 31 * min), false, 'at least 50 games');
  assert.equal(dataDue('2000:1:1500:2000', '2000:1:1500:2050', 31 * min), true, 'a backfill of 50 captures counts too');
});

test('a build on helper threads is the same build, in the same order (0.15.0)', async () => {
  const data = loadTestStaticData();
  // The helpers load Data Dragon's files from their data folder, as the app's do.
  const snap = JSON.parse(fs.readFileSync(new URL('../test/ddragon-snapshot.json', import.meta.url), 'utf8'));
  const dataDir = path.join(root, 'data-threads');
  const dd = path.join(dataDir, 'ddragon', snap.version);
  fs.mkdirSync(dd, { recursive: true });
  for (const [file, raw] of [['item.json', snap.raw.item], ['champion.json', snap.raw.champion], ['runesReforged.json', snap.raw.runesReforged],
    ['summoner.json', snap.raw.summoner], ['Irelia.json', snap.raw.champ]]) fs.writeFileSync(path.join(dd, file), JSON.stringify(raw));
  const { buildFromDb, buildFromDbAsync } = await import('./service.ts');
  const one = buildFromDb(db, data, DEFAULT_SCOPE, 60_000);
  const many = await buildFromDbAsync(db, data, DEFAULT_SCOPE, 60_000, undefined, null, null, { dataDir, threads: 2, stop: () => false, minGames: 0 });
  assert.ok(one.prepared.length > 100);
  const plain = (b: typeof one) => JSON.stringify(b.prepared, (_k, v) => (v instanceof Float64Array || v instanceof Float32Array ? Array.from(v) : Number.isNaN(v) ? 'NaN' : v));
  assert.equal(plain(many), plain(one));
  assert.deepEqual(many.model.zDraft.flat, one.model.zDraft.flat);
  // Stopped between batches: no model.
  const { BuildCancelled } = await import('./prepareTeam.ts');
  await assert.rejects(buildFromDbAsync(db, data, DEFAULT_SCOPE, 60_000, undefined, null, null, { dataDir, threads: 2, stop: () => true, minGames: 0 }), BuildCancelled);

  // The evaluation's reading (0.15.1): stopped part way, the rest on this thread, the same games in the same order;
  // each row's timeline dropped once used.
  const { prepareOnThreads } = await import('./prepareTeam.ts');
  const { prepareGames } = await import('./dataset.ts');
  const { modelPatches } = await import('./service.ts');
  const { ChampionProfiles } = await import('./profiles.ts');
  const patches = modelPatches(db, data);
  const sources = DEFAULT_SCOPE.draftSources;
  const profileRows = { current: db.championTotals(patches), older: db.olderChampionTotals(patches) };
  const profiles = new ChampionProfiles(profileRows.current, profileRows.older, data);
  const rows = [...db.modelGamesRawIter(patches, sources)];
  assert.ok(rows.length > 250, 'more than one batch');
  let asked = 0;
  const halfway = await prepareOnThreads(rows, db.modelParticipants(patches, sources), {
    dataDir, versions: data.versions ?? [data.version], profileRows, startingItemWindowMs: 60_000, dicts: db.timelineDicts(), threads: 2,
    stop: () => ++asked > 1, local: (rest, parts) => prepareGames(rest, parts, data, profiles, { startingItemWindowMs: 60_000 }),
  });
  const alone = prepareGames(db.modelGamesIter(patches, sources), db.modelParticipants(patches, sources), data, profiles, { startingItemWindowMs: 60_000 });
  const plainGames = (g: unknown) => JSON.stringify(g, (_k, v) => (v instanceof Float64Array || v instanceof Float32Array ? Array.from(v) : Number.isNaN(v) ? 'NaN' : v));
  assert.equal(plainGames(halfway), plainGames(alone));
  assert.ok(rows.every((r) => r.timeline === null && (r.timeline_z ?? null) === null));
});

test('service: late-game swaps (0.17.0) follow the evaluation unless switched, show at once, and make no build due', async () => {
  const { currentSwaps, withSwitches } = await import('./service.ts');
  const data = loadTestStaticData();
  const excl = buildExclusivityIndex(data);
  const service = new ModelService({
    db, data: () => data, excl: () => excl, scope: () => DEFAULT_SCOPE, regionPatch: () => () => undefined,
    ownPlatform: () => 'KR', busy: () => false, startingItemWindowMs: 60_000,
  });
  const latest = (swaps?: object) => fs.writeFileSync(path.join(evalDir(), 'latest.json'), JSON.stringify({
    evaluatedAt: Date.now(), gate: 'failed', bandwidth: 0.7, k: 100, lambda: 1, dataVersion: '', ...(swaps ? { swaps } : {}),
  }));
  // Following the evaluation: off until one found following the advice better than keeping every item.
  latest();
  service.evaluated();
  service.rebuild();
  assert.deepEqual(service.info()!.swaps, { active: false, choice: 'auto', gate: 'not evaluated' });
  latest({ gate: 'failed' });
  assert.deepEqual(service.info()!.swaps, { active: false, choice: 'auto', gate: 'failed' });
  latest({ gate: 'passed' });
  service.evaluated();
  assert.equal(currentSwaps(), true);
  assert.deepEqual(service.info()!.swaps, { active: true, choice: 'auto', gate: 'passed' });
  assert.equal(service.needsBuild(), false, 'the model is built the same with them');

  // Your switch (saved by the main thread, which also stops following the evaluation): Off wins over the verdict, and
  // the status as just saved shows it before the model thread answers.
  const before = service.info()!;
  updateAppConfig({ lateSwaps: 'off', patchMemory: 'off', pregameScoring: 'off' });
  service.optionsChanged();
  assert.deepEqual(withSwitches(before).swaps, { active: false, choice: 'off', gate: 'passed' });
  assert.deepEqual(service.info()!.swaps, { active: false, choice: 'off', gate: 'passed' });
  assert.equal(service.needsBuild(), false, 'moving it makes no build due');
  updateAppConfig({ lateSwaps: 'on' });
  assert.deepEqual(withSwitches(before).swaps, { active: true, choice: 'on', gate: 'passed' });
  // Not following, with no choice saved (Follow the evaluation left before 0.17.0): off, as runes against the draft.
  updateAppConfig({ lateSwaps: undefined });
  assert.deepEqual([currentSwaps(), service.info()!.swaps!.choice], [false, 'off']);
  updateAppConfig({ lateSwaps: undefined, patchMemory: undefined, pregameScoring: undefined });
  service.optionsChanged();
});
