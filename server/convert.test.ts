// The background conversion (server/convert.ts, 0.14.0): a data folder of an earlier version (one gzip file per API
// response, plain timelines) comes out with every response in riot-cache.db and every timeline compressed, each one
// reading back exactly as it was, the old copies gone; champ select stops it and it carries on after.
import assert from 'node:assert/strict';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { after, test } from 'node:test';
import zlib from 'node:zlib';
import type { ConversionStatus } from '../shared/types.ts';

const root = fs.mkdtempSync(path.join(os.tmpdir(), 'irelia-convert-'));
process.env.IRELIA_CONFIG_DIR = path.join(root, 'config');
const dataDir = path.join(root, 'data');
const cacheDir = path.join(dataDir, 'cache');

const { openDb } = await import('./db.ts');
const { Conversion, TIMELINE_CURSOR } = await import('./convert.ts');
const { CacheStore, closeCacheStores, DiskCache, legacyFile } = await import('./riot/diskCache.ts');
const { IRELIA, makeMatch, makeTimeline } = await import('./test/fixtures.ts');
const { captureTimeline } = await import('./capture.ts');

const db = openDb(path.join(dataDir, 'irelia.db'));
after(() => { db.close(); closeCacheStores(); fs.rmSync(root, { recursive: true, force: true, maxRetries: 5 }); });

// An earlier version's folder: gzip files with their times, and timelines stored as plain JSON.
const files = new Map<string, { file: string; json: string; mtime: number }>();
const NOW = Date.now();
for (let i = 0; i < 30; i++) {
  const id = `KR_${9000 + i}`;
  const m = makeMatch({ id, champions: [IRELIA, 64, 103, 222, 412, 122, 121, 134, 21, 89] });
  const tl = makeTimeline(m);
  for (const [kind, value] of [['match', m], ['timeline', tl]] as const) {
    const file = legacyFile(cacheDir, kind, 'asia', id);
    fs.mkdirSync(path.dirname(file), { recursive: true });
    const json = JSON.stringify(value);
    fs.writeFileSync(file, zlib.gzipSync(json));
    const mtime = NOW - i * 60_000;
    fs.utimesSync(file, new Date(mtime), new Date(mtime));
    files.set(`${kind}:${id}`, { file, json, mtime });
  }
  db.insertMatch({ id, platform: 'KR', patch: '16.19', version: m.info.gameVersion, queue: 420, creation: m.info.gameCreation, duration: m.info.gameDuration, hasIrelia: true });
  // As 0.13.0 stored it: the capture in `data`.
  const cap = captureTimeline(tl);
  db.raw.prepare('INSERT INTO timelines(match_id, frame_interval, data) VALUES(?, ?, ?)').run(id, cap.interval, JSON.stringify(cap));
}
const league = legacyFile(cacheDir, 'league', 'KR', 'CHALLENGER');
fs.mkdirSync(path.dirname(league), { recursive: true });
fs.writeFileSync(league, zlib.gzipSync('{"entries":[1,2,3]}'));
const plain = new Map((db.raw.prepare('SELECT match_id, data FROM timelines').all() as { match_id: string; data: string }[]).map((r) => [r.match_id, r.data]));

function run(conv: InstanceType<typeof Conversion>, until: (s: ConversionStatus) => boolean, ms = 60_000) {
  return new Promise<ConversionStatus>((resolve, reject) => {
    const timer = setTimeout(() => reject(new Error('timed out')), ms);
    const poll = setInterval(() => {
      if (conv.status && until(conv.status)) { clearTimeout(timer); clearInterval(poll); resolve(conv.status); }
    }, 20);
  });
}

test('conversion: every API response and timeline re-encoded, read back the same, old copies gone; champ select stops it', async () => {
  const statuses: ConversionStatus[] = [];
  const conv = new Conversion({
    dataDir: () => dataDir, cacheDir: () => cacheDir,
    timelinesLeft: () => db.plainTimelinesAfter(Number(db.getMeta(TIMELINE_CURSOR) ?? 0)),
    onStatus: (s) => statuses.push(s),
  });
  assert.equal(conv.needed(), true);
  conv.check();
  // Champ select as soon as it has begun: it stops, and says it is paused.
  await run(conv, (s) => (s.cache?.done ?? 0) > 0 || s.timelines !== null || !s.running);
  conv.setBusy(true);
  assert.equal(conv.status?.paused, true);
  conv.setBusy(false);
  const done = await run(conv, (s) => !s.running && (s.done || s.error !== null));
  assert.equal(done.error, null);
  assert.equal(done.done, true);
  assert.equal(conv.needed(), false);

  // The old files and their folders are gone; each response reads back as it was, with its file's time.
  for (const k of ['match', 'timeline', 'league']) assert.equal(fs.existsSync(path.join(cacheDir, k)), false, `${k} folder`);
  const store = new CacheStore(cacheDir);
  for (const [k, f] of files) {
    const [kind, id] = k.split(':') as ['match' | 'timeline', string];
    const hit = store.read(kind, 'asia', id);
    assert.ok(hit, k);
    assert.equal(hit.raw.toString('utf8'), f.json, k);
    assert.equal(hit.storedAt, f.mtime, `${k}: its file's time`);
  }
  assert.equal(store.dicts.current()?.id, 1, 'compressed with a dictionary made from a match and a timeline');
  store.close();
  const cache = new DiskCache(cacheDir);
  assert.deepEqual(await cache.get('league', 'KR', 'CHALLENGER'), { entries: [1, 2, 3] });
  cache.close();

  // Timelines: compressed, the plain copy empty, and the same JSON through timeline_json and timelinesOf.
  const rows = db.raw.prepare('SELECT match_id, data, data_z FROM timelines').all() as { match_id: string; data: string; data_z: Uint8Array | null }[];
  assert.ok(rows.every((r) => r.data === '' && r.data_z instanceof Uint8Array), 'every timeline compressed');
  assert.deepEqual(db.timelinesOf([...plain.keys()]), plain);
  const zBytes = rows.reduce((n, r) => n + r.data_z!.length, 0), plainBytes = [...plain.values()].reduce((n, d) => n + d.length, 0);
  assert.ok(zBytes < plainBytes / 4, `compressed ${zBytes} of ${plainBytes} bytes`);
  assert.ok(statuses.length > 0);
});

test('conversion: a new timeline is stored compressed, and the archive keeps it as JSON', () => {
  const m = makeMatch({ id: 'KR_9999', champions: [IRELIA, 64, 103, 222, 412, 122, 121, 134, 21, 89] });
  db.insertMatch({ id: 'KR_9999', platform: 'KR', patch: '16.10', version: '16.10.1.1', queue: 420, creation: m.info.gameCreation, duration: m.info.gameDuration, hasIrelia: true });
  const cap = JSON.stringify(captureTimeline(makeTimeline(m)));
  db.insertTimeline('KR_9999', 60_000, cap);
  const row = db.raw.prepare("SELECT data, data_z FROM timelines WHERE match_id = 'KR_9999'").get() as { data: string; data_z: Uint8Array };
  assert.equal(row.data, '');
  assert.equal(db.timelinesOf(['KR_9999']).get('KR_9999'), cap);
  assert.equal(db.archiveBatch(['16.19']), 1);
  const archived = JSON.parse(zlib.gunzipSync(db.raw.prepare("SELECT data FROM archive WHERE match_id = 'KR_9999'").get()!.data as Uint8Array).toString('utf8'));
  assert.equal(archived.timelines[0].data, cap, 'archived as JSON');
  assert.equal(db.restoreBatch(null), 1);
  assert.equal(db.timelinesOf(['KR_9999']).get('KR_9999'), cap, 'restored compressed, the same');
  const back = db.raw.prepare("SELECT data, data_z FROM timelines WHERE match_id = 'KR_9999'").get() as { data: string; data_z: Uint8Array | null };
  assert.equal(back.data, '');
  assert.ok(back.data_z instanceof Uint8Array);
});

test('cache size index (0.14.1): built once, by a worker thread; the sizes and the keys read from it, the same as before', () => {
  const store = new CacheStore(cacheDir);
  const usage = 'SELECT kind, COUNT(*) AS files, SUM(LENGTH(data)) AS bytes FROM entries GROUP BY kind';
  const plan = () => (store.db.prepare(`EXPLAIN QUERY PLAN ${usage}`).all() as { detail: string }[]).map((r) => r.detail).join(' ');
  const version = () => (store.db.prepare("SELECT value FROM meta WHERE key = 'schema_version'").get() as { value: string }).value;
  const sorted = <T>(xs: T[]) => xs.map((x) => JSON.stringify(x)).sort();
  try {
    assert.doesNotMatch(plan(), /entries_size/, 'opening the store (the main thread does) builds nothing');
    const before = { usage: store.usage(), keys: sorted(store.matchEntries()) };
    assert.equal(store.ensureSizeIndex(), true);
    assert.equal(store.ensureSizeIndex(), false, 'once');
    assert.match(plan(), /COVERING INDEX entries_size/);
    assert.deepEqual(store.usage(), before.usage);
    assert.deepEqual(sorted(store.matchEntries()), before.keys);
    assert.equal(version(), '2');
    new CacheStore(cacheDir).close();
    assert.equal(version(), '2', 'another thread opening the file leaves the version as it is');
  } finally { store.close(); }
});
