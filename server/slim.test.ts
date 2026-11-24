// Slim stored data (server/slim.ts, slimControl.ts, slimWorker.ts; 0.14.0) on a synthetic data folder: a run whose
// results stay the same keeps its backup, and Roll back and Keep the slimmer data both work; a difference rolls it back
// by itself; a run interrupted at any step comes back at the next start; too little free space is refused.
import assert from 'node:assert/strict';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { after, test } from 'node:test';
import zlib from 'node:zlib';
import type { SlimStatus } from '../shared/types.ts';

const root = fs.mkdtempSync(path.join(os.tmpdir(), 'irelia-slim-'));
process.env.IRELIA_CONFIG_DIR = path.join(root, 'config');
const dataDir = path.join(root, 'data');
const cacheDir = path.join(dataDir, 'cache');

const { captureTimeline, participantRows, CAPTURE_VERSION } = await import('./capture.ts');
const { openDb, swappableDb } = await import('./db.ts');
const { captureGame } = await import('./scraper/ingest.ts');
const { apAdGames, synthMatch } = await import('./test/modelFixtures.ts');
const { loadTestStaticData } = await import('./test/staticData.ts');
const { CacheStore, closeCacheStores, DiskCache } = await import('./riot/diskCache.ts');
const slim = await import('./slim.ts');
const { SlimControl } = await import('./slimControl.ts');

// Data Dragon from the tests' snapshot, in the folder (the worker loads it from there).
const snap = JSON.parse(fs.readFileSync(new URL('./test/ddragon-snapshot.json', import.meta.url), 'utf8'));
const ddDir = path.join(dataDir, 'ddragon', snap.version);
fs.mkdirSync(ddDir, { recursive: true });
for (const [file, raw] of [['item.json', snap.raw.item], ['champion.json', snap.raw.champion], ['runesReforged.json', snap.raw.runesReforged],
  ['summoner.json', snap.raw.summoner], ['Irelia.json', snap.raw.champ]]) fs.writeFileSync(path.join(ddDir, file), JSON.stringify(raw));
const data = loadTestStaticData();

const store = swappableDb(openDb(path.join(dataDir, 'irelia.db')));
const db = store.handle;
const cache = new DiskCache(cacheDir);
after(() => { cache.close(); closeCacheStores(); try { db.close(); } catch { /* closed */ } fs.rmSync(root, { recursive: true, force: true, maxRetries: 5 }); });

/** Games on `patch`: stored as the scraper stores them, and their match and timeline cached as Riot sent them. */
async function store1(games: ReturnType<typeof apAdGames>, patch: string, cacheIt: boolean) {
  for (const s of games) {
    const { match, timeline } = synthMatch(s);
    const g = captureGame(match, timeline, match.info.participants[0], 'otp', undefined);
    db.tx(() => {
      db.insertMatch({ id: s.id, platform: 'KR', patch, version: match.info.gameVersion, queue: 420, creation: match.info.gameCreation, duration: match.info.gameDuration, hasIrelia: true });
      db.insertGame({ ...g.row, patch });
      db.insertParticipants(s.id, participantRows(match));
      db.insertTimeline(s.id, g.timeline.interval, JSON.stringify(captureTimeline(timeline)));
      db.setQuality(s.id, false, 'GameComplete', CAPTURE_VERSION);
    });
    if (cacheIt) { await cache.set('match', 'asia', s.id, match); await cache.set('timeline', 'asia', s.id, timeline); }
  }
}
// Today's games (the model's, cached: slimmed by (b)); 16.12's (older than two patches: their raw timelines go, (a));
// 16.10's, archived (older than patch memory's window: (c)).
await store1(apAdGames(120, 30), data.patch, true);
await store1(apAdGames(12, 6, 11).map((s, i) => ({ ...s, id: `KR_12${String(i).padStart(4, '0')}` })), '16.12', true);
await store1(apAdGames(12, 6, 13).map((s, i) => ({ ...s, id: `KR_10${String(i).padStart(4, '0')}` })), '16.10', false);
assert.ok(db.archiveBatch([data.patch, data.previousPatch, '16.12']) > 0);

const entries = () => {
  const s = new CacheStore(cacheDir);
  try { return new Map((s.db.prepare('SELECT kind, scope, key, data FROM entries').all() as { kind: 'match' | 'timeline'; scope: string; key: string }[])
    .map((e) => [`${e.kind}:${e.key}`, s.read(e.kind, e.scope, e.key)!.raw.toString('utf8')])); } finally { s.close(); }
};
const archiveRows = () => new Map((db.raw.prepare('SELECT match_id, data FROM archive').all() as { match_id: string; data: Uint8Array }[])
  .map((r) => [r.match_id, zlib.gunzipSync(r.data).toString('utf8')]));
const original = { entries: entries(), archive: archiveRows() };

function control() {
  const statuses: SlimStatus[] = [];
  const c = new SlimControl({
    dataDir: () => dataDir, cacheDir: () => cacheDir, versions: () => [snap.version], db: () => db,
    pauseWriters: async () => {}, resumeWriters: () => {},
    withDatabaseClosed: async (between) => {
      store.swap(openDb(':memory:')).close();
      closeCacheStores();
      try { await between(); } finally { store.swap(openDb(path.join(dataDir, 'irelia.db'))).close(); }
    },
    busy: () => false, blocked: () => null, onStatus: (s) => statuses.push(s),
  });
  return { c, statuses };
}
function settled(c: InstanceType<typeof SlimControl>, ms = 240_000) {
  return new Promise<SlimStatus>((resolve, reject) => {
    const t0 = Date.now();
    const poll = setInterval(() => {
      if (!c.status.running) { clearInterval(poll); resolve(c.status); } else if (Date.now() - t0 > ms) { clearInterval(poll); reject(new Error('timed out')); }
    }, 50);
  });
}

test('slim: refused without the free space for its backup, and nothing changes', async () => {
  process.env.IRELIA_SLIM_FREE_BYTES = '1000';
  try {
    const { c } = control();
    await c.start();
    const s = await settled(c);
    assert.equal(s.manifest?.step, 'cancelled');
    assert.match(s.manifest?.note ?? '', /Not enough free space for the backup: .* needed/);
    assert.equal(fs.existsSync(slim.slimDir(dataDir)), false, 'no backup folder');
    assert.deepEqual(entries(), original.entries);
  } finally { delete process.env.IRELIA_SLIM_FREE_BYTES; }
});

test('slim: a difference in the results rolls it back by itself, checked by SHA-256', async () => {
  process.env.IRELIA_SLIM_TEST_DIFFERENCE = '1';
  try {
    const wins = db.raw.prepare('SELECT match_id, win FROM irelia_games ORDER BY match_id').all();
    const { c } = control();
    await c.start();
    const s = await settled(c);
    assert.equal(s.manifest?.step, 'rolled-back', JSON.stringify(s));
    assert.ok((s.manifest?.differences ?? []).length > 0, 'what differed');
    assert.match(s.manifest?.note ?? '', /Rolled back by itself: .*SHA-256/);
    assert.deepEqual(db.raw.prepare('SELECT match_id, win FROM irelia_games ORDER BY match_id').all(), wins, 'the database as it was');
    assert.deepEqual(entries(), original.entries, 'the cache as it was');
    assert.deepEqual(archiveRows(), original.archive);
    assert.equal(fs.existsSync(slim.slimDir(dataDir)), false);
  } finally { delete process.env.IRELIA_SLIM_TEST_DIFFERENCE; }
});

test('slim: the same results keep the backup; Roll back puts everything back, Keep the slimmer data deletes the backup', async () => {
  const { c, statuses } = control();
  await c.start();
  const s = await settled(c);
  assert.equal(s.manifest?.step, 'verified', JSON.stringify(s));
  // 0.14.2: every phase shows how far it is, what it counted and a time to go by.
  const labels = new Set(statuses.filter((x) => x.running).map((x) => x.label));
  for (const l of ['Backing up: copying the database…', 'Backing up: moving the affected cache entries into the backup…', 'Recording the results before: the quick evaluation…',
    'Recording the results before: the drafts…', 'Slimming the cached matches and timelines…', 'Slimming the archive…', 'Freeing the disk space it saved…', 'Checking the results after: the drafts…']) {
    assert.ok(labels.has(l), `phase "${l}" in ${[...labels].join(' | ')}`);
  }
  const cached = statuses.filter((x) => x.label === 'Slimming the cached matches and timelines…');
  assert.deepEqual(cached.at(-1)?.count, { done: cached.at(-1)!.count!.total, total: cached.at(-1)!.count!.total, unit: 'games' });
  assert.equal(cached.at(-1)?.part, 1);
  assert.ok(statuses.every((x) => !x.running || x.label === 'Checking the free space…' || x.progress), 'a progress with every phase');
  // The check's phases expect what the baseline's took: an estimate from its start.
  const check = statuses.find((x) => x.step === 'verify');
  assert.ok(check?.progress && check.progress.etaMs !== null, JSON.stringify(check));
  assert.equal(s.progress, null);
  assert.equal(s.count, null);
  assert.equal(s.error, null);
  assert.ok(fs.existsSync(slim.slimDir(dataDir)), 'the backup is kept');
  assert.ok((s.manifest?.backupBytes ?? 0) > 0);
  assert.equal(s.manifest?.plan?.timelines, 12, "(a) 16.12's raw timelines");
  assert.ok((s.manifest?.applied?.slimmed ?? 0) > 0, '(b) matches and timelines slimmed');
  assert.equal(s.manifest?.applied?.archived, 12, "(c) 16.10's archived matches");
  const slimmed = entries();
  assert.equal([...slimmed.keys()].filter((k) => k.startsWith('timeline:KR_12')).length, 0, '(a) gone from the cache');
  const one = JSON.parse(slimmed.get(`match:${[...original.entries.keys()].find((k) => k.startsWith('match:'))!.slice(6)}`)!);
  assert.equal(one.info.participants[0].challenges?.unrelatedCosmetic, undefined, '(b) a field no capture reads is gone');
  const archived = JSON.parse([...archiveRows().values()][0]);
  assert.ok(archived.participants.filter((p: { puuid: string }) => p.puuid === '').length === 9, '(c) the other nine players cleared');
  // Another run waits for your choice.
  await assert.rejects(c.start(), /Roll back or Keep/);

  // Roll back: the cache entries and the archive as they were.
  const back = await c.rollback();
  assert.equal(back.manifest?.step, 'rolled-back', JSON.stringify(back));
  assert.deepEqual(entries(), original.entries);
  assert.deepEqual(archiveRows(), original.archive);
  assert.equal(fs.existsSync(slim.slimDir(dataDir)), false);

  // Again, then Keep the slimmer data: the backup goes, the slim data stays.
  await c.start();
  assert.equal((await settled(c)).manifest?.step, 'verified');
  const kept = c.keep();
  assert.equal(kept.manifest?.step, 'kept');
  assert.equal(fs.existsSync(slim.slimDir(dataDir)), false);
  assert.notDeepEqual(entries(), original.entries);
  assert.throws(() => c.keep(), /no verified run/);
});

test('slim: a run interrupted at each step comes back at the next start', async () => {
  // Undo the kept run first: these runs start from the original data.
  for (const [k, json] of original.entries) {
    const [kind, key] = k.split(':') as ['match' | 'timeline', string];
    await cache.set(kind, 'asia', key, JSON.parse(json));
  }
  const write = db.raw.prepare('UPDATE archive SET data = ? WHERE match_id = ?');
  for (const [id, json] of original.archive) write.run(zlib.gzipSync(json), id);
  assert.deepEqual(entries(), original.entries);
  const before = db.raw.prepare('SELECT match_id, win FROM irelia_games ORDER BY match_id').all();

  // The slimmed cache as each run left it: 'apply' slims on one thread, 'verify' on three (0.14.4), the same bytes.
  const slimmedBlobs = new Map<string, Map<string, string>>();
  const blobs = () => {
    const s = new CacheStore(cacheDir);
    try { return new Map((s.db.prepare('SELECT kind, key, data FROM entries').all() as { kind: string; key: string; data: Uint8Array }[]).map((e) => [`${e.kind}:${e.key}`, Buffer.from(e.data).toString('base64')])); } finally { s.close(); }
  };
  for (const stop of ['backup', 'baseline', 'apply', 'verify'] as const) {
    const s = new CacheStore(cacheDir);
    const plan = slim.planSlim(db, s, data);
    const m = { version: 1 as const, startedAt: Date.now(), updatedAt: Date.now(), step: 'backup' as const, plan: plan.plan, db: null, cache: null };
    slim.writeManifest(dataDir, m, db);
    await slim.backup(dataDir, db, s, plan, m);
    if (stop !== 'backup') { slim.writeManifest(dataDir, { ...m, step: 'baseline' }, db); }
    if (stop === 'apply' || stop === 'verify') {
      slim.writeManifest(dataDir, { ...m, step: 'apply' }, db);
      await slim.apply(dataDir, db, s, { ...m, step: 'apply' }, undefined, { threads: stop === 'apply' ? 1 : 3 });
      slimmedBlobs.set(stop, blobs());
      // A crash in the middle: the database part-way through as well.
      db.raw.exec('UPDATE irelia_games SET win = 1 - win WHERE rowid = 1');
      if (stop === 'verify') slim.writeManifest(dataDir, { ...m, step: 'verify' }, db);
    }
    s.close();
    // The app closes (or crashes) here; the next start runs recovery before it opens the database.
    store.swap(openDb(':memory:')).close();
    closeCacheStores();
    const said: string[] = [];
    if (stop === 'apply') {
      // 0.14.3: a recovery cut short itself (the app closed again) is done again from the start at the next one.
      await assert.rejects(slim.recoverSlim(dataDir, cacheDir, (t) => { if (t.startsWith('putting the cache entries back: ') && !t.endsWith(': 0 of 0')) throw new Error('closed'); }), /closed/);
    }
    const r = await slim.recoverSlim(dataDir, cacheDir, (t) => said.push(t));
    if (stop === 'apply' || stop === 'verify') {
      assert.match(said[0], /^copying the database's backup \(\d+ MB\)$/);
      assert.match(said.at(-1)!, /^putting the cache entries back: ([\d,]+) of \1$/, said.join(' | '));
    }
    store.swap(openDb(path.join(dataDir, 'irelia.db'))).close();
    assert.equal(r?.step, stop === 'backup' || stop === 'baseline' ? 'cancelled' : 'rolled-back', `interrupted at ${stop}`);
    assert.equal(fs.existsSync(slim.slimDir(dataDir)), false, `${stop}: no backup left`);
    assert.deepEqual(entries(), original.entries, `${stop}: the cache as it was`);
    assert.deepEqual(archiveRows(), original.archive, `${stop}: the archive as it was`);
    assert.deepEqual(db.raw.prepare('SELECT match_id, win FROM irelia_games ORDER BY match_id').all(), before, `${stop}: the games as they were`);
  }
  assert.ok(slimmedBlobs.get('apply')!.size > 0);
  assert.deepEqual(slimmedBlobs.get('verify'), slimmedBlobs.get('apply'), 'three threads write the same bytes as one');
});

test("slim (a): an archived game's raw timeline goes only when the archive holds its capture at today's version", async () => {
  const id = [...original.archive.keys()][0];
  await cache.set('timeline', 'asia', id, { metadata: { matchId: id }, info: { frames: [] } });
  const write = db.raw.prepare('UPDATE archive SET data = ? WHERE match_id = ?');
  const plan = () => { const s = new CacheStore(cacheDir); try { return slim.planSlim(db, s, data); } finally { s.close(); } };
  try {
    assert.ok(plan().timelineKeys.some((k) => k.key === id), 'captured at today\'s version: its raw timeline can go');
    const rows = JSON.parse(original.archive.get(id)!) as { matches: { detail_version: number }[] };
    rows.matches[0].detail_version = CAPTURE_VERSION - 1;
    write.run(zlib.gzipSync(JSON.stringify(rows)), id);
    const older = plan();
    assert.equal(older.timelineKeys.some((k) => k.key === id), false, 'an older capture: kept, a later version may capture it again');
    assert.ok(older.slimKeys.some((k) => k.kind === 'timeline' && k.key === id));
  } finally {
    write.run(zlib.gzipSync(original.archive.get(id)!), id);
    const s = new CacheStore(cacheDir);
    s.delete('timeline', 'asia', id);
    s.close();
  }
});

test('slim: Stop ends a run at once and undoes it (0.15.0)', async () => {
  const before = entries();
  const { c, statuses } = control();
  await c.start();
  const s0 = c.stop();
  assert.equal(s0.stopping, true);
  assert.match(s0.label ?? '', /^Stopping/);
  const s = await settled(c);
  assert.equal(s.manifest?.step, 'cancelled', JSON.stringify(s));
  assert.match(s.manifest?.note ?? '', /you stopped it/);
  assert.equal(s.stopping, false);
  assert.equal(fs.existsSync(slim.slimDir(dataDir)), false, 'no backup left');
  assert.deepEqual(entries(), before, 'the cache as it was');
  assert.ok(statuses.length > 0);
  assert.throws(() => c.stop(), /not running/);
});
