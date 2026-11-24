// "database is locked" while fetching (0.8.5): another thread's writes must not make fetching's writes fail.
// A transaction that reads before it writes used to fail at once whenever another thread wrote meanwhile; now every
// write transaction takes the lock first (Db.tx) and waits for it. Archive and restore batches leave the lock free
// long enough between batches for a waiting write to get in. Threads other than the main one write nothing on open.
import assert from 'node:assert/strict';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { after, test } from 'node:test';
import { Worker } from 'node:worker_threads';

const root = fs.mkdtempSync(path.join(os.tmpdir(), 'irelia-locks-'));
process.env.IRELIA_CONFIG_DIR = path.join(root, 'config');

const { captureTimeline, participantRows, CAPTURE_VERSION } = await import('./capture.ts');
const { openDb } = await import('./db.ts');
const { captureGame } = await import('./scraper/ingest.ts');
const { apAdGames, synthMatch } = await import('./test/modelFixtures.ts');
const { loadTestStaticData } = await import('./test/staticData.ts');
const { OldPatches } = await import('./oldPatches.ts');

const file = path.join(root, 'irelia.db');
const db = openDb(file);
after(() => { db.close(); fs.rmSync(root, { recursive: true, force: true, maxRetries: 5 }); });
const data = loadTestStaticData();

function store(n: number, patch: string, from: number) {
  db.tx(() => {
    for (const s of apAdGames(n, 20, from).map((g, i) => ({ ...g, id: `KR_${from}${String(i).padStart(6, '0')}`, version: `${patch}.1.1` }))) {
      const { match, timeline } = synthMatch(s);
      const cap = captureTimeline(timeline);
      const { row } = captureGame(match, timeline, match.info.participants[0], 'otp', undefined);
      db.insertMatch({ id: s.id, platform: 'KR', patch, version: match.info.gameVersion, queue: 420, creation: match.info.gameCreation, duration: match.info.gameDuration, hasIrelia: true });
      db.insertGame(row);
      db.insertParticipants(s.id, participantRows(match));
      db.insertTimeline(s.id, cap.interval, JSON.stringify(cap));
      db.setQuality(s.id, false, 'GameComplete', CAPTURE_VERSION);
    }
  });
}
store(40, data.patch, 1);
store(1200, '16.8', 2);   // older than the six-patch window: six batches to archive

/**
 * Runs `work` on the main connection while the writer thread writes; returns what the writer saw, and how many of its
 * writes went in while `work` ran (`during`). The writer stops even when `work` fails: left running, it kept the test
 * file alive until CI's timeout.
 */
async function whileWriting(work: () => Promise<unknown> | unknown) {
  const writer = new Worker(new URL('./worker-dev.mjs', import.meta.url), {
    workerData: { entry: new URL('./test/lockWriter.ts', import.meta.url).href, file, busyTimeoutMs: 30_000 },
  });
  try {
    await new Promise((r) => writer.once('message', r));
    await new Promise((r) => setTimeout(r, 200));
    const from = Date.now();
    await work();
    const to = Date.now();
    await new Promise((r) => setTimeout(r, 200));
    writer.postMessage('stop');
    const seen = await new Promise<{ writes: number; slowest: number; errors: string[]; at: number[] }>((r) => writer.once('message', r));
    return { ...seen, during: seen.at.filter((t) => t >= from && t <= to).length };
  } finally {
    await writer.terminate();
  }
}

const old = new OldPatches({ db: () => db, data: () => data, cacheDir: () => path.join(root, 'cache'), blocked: () => false, idle: () => true });

test('database locks: fetching keeps writing while the archive switch archives and restores old patches', async () => {
  const seen = await whileWriting(async () => {
    const on = await old.setArchive(true);
    assert.deepEqual(on.archived.map((a) => [a.patch, a.games]), [['16.8', 1200]]);
    const off = await old.setArchive(false);
    assert.equal(off.archived.length, 0);
  });
  assert.deepEqual(seen.errors, [], 'no "database is locked"');
  // Between the batches, not only once they are all done (how many depends on the machine: a slow one gets a few).
  assert.ok(seen.during > 0, `the writer kept writing (${seen.during} of ${seen.writes} writes during it, slowest ${seen.slowest} ms)`);
});

test('database locks: fetching keeps writing while games of old patches are deleted', async () => {
  const seen = await whileWriting(() => { assert.equal(db.purgeStale([data.patch, data.previousPatch]), 1200); });
  assert.deepEqual(seen.errors, []);
  assert.ok(seen.writes > 0);
});

test('database locks: the other threads write nothing when they open the database', () => {
  // Another connection holds the write lock: a thread's open must not wait for it (before 0.8.5 it ran the schema's
  // fixes, writes that waited up to 5 s and then failed).
  const holder = openDb(file, { migrate: false });
  holder.raw.exec('BEGIN IMMEDIATE');
  try {
    const t = performance.now();
    const reader = openDb(file, { migrate: false, busyTimeoutMs: 2000 });
    assert.ok(performance.now() - t < 1000, 'opened without waiting for the lock');
    assert.ok(reader.dataVersion());
    reader.close();
  } finally { holder.raw.exec('ROLLBACK'); holder.close(); }
});
