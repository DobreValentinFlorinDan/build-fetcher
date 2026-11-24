// Old patches (server/oldPatches.ts): the archive switch archives what is older than patch memory's window and keeps
// doing so as patches leave it, off restores everything; the cache switch deletes cached files of old patches once per
// window; nothing runs while the data folder is busy.
import assert from 'node:assert/strict';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { after, test } from 'node:test';

// Settings go to a temporary folder: appConfig.ts reads this when it loads.
const root = fs.mkdtempSync(path.join(os.tmpdir(), 'irelia-old-patches-'));
process.env.IRELIA_CONFIG_DIR = path.join(root, 'config');

const { captureTimeline, participantRows, CAPTURE_VERSION } = await import('./capture.ts');
const { openDb } = await import('./db.ts');
const { cacheStore, closeCacheStores, DiskCache } = await import('./riot/diskCache.ts');
const { captureGame } = await import('./scraper/ingest.ts');
const { apAdGames, synthMatch } = await import('./test/modelFixtures.ts');
const { loadTestStaticData } = await import('./test/staticData.ts');
const { OldPatches } = await import('./oldPatches.ts');

const db = openDb(path.join(root, 'irelia.db'));
const cacheDir = path.join(root, 'cache');
after(() => { db.close(); closeCacheStores(); fs.rmSync(root, { recursive: true, force: true, maxRetries: 5 }); });

const data = loadTestStaticData();
function store(n: number, patch: string, from: number) {
  db.tx(() => {
    for (const s of apAdGames(n, 5, from).map((g, i) => ({ ...g, id: `KR_${from * 1000 + i}`, version: `${patch}.1.1` }))) {
      const { match, timeline } = synthMatch(s);
      const { row } = captureGame(match, timeline, match.info.participants[0], 'otp', undefined);
      db.insertMatch({ id: s.id, platform: 'KR', patch, version: match.info.gameVersion, queue: 420, creation: match.info.gameCreation, duration: match.info.gameDuration, hasIrelia: true });
      db.insertGame(row);
      db.insertParticipants(s.id, participantRows(match));
      db.insertTimeline(s.id, captureTimeline(timeline).interval, JSON.stringify(captureTimeline(timeline)));
      db.setQuality(s.id, false, 'GameComplete', CAPTURE_VERSION);
    }
  });
}
const patchesStored = () => (db.raw.prepare('SELECT patch, COUNT(*) AS n FROM matches GROUP BY patch ORDER BY patch').all() as { patch: string; n: number }[])
  .map((r) => `${r.patch}:${r.n}`);

let blocked = false, idle = true;
const old = new OldPatches({ db: () => db, data: () => data, cacheDir: () => cacheDir, blocked: () => blocked, idle: () => idle });

test('old patches: the archive switch archives what is older than the window, keeps doing so, and off restores it all', async () => {
  store(20, '16.19', 1);
  store(6, '16.16', 2);   // within the window (six patches): kept
  store(4, '16.10', 3);   // older: archived
  assert.deepEqual([old.status().archive, old.status().trimCache], [false, false]);
  await old.maintain();
  assert.deepEqual(patchesStored(), ['16.10:4', '16.16:6', '16.19:20'], 'off: nothing happens by itself');

  const on = await old.setArchive(true);
  assert.equal(on.archive, true);
  assert.deepEqual(on.archived.map((a) => [a.patch, a.games]), [['16.10', 4]]);
  assert.deepEqual(patchesStored(), ['16.16:6', '16.19:20']);

  // A game of an old patch arrives (or a patch leaves the window): the next check archives it, unless champ select or
  // a game is on (then the one after).
  store(1, '16.09', 4);
  idle = false;
  await old.maintain();
  assert.deepEqual(old.status().archived.map((a) => [a.patch, a.games]), [['16.10', 4]]);
  idle = true;
  await old.maintain();
  assert.deepEqual(old.status().archived.map((a) => [a.patch, a.games]), [['16.09', 1], ['16.10', 4]]);

  const off = await old.setArchive(false);
  assert.deepEqual([off.archive, off.archived.length, off.error], [false, 0, null]);
  assert.deepEqual(patchesStored(), ['16.09:1', '16.10:4', '16.16:6', '16.19:20']);
});

test('old patches: the cache switch deletes cached files of old patches, once per window', async () => {
  const cache = new DiskCache(cacheDir);
  await cache.set('match', 'KR', 'KR_3000', { info: { gameVersion: '16.10.1.1' } });   // stored, old
  await cache.set('timeline', 'KR', 'KR_3000', { frames: [] });
  await cache.set('match', 'KR', 'KR_1000', { info: { gameVersion: '16.19.1.1' } });   // stored, today's
  await cache.set('match', 'KR', 'KR_9999', { info: { gameVersion: '16.08.1.1' } });   // not stored, old by its own version
  const on = await old.setTrimCache(true);
  assert.equal(on.trimCache, true);
  assert.match(on.last ?? '', /Deleted 3 cached files/);
  const keys = () => cacheStore(cacheDir).matchEntries().map((e) => `${e.kind}:${e.key}`).sort();
  assert.deepEqual(keys(), ['match:KR_1000']);

  // The window hasn't moved: the next check leaves the cache alone.
  await cache.set('match', 'KR', 'KR_3001', { info: { gameVersion: '16.10.1.1' } });
  await old.maintain();
  assert.deepEqual(keys(), ['match:KR_1000', 'match:KR_3001']);
  cache.close();
});

test('old patches: nothing runs while the data folder is moved or compacted', async () => {
  blocked = true;
  const s = await old.setArchive(true);
  assert.match(s.error ?? '', /being moved/);
  assert.equal(s.archived.length, 0);
  blocked = false;
  const again = await old.setArchive(true);
  assert.equal(again.error, null);
  assert.ok(again.archived.length > 0);
});
