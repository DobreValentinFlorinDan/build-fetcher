import fs from 'node:fs/promises';
import { mkdirSync, statSync } from 'node:fs';
import path from 'node:path';
import { DatabaseSync } from 'node:sqlite';
import { promisify } from 'node:util';
import zlib from 'node:zlib';
import { CODEC_SCHEMA, Dictionaries, pack, unpack } from '../codec.ts';
import { measure, remove, walkFiles } from '../fsUtil.ts';

const gunzip = promisify(zlib.gunzip);

/** How long each kind of Riot response stays valid on disk. Matches and timelines never change. */
export const CACHE_TTL = {
  match: Infinity,
  timeline: Infinity,
  league: 6 * 3600_000,
  mastery: 3 * 24 * 3600_000,
  matchIds: 10 * 60_000,
  account: 7 * 24 * 3600_000,   // account-v1 region of a PUUID (the user's own account)
} as const;
export type CacheKind = keyof typeof CACHE_TTL;

const KINDS = Object.keys(CACHE_TTL) as CacheKind[];

/** The cache's database in its folder (<dataDir>/cache/riot-cache.db, 0.14.0). */
export const CACHE_DB = 'riot-cache.db';
export const cacheDbFile = (root: string) => path.join(root, CACHE_DB);
/** A key as it was a file name (before 0.14.0): converted files keep their entry, and keys are the same as they were. */
const keyOf = (key: string) => key.replace(/[^\w.-]/g, '_');
/** Where an entry was before 0.14.0: <root>/<kind>/<scope>/<key>.json.gz (read until the files are converted). */
export const legacyFile = (root: string, kind: CacheKind, scope: string, key: string) => path.join(root, kind, scope, `${keyOf(key)}.json.gz`);

const SCHEMA = `
CREATE TABLE IF NOT EXISTS entries (
  kind       TEXT NOT NULL,
  scope      TEXT NOT NULL,               -- platform or region
  key        TEXT NOT NULL,
  stored_at  INTEGER NOT NULL,            -- epoch ms: the TTL counts from it
  data       BLOB NOT NULL,               -- the JSON response, compressed (server/codec.ts)
  PRIMARY KEY (kind, scope, key)
);
CREATE TABLE IF NOT EXISTS meta (
  key   TEXT PRIMARY KEY,
  value TEXT NOT NULL
);
${CODEC_SCHEMA}
`;
const SCHEMA_VERSION = 1;
/**
 * 0.14.1 (schema version 2): each entry's size by kind, scope and key, so the Storage card's sizes and the list of
 * cached matches and timelines are read from this index instead of every entry (6.6 s on a 3 GB cache before, the
 * main thread waiting; about 30 ms now). About 35 bytes an entry.
 */
const SIZE_INDEX = 'CREATE INDEX IF NOT EXISTS entries_size ON entries(kind, scope, key, length(data))';
const SIZE_INDEX_VERSION = 2;
/** The schema version in meta only goes up (an older thread opening the file doesn't take it back). */
const SET_VERSION = `INSERT INTO meta(key, value) VALUES('schema_version', ?)
  ON CONFLICT(key) DO UPDATE SET value = CAST(MAX(CAST(value AS INTEGER), CAST(excluded.value AS INTEGER)) AS TEXT)`;

/**
 * The Riot API cache's database (0.14.0): one row per response, compressed with a dictionary made from a match and a
 * timeline of this cache. It replaced one gzip file per response: about 40 % less disk, and no file-system overhead
 * for hundreds of thousands of small files. Pages of 16 KB fit the compressed timelines; with incremental auto-vacuum,
 * clearing gives the space back.
 */
export class CacheStore {
  readonly db: DatabaseSync;
  readonly dicts: Dictionaries;
  private q;

  constructor(readonly root: string, o: { busyTimeoutMs?: number } = {}) {
    mkdirSync(root, { recursive: true });
    this.db = new DatabaseSync(cacheDbFile(root));
    this.db.exec(`PRAGMA busy_timeout = ${o.busyTimeoutMs ?? 30_000};`);
    // Before the first table: they only take effect on a new file.
    this.db.exec('PRAGMA page_size = 16384; PRAGMA auto_vacuum = INCREMENTAL;');
    this.db.exec('PRAGMA journal_mode = WAL; PRAGMA synchronous = NORMAL; PRAGMA journal_size_limit = 67108864;');
    this.db.exec('BEGIN IMMEDIATE');
    try {
      this.db.exec(SCHEMA);
      this.db.prepare(SET_VERSION).run(String(SCHEMA_VERSION));
      this.db.exec('COMMIT');
    } catch (err) { this.db.exec('ROLLBACK'); throw err; }
    this.dicts = new Dictionaries(this.db);
    this.q = {
      get: this.db.prepare('SELECT stored_at, data FROM entries WHERE kind = ? AND scope = ? AND key = ?'),
      put: this.db.prepare(`INSERT INTO entries(kind, scope, key, stored_at, data) VALUES(?, ?, ?, ?, ?)
        ON CONFLICT(kind, scope, key) DO UPDATE SET stored_at = excluded.stored_at, data = excluded.data`),
      newest: this.db.prepare('SELECT data FROM entries WHERE kind = ? ORDER BY stored_at DESC LIMIT 1'),
      usage: this.db.prepare('SELECT kind, COUNT(*) AS files, SUM(LENGTH(data)) AS bytes FROM entries GROUP BY kind'),
      expired: this.db.prepare('DELETE FROM entries WHERE kind = ? AND stored_at < ?'),
      matchKeys: this.db.prepare("SELECT kind, scope, key, LENGTH(data) AS bytes FROM entries WHERE kind IN ('match', 'timeline')"),
      del: this.db.prepare('DELETE FROM entries WHERE kind = ? AND scope = ? AND key = ?'),
      firstRows: this.db.prepare('SELECT kind, scope, key FROM entries LIMIT ?'),
      count: this.db.prepare('SELECT COUNT(*) AS n FROM entries'),
    };
  }

  /** The stored response's JSON bytes and when it was stored, or undefined. */
  read(kind: CacheKind, scope: string, key: string): { storedAt: number; raw: Buffer } | undefined {
    const row = this.q.get.get(kind, scope, keyOf(key)) as { stored_at: number; data: Uint8Array } | undefined;
    return row ? { storedAt: row.stored_at, raw: unpack(row.data, this.dicts.get) } : undefined;
  }

  /** Stores the JSON bytes `raw` (compressed), stored at `storedAt`. Returns the blob written. */
  write(kind: CacheKind, scope: string, key: string, raw: Uint8Array, storedAt = Date.now()): Buffer {
    const blob = this.encode(kind, raw);
    this.put(kind, scope, key, blob, storedAt);
    return blob;
  }

  /** A blob's JSON bytes. */
  decode(blob: Uint8Array) { return unpack(blob, this.dicts.get); }
  /** Whether `blob` reads back as `raw`. */
  encodeCheck(blob: Uint8Array, raw: Uint8Array) { try { return unpack(blob, this.dicts.get).equals(raw); } catch { return false; } }

  /** The blob `write` stores for `raw` (compressed outside a transaction, then `put`). */
  encode(kind: CacheKind, raw: Uint8Array) { return pack(raw, this.dictionary(kind, raw)); }
  put(kind: CacheKind, scope: string, key: string, blob: Uint8Array, storedAt: number) { this.q.put.run(kind, scope, keyOf(key), storedAt, blob); }

  /**
   * The dictionary to write with: the current one, else (no match and timeline seen yet) one made now from the newest
   * stored match and this timeline, or this match and the newest timeline. Until then, none.
   */
  private dictionary(kind: CacheKind, raw: Uint8Array) {
    const d = this.dicts.current();
    if (d || (kind !== 'match' && kind !== 'timeline')) return d;
    const other = this.q.newest.get(kind === 'match' ? 'timeline' : 'match') as { data: Uint8Array } | undefined;
    if (!other) return null;
    const otherRaw = unpack(other.data, this.dicts.get);
    return this.dicts.create(kind === 'match' ? [raw, otherRaw] : [otherRaw, raw], 'a match and a timeline of this cache');
  }

  /** A dictionary from these examples (the conversion of the old files makes one before the first entry). */
  ensureDictionary(match: Uint8Array, timeline: Uint8Array) {
    return this.dicts.current() ?? this.dicts.create([match, timeline], 'a match and a timeline of this cache');
  }

  delete(kind: CacheKind, scope: string, key: string) { this.q.del.run(kind, scope, keyOf(key)); }

  usage() {
    return this.q.usage.all() as { kind: CacheKind; files: number; bytes: number }[];
  }

  count() { return (this.q.count.get() as { n: number }).n; }

  /**
   * Builds the size index (SIZE_INDEX) if the file doesn't have it yet: true when it did. It reads every entry once
   * (seconds on a large cache) with the cache's write lock, so a worker thread does it (the fetching thread at start).
   */
  ensureSizeIndex() {
    if (this.db.prepare("SELECT 1 AS x FROM sqlite_master WHERE type = 'index' AND name = 'entries_size'").get()) return false;
    this.db.exec('BEGIN IMMEDIATE');
    try {
      this.db.exec(SIZE_INDEX);
      this.db.prepare(SET_VERSION).run(String(SIZE_INDEX_VERSION));
      this.db.exec('COMMIT');
    } catch (err) { this.db.exec('ROLLBACK'); throw err; }
    return true;
  }

  /** Gives free pages back to the file system (auto_vacuum = INCREMENTAL): after deletions. */
  reclaim() { this.db.exec('PRAGMA incremental_vacuum; PRAGMA wal_checkpoint(TRUNCATE);'); }

  /** Deletes everything, a batch at a time (`onProgress` with the fraction done). */
  clear(onProgress?: (fraction: number) => void) {
    const total = this.count();
    let done = 0;
    for (;;) {
      const rows = this.q.firstRows.all(2000) as { kind: CacheKind; scope: string; key: string }[];
      if (!rows.length) break;
      this.db.exec('BEGIN IMMEDIATE');
      try { for (const r of rows) this.q.del.run(r.kind, r.scope, r.key); this.db.exec('COMMIT'); } catch (err) { this.db.exec('ROLLBACK'); throw err; }
      done += rows.length;
      onProgress?.(total ? done / total : 1);
    }
    this.reclaim();
    return { files: done };
  }

  /** Deletes the entries past their TTL. */
  clearExpired(now = Date.now()) {
    let files = 0;
    for (const kind of KINDS) if (CACHE_TTL[kind] !== Infinity) files += Number(this.q.expired.run(kind, now - CACHE_TTL[kind]).changes);
    if (files) this.reclaim();
    return { files };
  }

  /** Every cached match and timeline: kind, scope, key and compressed size. */
  matchEntries() { return this.q.matchKeys.all() as { kind: 'match' | 'timeline'; scope: string; key: string; bytes: number }[]; }

  close() { try { this.db.close(); } catch { /* already closed */ } }
}

/** The main thread's stores, one per folder (Storage card, cache cleanup); closed before the data folder moves. */
const shared = new Map<string, CacheStore>();
export function cacheStore(root: string): CacheStore {
  let s = shared.get(root);
  if (!s) { s = new CacheStore(root, { busyTimeoutMs: 5000 }); shared.set(root, s); }
  return s;
}
export function closeCacheStores() {
  for (const s of shared.values()) s.close();
  shared.clear();
}

/** Whether the folder still has cache files from before 0.14.0 (any kind's folder). */
function hasLegacyFiles(root: string) {
  return KINDS.some((k) => statSync(path.join(root, k), { throwIfNoEntry: false })?.isDirectory());
}

/**
 * The Riot API cache: compressed JSON responses in <dataDir>/cache/riot-cache.db (CacheStore), and until they are
 * converted (server/convert.ts), the gzip files of earlier versions under <dataDir>/cache/<kind>/<scope>/<key>.json.gz.
 * The root comes from config.riotCacheDir, which follows the user's data folder (%APPDATA%/irelia-companion/config.json
 * → dataDir), so a moved folder keeps its cache. Every Riot call goes through it first, so re-scans, engine changes
 * and restarts cost no API budget.
 */
export class DiskCache {
  hits = 0;
  misses = 0;
  private store: CacheStore | null = null;
  /** Old files may still be there: looked at when the database has no entry (checked again every few minutes). */
  private legacy: { any: boolean; at: number } | null = null;

  constructor(private root: string) {}

  private db() { return (this.store ??= new CacheStore(this.root)); }
  /** CacheStore.ensureSizeIndex (0.14.1). */
  ensureSizeIndex() { return this.db().ensureSizeIndex(); }
  private get anyLegacy() {
    if (!this.legacy || (this.legacy.any && Date.now() - this.legacy.at > 5 * 60_000)) this.legacy = { any: hasLegacyFiles(this.root), at: Date.now() };
    return this.legacy.any;
  }

  /** A cached answer, unless it expired or (`since`, 0.21.2) was stored before that moment. */
  async get<T>(kind: CacheKind, scope: string, key: string, o: { since?: number } = {}): Promise<T | undefined> {
    const since = o.since ?? -Infinity;
    try {
      const hit = this.db().read(kind, scope, key);
      if (hit) {
        if (CACHE_TTL[kind] !== Infinity && Date.now() - hit.storedAt > CACHE_TTL[kind]) { this.misses++; return undefined; }
        if (hit.storedAt < since) { this.misses++; return undefined; }
        this.hits++;
        return JSON.parse(hit.raw.toString('utf8')) as T;
      }
      if (this.anyLegacy) {
        const file = legacyFile(this.root, kind, scope, key);
        if (CACHE_TTL[kind] !== Infinity || o.since !== undefined) {
          const { mtimeMs } = await fs.stat(file);
          if (Date.now() - mtimeMs > CACHE_TTL[kind] || mtimeMs < since) { this.misses++; return undefined; }
        }
        const value = JSON.parse((await gunzip(await fs.readFile(file))).toString('utf8')) as T;
        this.hits++;
        return value;
      }
    } catch { /* a miss */ }
    this.misses++;
    return undefined;
  }

  async set(kind: CacheKind, scope: string, key: string, value: unknown) {
    this.db().write(kind, scope, key, Buffer.from(JSON.stringify(value)));
    // An expired old file of the same entry would be read again after the new one expires: it goes.
    if (this.anyLegacy) await fs.rm(legacyFile(this.root, kind, scope, key), { force: true });
  }

  close() { this.store?.close(); this.store = null; }

  /** Disk usage per cache kind (match, timeline, league, ...): the database's entries and the old files left. */
  static async usage(root: string) {
    const store = cacheStore(root);
    const inDb = new Map(store.usage().map((u) => [u.kind, u]));
    const parts: { name: string; files: number; bytes: number }[] = await Promise.all(KINDS.map(async (kind) => {
      const files = await measure(path.join(root, kind));
      const db = inDb.get(kind);
      return { name: kind, files: files.files + (db?.files ?? 0), bytes: files.bytes + (db?.bytes ?? 0) };
    }));
    // The database's own pages (indexes, free space, the write-ahead log) beyond the entries.
    const blobs = [...inDb.values()].reduce((s, u) => s + u.bytes, 0);
    const file = (await Promise.all(['', '-wal'].map((x) => measure(`${cacheDbFile(root)}${x}`)))).reduce((s, m) => s + m.bytes, 0);
    if (file > blobs) parts.push({ name: 'index and free space', files: 0, bytes: file - blobs });
    return parts.filter((p) => p.files > 0 || p.bytes > 0);
  }

  /**
   * Deletes cached responses. `expiredOnly` keeps entries still inside their TTL, so matches and
   * timelines (immutable, kept forever) survive and only stale ladders / mastery / id lists go.
   */
  static async prune(root: string, { expiredOnly = false, onProgress }: { expiredOnly?: boolean; onProgress?: (fraction: number) => void } = {}) {
    const store = cacheStore(root);
    const before = (await measure(cacheDbFile(root))).bytes + (await measure(`${cacheDbFile(root)}-wal`)).bytes;
    let files = 0, bytes = 0;
    if (!expiredOnly) {
      const sizes = await Promise.all(KINDS.map((kind) => measure(path.join(root, kind))));
      const legacy = sizes.reduce((s, m) => s + m.files, 0);
      const inDb = store.count();
      const total = legacy + inDb;
      files += store.clear((f) => onProgress?.(total ? (f * inDb) / total : 1)).files;
      for (const [i, kind] of KINDS.entries()) {
        await remove(path.join(root, kind));
        files += sizes[i].files; bytes += sizes[i].bytes;
        onProgress?.(total ? (inDb + sizes.slice(0, i + 1).reduce((s, m) => s + m.files, 0)) / total : 1);
      }
    } else {
      files += store.clearExpired().files;
      for (const kind of KINDS) {
        const dir = path.join(root, kind);
        if (CACHE_TTL[kind] === Infinity) continue;
        const cutoff = Date.now() - CACHE_TTL[kind];
        for await (const f of walkFiles(dir)) {
          if (f.mtimeMs >= cutoff) continue;
          await fs.rm(f.path, { force: true });
          files++; bytes += f.size;
        }
      }
    }
    const after = (await measure(cacheDbFile(root))).bytes + (await measure(`${cacheDbFile(root)}-wal`)).bytes;
    return { files, bytes: bytes + Math.max(0, before - after) };
  }

  /**
   * Deletes cached matches and timelines of games `isOld` says the app won't read again: by match id, and for a match
   * the database doesn't know, by the cached match's own game version (`version`; a timeline has none). Undecided
   * ones stay.
   */
  static async pruneMatches(root: string, isOld: (matchId: string, version: () => Promise<string | undefined>) => boolean | Promise<boolean>) {
    let files = 0, bytes = 0;
    const verdicts = new Map<string, boolean>();
    const store = cacheStore(root);
    const versionOf = async (raw: () => Promise<Buffer | undefined>) => {
      try { return (JSON.parse((await raw())?.toString('utf8') ?? '{}') as { info?: { gameVersion?: string } }).info?.gameVersion; } catch { return undefined; }
    };
    for (const e of store.matchEntries()) {
      const id = e.key;
      let old = verdicts.get(id);
      if (old === undefined) {
        old = await isOld(id, () => (e.kind === 'match' ? versionOf(async () => store.read('match', e.scope, e.key)?.raw) : Promise.resolve(undefined)));
        verdicts.set(id, old);
      }
      if (!old) continue;
      store.delete(e.kind, e.scope, e.key);
      files++; bytes += e.bytes;
    }
    for (const kind of ['match', 'timeline'] as const) {
      for await (const f of walkFiles(path.join(root, kind))) {
        if (!f.path.endsWith('.json.gz')) continue;
        const id = path.basename(f.path, '.json.gz');
        let old = verdicts.get(id);
        if (old === undefined) {
          old = await isOld(id, () => (kind === 'match' ? versionOf(async () => gunzip(await fs.readFile(f.path))) : Promise.resolve(undefined)));
          verdicts.set(id, old);
        }
        if (!old) continue;
        await fs.rm(f.path, { force: true });
        files++; bytes += f.size;
      }
    }
    if (files) store.reclaim();
    return { files, bytes };
  }
}
