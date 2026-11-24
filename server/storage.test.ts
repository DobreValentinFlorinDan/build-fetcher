import assert from 'node:assert/strict';
import fs from 'node:fs/promises';
import os from 'node:os';
import path from 'node:path';
import { after, before, test } from 'node:test';
import zlib from 'node:zlib';
import { openDb } from './db.ts';
import { cacheStore, CacheStore, closeCacheStores, DiskCache, legacyFile } from './riot/diskCache.ts';
import { copyDataDir, inspectTarget, isInside, removeOwnedData, storageStats, verifyCopy } from './storage.ts';

let root: string;
let from: string;
const old = new Date(Date.now() - 30 * 24 * 3600_000);

before(async () => {
  root = await fs.mkdtemp(path.join(os.tmpdir(), 'irelia-storage-'));
  from = path.join(root, 'data');
  const db = openDb(path.join(from, 'irelia.db'));
  db.upsertPlayer('puuid-1', 'KR', 'CHALLENGER', 1200);
  db.insertMatch({ id: 'KR_1', platform: 'KR', patch: '16.19', version: '16.19.1', queue: 420, creation: 0, duration: 1800, hasIrelia: false });
  db.close();

  const cache = new DiskCache(path.join(from, 'cache'));
  await cache.set('match', 'KR', 'KR_1', { info: { gameVersion: '16.19.1' } });
  cache.close();
  const store = new CacheStore(path.join(from, 'cache'));
  store.write('league', 'KR', 'CHALLENGER', Buffer.from('{"entries":[]}'), old.getTime());   // past its 6 h TTL
  store.close();
  // A file from before 0.14.0, not converted yet (also past its TTL).
  const legacy = legacyFile(path.join(from, 'cache'), 'mastery', 'KR', 'p1');
  await fs.mkdir(path.dirname(legacy), { recursive: true });
  await fs.writeFile(legacy, zlib.gzipSync('[]'));
  await fs.utimes(legacy, old, old);
  await fs.mkdir(path.join(from, 'ddragon', '16.19.1'), { recursive: true });
  await fs.writeFile(path.join(from, 'ddragon', '16.19.1', 'item.json'), '{"data":{}}');
  await fs.writeFile(path.join(from, 'notes.txt'), 'not ours');
});
after(() => { closeCacheStores(); return fs.rm(root, { recursive: true, force: true }); });
const kinds = (parts: { name: string }[]) => parts.map((p) => p.name).filter((n) => n !== 'index and free space').sort();

test('stats split the folder into database, Riot cache, Data Dragon and other files', async () => {
  const s = await storageStats(from, { defaultDataDir: from, configFile: 'x', lockedBy: null, ddragonVersion: '16.19.1', busy: false });
  const byKey = Object.fromEntries(s.categories.map((c) => [c.key, c]));
  assert.ok(byKey.database.bytes > 0);
  assert.deepEqual(kinds(byKey.riotCache.parts), ['league', 'mastery', 'match']);
  assert.deepEqual(byKey.ddragon.parts.map((p) => p.name), ['16.19.1']);
  assert.deepEqual(byKey.other.parts.map((p) => p.name), ['notes.txt']);
  assert.equal(s.totalBytes, s.categories.reduce((n, c) => n + c.bytes, 0));
});

test('pre-flight rejects relative, nested and parent folders', async () => {
  assert.match((await inspectTarget('relative\\dir', from)).problems.join(), /absolute/);
  assert.match((await inspectTarget(from, from)).problems.join(), /already/);
  assert.match((await inspectTarget(path.join(from, 'sub'), from)).problems.join(), /inside/);
  assert.match((await inspectTarget(root, from)).problems.join(), /contain/);
  const ok = await inspectTarget(path.join(root, 'new-home'), from);
  assert.deepEqual(ok.problems, []);
  assert.equal(ok.exists, false);
  assert.equal(ok.hasData, false);
  assert.ok(ok.requiredBytes > 0);
  assert.equal(await fs.stat(path.join(root, 'new-home')).catch(() => null), null);   // inspection creates nothing
  assert.equal(isInside(path.join(root, 'a', 'b'), path.join(root, 'a')), true);
  assert.equal(isInside(path.join(root, 'ab'), path.join(root, 'a')), false);
});

test('copy + verify moves only the app files, keeps cache mtimes, then old copy is removed', async () => {
  const to = path.join(root, 'D-drive', 'IreliaData');
  const db = openDb(path.join(from, 'irelia.db'));
  const rows = db.rowCounts();
  const phases: string[] = [];
  const copy = await copyDataDir(from, to, (file) => db.vacuumInto(file), (p) => phases.push(p.phase));
  db.close();
  closeCacheStores();
  await verifyCopy(from, to, copy.files, rows, copy.cacheEntries);
  assert.equal(copy.cacheEntries, 2, 'the cache database, as a database');

  assert.deepEqual(rows, {
    players: 1, matches: 1, irelia_games: 0, participants: 0, timelines: 0, game_versions: 1, champion_patch_stats: 0, match_queue: 0, archive: 0,
  });
  assert.ok(phases.includes('database') && phases.includes('copying'));
  const copiedMtime = (await fs.stat(legacyFile(path.join(to, 'cache'), 'mastery', 'KR', 'p1'))).mtimeMs;
  assert.ok(Math.abs(copiedMtime - old.getTime()) < 0.01, `mtime ${copiedMtime} != ${old.getTime()}`);   // utimes round-trips through float seconds
  assert.equal(await fs.stat(path.join(to, 'notes.txt')).catch(() => null), null);
  assert.equal((await inspectTarget(to, from)).hasData, true);   // a second move into it would be refused

  const moved = openDb(path.join(to, 'irelia.db'));
  assert.deepEqual(moved.rowCounts(), rows);
  moved.close();

  assert.deepEqual(await removeOwnedData(from), ['notes.txt']);   // only our entries go
  assert.equal(await fs.readFile(path.join(from, 'notes.txt'), 'utf8'), 'not ours');
});

test('verify fails loudly on a truncated copy', async () => {
  const to = path.join(root, 'broken');
  const src = path.join(root, 'D-drive', 'IreliaData');
  const db = openDb(path.join(src, 'irelia.db'));
  const copy = await copyDataDir(src, to, (file) => db.vacuumInto(file), () => {});
  db.close();
  await fs.writeFile(legacyFile(path.join(to, 'cache'), 'mastery', 'KR', 'p1'), 'x');
  await assert.rejects(verifyCopy(src, to, copy.files, null), /Copy check failed/);
});

test('expired-only pruning keeps immutable matches and drops stale ladders', async () => {
  const cacheRoot = path.join(root, 'D-drive', 'IreliaData', 'cache');
  const r = await DiskCache.prune(cacheRoot, { expiredOnly: true });
  assert.equal(r.files, 2, 'the stale ladder and the stale mastery file');
  assert.deepEqual(kinds(await DiskCache.usage(cacheRoot)), ['match']);
});

test("old-patch cleanup deletes cached matches and timelines of games older than the window, and nothing it can't place", async () => {
  const cacheRoot = path.join(root, 'old-patches', 'cache');
  const cache = new DiskCache(cacheRoot);
  const match = (version: string) => ({ info: { gameVersion: version } });
  await cache.set('match', 'asia', 'KR_1', match('16.10.1.1'));      // stored, old patch
  await cache.set('timeline', 'asia', 'KR_1', { frames: [] });
  await cache.set('match', 'asia', 'KR_2', match('16.11.1.1'));      // unknown to the database: its own version is old
  await cache.set('match', 'asia', 'KR_3', match('16.19.1.1'));      // unknown, today's patch
  await cache.set('timeline', 'asia', 'KR_4', { frames: [] });       // unknown, no match to tell
  await cache.set('match', 'asia', 'KR_5', match('16.19.1.1'));      // stored, today's patch
  const stored = new Map([['KR_1', '16.10'], ['KR_5', '16.19']]);
  const r = await DiskCache.pruneMatches(cacheRoot, async (id, version) => {
    const patch = stored.get(id) ?? (await version())?.split('.').slice(0, 2).join('.');
    return patch !== undefined && Number(patch.split('.')[1]) < 14;
  });
  assert.equal(r.files, 3);
  const left = (kind: string) => cacheStore(cacheRoot).matchEntries().filter((e) => e.kind === kind).map((e) => e.key).sort();
  assert.deepEqual(left('match'), ['KR_3', 'KR_5']);
  assert.deepEqual(left('timeline'), ['KR_4']);
  cache.close();
});
