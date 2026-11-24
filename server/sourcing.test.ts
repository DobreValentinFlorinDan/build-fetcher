// Sourcing v2 against an in-memory Riot API (test/fixtures.ts): the patch window, pilot classes, harvesting, the
// per-host lanes, data-quality flags on ingest, patches per region, hotfixes, own games and build sources.
import assert from 'node:assert/strict';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { after, test } from 'node:test';
import { openDb, type Db } from './db.ts';
import { recommend } from './engine/recommend.ts';
import { buildExclusivityIndex } from './engine/exclusivity.ts';
import { hotfixes, regionPatch, regionPatches, PATCH_CYCLE_MS } from './patches.ts';
import { RiotClient } from './riot/client.ts';
import { CacheStore, DiskCache } from './riot/diskCache.ts';
import { parseScope, type ScrapeScope } from './scrapeScope.ts';
import { fetchMatch, storeMatch } from './scraper/ingest.ts';
import { classifyPilot, historyScore, isActive, matchScore } from './scraper/pilots.ts';
import { Sourcing, type LaneKey } from './scraper/sourcing.ts';
import { FakeRiot, IRELIA, makeMatch, makeTimeline } from './test/fixtures.ts';
import { loadTestStaticData } from './test/staticData.ts';

const root = fs.mkdtempSync(path.join(os.tmpdir(), 'irelia-sourcing-'));
const opened: Db[] = [];
const closeLater: { close(): void }[] = [];
after(() => {
  // Windows can't delete a database file that is still open (the API cache's too, since 0.14.0).
  for (const db of opened) try { db.close(); } catch { /* closed by its test */ }
  for (const c of closeLater) c.close();
  fs.rmSync(root, { recursive: true, force: true, maxRetries: 5, retryDelay: 100 });
});

const NOW = Date.now();
const DAY = 24 * 3600_000;
const PATCH_START = NOW - 9 * DAY;          // KR moved to 16.19 nine days ago
const signal = new AbortController().signal;
const AHRI = 103, YASUO = 157, SYNDRA = 134, DARIUS = 122;

let n = 0;
function setup(scope: Partial<Record<keyof ScrapeScope, unknown>> = {}, fallback = { patch: '16.19', previousPatch: '16.18' }) {
  const db = openDb(path.join(root, `s${++n}.db`));
  opened.push(db);
  const riot = new FakeRiot();
  let me: { puuid: string; platform: 'KR' } | null = null;
  const logs: string[] = [];
  const s = new Sourcing({
    db, riot, scope: () => parseScope({ platforms: ['KR'], tiers: ['CHALLENGER'], ...scope }), fallbackPatch: () => fallback,
    me: () => me, concurrency: 2, log: (t) => logs.push(t), now: () => NOW,
  });
  return { db, riot, s, logs, setMe: (m: typeof me) => { me = m; s.invalidate(); } };
}

/** Brackets the KR patch: the newest 16.18 game a day before PATCH_START, the oldest 16.19 game at it. */
function seedRegion(db: Db, platform = 'KR') {
  db.insertMatch({ id: `${platform}_100`, platform, patch: '16.18', version: '16.18.700.1', queue: 420, creation: PATCH_START - 3600_000, duration: 1800, hasIrelia: false });
  db.insertMatch({ id: `${platform}_101`, platform, patch: '16.19', version: '16.19.712.4471', queue: 420, creation: PATCH_START + 3600_000, duration: 1800, hasIrelia: false });
}

/** A ranked match where `puuid` (participant 1, team 100, TOP) plays `champion`. */
function game(id: string, puuid: string, o: { champion?: number; ago?: number; version?: string; position?: string; others?: Partial<Record<number, { puuid: string; champion: number; position?: string }>>; minutes?: number; remake?: boolean } = {}) {
  const champions = [o.champion ?? IRELIA, 64, AHRI === o.champion ? 7 : AHRI, 222, 412, DARIUS, 121, SYNDRA, 21, 89];
  const puuids = champions.map((_, i) => `${id}-p${i + 1}`);
  const positions = ['TOP', 'JUNGLE', 'MIDDLE', 'BOTTOM', 'UTILITY', 'TOP', 'JUNGLE', 'MIDDLE', 'BOTTOM', 'UTILITY'];
  puuids[0] = puuid;
  if (o.position) positions[0] = o.position;
  for (const [pid, other] of Object.entries(o.others ?? {})) {
    puuids[Number(pid) - 1] = other!.puuid; champions[Number(pid) - 1] = other!.champion;
    if (other!.position) positions[Number(pid) - 1] = other!.position;
  }
  return makeMatch({ id, puuids, champions, positions, creation: NOW - (o.ago ?? 1) * DAY, version: o.version ?? '16.19.712.4471', minutes: o.minutes, remakeVote: o.remake });
}

const runAll = async (s: Sourcing, max = 200) => {
  // Every lane works at once, the way the worker runs one lane per host.
  for (let i = 0; i < max; i++) {
    const jobs = s.lanes().map((l) => s.nextJob(l)).filter((j) => j !== null);
    if (!jobs.length) return i;
    await Promise.all(jobs.map((j) => j.run(signal)));
  }
  throw new Error('lanes never ran out of work');
};

// ---- the patch window ---------------------------------------------------------------------------------

test('match ids are paged from the patch start, 100 at a time, and stop at the previous patch', async () => {
  const { db, riot, s } = setup();
  seedRegion(db);
  for (let i = 0; i < 230; i++) riot.add(game(`KR_${2000 + i}`, 'pilot', { ago: 0.01 + (i * 8) / 230 }), null);   // 16.19
  for (let i = 0; i < 20; i++) riot.add(game(`KR_${1000 + i}`, 'pilot', { ago: 10 + i * 0.1, version: '16.18.700.1' }), null);
  assert.equal(s.region('KR').start, PATCH_START - 3600_000, 'starts at the newest previous-patch game');

  const ids = await s.listHistory({ puuid: 'pilot', platform: 'KR' }, signal);
  assert.equal(ids.length, 230);
  const pages = riot.calls.filter((c) => c.method === 'matchIdsPage').map((c) => c.args[2] as { start: number; count: number; startTime: number });
  assert.deepEqual(pages.map((p) => [p.start, p.count]), [[0, 100], [100, 100], [200, 100]]);
  assert.ok(pages.every((p) => p.startTime === Math.floor((PATCH_START - 3600_000) / 1000)), 'startTime is epoch seconds');

  const capped = setup({ maxMatchesPerPilot: 150 });
  seedRegion(capped.db);
  capped.riot.histories = riot.histories;
  assert.equal((await capped.s.listHistory({ puuid: 'pilot', platform: 'KR' }, signal)).length, 150);

  // A rescan stops after a page made only of matches already stored.
  for (const id of ids.slice(0, 100)) db.insertMatch({ id, platform: 'KR', patch: '16.19', version: '16.19.712.4471', queue: 420, creation: NOW, duration: 1800, hasIrelia: false });
  riot.calls = [];
  await s.listHistory({ puuid: 'pilot', platform: 'KR' }, signal);
  assert.equal(riot.count('matchIdsPage'), 1);

  const newest = setup({ historyWindow: 'newest' });
  newest.riot.histories = riot.histories;
  assert.equal((await newest.s.listHistory({ puuid: 'pilot', platform: 'KR' }, signal)).length, 20, 'the pre-v3 window');
});

test('a match-id page is cached under every paging parameter; fresh skips the cache', async () => {
  const cacheDir = path.join(root, 'client-cache');
  const sent: string[] = [];
  const realFetch = globalThis.fetch;
  globalThis.fetch = (async (url: string) => {
    sent.push(String(url));
    return new Response(JSON.stringify(['KR_1']), { status: 200, headers: { 'content-type': 'application/json' } });
  }) as typeof fetch;
  try {
    const cache = new DiskCache(cacheDir);
    closeLater.push(cache);
    const riot = new RiotClient('RGAPI-test', cache, [{ limit: 1000, windowMs: 1000 }], { safety: 1, burst: 3, concurrency: 1, slowdown: 0.8, recoveryMs: 1000 });
    await riot.matchIdsPage('KR', 'p', { queue: 420, startTime: 1700000000, start: 0, count: 100 });
    await riot.matchIdsPage('KR', 'p', { queue: 420, startTime: 1700000000, start: 100, count: 100 });
    await riot.matchIdsPage('KR', 'p', { queue: 420, startTime: 1700000000, start: 0, count: 100 });
    assert.equal(sent.length, 2, 'the repeated page came from the cache');
    assert.match(sent[0], /ids\?queue=420&startTime=1700000000&start=0&count=100$/);
    assert.match(sent[1], /start=100&count=100$/);
    await riot.matchIdsPage('KR', 'p', { queue: 420, startTime: 1700000000, start: 0, count: 100, fresh: true });
    assert.equal(sent.length, 3);
    const store = new CacheStore(cacheDir);
    const keys = (store.db.prepare("SELECT key FROM entries WHERE kind = 'matchIds' AND scope = 'KR' ORDER BY key").all() as { key: string }[]).map((r) => r.key);
    store.close();
    assert.deepEqual(keys, ['p-420-1700000000-0-100', 'p-420-1700000000-100-100']);
  } finally {
    globalThis.fetch = realFetch;
  }
});

// ---- pilots -------------------------------------------------------------------------------------------

test('pilot classes: OTP is Irelia #1 with 150k, a main has 100k among the top masteries, activity is lastPlayTime', () => {
  const scope = parseScope({});
  const otp = classifyPilot([{ championId: IRELIA, championPoints: 400_000, lastPlayTime: NOW }], null, IRELIA, scope);
  assert.deepEqual(otp, { pilotClass: 'otp', ireliaPoints: 400_000, ireliaLastPlay: NOW, ireliaRank: 1 });
  const main = classifyPilot([{ championId: YASUO, championPoints: 900_000 }, { championId: IRELIA, championPoints: 120_000, lastPlayTime: NOW - DAY }], null, IRELIA, scope);
  assert.deepEqual([main.pilotClass, main.ireliaRank], ['main', 2]);
  assert.equal(classifyPilot([{ championId: IRELIA, championPoints: 90_000 }], null, IRELIA, scope).pilotClass, 'none', '#1 but too few points');
  const deep = classifyPilot([{ championId: YASUO, championPoints: 900_000 }], { championId: IRELIA, championPoints: 300_000, lastPlayTime: NOW }, IRELIA, scope);
  assert.deepEqual([deep.pilotClass, deep.ireliaRank, deep.ireliaPoints], ['main', null, 300_000], 'by-champion entry of a harvested player');
  assert.equal(isActive({ irelia_last_play: NOW - DAY }, PATCH_START), true);
  assert.equal(isActive({ irelia_last_play: PATCH_START - DAY }, PATCH_START), false);
  assert.equal(isActive({ irelia_last_play: null }, PATCH_START), true, 'unknown activity is scanned');
});

test('per-host lanes: OTPs and active mains are scanned, inactive and non-pilots never are', async () => {
  const { db, riot, s, logs } = setup({ pilotClasses: ['otp', 'main'] });
  seedRegion(db);
  riot.ladders.set('KR:CHALLENGER', ['A', 'B', 'C', 'D'].map((puuid, i) => ({ puuid, leaguePoints: 1000 - i })));
  riot.masteries.set('A', [{ championId: IRELIA, championPoints: 400_000, lastPlayTime: NOW - DAY }]);
  riot.masteries.set('B', [{ championId: YASUO, championPoints: 500_000 }, { championId: IRELIA, championPoints: 200_000, lastPlayTime: NOW - 2 * DAY }]);
  riot.masteries.set('C', [{ championId: AHRI, championPoints: 800_000 }]);
  riot.masteries.set('D', [{ championId: IRELIA, championPoints: 300_000, lastPlayTime: PATCH_START - 5 * DAY }]);
  for (let i = 0; i < 3; i++) riot.add(game(`KR_${3000 + i}`, 'A', { ago: 1 + i }));
  riot.add(game('KR_3010', 'A', { champion: AHRI, ago: 0.5 }));
  riot.add(game('KR_2990', 'A', { ago: 12, version: '16.18.700.1' }));
  for (let i = 0; i < 2; i++) riot.add(game(`KR_${3100 + i}`, 'B', { ago: 2 + i, position: 'MIDDLE' }));
  riot.add(game('KR_2900', 'D', { ago: 20, version: '16.18.700.1' }));

  assert.deepEqual(s.lanes().map((l: LaneKey) => (l.kind === 'platform' ? l.platform : l.region)), ['KR', 'ASIA']);
  assert.equal(s.nextJob(s.lanes()[0])!.label, 'Seeding KR Challenger ladder');
  await runAll(s);

  const games = db.raw.prepare('SELECT match_id, puuid, pilot_source, position FROM irelia_games ORDER BY match_id').all() as { match_id: string; puuid: string; pilot_source: string }[];
  assert.deepEqual(games.map((g) => [g.match_id, g.pilot_source]), [
    ['KR_3000', 'otp'], ['KR_3001', 'otp'], ['KR_3002', 'otp'], ['KR_3100', 'main'], ['KR_3101', 'main'],
  ]);
  const classes = Object.fromEntries((db.raw.prepare('SELECT puuid, pilot_class FROM players').all() as { puuid: string; pilot_class: string }[]).map((p) => [p.puuid, p.pilot_class]));
  assert.deepEqual(classes, { A: 'otp', B: 'main', C: 'none', D: 'otp' });
  const listed = new Set(riot.calls.filter((c) => c.method === 'matchIdsPage').map((c) => c.args[1]));
  assert.deepEqual([...listed].sort(), ['A', 'B'], 'D played Irelia before this patch, C is no pilot');
  assert.equal(riot.count('match'), 6, 'the 16.18 game was never listed (startTime)');
  assert.equal(riot.count('timeline'), 5, 'one timeline per Irelia game');
  assert.deepEqual(db.player('A')!.history_matches, 4);
  assert.deepEqual(db.player('A')!.history_irelia, 3);
  assert.ok(logs.some((l) => /Irelia main found/.test(l)));
});

test('a new patch rechecks every pilot at once, not after their few days, so their games on it are read the same day (0.21.1)', async () => {
  const { db, riot, s } = setup({ pilotClasses: ['otp', 'main'] });
  // KR moved to 16.19 twelve hours ago.
  db.insertMatch({ id: 'KR_100', platform: 'KR', patch: '16.18', version: '16.18.700.1', queue: 420, creation: NOW - 13 * 3600_000, duration: 1800, hasIrelia: false });
  db.insertMatch({ id: 'KR_101', platform: 'KR', patch: '16.19', version: '16.19.712.4471', queue: 420, creation: NOW - 11 * 3600_000, duration: 1800, hasIrelia: false });
  db.setMeta('seeded_at:KR:CHALLENGER', String(NOW));
  // Two OTPs whose last Irelia game was before the patch. E was checked a day ago, before the patch began (within its
  // 3-day recheck); F is changed below.
  for (const puuid of ['E', 'F']) {
    db.upsertPlayer(puuid, 'KR', 'CHALLENGER', 900);
    db.setPilot(puuid, { pilotClass: 'otp', ireliaPoints: 500_000, ireliaLastPlay: NOW - 2 * DAY, ireliaRank: 1 });
  }
  db.raw.prepare("UPDATE players SET last_checked = ?, is_otp = 1 WHERE puuid = 'E'").run(NOW - DAY);
  // F was checked an hour ago and has played Irelia on the new patch already.
  db.raw.prepare("UPDATE players SET last_checked = ?, is_otp = 1, irelia_last_play = ? WHERE puuid = 'F'").run(NOW - 3600_000, NOW - 3 * 3600_000);
  const [platform, regional] = s.lanes();
  const job = s.nextJob(platform)!;
  assert.equal(job.label, 'Refreshing pilot activity');
  // E played Irelia two hours ago, on the new patch.
  riot.masteries.set('E', [{ championId: IRELIA, championPoints: 500_000, lastPlayTime: NOW - 2 * 3600_000 }]);
  riot.add(game('KR_4000', 'E', { ago: 2 / 24 }));
  const before = riot.calls.filter((c) => c.method === 'topMasteries' || c.method === 'championMastery').length;
  await job.run(signal);
  assert.deepEqual([...new Set(riot.calls.slice(before).filter((c) => c.method === 'topMasteries' || c.method === 'championMastery').map((c) => c.args[1]))], ['E'], 'F, checked since the patch began, waits its turn');
  assert.equal(s.nextJob(regional)!.label, 'Reading KR OTP match history');
  await runAll(s);
  assert.ok(db.raw.prepare("SELECT 1 FROM irelia_games WHERE match_id = 'KR_4000'").get(), "E's game on the new patch is stored");
});

test('the new patch\'s recheck asks Riot again: a mastery answer cached before the patch began is not reused (0.21.2)', async () => {
  const cacheDir = path.join(root, 'mastery-cache');
  const sent: string[] = [];
  const realFetch = globalThis.fetch;
  globalThis.fetch = (async (url: string) => {
    sent.push(String(url));
    // Riot now: E played Irelia two hours ago, on the new patch.
    return new Response(JSON.stringify([{ championId: IRELIA, championPoints: 500_000, lastPlayTime: NOW - 2 * 3600_000 }]),
      { status: 200, headers: { 'content-type': 'application/json' } });
  }) as typeof fetch;
  try {
    const db = openDb(path.join(root, `s${++n}.db`));
    opened.push(db);
    const cache = new DiskCache(cacheDir);
    closeLater.push(cache);
    const riot = new RiotClient('RGAPI-test', cache, [{ limit: 1000, windowMs: 1000 }], { safety: 1, burst: 3, concurrency: 1, slowdown: 0.8, recoveryMs: 1000 });
    const s = new Sourcing({
      db, riot, scope: () => parseScope({ platforms: ['KR'], tiers: ['CHALLENGER'], pilotClasses: ['otp', 'main'] }),
      fallbackPatch: () => ({ patch: '16.19', previousPatch: '16.18' }), me: () => null, concurrency: 1, log: () => {}, now: () => NOW,
    });
    // KR moved to 16.19 twelve hours ago; E and G are OTPs checked a day ago, before it.
    db.insertMatch({ id: 'KR_100', platform: 'KR', patch: '16.18', version: '16.18.700.1', queue: 420, creation: NOW - 13 * 3600_000, duration: 1800, hasIrelia: false });
    db.insertMatch({ id: 'KR_101', platform: 'KR', patch: '16.19', version: '16.19.712.4471', queue: 420, creation: NOW - 11 * 3600_000, duration: 1800, hasIrelia: false });
    db.setMeta('seeded_at:KR:CHALLENGER', String(NOW));
    for (const puuid of ['E', 'G']) {
      db.upsertPlayer(puuid, 'KR', 'CHALLENGER', 900);
      db.setPilot(puuid, { pilotClass: 'otp', ireliaPoints: 500_000, ireliaLastPlay: NOW - 2 * DAY, ireliaRank: 1 });
      db.raw.prepare('UPDATE players SET last_checked = ?, is_otp = 1 WHERE puuid = ?').run(NOW - DAY, puuid);
    }
    // E's answer was cached a day ago, before the patch (within the cache's 3 days); G's an hour ago, after it began.
    const old = [{ championId: IRELIA, championPoints: 500_000, lastPlayTime: NOW - 2 * DAY }];
    await cache.set('mastery', 'KR', 'E-top3', old);
    await cache.set('mastery', 'KR', 'G-top3', [{ championId: IRELIA, championPoints: 500_000, lastPlayTime: NOW - 3 * 3600_000 }]);
    const store = new CacheStore(cacheDir);
    store.db.prepare("UPDATE entries SET stored_at = ? WHERE kind = 'mastery' AND key = 'E-top3'").run(NOW - DAY);
    store.db.prepare("UPDATE entries SET stored_at = ? WHERE kind = 'mastery' AND key = 'G-top3'").run(NOW - 3600_000);
    store.close();
    const [platform] = s.lanes();
    const job = s.nextJob(platform)!;
    assert.equal(job.label, 'Refreshing pilot activity');
    await job.run(signal);
    assert.deepEqual(sent.map((u) => /by-puuid\/(\w+)\/top/.exec(u)?.[1]), ['E'], "E's old answer isn't reused; G's, from since the patch began, is");
    assert.equal(db.player('E')!.irelia_last_play, NOW - 2 * 3600_000, 'E played on the new patch');
    assert.equal(db.player('G')!.irelia_last_play, NOW - 3 * 3600_000);
    const regional = s.lanes()[1];
    assert.equal(s.nextJob(regional)!.label, 'Reading KR OTP match history', 'both are due for their history now');

    // A pilot 0.21.1 "rechecked" from a stale cached answer (checked since the patch began, no Irelia game on it) is
    // checked once more, once per platform and patch.
    db.upsertPlayer('H', 'KR', 'CHALLENGER', 800);
    db.setPilot('H', { pilotClass: 'otp', ireliaPoints: 400_000, ireliaLastPlay: NOW - 2 * DAY, ireliaRank: 1 });
    db.raw.prepare("UPDATE players SET last_checked = ?, is_otp = 1 WHERE puuid = 'H'").run(NOW - 3600_000);
    db.setMeta('activity_patch:KR', '16.18');
    const again = s.nextJob(platform)!;
    assert.equal(again.label, 'Refreshing pilot activity');
    sent.length = 0;
    await again.run(signal);
    assert.deepEqual(sent.map((u) => /by-puuid\/(\w+)\/top/.exec(u)?.[1]), ['H']);
    assert.equal(db.getMeta('activity_patch:KR'), '16.19');
    assert.equal(s.nextJob(platform), null, 'once: nothing more to check on this patch');
  } finally {
    globalThis.fetch = realFetch;
  }
});

test('regional lanes pick the most Irelia games per request: a harvested timeline, then a strong pilot\'s match', () => {
  const { db, s } = setup();
  seedRegion(db);
  db.upsertPlayer('P', 'KR', 'CHALLENGER', 900);
  db.setPilot('P', { pilotClass: 'otp', ireliaPoints: 400_000, ireliaLastPlay: NOW, ireliaRank: 1 });
  const lane = s.lanes().find((l) => l.kind === 'regional')!;
  const first = historyScore(db.player('P')!, NOW, 300);
  assert.match(s.nextJob(lane)!.label, /Reading KR OTP match history/, 'nothing queued: read a history');

  db.enqueue({ match_id: 'KR_5', platform: 'KR', kind: 'match', puuid: 'P', source: 'otp', score: matchScore(0.7) });
  const job = s.nextJob(lane)!;
  assert.ok(matchScore(0.7) >= first);
  assert.equal(job.label, 'Fetching KR matches');
  db.enqueue({ match_id: 'KR_6', platform: 'KR', kind: 'timeline', puuid: 'X', source: 'harvested', score: 1 });
  assert.match(s.nextJob(lane)!.label, /harvested Irelia timelines/);
  assert.ok(historyScore({ ...db.player('P')!, last_match_scan: NOW - 3600_000 }, NOW, 300) < first, 'a pilot scanned an hour ago is worth less');
});

// ---- harvesting ---------------------------------------------------------------------------------------

test('harvesting: another player\'s top or mid Irelia is stored for one timeline call and becomes a candidate pilot', async () => {
  const { db, riot, s } = setup();
  seedRegion(db);
  const env = s.ingestEnv();
  const mid = riot.add(game('KR_7000', 'P', { champion: AHRI, others: { 8: { puuid: 'X', champion: IRELIA, position: 'MIDDLE' } } }));
  const r = storeMatch(db, 'KR_7000', await fetchMatch(env, 'KR', 'KR_7000', { pilotPuuid: 'P', source: 'otp' }, signal));
  assert.equal(r, 'irelia');
  const g = db.raw.prepare('SELECT puuid, pilot_source, position, opponent_champion_id FROM irelia_games WHERE match_id = ?').get('KR_7000') as Record<string, unknown>;
  assert.deepEqual({ ...g }, { puuid: 'X', pilot_source: 'harvested', position: 'MIDDLE', opponent_champion_id: 7 }, 'LeBlanc, the pilot team\'s mid');
  assert.equal(db.player('X')!.found_by, 'harvest');
  assert.equal(riot.count('timeline'), 1);
  assert.equal(mid.info.participants[7].championId, IRELIA);

  riot.add(game('KR_7001', 'P', { champion: AHRI, others: { 2: { puuid: 'J', champion: IRELIA, position: 'JUNGLE' } } }));
  assert.equal(storeMatch(db, 'KR_7001', await fetchMatch(env, 'KR', 'KR_7001', { pilotPuuid: 'P', source: 'otp' }, signal)), 'other', 'jungle Irelia: not harvested');
  assert.equal(db.player('J'), undefined);

  const off = setup({ harvestIrelias: false });
  seedRegion(off.db);
  off.riot.add(mid);
  assert.equal(storeMatch(off.db, 'KR_7000', await fetchMatch(off.s.ingestEnv(), 'KR', 'KR_7000', { pilotPuuid: 'P', source: 'otp' }, signal)), 'other');

  // The harvested player is checked first on the platform host; Irelia outside their top masteries means by-champion.
  riot.masteries.set('X', [{ championId: YASUO, championPoints: 900_000 }, { championId: AHRI, championPoints: 300_000 }, { championId: 7, championPoints: 200_000 }, { championId: IRELIA, championPoints: 150_000, lastPlayTime: NOW }]);
  const platform = s.lanes().find((l) => l.kind === 'platform')!;
  db.setMeta('seeded_at:KR:CHALLENGER', String(NOW));
  const job = s.nextJob(platform)!;
  assert.equal(job.label, 'Checking harvested Irelia players');
  await job.run(signal);
  assert.equal(db.player('X')!.pilot_class, 'main');
  assert.equal(riot.count('championMastery'), 1);
  const next = s.nextJob(platform)!;
  assert.match(next.label, /ranks/, 'then the rank of a harvested pilot off the ladder');
  riot.ranks.set('X', [{ queueType: 'RANKED_SOLO_5x5', tier: 'DIAMOND', rank: 'I', leaguePoints: 75 }]);
  await next.run(signal);
  assert.deepEqual([db.player('X')!.tier, db.player('X')!.league_points], ['DIAMOND', 75]);
});

test('a known pilot\'s game is stored under their own class, whichever pilot\'s history listed it', async () => {
  const { db, riot, s } = setup();
  seedRegion(db);
  db.upsertPlayer('A', 'KR', 'CHALLENGER', 1000);
  db.setPilot('A', { pilotClass: 'otp', ireliaPoints: 400_000, ireliaLastPlay: NOW, ireliaRank: 1 });
  db.upsertPlayer('B', 'KR', 'CHALLENGER', 900);
  db.setPilot('B', { pilotClass: 'main', ireliaPoints: 150_000, ireliaLastPlay: NOW, ireliaRank: 2 });
  // Main B (on Ahri) lists the match first; OTP A played Irelia in it, in the jungle.
  riot.add(game('KR_7500', 'B', { champion: AHRI, others: { 7: { puuid: 'A', champion: IRELIA, position: 'JUNGLE' } } }));
  assert.equal(storeMatch(db, 'KR_7500', await fetchMatch(s.ingestEnv(), 'KR', 'KR_7500', { pilotPuuid: 'B', source: 'main' }, signal)), 'irelia');
  assert.deepEqual([db.gameSource('KR_7500'), (db.gamesForPatches(['16.19'])[0] ?? {}).puuid], ['otp', 'A'], 'in the OTP build, not "harvested"');
  assert.equal(db.player('A')!.found_by, 'ladder', 'a pilot is never turned into a harvest candidate');
  assert.equal(db.player('B')!.history_irelia, 0);
});

test('a match queued by a pilot and then as one of your games becomes yours', () => {
  const { db } = setup();
  db.enqueue({ match_id: 'KR_1', platform: 'KR', kind: 'match', puuid: 'B', source: 'main', score: 0.2 });
  db.enqueue({ match_id: 'KR_1', platform: 'KR', kind: 'match', puuid: 'ME', source: 'me', score: 2 });
  db.enqueue({ match_id: 'KR_1', platform: 'KR', kind: 'match', puuid: 'C', source: 'otp', score: 0.4 });
  const q = db.nextQueued(['KR'])!;
  assert.deepEqual([q.source, q.puuid, q.score], ['me', 'ME', 2]);
});

// ---- data quality on ingest ---------------------------------------------------------------------------

test('remakes are recorded without a timeline call and never become Irelia games', async () => {
  const { db, riot, s } = setup();
  seedRegion(db);
  riot.add(game('KR_8000', 'P', { minutes: 4, remake: true }));
  assert.equal(storeMatch(db, 'KR_8000', await fetchMatch(s.ingestEnv(), 'KR', 'KR_8000', { pilotPuuid: 'P', source: 'otp' }, signal)), 'remake');
  assert.equal(riot.count('timeline'), 0);
  assert.equal((db.raw.prepare('SELECT remake FROM matches WHERE match_id = ?').get('KR_8000') as { remake: number }).remake, 1);
  assert.equal(db.gameSource('KR_8000'), undefined);
});

test('a leaver flags the match and the engine skips it; the timeline lane opponent wins over teamPosition', async () => {
  const { db, riot, s } = setup();
  seedRegion(db);
  const env = s.ingestEnv();
  const left = game('KR_8100', 'P');
  riot.add(left, makeTimeline(left, { leaver: { pid: 9, fromMinute: 15 } }));
  const swap = game('KR_8101', 'P');
  riot.add(swap, makeTimeline(swap, { spots: { 6: [7400, 7400], 8: [1500, 12500] } }));
  for (const id of ['KR_8100', 'KR_8101']) storeMatch(db, id, await fetchMatch(env, 'KR', id, { pilotPuuid: 'P', source: 'otp' }, signal));

  const rows = db.gamesForPatches(['16.19']);
  assert.deepEqual(rows.map((r) => [r.match_id, r.leaver]), [['KR_8100', 1], ['KR_8101', 0]]);
  const swapped = rows.find((r) => r.match_id === 'KR_8101')!;
  assert.deepEqual([swapped.opponent_champion_id, swapped.position_opponent_id, swapped.timeline_opponent_id], [SYNDRA, DARIUS, SYNDRA]);
  assert.equal((db.raw.prepare('SELECT COUNT(*) AS n FROM participants').get() as { n: number }).n, 20);
  assert.equal((db.raw.prepare('SELECT COUNT(*) AS n FROM timelines').get() as { n: number }).n, 2);

  const data = loadTestStaticData();
  const rec = recommend(rows.map((r) => ({ ...r, patch: data.patch })), data, buildExclusivityIndex(data), null, {
    minMatchupGames: 15, generalPriorWeight: 0.15, minLaneGames: 30, otherLaneWeight: 0.15, previousPatchWeight: 0.35, minCurrentPatchGames: 40, startingItemWindowMs: 60_000,
  }, 'TOP');
  assert.equal(rec.validation.invalidReasons['a player left the game'], 1);
  assert.equal(rec.validation.freshGames, 1);
});

test('a game Riot ended early (endOfGameResult other than GameComplete) never feeds a build', async () => {
  const { db, riot, s } = setup();
  seedRegion(db);
  const aborted = game('KR_8200', 'P');
  aborted.info.endOfGameResult = 'Abort_AntiCheatExit';
  riot.add(aborted);
  riot.add(game('KR_8201', 'P'));
  for (const id of ['KR_8200', 'KR_8201']) storeMatch(db, id, await fetchMatch(s.ingestEnv(), 'KR', id, { pilotPuuid: 'P', source: 'otp' }, signal));
  const rows = db.gamesForPatches(['16.19']);
  assert.deepEqual(rows.map((r) => r.end_result), ['Abort_AntiCheatExit', 'GameComplete']);
  const data = loadTestStaticData();
  const rec = recommend(rows.map((r) => ({ ...r, patch: data.patch })), data, buildExclusivityIndex(data), null, {
    minMatchupGames: 15, generalPriorWeight: 0.15, minLaneGames: 30, otherLaneWeight: 0.15, previousPatchWeight: 0.35, minCurrentPatchGames: 40, startingItemWindowMs: 60_000,
  }, 'TOP');
  assert.deepEqual([rec.validation.freshGames, rec.validation.invalidReasons['aborted game']], [1, 1]);
});

// ---- patches per region, hotfixes ---------------------------------------------------------------------

test('each region keeps its own current patch: KR still on 16.19 after Data Dragon moved to 16.20', async () => {
  const fallback = { patch: '16.20', previousPatch: '16.19' };
  const { db, riot, s } = setup({ platforms: ['KR', 'EUW1'] }, fallback);
  seedRegion(db, 'KR');
  db.insertMatch({ id: 'EUW1_1', platform: 'EUW1', patch: '16.20', version: '16.20.1.1', queue: 420, creation: NOW - DAY, duration: 1800, hasIrelia: false });
  assert.deepEqual([s.region('KR').patch, s.region('KR').previousPatch], ['16.19', '16.18']);
  assert.equal(s.region('EUW1').patch, '16.20');
  assert.equal(s.region('NA1').fromDataDragon, true);
  assert.equal(s.region('NA1').patch, '16.20');

  riot.add(game('KR_9000', 'P'));
  riot.add(game('EUW1_9000', 'Q'));
  assert.equal(storeMatch(db, 'KR_9000', await fetchMatch(s.ingestEnv(), 'KR', 'KR_9000', { pilotPuuid: 'P', source: 'otp' }, signal)), 'irelia');
  assert.equal(storeMatch(db, 'EUW1_9000', await fetchMatch(s.ingestEnv(), 'EUW1', 'EUW1_9000', { pilotPuuid: 'Q', source: 'otp' }, signal)), 'stale');

  // The engine: a KR 16.19 game is current for KR even though Data Dragon says 16.20.
  const data = { ...loadTestStaticData(), patch: '16.20', previousPatch: '16.19' };
  const rows = db.gamesForPatches(['16.19']);
  const settings = { minMatchupGames: 15, generalPriorWeight: 0.15, minLaneGames: 30, otherLaneWeight: 0.15, previousPatchWeight: 0.35, minCurrentPatchGames: 40, startingItemWindowMs: 60_000 };
  const regions = regionPatches(db.gameVersions(), data);
  const perRegion = recommend(rows, data, buildExclusivityIndex(data), null, settings, 'TOP', (p) => regions.get(p));
  assert.deepEqual([perRegion.validation.freshGames, perRegion.validation.previousPatchGames, perRegion.patch], [1, 0, '16.19']);
  const ddOnly = recommend(rows, data, buildExclusivityIndex(data), null, settings, 'TOP');
  assert.deepEqual([ddOnly.validation.freshGames, ddOnly.validation.previousPatchGames], [0, 1], 'without regions it would be a previous-patch game');
});

test('hotfixes: every later build of the patch, plus dates entered by hand', () => {
  const versions = [
    { platform: 'KR', game_version: '16.18.700.1', patch: '16.18', first_seen: 1, last_seen: PATCH_START - 1, games: 50 },
    { platform: 'KR', game_version: '16.19.712.4471', patch: '16.19', first_seen: PATCH_START, last_seen: PATCH_START + 2 * DAY, games: 300 },
    { platform: 'KR', game_version: '16.19.715.1000', patch: '16.19', first_seen: PATCH_START + 2 * DAY, last_seen: NOW, games: 200 },
  ];
  const kr = regionPatch('KR', versions, { patch: '16.19', previousPatch: '16.18' }, NOW);
  assert.deepEqual([kr.start, kr.startKnown, kr.builds.length], [PATCH_START - 1, true, 2]);
  const fixes = hotfixes(kr, [{ patch: '16.19', date: new Date(PATCH_START + 5 * DAY).toISOString(), note: 'Irelia Q' }, { patch: '16.18', date: '2026-09-01' }]);
  assert.deepEqual(fixes.map((h) => [h.source, h.label, h.at]), [['build', '16.19.715.1000', PATCH_START + 2 * DAY], ['manual', 'Irelia Q', PATCH_START + 5 * DAY]]);
  const fresh = regionPatch('KR', versions.slice(1), { patch: '16.19', previousPatch: '16.18' }, NOW);
  assert.deepEqual([fresh.start, fresh.startKnown], [PATCH_START - PATCH_CYCLE_MS, false], 'no previous-patch game: a full cycle back');
});

// ---- the user's own games -----------------------------------------------------------------------------

test('own games: solo and flex Irelia games of this patch and the previous one, as source "me", never in the builds', async () => {
  const { db, riot, s, setMe } = setup();
  seedRegion(db);
  riot.add(game('KR_9500', 'ME', { ago: 1 }));
  riot.add(game('KR_9501', 'ME', { ago: 12, version: '16.18.700.1' }));   // previous patch: kept for the personal layer
  riot.add(game('KR_9502', 'ME', { champion: AHRI, ago: 2 }));
  riot.add(makeMatch({ ...game('KR_9503', 'ME', { ago: 3 }), id: 'KR_9503', queue: 440, puuids: game('KR_9503', 'ME').metadata.participants, creation: NOW - 3 * DAY }));
  // An older game of the user that an OTP scan stored before the account was known.
  const before = game('KR_9400', 'ME', { ago: 4 });
  riot.add(before);
  storeMatch(db, 'KR_9400', await fetchMatch(s.ingestEnv(), 'KR', 'KR_9400', { pilotPuuid: 'ME', source: 'otp' }, signal));
  assert.equal(db.gameSource('KR_9400'), 'otp');

  setMe({ puuid: 'ME', platform: 'KR' });
  s.requestOwnGames([0]);
  const lane = s.lanes().find((l) => l.kind === 'regional')!;
  assert.equal(s.nextJob(lane)!.label, 'Fetching your Irelia games');
  await runAll(s);

  const pages = riot.calls.filter((c) => c.method === 'matchIdsPage' && c.args[1] === 'ME').map((c) => c.args[2] as { queue: number; fresh?: boolean; startTime: number });
  assert.deepEqual([...new Set(pages.map((p) => p.queue))], [420, 440]);
  assert.ok(pages.every((p) => p.fresh === true), 'the id cache never hides a game that just ended');
  assert.equal(pages[0].startTime, Math.floor((s.region('KR').start - PATCH_CYCLE_MS) / 1000), 'one patch further back');
  const mine = db.raw.prepare("SELECT match_id FROM irelia_games WHERE pilot_source = 'me' ORDER BY match_id").all() as { match_id: string }[];
  assert.deepEqual(mine.map((r) => r.match_id), ['KR_9400', 'KR_9500', 'KR_9501', 'KR_9503']);
  assert.deepEqual(db.gamesForPatches(['16.19', '16.18']).map((r) => r.match_id), [], 'the high-elo build never sees them');
  assert.deepEqual(db.gamesForPatches(['16.19', '16.18'], ['otp', 'main', 'harvested']).length, 0);
});

test('build sources: the baseline reads OTP games (and games stored before v3); others only when the scope says so', () => {
  const db = openDb(path.join(root, 'sources.db'));
  opened.push(db);
  const add = (id: string, source: string | null) => {
    db.insertMatch({ id, platform: 'KR', patch: '16.19', version: '16.19.1', queue: 420, creation: NOW, duration: 1800, hasIrelia: true });
    db.insertGame({
      match_id: id, puuid: id, patch: '16.19', win: 1, position: 'TOP', opponent_champion_id: DARIUS, summoner1: 4, summoner2: 12,
      primary_style: 8000, sub_style: 8400, perks: '[]', stat_perks: '[]', skill_order: '', purchases: '[]', final_items: '[]',
      kills: 0, deaths: 0, assists: 0, game_duration: 1800, created_at: 0, pilot_source: source as 'otp' | null,
    });
  };
  add('KR_1', 'otp'); add('KR_2', 'main'); add('KR_3', 'harvested'); add('KR_4', 'me'); add('KR_5', null);
  assert.deepEqual(db.gamesForPatches(['16.19']).map((r) => r.match_id), ['KR_1', 'KR_5']);
  assert.deepEqual(db.gamesForPatches(['16.19'], ['otp', 'main', 'harvested']).map((r) => r.match_id), ['KR_1', 'KR_2', 'KR_3', 'KR_5']);
  assert.deepEqual(db.opponentCounts(['16.19'], 'TOP').map((r) => ({ ...r })), [{ id: DARIUS, games: 2 }]);
  assert.deepEqual(db.sourceCounts(), { harvested: 1, main: 1, me: 1, otp: 2 });
  assert.throws(() => parseScope({ buildSources: ['me'] }), /build source me/);
  db.close();
});
