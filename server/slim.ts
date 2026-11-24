// Slim stored data (0.14.0, Settings → Storage): the one action that deletes stored data, behind a backup, a check of
// the results and a rollback. It never runs by itself, nor in champ select or a game. What it slims:
//   (a) cached raw timelines of matches older than the newest two patches, whose games the database has captured;
//   (b) the fields of the other cached matches and timelines no capture reads (slimReads.ts);
//   (c) in the archive (patches older than patch memory's window, whose per-champion sums champion_patch_stats
//       already holds), the columns of the other nine players' rows that neither the draft model nor the archive's
//       sums read.
// Steps, each recorded in a manifest (backup-0.14.0/manifest.json, and meta slim_manifest) so an interrupted run
// carries on or rolls back at the next start:
//   1. free space for the backup, else refuse;  2. backup: a VACUUM INTO copy of the database and the affected cache
//   entries moved into backup-0.14.0/cache-entries.db;  3. baseline: a quick evaluation and 200 drafts' outputs;
//   4. apply;  5. verify: the same again, every number identical, else the automatic rollback.
// Then the backup stays until you choose: Roll back (restore and check) or Keep the slimmer data (the backup goes).
import { createHash } from 'node:crypto';
import fs from 'node:fs';
import path from 'node:path';
import { DatabaseSync } from 'node:sqlite';
import zlib from 'node:zlib';
import type { Lane, SlimCount, SlimManifest, SlimPlan } from '../shared/types.ts';
import { CAPTURE_VERSION } from './capture.ts';
import { config } from './config.ts';
import type { Db } from './db.ts';
import type { StaticData } from './ddragon.ts';
import { buildExclusivityIndex, isPurchasableItem } from './engine/exclusivity.ts';
import { recommend } from './engine/recommend.ts';
import { isCompletedItem } from './engine/validator.ts';
import { standardise } from './model/context.ts';
import { gameTimeline, momentsAt } from './model/dataset.ts';
import { observed, weighGames } from './model/draft.ts';
import { estimateDecision } from './model/estimate.ts';
import { evaluateDataFolder } from './model/runEval.ts';
import { beforeWindow, modelPatches, ModelService } from './model/service.ts';
import { regionPatches } from './patches.ts';
import { stepFraction } from './progress.ts';
import { CacheStore, type CacheKind } from './riot/diskCache.ts';
import { DEFAULT_SCOPE, loadScope } from './scrapeScope.ts';
import { slimGame, type PartCodec, type PartGame, type PartResult } from './slimPart.ts';
import { availableMemory, cores, sharedHeapCeiling } from './model/evalTeam.ts';
import { startWorker } from './workers.ts';
import { freeSpace, HEADROOM } from './storage.ts';

export const SLIM_DIR = 'backup-0.14.0';
export const SLIM_META = 'slim_manifest';
const MANIFEST = 'manifest.json';
const DB_COPY = 'irelia.db';
const ENTRIES = 'cache-entries.db';
const ARCHIVE_LIST = 'archive-slimmed.json';
const BASELINE = 'baseline.json';

export const slimDir = (dataDir: string) => path.join(dataDir, SLIM_DIR);

/** What a rollback is doing, for whoever waits on it (the app's start shows it, 0.14.3). */
export type RollbackTick = (text: string) => void;
const RESTORE_BATCH = 500;

/**
 * A step's progress, phase by phase (0.14.2): the phase's id and label, how far it is (0-1; null: it can't be counted)
 * and what it has counted.
 */
export type SlimTick = (phase: string, label: string, part: number | null, count?: SlimCount | null) => void;

const MB = 1024 * 1024;
const mb = (bytes: number) => Math.round(bytes / MB);

/** The backup's database copy as VACUUM INTO writes it: its bytes so far, of about `total` (the database's pages in use). */
export function copyCount(dataDir: string, total: number): { part: number; count: SlimCount } {
  const done = fileSize(path.join(slimDir(dataDir), DB_COPY));
  return { part: Math.min(0.99, done / Math.max(1, total)), count: { done: mb(done), total: Math.max(mb(done), mb(total)), unit: 'MB' } };
}

// ------------------------------------------------------------------------------------------- manifest

export function readManifest(dataDir: string): SlimManifest | null {
  try { return JSON.parse(fs.readFileSync(path.join(slimDir(dataDir), MANIFEST), 'utf8')) as SlimManifest; } catch { return null; }
}

/** Saves the manifest in the backup folder (what recovery reads) and in the database's meta (when given one). */
export function writeManifest(dataDir: string, m: SlimManifest, db?: Pick<Db, 'setMeta'> | null) {
  m.updatedAt = Date.now();
  fs.mkdirSync(slimDir(dataDir), { recursive: true });
  const file = path.join(slimDir(dataDir), MANIFEST);
  fs.writeFileSync(`${file}.tmp`, `${JSON.stringify(m, null, 2)}\n`);
  fs.renameSync(`${file}.tmp`, file);
  try { db?.setMeta(SLIM_META, JSON.stringify(m)); } catch { /* the folder's copy is the one recovery reads */ }
}

export async function sha256File(file: string, onBytes?: (done: number, total: number) => void): Promise<string> {
  const h = createHash('sha256');
  const total = fileSize(file);
  let done = 0, last = 0;
  for await (const chunk of fs.createReadStream(file)) {
    h.update(chunk as Buffer);
    done += (chunk as Buffer).length;
    if (onBytes && done - last >= 64 * MB) { last = done; onBytes(done, total); }
  }
  return h.digest('hex');
}

// ------------------------------------------------------------------------------------------- plan

const dbFiles = (dataDir: string) => ['irelia.db', 'irelia.db-wal', 'irelia.db-shm'].map((f) => path.join(dataDir, f));
const fileSize = (f: string) => fs.statSync(f, { throwIfNoEntry: false })?.size ?? 0;

/**
 * What a run would touch: (a) the timelines it would move out, (b) the matches and timelines it would slim, (c) the
 * archived matches it would slim; and the space the backup needs.
 */
export function planSlim(db: Db, store: CacheStore, data: StaticData): { plan: SlimPlan; timelineKeys: Key[]; slimKeys: Key[]; archived: string[] } {
  const newest = new Set(modelPatches(db, data, 2));
  const old = beforeWindow(db, data);
  const matchInfo = new Map((db.raw.prepare(`SELECT m.match_id, m.patch, m.detail_version, (t.match_id IS NOT NULL) AS captured
    FROM matches m LEFT JOIN timelines t USING(match_id)`).all() as { match_id: string; patch: string; detail_version: number; captured: number }[])
    .map((r) => [r.match_id, r]));
  const archivedPatch = new Map((db.raw.prepare('SELECT match_id, patch FROM archive').all() as { match_id: string; patch: string }[]).map((r) => [r.match_id, r.patch]));
  // An archived match counts as captured as a stored one does: at today's capture version, with its timeline (read
  // from the archive's copy, only for the archived matches whose raw timeline is cached).
  const archivedRow = db.raw.prepare('SELECT data FROM archive WHERE match_id = ?');
  const archivedCaptured = (id: string) => {
    const row = archivedRow.get(id) as { data: Uint8Array } | undefined;
    if (!row) return false;
    try {
      const rows = JSON.parse(zlib.gunzipSync(row.data).toString('utf8')) as { matches?: { detail_version?: number }[]; timelines?: unknown[] };
      return (rows.matches?.[0]?.detail_version ?? 0) >= CAPTURE_VERSION && (rows.timelines?.length ?? 0) > 0;
    } catch { return false; }
  };
  const timelineKeys: Key[] = [], slimKeys: Key[] = [];
  let timelineBytes = 0, slimBytes = 0;
  for (const e of store.matchEntries()) {
    const m = matchInfo.get(e.key);
    const patch = m?.patch ?? archivedPatch.get(e.key);
    // (a) A raw timeline whose game is captured (stored, or archived), older than the two newest patches.
    const captured = (m && m.detail_version >= CAPTURE_VERSION && m.captured === 1)
      || (!m && e.kind === 'timeline' && patch !== undefined && !newest.has(patch) && archivedPatch.has(e.key) && archivedCaptured(e.key));
    if (e.kind === 'timeline' && patch !== undefined && !newest.has(patch) && captured) { timelineKeys.push(e); timelineBytes += e.bytes; continue; }
    slimKeys.push(e); slimBytes += e.bytes;
  }
  const archived = [...archivedPatch].filter(([, patch]) => old(patch) === true).map(([id]) => id);
  const archiveBytes = archived.length
    ? (db.raw.prepare('SELECT COALESCE(SUM(LENGTH(data)), 0) AS b FROM archive WHERE match_id IN (SELECT value FROM json_each(?))').get(JSON.stringify(archived)) as { b: number }).b : 0;
  const page = db.raw.prepare('SELECT (SELECT page_count FROM pragma_page_count()) - (SELECT freelist_count FROM pragma_freelist_count()) AS pages, (SELECT page_size FROM pragma_page_size()) AS size').get() as { pages: number; size: number };
  const dbCopyBytes = page.pages * page.size;
  return {
    plan: {
      timelines: timelineKeys.length, timelineBytes, entries: slimKeys.length, entryBytes: slimBytes,
      archivedMatches: archived.length, archiveBytes, dbCopyBytes, neededBytes: dbCopyBytes + timelineBytes + slimBytes + HEADROOM,
    },
    timelineKeys, slimKeys, archived,
  };
}
type Key = { kind: 'match' | 'timeline'; scope: string; key: string; bytes: number };

/** null when the backup fits with the reserve left; else why not. */
export async function spaceProblem(dataDir: string, neededBytes: number): Promise<string | null> {
  // IRELIA_SLIM_FREE_BYTES: a drive this full, for the tests.
  const fake = process.env.IRELIA_SLIM_FREE_BYTES;
  const free = fake ? Number(fake) : (await freeSpace(dataDir))?.freeBytes;
  if (free === undefined || free >= neededBytes) return null;
  const gb = (b: number) => `${(b / 1e9).toFixed(2)} GB`;
  return `Not enough free space for the backup: ${gb(neededBytes)} needed (with the usual reserve), ${gb(free)} free. Nothing was changed.`;
}

// ------------------------------------------------------------------------------------------- backup

const ENTRY_SCHEMA = `CREATE TABLE IF NOT EXISTS entries (
  kind TEXT NOT NULL, scope TEXT NOT NULL, key TEXT NOT NULL, stored_at INTEGER NOT NULL, data BLOB NOT NULL,
  part TEXT NOT NULL,                       -- 'a': moved out for good; 'b': slimmed (its original kept here)
  PRIMARY KEY (kind, scope, key));`;

/**
 * Step 2: the database copied (VACUUM INTO, then its SHA-256), and every affected cache entry moved into the backup's
 * entries database (with the cache's own blob: the dictionary it names stays in the cache's codec_dicts).
 */
export async function backup(dataDir: string, db: Db, store: CacheStore, plan: ReturnType<typeof planSlim>, m: SlimManifest, tick?: SlimTick) {
  const dir = slimDir(dataDir);
  fs.mkdirSync(dir, { recursive: true });
  const copy = path.join(dir, DB_COPY);
  fs.rmSync(copy, { force: true });
  // How far the copy is, the main thread sees from its size (this thread waits on VACUUM INTO meanwhile).
  tick?.('copy', 'Backing up: copying the database…', 0);
  db.vacuumInto(copy);
  const hashing = 'Backing up: checking the copy (SHA-256)…';
  tick?.('hash', hashing, 0);
  const sha256 = await sha256File(copy, (done, total) => tick?.('hash', hashing, done / Math.max(1, total), { done: mb(done), total: mb(total), unit: 'MB' }));
  m.db = { file: DB_COPY, bytes: fileSize(copy), sha256 };
  m.archived = plan.archived.length;
  fs.writeFileSync(path.join(dir, ARCHIVE_LIST), JSON.stringify(plan.archived));
  writeManifest(dataDir, m, db);

  const out = new DatabaseSync(path.join(dir, ENTRIES));
  try {
    out.exec('PRAGMA journal_mode = WAL; PRAGMA synchronous = NORMAL;');
    out.exec(ENTRY_SCHEMA);
    const put = out.prepare('INSERT OR REPLACE INTO entries(kind, scope, key, stored_at, data, part) VALUES(?, ?, ?, ?, ?, ?)');
    const get = store.db.prepare('SELECT stored_at, data FROM entries WHERE kind = ? AND scope = ? AND key = ?');
    const all = [...plan.timelineKeys.map((k) => ({ ...k, part: 'a' })), ...plan.slimKeys.map((k) => ({ ...k, part: 'b' }))];
    let bytes = 0;
    const moving = 'Backing up: moving the affected cache entries into the backup…';
    tick?.('entries', moving, 0, { done: 0, total: all.length, unit: 'cache entries' });
    for (let i = 0; i < all.length; i += 200) {
      const batch = all.slice(i, i + 200);
      // Into the backup first (committed), then out of the cache: an entry is always in one of the two.
      out.exec('BEGIN');
      const rows: typeof batch = [];
      try {
        for (const k of batch) {
          const row = get.get(k.kind, k.scope, k.key) as { stored_at: number; data: Uint8Array } | undefined;
          if (!row) continue;
          put.run(k.kind, k.scope, k.key, row.stored_at, row.data, k.part);
          bytes += row.data.length;
          rows.push(k);
        }
        out.exec('COMMIT');
      } catch (err) { out.exec('ROLLBACK'); throw err; }
      store.db.exec('BEGIN IMMEDIATE');
      try { for (const k of rows) store.delete(k.kind, k.scope, k.key); store.db.exec('COMMIT'); } catch (err) { store.db.exec('ROLLBACK'); throw err; }
      tick?.('entries', moving, Math.min(1, (i + batch.length) / Math.max(1, all.length)), { done: i + batch.length, total: all.length, unit: 'cache entries' });
    }
    m.cache = { file: ENTRIES, entries: (out.prepare('SELECT COUNT(*) AS n FROM entries').get() as { n: number }).n, bytes };
  } finally { out.close(); }
  writeManifest(dataDir, m, db);
}

// ------------------------------------------------------------------------------------------- apply

/** Columns of the other nine players' rows that nothing reads once a match is archived (its sums are in champion_patch_stats). */
const CLEARED: Record<string, unknown> = { puuid: '', champ_level: 0, kills: 0, deaths: 0, assists: 0, gold_earned: 0, cs: 0, items: '[]', cc_dealt: 0, time_played: 0, challenges: null };

/** An archived match's rows with the other nine players' unread columns cleared (null: nothing to clear). */
export function slimArchived(rows: Record<string, Record<string, unknown>[]>): Record<string, Record<string, unknown>[]> | null {
  const irelia = new Set((rows.irelia_games ?? []).map((g) => g.participant_id));
  let changed = false;
  const participants = (rows.participants ?? []).map((p) => {
    if (irelia.has(p.participant_id)) return p;
    const next = { ...p, ...CLEARED };
    if (Object.keys(CLEARED).some((k) => p[k] !== next[k])) changed = true;
    return next;
  });
  return changed ? { ...rows, participants } : null;
}

/** Games per message to a slimming thread, and per transaction of the writes. */
const PART_GAMES = 50;
/** A slimming thread's heap at most (a timeline's JSON and its slim copy, 50 games at a time). */
const PART_HEAP_MB = 384;

/**
 * Threads to slim on (0.14.4): every core but the one writing, as the free memory allows, at most 8 (the writes, on
 * one thread, keep up with about that many). Where every thread shares one heap (the desktop app, 0.15.2), their caps
 * together stay within 1.5 GB of its 4 GB (4 threads): the model thread and this one hold the rest.
 */
export function slimThreads() {
  const byHeap = sharedHeapCeiling() ? Math.floor(SLIM_SHARED_HEAP_MB / PART_HEAP_MB) : Infinity;
  return Math.max(1, Math.min(8, byHeap, cores() - 1, Math.floor(availableMemory() / 2 / (PART_HEAP_MB * MB))));
}
/** What the slimming threads may hold of a shared heap together. */
const SLIM_SHARED_HEAP_MB = 1536;

/**
 * Step 4: (a) stays out; (b) each match and timeline written back slim where every capture comes out the same; (c).
 * (b) runs on `threads` (0.14.4; default slimThreads()): the games go out 50 at a time to the slimming threads, and
 * this thread writes what comes back, a transaction per 50 games. The blobs are the same on any number of threads.
 */
export async function apply(dataDir: string, db: Db, store: CacheStore, m: SlimManifest, tick?: SlimTick, o: { threads?: number } = {}) {
  const dir = slimDir(dataDir);
  const src = new DatabaseSync(path.join(dir, ENTRIES), { readOnly: true });
  let slimmed = 0, kept = 0, savedBytes = 0;
  try {
    const ids = src.prepare("SELECT DISTINCT scope, key FROM entries WHERE part = 'b' ORDER BY scope, key").all() as { scope: string; key: string }[];
    // By the primary key (kind, scope, key): without the kinds, each game scanned the whole backup, blobs and all (0.14.4).
    const one = src.prepare("SELECT kind, stored_at, data FROM entries WHERE kind IN ('match', 'timeline') AND scope = ? AND key = ? AND part = 'b'");
    const cached = 'Slimming the cached matches and timelines…';
    tick?.('cache', cached, 0, { done: 0, total: ids.length, unit: 'games' });
    // Without a dictionary yet, the first blob written makes one (CacheStore.encode): then on this thread alone.
    const current = store.dicts.current();
    const threads = current ? Math.max(1, o.threads ?? slimThreads()) : 1;
    const codec: PartCodec = { get: store.dicts.get, current, pack: (kind, raw) => store.encode(kind as CacheKind, raw) };
    const write = (results: PartResult[]) => {
      store.db.exec('BEGIN IMMEDIATE');
      try {
        for (const r of results) {
          for (const p of r.puts) {
            store.put(p.kind as CacheKind, r.scope, r.key, p.blob, p.storedAt);
            if (p.slim) { slimmed++; savedBytes += p.saved; } else kept++;
          }
        }
        store.db.exec('COMMIT');
      } catch (err) { store.db.exec('ROLLBACK'); throw err; }
    };
    const chunks = Math.ceil(ids.length / PART_GAMES);
    const chunk = (c: number): PartGame[] => ids.slice(c * PART_GAMES, (c + 1) * PART_GAMES)
      .map(({ scope, key }) => ({ scope, key, rows: one.all(scope, key) as PartGame['rows'] }));
    let done = 0;
    const progress = (n: number) => {
      done += n;
      tick?.('cache', cached, done / Math.max(1, ids.length), { done, total: ids.length, unit: 'games' });
    };
    if (threads === 1) {
      for (let c = 0; c < chunks; c++) {
        const games = chunk(c);
        write(games.map((g) => slimGame(g, codec)));
        progress(games.length);
      }
    } else {
      await onThreads(threads, store.dicts.all(), current!.id, chunks, chunk, (results) => { write(results); progress(results.length); });
    }
  } finally { src.close(); }

  // (c) The archive's old patches.
  const list = JSON.parse(fs.readFileSync(path.join(dir, ARCHIVE_LIST), 'utf8')) as string[];
  const read = db.raw.prepare('SELECT data FROM archive WHERE match_id = ?');
  const write = db.raw.prepare('UPDATE archive SET data = ? WHERE match_id = ?');
  let archived = 0, archiveSaved = 0;
  const archiving = 'Slimming the archive…';
  tick?.('archive', archiving, 0, { done: 0, total: list.length, unit: 'archived games' });
  for (let i = 0; i < list.length; i += 200) {
    db.tx(() => {
      for (const id of list.slice(i, i + 200)) {
        const row = read.get(id) as { data: Uint8Array } | undefined;
        if (!row) continue;
        const rows = JSON.parse(zlib.gunzipSync(row.data).toString('utf8')) as Record<string, Record<string, unknown>[]>;
        const next = slimArchived(rows);
        if (!next) continue;
        const blob = zlib.gzipSync(JSON.stringify(next));
        // Read back: every row but the cleared columns as it was.
        const back = JSON.parse(zlib.gunzipSync(blob).toString('utf8')) as typeof rows;
        if (JSON.stringify(back) !== JSON.stringify(next)) throw new Error(`Archived match ${id} did not read back the same`);
        write.run(blob, id);
        archived++; archiveSaved += row.data.length - blob.length;
      }
    });
    const done = Math.min(list.length, i + 200);
    tick?.('archive', archiving, done / list.length, { done, total: list.length, unit: 'archived games' });
  }
  tick?.('reclaim', 'Freeing the disk space it saved…', null);
  store.reclaim();
  db.reclaim();
  m.applied = { slimmed, kept, archived, savedBytes: savedBytes + archiveSaved };
  writeManifest(dataDir, m, db);
}

/** (b) on `threads` slimming threads: each takes the next 50 games as it finishes; `onResults` writes them, in turn. */
function onThreads(threads: number, dicts: [number, Buffer][], current: number, chunks: number, chunk: (c: number) => PartGame[], onResults: (r: PartResult[]) => void) {
  return new Promise<void>((resolve, reject) => {
    if (chunks === 0) { resolve(); return; }
    const workers = Array.from({ length: Math.min(threads, chunks) }, () => startWorker('slimPart', { dicts, current }, [], PART_HEAP_MB));
    let next = 0, finished = 0, over = false;
    const end = (err?: Error) => {
      if (over) return;
      over = true;
      for (const w of workers) void w.terminate();
      if (err) reject(err); else resolve();
    };
    const send = (w: (typeof workers)[number]) => {
      if (next < chunks) { const c = next++; w.postMessage({ id: c, games: chunk(c) }); }
    };
    for (const w of workers) {
      w.on('message', (msg: { id: number; results?: PartResult[]; error?: string }) => {
        if (over) return;
        if (msg.error || !msg.results) { end(new Error(`Slimming failed: ${msg.error ?? 'no result'}`)); return; }
        try { onResults(msg.results); } catch (err) { end(err as Error); return; }
        if (++finished === chunks) end(); else send(w);
      });
      w.on('error', (err) => end(err as Error));
      w.on('exit', (code) => { if (!over) end(new Error(`A slimming thread stopped (code ${code})`)); });
      send(w);
    }
  });
}

// ------------------------------------------------------------------------------------------- the results

/** A seeded order of the stored games (FNV-1a of the match id): the same 200 drafts every time. */
const order = (id: string) => { let h = 2166136261; for (let i = 0; i < id.length; i++) h = Math.imul(h ^ id.charCodeAt(i), 16777619); return h >>> 0; };

/**
 * What the slimming must not change (steps 3 and 5): a quick evaluation (not saved: your last one stays the last),
 * and for 200 stored games taken in a fixed order, the lane baseline, the draft-adjusted build, and the live "Next" at
 * 5, 10, 15, 20 and 25 minutes of their timeline, from the model built as the app builds it.
 */
export async function results(db: Db, data: StaticData, o: {
  threads?: number; drafts?: number; quickEval?: boolean; tick?: SlimTick;
  /** A full garbage collection between the parts (0.15.2): each leaves its own behind, and the heap is shared in the app. */
  collect?: () => void;
} = {}) {
  const tick = o.tick;
  const evaluating = 'the quick evaluation';
  if (o.quickEval !== false) tick?.('eval', evaluating, 0);
  const evaluation = o.quickEval === false ? null : await evaluateDataFolder(db, data, {
    quick: true, save: false, threads: o.threads, collect: o.collect,
    progress: tick && ((p) => tick('eval', evaluating, stepFraction(p.step, p.steps, p.part ?? 0), { done: p.step, total: p.steps, unit: 'evaluation steps' })),
  }).then((r) => { const { evaluatedAt: _, ...rest } = r.report; return rest; });
  o.collect?.();
  tick?.('model', 'building the model', null);
  const scope = (() => { try { return loadScope(); } catch { return DEFAULT_SCOPE; } })();
  const excl = buildExclusivityIndex(data);
  const regions = regionPatches(db.gameVersions(), data);
  const regionPatch = (p: string) => regions.get(p);
  const service = new ModelService({
    db, data: () => data, excl: () => excl, scope: () => scope, regionPatch: () => regionPatch,
    ownPlatform: () => null, busy: () => false, startingItemWindowMs: config.startingItemWindowMs,
  });
  const built = service.rebuild();
  if (!built) return { evaluation, drafts: null, error: service.error() };
  o.collect?.();
  const settings = service.settings();
  const sources = scope.buildSources;
  const patches = db.patchBreakdown(sources).map((p) => p.patch);
  const laneGames = db.gamesForPatches(patches, sources);
  const baselines = new Map<string, unknown>();
  const baselineOf = (opponent: number | null, lane: Lane) => {
    const k = `${lane}|${opponent}`;
    if (!baselines.has(k)) baselines.set(k, recommend(laneGames, data, excl, opponent && data.champions.has(opponent) ? opponent : null, config, lane, regionPatch));
    return baselines.get(k) as ReturnType<typeof recommend>;
  };
  const games = built.model.scored.filter((s) => s.g.source !== 'me').map((s) => s.g)
    .sort((a, b) => order(a.matchId) - order(b.matchId) || (a.matchId < b.matchId ? -1 : 1)).slice(0, o.drafts ?? 200);
  // The live "Next" per item slot, as the live re-rank scores it (model/live.ts), on each replayed moment.
  const weighted = weighGames(built.model, data, regionPatch, settings.weights);
  const bySlot = new Map<number, ReturnType<typeof observed>>();
  const obsFor = (slot: number) => {
    let obs = bySlot.get(slot);
    if (!obs) {
      obs = observed(weighted.flatMap(({ i, s, base, patch, age }) => {
        const c = s.g.items[slot];
        if (!c) return [];
        const w = base * patch([c.item]);
        return w > 0 ? [{ option: String(c.item), wpa: s.itemWpa[slot], win: s.g.win, w, lane: s.g.lane, opponent: s.g.opponent, pilot: s.g.pilot, zs: built.model.zItems, zRow: built.model.itemRow[i] + slot, age }] : [];
      }), settings.weights);
      bySlot.set(slot, obs);
    }
    return obs;
  };
  const timelines = db.timelinesOf(games.map((g) => g.matchId));
  const rowOf = db.raw.prepare('SELECT * FROM irelia_games WHERE match_id = ?');
  tick?.('drafts', 'the drafts', 0, { done: 0, total: games.length, unit: 'drafts' });
  const drafts = games.map((g, n) => {
    if (n % 10 === 9) tick?.('drafts', 'the drafts', (n + 1) / games.length, { done: n + 1, total: games.length, unit: 'drafts' });
    const baseline = baselineOf(g.opponent, g.lane);
    const draft = service.draft({ lane: g.lane, opponent: g.opponent, enemies: g.enemies, allies: g.allies, regions: null }, baseline, false).draft;
    const raw = timelines.get(g.matchId) ?? null;
    const row = rowOf.get(g.matchId) as Record<string, unknown> | undefined;
    const tl = row && raw ? gameTimeline({ ...row, platform: g.platform, timeline: raw } as never, db.participantsOf(g.matchId), (id) => data.items.get(id)?.goldTotal ?? 0) : null;
    const next = tl ? momentsAt(tl, [5, 10, 15, 20, 25].map((m) => m * 60_000), built.profiles, data).map((mo) => {
      const slot = mo.owned.filter((id) => isCompletedItem(data.items.get(id))).length;
      if (slot >= 6) return null;
      const z = [...standardise(g.draft, built.model.draftScale), ...standardise(mo.live, built.model.liveScale)];
      const est = estimateDecision(obsFor(slot), { lane: g.lane, opponent: g.opponent, z }, settings.estimate);
      return est.options.filter((x) => isPurchasableItem(data.items.get(Number(x.option)))).slice(0, 3).map((x) => [x.option, x.theta, x.nEff]);
    }) : null;
    return { id: g.matchId, baseline, draft, next };
  });
  return { evaluation, drafts, error: null };
}
export type SlimResults = Awaited<ReturnType<typeof results>>;

/** The first places two results differ, as paths ("drafts.12.draft.picks.3.value: 3153 → 3078"), at most `max`. */
export function differences(a: unknown, b: unknown, max = 10, at = ''): string[] {
  const out: string[] = [];
  const walk = (x: unknown, y: unknown, p: string) => {
    if (out.length >= max) return;
    if (x !== null && y !== null && typeof x === 'object' && typeof y === 'object') {
      const keys = [...new Set([...Object.keys(x as object), ...Object.keys(y as object)])];
      for (const k of keys) walk((x as Record<string, unknown>)[k], (y as Record<string, unknown>)[k], p ? `${p}.${k}` : k);
      return;
    }
    if (JSON.stringify(x) !== JSON.stringify(y)) out.push(`${p}: ${JSON.stringify(x)?.slice(0, 60)} → ${JSON.stringify(y)?.slice(0, 60)}`);
  };
  walk(a, b, at);
  return out;
}

export function saveBaseline(dataDir: string, r: SlimResults) {
  const json = JSON.stringify(r);
  fs.writeFileSync(path.join(slimDir(dataDir), BASELINE), json);
  return createHash('sha256').update(json).digest('hex');
}
export const readBaseline = (dataDir: string) => JSON.parse(fs.readFileSync(path.join(slimDir(dataDir), BASELINE), 'utf8')) as SlimResults;

// ------------------------------------------------------------------------------------------- rolling back, keeping

/** The cache entries back from the backup, each as it was (overwriting its slim copy); checked by count and bytes. */
export async function restoreEntries(dataDir: string, cacheRoot: string, tick?: RollbackTick) {
  const file = path.join(slimDir(dataDir), ENTRIES);
  if (!fs.existsSync(file)) return { entries: 0 };
  const src = new DatabaseSync(file, { readOnly: true });
  const store = new CacheStore(cacheRoot);
  try {
    // A batch at a time (0.14.3): the entries can be many GB, and the start waits on this while it says how far it is.
    const total = (src.prepare('SELECT COUNT(*) AS n FROM entries').get() as { n: number }).n;
    const page = src.prepare('SELECT rowid AS id, kind, scope, key, stored_at, data FROM entries WHERE rowid > ? ORDER BY rowid LIMIT ?');
    const get = store.db.prepare('SELECT data FROM entries WHERE kind = ? AND scope = ? AND key = ?');
    let last = 0, done = 0;
    tick?.(`putting the cache entries back: 0 of ${total.toLocaleString('en-US')}`);
    for (;;) {
      const rows = page.all(last, RESTORE_BATCH) as { id: number; kind: CacheKind; scope: string; key: string; stored_at: number; data: Uint8Array }[];
      if (!rows.length) break;
      store.db.exec('BEGIN IMMEDIATE');
      try { for (const r of rows) store.put(r.kind, r.scope, r.key, r.data, r.stored_at); store.db.exec('COMMIT'); } catch (err) { store.db.exec('ROLLBACK'); throw err; }
      // Checked: every entry reads back as the backup's bytes.
      for (const r of rows) {
        const back = get.get(r.kind, r.scope, r.key) as { data: Uint8Array } | undefined;
        if (!back || Buffer.compare(back.data, r.data) !== 0) throw new Error(`The cache entry ${r.kind} ${r.key} did not come back as it was`);
      }
      last = rows[rows.length - 1].id;
      done += rows.length;
      tick?.(`putting the cache entries back: ${done.toLocaleString('en-US')} of ${total.toLocaleString('en-US')}`);
      await new Promise((r) => setImmediate(r));
    }
    return { entries: done };
  } finally { src.close(); store.close(); }
}

/**
 * The automatic rollback (and an interrupted run's, at the next start): the database file replaced by the backup's
 * copy, checked by its SHA-256, and the cache entries put back. Nothing may have the database open (the caller closes
 * every connection first); fetching was paused since the backup, so nothing written meanwhile is lost.
 */
export async function rollbackFile(dataDir: string, cacheRoot: string, m: SlimManifest, tick?: RollbackTick) {
  const dir = slimDir(dataDir);
  if (!m.db) throw new Error('No database copy to restore from');
  const target = path.join(dataDir, 'irelia.db');
  const tmp = `${target}.restore`;
  const checking = (what: string) => (done: number, total: number) => tick?.(`${what}: ${mb(done).toLocaleString('en-US')} of ${mb(total).toLocaleString('en-US')} MB`);
  tick?.(`copying the database's backup (${mb(m.db.bytes).toLocaleString('en-US')} MB)`);
  await fs.promises.copyFile(path.join(dir, m.db.file), tmp);
  if (await sha256File(tmp, checking('checking the copy (SHA-256)')) !== m.db.sha256) throw new Error('The backup copy of the database does not match its checksum: nothing was restored');
  for (const f of dbFiles(dataDir).slice(1)) fs.rmSync(f, { force: true });
  fs.renameSync(tmp, target);
  const restored = await sha256File(target, checking('checking the restored database (SHA-256)'));
  if (restored !== m.db.sha256) throw new Error('The restored database does not match the backup');
  return { ...(await restoreEntries(dataDir, cacheRoot, tick)), sha256: restored };
}

/**
 * Roll back, chosen later (the run was verified and the app has run since): the archived rows (c) put back from the
 * backup's copy row by row, so the games fetched since stay, checked by a SHA-256 of those rows on both sides; and
 * the cache entries put back.
 */
export async function rollbackRows(dataDir: string, cacheRoot: string, db: Db) {
  const dir = slimDir(dataDir);
  const list = JSON.parse(fs.readFileSync(path.join(dir, ARCHIVE_LIST), 'utf8')) as string[];
  const copy = new DatabaseSync(path.join(dir, DB_COPY), { readOnly: true });
  const digest = (rows: { match_id: string; data: Uint8Array }[]) => {
    const h = createHash('sha256');
    for (const r of rows.sort((a, b) => (a.match_id < b.match_id ? -1 : 1))) h.update(r.match_id).update(r.data);
    return h.digest('hex');
  };
  try {
    const fromCopy = copy.prepare('SELECT match_id, data FROM archive WHERE match_id IN (SELECT value FROM json_each(?))').all(JSON.stringify(list)) as { match_id: string; data: Uint8Array }[];
    const present = new Set((db.raw.prepare('SELECT match_id FROM archive WHERE match_id IN (SELECT value FROM json_each(?))').all(JSON.stringify(list)) as { match_id: string }[]).map((r) => r.match_id));
    // Rows restored to the tables since (no longer archived) came back from the slim copy: what they hold is the same
    // but the cleared columns; those can't be put back here, and are counted.
    const back = fromCopy.filter((r) => present.has(r.match_id));
    const write = db.raw.prepare('UPDATE archive SET data = ? WHERE match_id = ?');
    db.tx(() => { for (const r of back) write.run(r.data, r.match_id); });
    const now = db.raw.prepare('SELECT match_id, data FROM archive WHERE match_id IN (SELECT value FROM json_each(?))').all(JSON.stringify(back.map((r) => r.match_id))) as { match_id: string; data: Uint8Array }[];
    if (digest(now) !== digest(back)) throw new Error('The archived matches did not come back as they were');
    return { ...(await restoreEntries(dataDir, cacheRoot)), archived: back.length, restoredSince: fromCopy.length - back.length };
  } finally { copy.close(); }
}

/** Deletes the backup folder (Keep the slimmer data, or after a rollback); the manifest's last state stays in meta. */
export function removeBackup(dataDir: string) {
  fs.rmSync(slimDir(dataDir), { recursive: true, force: true, maxRetries: 5 });
}

/** The backup's size on disk. */
export function backupBytes(dataDir: string) {
  const dir = slimDir(dataDir);
  let n = 0;
  for (const f of fs.readdirSync(dir, { withFileTypes: true }).filter((e) => e.isFile())) n += fileSize(path.join(dir, f.name));
  return n;
}

/**
 * At start, before anything opens the database: a run cut short by a crash or a closed app. Before step 4 nothing
 * was changed (the backup goes); from step 4 on it is rolled back (the database file and the cache entries). Returns
 * what it did, for the Storage card.
 */
export async function recoverSlim(dataDir: string, cacheRoot: string, tick?: RollbackTick): Promise<SlimManifest | null> {
  const m = readManifest(dataDir);
  if (!m || ['verified', 'kept', 'rolled-back', 'cancelled'].includes(m.step)) return m;
  if (m.step === 'backup' || m.step === 'baseline') {
    // Entries moved out at the backup step go back first.
    await restoreEntries(dataDir, cacheRoot, tick);
    removeBackup(dataDir);
    const done: SlimManifest = { ...m, step: 'cancelled', note: 'Interrupted before slimming: nothing was changed.', updatedAt: Date.now() };
    return done;
  }
  const r = await rollbackFile(dataDir, cacheRoot, m, tick);
  removeBackup(dataDir);
  return { ...m, step: 'rolled-back', note: `Interrupted while slimming: rolled back (database checked by SHA-256, ${r.entries.toLocaleString('en-US')} cache entries put back).`, updatedAt: Date.now() };
}
