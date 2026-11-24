import { constants as fsc } from 'node:fs';
import fs from 'node:fs/promises';
import path from 'node:path';
import { DatabaseSync } from 'node:sqlite';
import type { StorageCategory, StorageInspection, StorageProgress, StorageStats } from '../shared/types.ts';
import { dataPaths } from './config.ts';
import { measure, remove, walkFiles, type FileEntry } from './fsUtil.ts';
import { CACHE_DB, DiskCache } from './riot/diskCache.ts';

// Data folder layout:
//   irelia.db (+ -wal / -shm)   SQLite: players, matches, Irelia games, saved scans
//   ddragon/<version>/          Data Dragon JSON per patch
//   cache/riot-cache.db         Riot API responses, compressed (riot/diskCache.ts, 0.14.0)
//   cache/<kind>/<platform>/    gzipped Riot API responses from before 0.14.0, until they are converted
// Only these entries belong to the app. A move copies exactly these and cleanup deletes exactly
// these, so pointing the app at a folder that also holds other files never wipes them.
const DB_FILE = 'irelia.db';
const DB_SIDECARS = ['irelia.db-wal', 'irelia.db-shm'];
// backup-0.14.0/: Slim stored data's backup while a verified run waits for Roll back or Keep (server/slim.ts).
const DIRS = ['ddragon', 'cache', 'backup-0.14.0'];
/** The cache's database and its sidecars: copied as a database (VACUUM INTO), not file by file. */
const CACHE_DB_FILES = new Set([CACHE_DB, `${CACHE_DB}-wal`, `${CACHE_DB}-shm`]);
const OWNED = [DB_FILE, ...DB_SIDECARS, ...DIRS];
export const HEADROOM = 64 * 1024 * 1024;
// Deepest file: cache/mastery/<platform>/<78-char puuid>.json.gz is ~110 characters below the folder.
// Windows (and SQLite on it) cap full paths at 260, so the folder itself must stay short.
const MAX_DIR_CHARS = 140;

const norm = (p: string) => (process.platform === 'win32' ? path.resolve(p).toLowerCase() : path.resolve(p));
export const samePath = (a: string, b: string) => norm(a) === norm(b);
/** True when `child` is strictly inside `parent`. */
export function isInside(child: string, parent: string) {
  const rel = path.relative(norm(parent), norm(child));
  return !!rel && !rel.startsWith('..') && !path.isAbsolute(rel);
}

const exists = (p: string) => fs.stat(p).then(() => true, () => false);

async function nearestExisting(p: string) {
  let dir = path.resolve(p);
  while (!(await exists(dir))) {
    const up = path.dirname(dir);
    if (up === dir) return null;
    dir = up;
  }
  return dir;
}

export async function freeSpace(p: string) {
  const base = await nearestExisting(p);
  if (!base) return null;
  try {
    const s = await fs.statfs(base);
    return { freeBytes: s.bavail * s.bsize, totalBytes: s.blocks * s.bsize };
  } catch { return null; }
}

/** Writes and removes a probe file in the folder (or the nearest existing parent if it does not exist yet). */
async function writable(p: string) {
  const base = await nearestExisting(p);
  if (!base) return false;
  const probe = path.join(base, `.irelia-write-test-${process.pid}`);
  try { await fs.writeFile(probe, ''); await fs.rm(probe, { force: true }); return true; } catch { return false; }
}

async function sizeOf(dir: string, names: string[]) {
  return Promise.all(names.map(async (name) => ({ name, ...(await measure(path.join(dir, name))) })));
}

// ------------------------------------------------------------------------------- stats

export async function storageStats(dataDir: string, o: {
  defaultDataDir: string; configFile: string; lockedBy: string | null; ddragonVersion: string; busy: boolean;
}): Promise<StorageStats> {
  const p = dataPaths(dataDir);
  const versions = await fs.readdir(p.ddragonCacheDir).catch(() => [] as string[]);
  const others = (await fs.readdir(dataDir).catch(() => [] as string[])).filter((n) => !OWNED.includes(n));

  const category = (key: StorageCategory['key'], label: string, parts: StorageCategory['parts']): StorageCategory => {
    const kept = parts.filter((x) => x.files > 0 || x.bytes > 0).sort((a, b) => b.bytes - a.bytes);
    return { key, label, parts: kept, bytes: kept.reduce((s, x) => s + x.bytes, 0), files: kept.reduce((s, x) => s + x.files, 0) };
  };
  const categories = [
    category('database', 'SQLite database', await sizeOf(dataDir, [DB_FILE, ...DB_SIDECARS])),
    category('riotCache', 'Riot API cache', await DiskCache.usage(p.riotCacheDir)),
    category('ddragon', 'Data Dragon', await sizeOf(p.ddragonCacheDir, versions)),
    category('other', 'Other files', await sizeOf(dataDir, others)),
    // Slim stored data's backup (0.14.0), while it waits for Roll back or Keep.
    category('backup', 'Slim stored data backup', await sizeOf(dataDir, ['backup-0.14.0'])),
  ].filter((c) => c.key !== 'backup' || c.bytes > 0);

  return {
    dataDir, defaultDataDir: o.defaultDataDir, configFile: o.configFile, lockedBy: o.lockedBy,
    totalBytes: categories.reduce((s, c) => s + c.bytes, 0),
    categories,
    disk: await freeSpace(dataDir),
    ddragonVersion: o.ddragonVersion,
    busy: o.busy,
  };
}

/** Deletes Data Dragon versions other than the one in use. */
export async function pruneDdragon(dir: string, keepVersion: string, alsoKeep: string[] = []) {
  let files = 0, bytes = 0;
  for (const v of await fs.readdir(dir).catch(() => [] as string[])) {
    if (v === keepVersion || alsoKeep.includes(v)) continue;
    const m = await measure(path.join(dir, v));
    await remove(path.join(dir, v));
    files += m.files; bytes += m.bytes;
  }
  return { files, bytes };
}

// ------------------------------------------------------------------------------- moving the folder

/** Pre-flight check for a new data folder. `problems` block the move; `hasData` only blocks move/copy. */
export async function inspectTarget(raw: string, currentDir: string): Promise<StorageInspection> {
  const input = raw.trim();
  const target = path.resolve(input || '.');
  const problems: string[] = [];
  if (!input || !path.isAbsolute(input)) problems.push('Choose an absolute folder path, e.g. D:\\Games\\IreliaData.');
  else if (samePath(target, currentDir)) problems.push('That is already the data folder.');
  else if (isInside(target, currentDir)) problems.push('The new folder cannot be inside the current data folder.');
  else if (isInside(currentDir, target)) problems.push('The new folder cannot contain the current data folder.');
  if (target.length > MAX_DIR_CHARS) problems.push(`Choose a shorter path (at most ${MAX_DIR_CHARS} characters): Windows limits full file paths to 260.`);

  const st = await fs.stat(target).catch(() => null);
  if (st && !st.isDirectory()) problems.push('That path is a file, not a folder.');
  const hasData = !!st?.isDirectory() && (await Promise.all(OWNED.map((n) => exists(path.join(target, n))))).some(Boolean);
  const canWrite = await writable(target);
  if (!canWrite) problems.push('Windows denied write access to that folder.');

  const requiredBytes = (await sizeOf(currentDir, OWNED)).reduce((s, x) => s + x.bytes, 0);
  const disk = await freeSpace(target);
  if (disk && disk.freeBytes < requiredBytes + HEADROOM) {
    problems.push(`Not enough free space: ${fmtBytes(requiredBytes + HEADROOM)} needed, ${fmtBytes(disk.freeBytes)} free.`);
  }
  return { path: target, exists: !!st, writable: canWrite, hasData, freeBytes: disk?.freeBytes ?? null, requiredBytes, problems };
}

type Progress = (p: StorageProgress) => void;

/**
 * Copies the app's entries from `from` to `to`. The database is copied with `vacuumInto`
 * (SQLite VACUUM INTO: a consistent, compacted snapshot with the WAL folded in, safe while the
 * connection stays open). Cache files keep their mtime so cache TTLs survive the move.
 * Never overwrites: every copy uses COPYFILE_EXCL.
 */
export async function copyDataDir(from: string, to: string, vacuumInto: (file: string) => void, onProgress: Progress) {
  await fs.mkdir(to, { recursive: true });
  const files: FileEntry[] = [];
  const cacheDb = path.join(from, 'cache', CACHE_DB);
  for (const dir of DIRS) {
    for await (const f of walkFiles(path.join(from, dir))) if (!(path.dirname(f.path) === path.dirname(cacheDb) && CACHE_DB_FILES.has(path.basename(f.path)))) files.push(f);
  }
  const dbBytes = (await sizeOf(from, [DB_FILE, ...DB_SIDECARS])).reduce((s, x) => s + x.bytes, 0);
  const cacheBytes = (await sizeOf(path.join(from, 'cache'), [...CACHE_DB_FILES])).reduce((s, x) => s + x.bytes, 0);
  const totalBytes = dbBytes + cacheBytes + files.reduce((s, f) => s + f.size, 0);
  let doneBytes = 0;

  if (await exists(path.join(from, DB_FILE))) {
    onProgress({ phase: 'database', doneBytes, totalBytes, text: 'Copying database (VACUUM INTO)…' });
    vacuumInto(path.join(to, DB_FILE));
  }
  doneBytes += dbBytes;
  // The API cache's database the same way: a consistent copy whatever its write-ahead log holds.
  let cacheEntries: number | null = null;
  if (await exists(cacheDb)) {
    onProgress({ phase: 'database', doneBytes, totalBytes, text: 'Copying the API cache (VACUUM INTO)…' });
    await fs.mkdir(path.join(to, 'cache'), { recursive: true });
    const src = new DatabaseSync(cacheDb);
    try {
      src.prepare('VACUUM INTO ?').run(path.join(to, 'cache', CACHE_DB));
      cacheEntries = (src.prepare('SELECT COUNT(*) AS n FROM entries').get() as { n: number }).n;
    } finally { src.close(); }
  }
  doneBytes += cacheBytes;

  let tick = 0;
  for (const [i, f] of files.entries()) {
    const dest = path.join(to, path.relative(from, f.path));
    await fs.mkdir(path.dirname(dest), { recursive: true });
    await fs.copyFile(f.path, dest, fsc.COPYFILE_EXCL);
    await fs.utimes(dest, new Date(), f.mtimeMs / 1000);   // seconds: new Date() would truncate to whole ms
    doneBytes += f.size;
    if (Date.now() - tick > 150 || i === files.length - 1) {
      tick = Date.now();
      onProgress({ phase: 'copying', doneBytes, totalBytes, text: `Copying cache files ${i + 1} of ${files.length}…` });
    }
  }
  return { files, totalBytes, cacheEntries };
}

/**
 * Byte-for-byte size check of every copied file plus SQLite quick_check and row counts (`cacheEntries`: the API
 * cache's database too).
 */
export async function verifyCopy(from: string, to: string, files: FileEntry[], rows: Record<string, number> | null, cacheEntries: number | null = null) {
  for (const f of files) {
    const dest = path.join(to, path.relative(from, f.path));
    const st = await fs.stat(dest).catch(() => null);
    if (!st || st.size !== f.size) throw new Error(`Copy check failed for ${path.relative(from, f.path)}`);
  }
  if (cacheEntries !== null) {
    const cache = new DatabaseSync(path.join(to, 'cache', CACHE_DB), { readOnly: true });
    try {
      const check = (cache.prepare('PRAGMA quick_check').get() as { quick_check: string }).quick_check;
      if (check !== 'ok') throw new Error(`Copied API cache failed its integrity check: ${check}`);
      const got = (cache.prepare('SELECT COUNT(*) AS n FROM entries').get() as { n: number }).n;
      if (got !== cacheEntries) throw new Error(`Copied API cache has ${got} entries, expected ${cacheEntries}`);
    } finally { cache.close(); }
  }
  if (!rows) return;
  const db = new DatabaseSync(path.join(to, DB_FILE), { readOnly: true });
  try {
    const check = (db.prepare('PRAGMA quick_check').get() as { quick_check: string }).quick_check;
    if (check !== 'ok') throw new Error(`Copied database failed its integrity check: ${check}`);
    for (const [table, n] of Object.entries(rows)) {
      const got = (db.prepare(`SELECT COUNT(*) AS n FROM ${table}`).get() as { n: number }).n;
      if (got !== n) throw new Error(`Copied database has ${got} ${table} rows, expected ${n}`);
    }
  } finally { db.close(); }
}

/** Deletes the app's own entries from `dir`, then the folder itself if nothing else is left. Returns what remains. */
export async function removeOwnedData(dir: string) {
  for (const name of OWNED) await remove(path.join(dir, name));
  const left = await fs.readdir(dir).catch(() => [] as string[]);
  if (!left.length) await fs.rmdir(dir).catch(() => {});
  return left;
}

export function fmtBytes(n: number) {
  const u = ['B', 'KB', 'MB', 'GB', 'TB'];
  let i = 0;
  while (n >= 1024 && i < u.length - 1) { n /= 1024; i++; }
  return `${n.toFixed(i && n < 10 ? 1 : 0)} ${u[i]}`;
}
