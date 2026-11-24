import fs from 'node:fs/promises';
import path from 'node:path';

export interface FileEntry { path: string; size: number; mtimeMs: number }

/** Files stat'ed at once: each stat is a round trip to the file system (slow on Windows), so they overlap. */
const STAT_BATCH = 64;

/**
 * Every regular file under `dir` (depth-first, in directory order). A missing directory yields nothing. The Riot
 * cache holds hundreds of thousands of files: their stats run a batch at a time instead of one after another.
 */
export async function* walkFiles(dir: string): AsyncGenerator<FileEntry> {
  let entries;
  try { entries = await fs.readdir(dir, { withFileTypes: true }); } catch { return; }
  let pending: string[] = [];
  async function* flush() {
    const stats = await Promise.all(pending.map((p) => fs.stat(p).catch(() => null)));
    const batch = pending;
    pending = [];
    for (const [i, st] of stats.entries()) if (st) yield { path: batch[i], size: st.size, mtimeMs: st.mtimeMs };
  }
  for (const e of entries) {
    const p = path.join(dir, e.name);
    if (e.isDirectory()) {
      yield* flush();
      yield* walkFiles(p);
    } else if (e.isFile()) {
      pending.push(p);
      if (pending.length >= STAT_BATCH) yield* flush();
    }
  }
  yield* flush();
}

/** Total size and file count of a file or directory tree (0 when it does not exist): stats a batch at a time. */
export async function measure(target: string): Promise<{ bytes: number; files: number }> {
  const st = await fs.stat(target).catch(() => null);
  if (!st) return { bytes: 0, files: 0 };
  if (st.isFile()) return { bytes: st.size, files: 1 };
  let bytes = 0, files = 0;
  const visit = async (dir: string): Promise<void> => {
    let entries;
    try { entries = await fs.readdir(dir, { withFileTypes: true }); } catch { return; }
    const subdirs: string[] = [], paths: string[] = [];
    for (const e of entries) {
      if (e.isDirectory()) subdirs.push(path.join(dir, e.name));
      else if (e.isFile()) paths.push(path.join(dir, e.name));
    }
    for (let i = 0; i < paths.length; i += STAT_BATCH) {
      for (const s of await Promise.all(paths.slice(i, i + STAT_BATCH).map((p) => fs.stat(p).catch(() => null)))) {
        if (s) { bytes += s.size; files++; }
      }
    }
    for (const d of subdirs) await visit(d);
  };
  await visit(target);
  return { bytes, files };
}

/** Recursive delete that tolerates Windows' transient EBUSY/EPERM on just-closed files. */
export function remove(target: string) {
  return fs.rm(target, { recursive: true, force: true, maxRetries: 5, retryDelay: 200 });
}
