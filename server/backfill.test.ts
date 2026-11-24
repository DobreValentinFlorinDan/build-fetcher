// Upgrading a v0.3.0 data folder: the schema migration, the backfill from the disk cache (idempotent, resumable),
// and the read-only audit, all on a fixture folder made with the v2 schema and real-shaped cache files.
import assert from 'node:assert/strict';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { DatabaseSync } from 'node:sqlite';
import { after, test } from 'node:test';
import { auditData, formatAudit } from './audit.ts';
import { CAPTURE_VERSION } from './capture.ts';
import { openDb, SCHEMA_VERSION } from './db.ts';
import { extractIreliaGame, type MatchDto } from './engine/parseMatch.ts';
import { regionPatch } from './patches.ts';
import { DiskCache } from './riot/diskCache.ts';
import { runBackfill, type BackfillProgress } from './scraper/backfill.ts';
import { IRELIA, makeMatch, makeTimeline } from './test/fixtures.ts';
import { SCHEMA_V2 } from './test/schemaV2.ts';

const root = fs.mkdtempSync(path.join(os.tmpdir(), 'irelia-backfill-'));
// The caches' databases are closed first: Windows doesn't delete a file that is open.
const caches: DiskCache[] = [];
after(() => { for (const c of caches) c.close(); fs.rmSync(root, { recursive: true, force: true }); });

const NOW = Date.now();
const DAY = 24 * 3600_000;
const START = NOW - 9 * DAY;
const SYNDRA = 134, DARIUS = 122;

/**
 * A v0.3.0 data folder: one OTP (P) with an Irelia game whose timeline shows a lane swap and a leaver, a remade
 * Irelia game, a match where another player (X) played Irelia top, an older-patch match, a match whose cache file
 * is gone, and a cached match the database never recorded.
 */
async function v2Folder(name: string) {
  const dir = path.join(root, name);
  fs.mkdirSync(dir, { recursive: true });
  const raw = new DatabaseSync(path.join(dir, 'irelia.db'));
  raw.exec(SCHEMA_V2);
  const cache = new DiskCache(path.join(dir, 'cache'));
  caches.push(cache);
  const player = raw.prepare('INSERT INTO players(puuid, platform, tier, league_points, is_otp, irelia_mastery, last_checked, updated_at) VALUES(?, ?, ?, ?, ?, ?, ?, ?)');
  player.run('P', 'KR', 'CHALLENGER', 1200, 1, 400_000, NOW - DAY, NOW);
  player.run('Q', 'KR', 'CHALLENGER', 900, 0, 0, NOW - DAY, NOW);
  player.run('R', 'KR', 'GRANDMASTER', 500, null, null, null, NOW);
  await cache.set('mastery', 'KR', 'P', [{ championId: IRELIA, championPoints: 400_000, lastPlayTime: NOW - DAY }]);

  const match = raw.prepare('INSERT INTO matches(match_id, platform, patch, game_version, queue_id, game_creation, game_duration, has_irelia, fetched_at) VALUES(?, ?, ?, ?, 420, ?, ?, ?, ?)');
  const game = raw.prepare(`INSERT INTO irelia_games(match_id, puuid, patch, win, position, opponent_champion_id, summoner1, summoner2, primary_style,
    sub_style, perks, stat_perks, skill_order, purchases, final_items, kills, deaths, assists, game_duration, created_at)
    VALUES(:match_id, :puuid, :patch, :win, :position, :opponent_champion_id, :summoner1, :summoner2, :primary_style, :sub_style, :perks,
      :stat_perks, :skill_order, :purchases, :final_items, :kills, :deaths, :assists, :game_duration, :created_at)`);
  const store = async (m: MatchDto, o: { irelia?: boolean; timeline?: ReturnType<typeof makeTimeline> | null; file?: boolean } = {}) => {
    const id = m.metadata.matchId;
    match.run(id, 'KR', m.info.gameVersion.split('.').slice(0, 2).join('.'), m.info.gameVersion, m.info.gameCreation, m.info.gameDuration, o.irelia ? 1 : 0, NOW);
    if (o.file !== false) await cache.set('match', 'KR', id, m);
    const tl = o.timeline === undefined ? makeTimeline(m) : o.timeline;
    if (o.irelia && tl) {
      await cache.set('timeline', 'KR', id, tl);
      game.run(extractIreliaGame(m, tl, m.info.participants[0]) as unknown as Record<string, string | number | null>);
    }
  };
  const puuids = (id: string, first = 'P') => Array.from({ length: 10 }, (_, i) => (i === 0 ? first : `${id}-p${i + 1}`));

  await store(makeMatch({ id: 'KR_1', version: '16.18.700.1', creation: START - DAY, puuids: puuids('KR_1') }));
  const swap = makeMatch({ id: 'KR_2', creation: NOW - 2 * DAY, puuids: puuids('KR_2') });
  await store(swap, { irelia: true, timeline: makeTimeline(swap, { spots: { 6: [7400, 7400], 8: [1500, 12500] }, leaver: { pid: 9, fromMinute: 20 } }) });
  await store(makeMatch({ id: 'KR_3', creation: NOW - DAY, minutes: 4, remakeVote: true, puuids: puuids('KR_3') }), { irelia: true });
  const harvest = makeMatch({ id: 'KR_4', creation: NOW - 3 * DAY, puuids: puuids('KR_4'), champions: [103, 64, 7, 222, 412, IRELIA, 121, SYNDRA, 21, 89] });
  harvest.info.participants[5].puuid = 'X';
  await store(harvest);
  await store(makeMatch({ id: 'KR_5', creation: NOW - 4 * DAY, puuids: puuids('KR_5') }), { file: false });
  await cache.set('match', 'KR', 'KR_9', makeMatch({ id: 'KR_9', creation: NOW - 5 * DAY }));   // on disk, not in the database
  raw.close();
  return { dir, cache };
}

const counts = (db: ReturnType<typeof openDb>) => Object.fromEntries(Object.entries(db.rowCounts()).filter(([k]) => k !== 'players'));

function backfillEnv(db: ReturnType<typeof openDb>, cache: DiskCache) {
  return {
    db, cache, championId: IRELIA, harvest: true, mePuuid: () => null,
    region: (p: string) => regionPatch(p, db.gameVersions(), { patch: '16.19', previousPatch: '16.18' }, NOW),
  };
}

test('migration: a v2 database gains the v3 tables and columns in place, idempotently, keeping every row', async () => {
  const { dir } = await v2Folder('migrate');
  const db = openDb(path.join(dir, 'irelia.db'));
  assert.equal(db.getMeta('schema_version'), String(SCHEMA_VERSION));
  assert.deepEqual(db.rowCounts(), { players: 3, matches: 5, irelia_games: 2, participants: 0, timelines: 0, game_versions: 0, champion_patch_stats: 0, match_queue: 0, archive: 0 });
  const games = db.raw.prepare('SELECT match_id, pilot_source, position_opponent_id, opponent_champion_id FROM irelia_games ORDER BY match_id').all();
  assert.deepEqual(games.map((g) => ({ ...g })), [
    { match_id: 'KR_2', pilot_source: 'otp', position_opponent_id: DARIUS, opponent_champion_id: DARIUS },
    { match_id: 'KR_3', pilot_source: 'otp', position_opponent_id: DARIUS, opponent_champion_id: DARIUS },
  ]);
  assert.deepEqual(['P', 'Q', 'R'].map((p) => db.player(p)!.pilot_class), ['otp', 'none', null]);
  assert.equal(db.countPendingDetail(CAPTURE_VERSION), 5);
  // Checked before v3 (top-1 mastery, which can't tell a main): rechecked once, ranked with the unchecked players.
  const due = (lo: number, hi: number) => db.masteryWork('KR', { pilotBefore: NOW - 3 * DAY, otherBefore: NOW - 14 * DAY, lo, hi, limit: 10 }).map((p) => p.puuid).sort();
  assert.deepEqual(due(2, 2), ['P', 'Q', 'R']);
  db.setPilot('Q', { pilotClass: 'none', ireliaPoints: 0, ireliaLastPlay: null, ireliaRank: null });
  assert.deepEqual(due(2, 3), ['P', 'R'], 'once classified by v3, Q waits for its 14-day recheck');
  db.close();
  const again = openDb(path.join(dir, 'irelia.db'));   // a second start changes nothing
  assert.equal(again.rowCounts().irelia_games, 2);
  again.close();
});

test('backfill: participants, flags, timelines and lane opponents from the disk cache; idempotent; harvests cached matches', async () => {
  const { dir, cache } = await v2Folder('backfill');
  const db = openDb(path.join(dir, 'irelia.db'));
  const seen: BackfillProgress[] = [];
  const p = await runBackfill(backfillEnv(db, cache), { signal: new AbortController().signal, onProgress: (x) => seen.push(x), batch: 2 });
  assert.deepEqual([p.done, p.total, p.games, p.missing, p.harvested], [5, 5, 2, 1, 1]);
  assert.ok(p.finishedAt !== null && seen.some((x) => x.done === 2), 'progress per batch');

  const first = counts(db);
  assert.deepEqual(first, { matches: 5, irelia_games: 2, participants: 40, timelines: 2, game_versions: 2, champion_patch_stats: 0, match_queue: 1, archive: 0 });
  const swap = db.raw.prepare('SELECT opponent_champion_id, position_opponent_id, timeline_opponent_id, participant_id, pilot_tier, pilot_mastery FROM irelia_games WHERE match_id = ?').get('KR_2');
  assert.deepEqual({ ...swap }, { opponent_champion_id: SYNDRA, position_opponent_id: DARIUS, timeline_opponent_id: SYNDRA, participant_id: 1, pilot_tier: 'CHALLENGER', pilot_mastery: 400_000 });
  const flags = db.raw.prepare('SELECT match_id, remake, leaver, detail_version FROM matches ORDER BY match_id').all().map((r) => ({ ...r }));
  assert.deepEqual(flags.find((f) => f.match_id === 'KR_2'), { match_id: 'KR_2', remake: 0, leaver: 1, detail_version: CAPTURE_VERSION });
  assert.deepEqual(flags.find((f) => f.match_id === 'KR_3'), { match_id: 'KR_3', remake: 1, leaver: 0, detail_version: CAPTURE_VERSION });
  assert.equal(flags.find((f) => f.match_id === 'KR_5')!.remake, null, 'no cache file: nothing to capture');
  assert.deepEqual({ ...db.nextQueued(['KR']) }, { match_id: 'KR_4', platform: 'KR', kind: 'timeline', puuid: 'X', source: 'harvested', score: 1, added_at: (db.nextQueued(['KR']) as unknown as { added_at: number }).added_at });
  assert.equal(db.player('X')!.found_by, 'harvest');
  assert.equal(db.player('P')!.irelia_last_play, NOW - DAY, 'activity read from the cached mastery answer');

  const again = await runBackfill(backfillEnv(db, cache), { signal: new AbortController().signal, onProgress: () => {} });
  assert.deepEqual([again.total, again.done], [0, 0]);
  const forced = await runBackfill(backfillEnv(db, cache), { signal: new AbortController().signal, onProgress: () => {}, force: true });
  assert.equal(forced.done, 5);
  assert.deepEqual(counts(db), first, 'rerunning rewrites the same rows');
  db.close();
});

test('backfill: interrupted after a batch, it resumes where it stopped and ends with the same data', async () => {
  const { dir, cache } = await v2Folder('resume');
  const db = openDb(path.join(dir, 'irelia.db'));
  const stop = new AbortController();
  await assert.rejects(runBackfill(backfillEnv(db, cache), { signal: stop.signal, batch: 2, onProgress: (x) => { if (x.done >= 2) stop.abort(); } }));
  assert.equal(db.countPendingDetail(CAPTURE_VERSION), 3);
  const rest = await runBackfill(backfillEnv(db, cache), { signal: new AbortController().signal, batch: 2, onProgress: () => {} });
  assert.deepEqual([rest.total, rest.done], [3, 3]);
  assert.deepEqual(counts(db), { matches: 5, irelia_games: 2, participants: 40, timelines: 2, game_versions: 2, champion_patch_stats: 0, match_queue: 1, archive: 0 });
  db.close();
});

test('purging old patches keeps per-champion totals of the purged games', async () => {
  const { dir, cache } = await v2Folder('purge');
  const db = openDb(path.join(dir, 'irelia.db'));
  await runBackfill(backfillEnv(db, cache), { signal: new AbortController().signal, onProgress: () => {} });
  assert.equal(db.purgeStale(['16.19']), 1);
  const kept = db.raw.prepare("SELECT champion_id, games, dmg_magic FROM champion_patch_stats WHERE patch = '16.18' AND champion_id = ?").get(SYNDRA);
  assert.deepEqual({ ...kept }, { champion_id: SYNDRA, games: 1, dmg_magic: 25_000 });
  assert.equal(db.rowCounts().participants, 30);
  db.close();
});

test('archiving old patches stores each match whole and compressed; restoring brings back exactly what was there', async () => {
  const { dir, cache } = await v2Folder('archive');
  const db = openDb(path.join(dir, 'irelia.db'));
  await runBackfill(backfillEnv(db, cache), { signal: new AbortController().signal, onProgress: () => {} });
  const tables = ['matches', 'irelia_games', 'participants', 'timelines', 'champion_patch_stats'];
  const dump = () => Object.fromEntries(tables.map((t) => [t, db.raw.prepare(`SELECT * FROM ${t} ORDER BY 1, 2, 3`).all().map((r) => ({ ...r }))]));
  const before = dump();
  assert.equal(before.timelines.length, 2);

  assert.equal(db.archiveBatch(['16.19'], 1), 1);
  assert.equal(db.archiveBatch(['16.19'], 1), 0, 'nothing left to archive');
  assert.deepEqual(db.archiveSummary().map((a) => [a.patch, a.games]), [['16.18', 1]]);
  // As after purging: the match is out of the working tables, its per-champion totals are kept.
  const kept = db.raw.prepare("SELECT champion_id, games, dmg_magic FROM champion_patch_stats WHERE patch = '16.18' AND champion_id = ?").get(SYNDRA);
  assert.deepEqual({ ...kept }, { champion_id: SYNDRA, games: 1, dmg_magic: 25_000 });
  assert.equal(db.rowCounts().participants, 30);
  assert.equal(db.rowCounts().archive, 1);

  assert.equal(db.restoreBatch(['16.17']), 0, 'another patch: nothing to restore');
  assert.equal(db.restoreBatch(null), 1);
  assert.deepEqual(dump(), before, 'every row back as it was, the kept totals gone again');
  assert.equal(db.rowCounts().archive, 0);
  // Archived and restored again: the totals don't add up twice.
  db.archiveBatch(['16.19']);
  db.restoreBatch(['16.18']);
  assert.deepEqual(dump(), before);
  db.close();
});

test('audit: read-only on a v2 folder, it measures coverage, quality, lane opponents, harvest potential and the yield model', async () => {
  const { dir } = await v2Folder('audit');
  // The Data Dragon files the app caches in the folder (here: the tests' snapshot), for the build comparison.
  const snap = JSON.parse(fs.readFileSync(new URL('./test/ddragon-snapshot.json', import.meta.url), 'utf8'));
  const dd = path.join(dir, 'ddragon', snap.version);
  fs.mkdirSync(dd, { recursive: true });
  for (const [file, key] of [['item.json', 'item'], ['champion.json', 'champion'], ['runesReforged.json', 'runesReforged'], ['summoner.json', 'summoner'], ['Irelia.json', 'champ']]) {
    fs.writeFileSync(path.join(dd, file), JSON.stringify(snap.raw[key]));
  }
  const before = fs.statSync(path.join(dir, 'irelia.db')).mtimeMs;
  const r = await auditData(dir, { now: NOW });
  assert.equal(r.schemaVersion, 2);
  assert.deepEqual([r.matches.total, r.ireliaShare.ireliaGames, r.ireliaShare.share], [5, 2, 0.4]);
  assert.deepEqual([r.players.total, r.players.checked, r.players.unchecked], [3, 2, 1]);
  assert.deepEqual([r.quality.matchesRead, r.quality.remakes, r.quality.ireliaRemakes, r.quality.ireliaLeavers, r.quality.timelinesRead], [4, 1, 1, 1, 2]);
  assert.deepEqual([r.laneOpponent.compared, r.laneOpponent.disagree, r.laneOpponent.share], [1, 1, 1]);
  assert.deepEqual([r.harvest.cachedMatchesWithOtherIrelia, r.harvest.currentPatchNotRemake, r.harvest.players], [1, 1, 1]);
  assert.deepEqual(r.cache, { matchFiles: 5, timelineFiles: 2, masteryFiles: 1, matchesWithoutFile: 1, gamesWithoutTimeline: 0, matchFilesNotInDb: 1, timelineFilesNotInDb: 0 });
  assert.deepEqual(r.builds.map((b) => [b.platform, b.patch]), [['KR', '16.19'], ['KR', '16.18']]);
  assert.deepEqual([r.yield.pilots, r.yield.activeShare], [1, 1]);
  assert.ok(r.disk.avgTimelineBytes > 0 && r.disk.per1000Games.totalMB > 0);
  assert.equal(r.projection[0].option, 'as now (20 newest ids)');
  assert.match(formatAudit(r), /Yield model[\s\S]*whole-patch pagination/);
  const top = Object.fromEntries(r.buildImpact!.filter((b) => b.lane === 'TOP').map((b) => [b.variant, b]));
  assert.deepEqual(Object.keys(top), ['v0.3.0', '+ remakes', '+ leavers', '+ timeline opponent', '+ patch per region']);
  assert.deepEqual([top['v0.3.0'].used, top['+ remakes'].used, top['+ leavers'].used], [2, 1, 0], 'the remake, then the leaver game drop out');
  assert.equal(top['+ remakes'].discarded.remake, 1);
  assert.deepEqual(top['v0.3.0'].topMatchups, [{ id: DARIUS, games: 2 }]);
  assert.deepEqual([...top['+ timeline opponent'].topMatchups].sort((a, b) => a.id - b.id), [{ id: DARIUS, games: 1 }, { id: SYNDRA, games: 1 }], 'the lane swap counts against Syndra');

  // Nothing was written: no v3 tables, no schema version.
  const raw = new DatabaseSync(path.join(dir, 'irelia.db'), { readOnly: true });
  const tables = (raw.prepare("SELECT name FROM sqlite_schema WHERE type = 'table'").all() as { name: string }[]).map((t) => t.name).sort();
  raw.close();
  assert.deepEqual(tables, ['irelia_games', 'matches', 'meta', 'players']);
  assert.equal(fs.statSync(path.join(dir, 'irelia.db')).mtimeMs, before);
});
