// The background conversion (0.14.0, server/convert.ts runs it): what earlier versions stored is re-encoded, a batch
// at a time, losslessly, and each item is read back and compared before its old copy goes.
//   1. The Riot API cache's gzip files (<cache>/<kind>/<scope>/<key>.json.gz) into riot-cache.db, keeping each one's
//      time (its mtime) for its TTL; the file goes once its entry reads back the same.
//   2. The database's timelines into data_z (db.ts compressTimelines), from a cursor kept in meta.
// It stops whenever the main thread terminates it (champ select, a game, a folder move) and resumes where it was.
import fs from 'node:fs/promises';
import path from 'node:path';
import { promisify } from 'node:util';
import zlib from 'node:zlib';
import { parentPort, workerData } from 'node:worker_threads';
import { config, setDataDir } from './config.ts';
import { TIMELINE_CURSOR } from './convert.ts';
import { openDb } from './db.ts';
import { CACHE_TTL, CacheStore, type CacheKind } from './riot/diskCache.ts';
import type { ConversionStatus } from '../shared/types.ts';

const gunzip = promisify(zlib.gunzip);
const boot = workerData as { dataDir: string };
setDataDir(boot.dataDir);

/** Entries per transaction, and the rest between batches: the thread gives way to fetching's writes. */
const BATCH = 40;
const REST_MS = 25;

const rest = () => new Promise((r) => setTimeout(r, REST_MS));
const post = (status: ConversionStatus) => parentPort!.postMessage({ type: 'status', status });

type Legacy = { kind: CacheKind; scope: string; key: string; file: string };

/**
 * The old cache files one at a time, matches and timelines first (most of the space). A folder may hold hundreds of
 * thousands of them: they are read from the folder as they go, never listed all at once (0.14.1).
 */
async function* legacyFiles(root: string): AsyncGenerator<Legacy> {
  const kinds = (Object.keys(CACHE_TTL) as CacheKind[]).sort((a, b) => Number(b === 'match' || b === 'timeline') - Number(a === 'match' || a === 'timeline'));
  for (const kind of kinds) {
    for (const scope of await fs.readdir(path.join(root, kind)).catch(() => [] as string[])) {
      let dir: Awaited<ReturnType<typeof fs.opendir>>;
      try { dir = await fs.opendir(path.join(root, kind, scope)); } catch { continue; }
      for await (const e of dir) {
        if (e.isFile() && e.name.endsWith('.json.gz')) yield { kind, scope, key: e.name.slice(0, -'.json.gz'.length), file: path.join(root, kind, scope, e.name) };
      }
    }
  }
}

async function convertCache(status: ConversionStatus) {
  const root = config.riotCacheDir;
  // A first pass counts them (the progress) and finds the newest match and timeline by id (match ids grow over time).
  let total = 0;
  const newest: Partial<Record<CacheKind, Legacy>> = {};
  for await (const f of legacyFiles(root)) {
    total++;
    if ((f.kind === 'match' || f.kind === 'timeline') && (!newest[f.kind] || f.key > newest[f.kind]!.key)) newest[f.kind] = f;
  }
  if (!total) return;
  const store = new CacheStore(root);
  try {
    status.cache = { done: 0, total, skipped: 0 };
    post(status);
    // The dictionary, from those two, before the first entry.
    const m = newest.match, t = newest.timeline;
    if (m && t && !store.dicts.current()) {
      try { store.ensureDictionary(await gunzip(await fs.readFile(m.file)), await gunzip(await fs.readFile(t.file))); } catch { /* none: compressed without */ }
    }
    const convert = async (batch: Legacy[]) => {
      const read = (await Promise.all(batch.map(async (f) => {
        try {
          const [st, gz] = await Promise.all([fs.stat(f.file), fs.readFile(f.file)]);
          return { ...f, mtime: Math.round(st.mtimeMs), raw: await gunzip(gz) as Buffer | null };
        } catch (err) {
          // Gone already (a folder can list a file it has just deleted): nothing to do. Else unreadable: left as it is.
          return (err as NodeJS.ErrnoException).code === 'ENOENT' ? null : { ...f, mtime: 0, raw: null };
        }
      }))).filter((f) => f !== null);
      // Compressed before the transaction: the cache's write lock is held for the writes and checks alone.
      const blobs = read.map((f) => (f.raw ? store.encode(f.kind, f.raw) : null));
      const done: string[] = [];
      store.db.exec('BEGIN IMMEDIATE');
      try {
        read.forEach((f, j) => {
          if (!f.raw) { status.cache!.skipped++; return; }
          const have = store.read(f.kind, f.scope, f.key);
          // Fetched again since (a newer entry): the old file has nothing to add.
          if (!have || have.storedAt < f.mtime) {
            store.put(f.kind, f.scope, f.key, blobs[j]!, f.mtime);
            const back = store.read(f.kind, f.scope, f.key);
            if (!back || !back.raw.equals(f.raw)) throw new Error(`${f.kind} ${f.key} did not read back the same: its file is kept`);
          }
          done.push(f.file);
        });
        store.db.exec('COMMIT');
      } catch (err) { store.db.exec('ROLLBACK'); throw err; }
      // Only once its entry is committed and read back: the file goes.
      await Promise.all(done.map((file) => fs.rm(file, { force: true })));
      status.cache!.done = Math.min(total, status.cache!.done + read.length);
      post(status);
      await rest();
    };
    let batch: Legacy[] = [];
    for await (const f of legacyFiles(root)) {
      batch.push(f);
      if (batch.length === BATCH) { await convert(batch); batch = []; }
    }
    if (batch.length) await convert(batch);
    // Empty folders go; a folder with a file it couldn't read stays (and the cache keeps reading it).
    for (const kind of Object.keys(CACHE_TTL)) {
      for (const scope of await fs.readdir(path.join(root, kind)).catch(() => [] as string[])) await fs.rmdir(path.join(root, kind, scope)).catch(() => {});
      await fs.rmdir(path.join(root, kind)).catch(() => {});
    }
    store.reclaim();
  } finally { store.close(); }
}

async function convertTimelines(status: ConversionStatus) {
  const db = openDb(config.dbPath, { migrate: false, busyTimeoutMs: 30_000 });
  try {
    let cursor = Number(db.getMeta(TIMELINE_CURSOR) ?? 0);
    const last = db.lastTimelineRow();
    if (cursor >= last) return;
    status.timelines = { fraction: last ? cursor / last : 1, converted: 0 };
    post(status);
    let batches = 0;
    for (;;) {
      const r = db.compressTimelines(cursor, 100);
      if (r.next === null) break;
      cursor = r.next;
      db.setMeta(TIMELINE_CURSOR, String(cursor));
      status.timelines.converted += r.converted;
      status.timelines.fraction = Math.min(1, cursor / Math.max(1, db.lastTimelineRow()));
      post(status);
      // The pages the plain copies leave come back as it goes (once the database has incremental auto-vacuum).
      if (++batches % 20 === 0) db.reclaim();
      await rest();
    }
    db.setMeta(TIMELINE_CURSOR, String(Math.max(cursor, db.lastTimelineRow())));
    db.reclaim();
    status.timelines.fraction = 1;
  } finally { db.close(); }
}

const status: ConversionStatus = { running: true, cache: null, timelines: null, error: null, done: false };
const message = (err: unknown) => (err instanceof Error ? err.message : String(err));
// Each on its own: a cache entry that won't convert leaves the timelines' conversion alone (and the other way round).
try { await convertCache(status); } catch (err) { status.error = message(err); }
try { await convertTimelines(status); } catch (err) { status.error = status.error ? `${status.error}; ${message(err)}` : message(err); }
status.done = status.error === null;
status.running = false;
post(status);
parentPort!.postMessage({ type: 'exit' });
