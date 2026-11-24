// Backing up the high-elo pool (0.18.0, trimmed in 0.19.0): a ZIP export of the pool, and importing one made on another
// computer. Everything here runs in its own thread (server/transferWorker.ts): copying a multi-gigabyte database
// would otherwise stop the dashboard.
//
// An export is a ZIP with:
//   manifest.json     what it is, when and where it was made, how many games
//   irelia-pool.db    a copy of the pool database (SQLite), other players' IDs blanked (stripOtherPlayers)
// It is written as <file>.partial and renamed when complete, so a synced folder (Google Drive) never uploads half of one.
//
// Imports only ever add games that are not there yet; nothing stored is replaced or deleted.
import fsp from 'node:fs/promises';
import os from 'node:os';
import path from 'node:path';
import { openDb, SCHEMA_VERSION, type Db, type MatchRecord, type PilotSource } from './db.ts';
import { EVAL_ENTRIES, evaluationToExport, stageIncomingEvaluation, type ExportedEvaluation } from './model/evalTravel.ts';
import { extractEntry, readEntry, readZip, ZipWriter, type ZipEntry } from './zip.ts';

export const EXPORT_FORMAT = 'irelia-companion-export';
export const POOL_EXPORT = 'irelia-pool';
/** How many matches go into one transaction when copying between databases. */
const BATCH = 200;

export interface ExportManifest {
  format: typeof EXPORT_FORMAT;
  version: 1;
  kind: 'pool';
  name: typeof POOL_EXPORT;
  exportedAt: number;
  /** The computer it was made on (os.hostname()): an auto-export folder doesn't offer a computer its own exports. */
  machine: string;
  app: string;
  schema: number;
  games: number;
  patches: { patch: string; games: number }[];
  /** Whether other players' IDs were blanked (always). */
  stripped: true;
  database: string;
  /**
   * The computer's last evaluation, when the backup carries one (0.21.0, model/evalTravel.ts): the computer importing
   * it takes it as its own when it is valid there, instead of evaluating every game again.
   */
  evaluation?: ExportedEvaluation;
}

export type Progress = (p: { label: string; fraction: number | null }) => void;

const stamp = (t: number) => new Date(t).toISOString().slice(0, 16).replace('T', '_').replace(':', '');
/** "irelia-pool_2026-10-30_1830.zip": the name, then when, so a folder keeps several and sorts them. */
export const exportFileName = (name: string, at: number) => `${name}_${stamp(at)}.zip`;
/** The export files of `name` in a folder: name, then the timestamp. */
export const exportPattern = (name: string) => new RegExp(`^${name.replace(/[.*+?^${}()|[\]\\]/g, '\\$&')}_\\d{4}-\\d{2}-\\d{2}_\\d{4}\\.zip$`);

const ALL_SOURCES: PilotSource[] = ['otp', 'main', 'harvested', 'me'];

/** A scratch folder next to the target (same disk, so space checks mean something), removed afterwards. */
async function scratch(near: string) {
  const dir = await fsp.mkdtemp(path.join(near, '.irelia-transfer-'));
  return { dir, done: () => fsp.rm(dir, { recursive: true, force: true, maxRetries: 3 }) };
}

/** Refuses early when the disk can't hold `bytes` more (plus 256 MB to spare). */
async function needSpace(dir: string, bytes: number, what: string) {
  try {
    const s = await fsp.statfs(dir);
    const free = s.bavail * s.bsize;
    if (free < bytes + 256 * 1024 * 1024) {
      throw new Error(`Not enough free space for ${what}: about ${Math.ceil((bytes + 256 * 1024 * 1024) / 1024 ** 3)} GB is needed on that drive.`);
    }
  } catch (err) {
    if (err instanceof Error && err.message.startsWith('Not enough')) throw err;   // statfs itself failing: try anyway
  }
}

function summary(db: Db) {
  const patches = db.patchBreakdown(ALL_SOURCES);
  return { games: patches.reduce((s, p) => s + p.games, 0), patches };
}
async function writeExport(file: string, manifest: ExportManifest, dbFile: string, progress: Progress, extra: [string, Buffer | { file: string }][] = []) {
  const size = (await fsp.stat(dbFile)).size;
  const zip = new ZipWriter(file);
  try {
    await zip.addBuffer('manifest.json', `${JSON.stringify(manifest, null, 2)}\n`);
    await zip.addFile(manifest.database, dbFile, (n) => progress({ label: 'Compressing', fraction: 0.3 + 0.65 * (n / Math.max(1, size)) }));
    for (const [name, data] of extra) {
      if (Buffer.isBuffer(data)) await zip.addBuffer(name, data);
      else await zip.addFile(name, data.file);
    }
    return await zip.finish();
  } catch (err) {
    await zip.abort();
    throw err;
  }
}

/**
 * The high-elo pool as a ZIP. Other players' IDs are blanked in the matches; the pool's pilots (the one-tricks and
 * mains it follows) and the Irelia players of its games keep theirs, or the other computer couldn't go on fetching them.
 * Players the ladder seeding found that aren't pilots are left out (the other computer seeds its own ladder).
 */
export async function exportPool(dataDir: string, folder: string, o: { app: string; progress: Progress }) {
  const src = path.join(dataDir, 'irelia.db');
  await fsp.mkdir(folder, { recursive: true });
  const size = (await fsp.stat(src)).size;
  await needSpace(folder, size * 1.6, 'the pool export');
  const tmp = await scratch(folder);
  try {
    o.progress({ label: 'Copying the pool database (this takes a while on a large one)', fraction: 0.02 });
    const copy = path.join(tmp.dir, 'irelia-pool.db');
    const db = openDb(src, { migrate: false, busyTimeoutMs: 30_000 });
    // The last evaluation goes with it (0.21.0), with the games it had seen read from this database (by row).
    let evaluation: ReturnType<typeof evaluationToExport> = null;
    try {
      db.raw.prepare('VACUUM INTO ?').run(copy);
      try { evaluation = evaluationToExport(db); } catch { evaluation = null; }
    } finally { db.close(); }
    o.progress({ label: 'Removing other players\' IDs', fraction: 0.2 });
    const c = openDb(copy);
    let s: ReturnType<typeof summary>;
    try {
      const keep = (c.raw.prepare(`SELECT puuid FROM players WHERE pilot_class IN ('otp', 'main')
        UNION SELECT DISTINCT puuid FROM irelia_games`).all() as { puuid: string }[]).map((r) => r.puuid);
      c.stripOtherPlayers(keep);
      c.raw.exec(`DELETE FROM players WHERE COALESCE(pilot_class, 'none') NOT IN ('otp', 'main');
        DELETE FROM meta WHERE key LIKE 'seeded_at:%';`);
      c.raw.exec('PRAGMA journal_mode = DELETE');
      s = summary(c);
    } finally { c.close(); }
    const at = Date.now();
    const manifest: ExportManifest = {
      format: EXPORT_FORMAT, version: 1, kind: 'pool', name: POOL_EXPORT, exportedAt: at, machine: os.hostname(), app: o.app, schema: SCHEMA_VERSION,
      games: s.games, patches: s.patches, stripped: true, database: 'irelia-pool.db',
      ...(evaluation ? { evaluation: evaluation.info } : {}),
    };
    const out = await writeExport(path.join(folder, exportFileName(POOL_EXPORT, at)), manifest, copy, o.progress, evaluation?.entries ?? []);
    return { ...out, games: s.games, exportedAt: at, evaluation: !!evaluation };
  } finally {
    await tmp.done();
  }
}
/** What a ZIP holds, from its manifest; throws a readable message for anything that isn't one of ours. */
export async function inspectExport(file: string): Promise<{ manifest: ExportManifest; entries: ZipEntry[] }> {
  const entries = await readZip(file);
  const m = entries.find((e) => e.name === 'manifest.json');
  if (!m) throw new Error('This ZIP is not an Irelia Companion export (no manifest.json).');
  const manifest = JSON.parse((await readEntry(file, m)).toString('utf8')) as ExportManifest & { kind: string };
  if (manifest.format !== EXPORT_FORMAT) throw new Error('This ZIP is not an Irelia Companion export.');
  if (manifest.kind !== 'pool') throw new Error('This export holds a player data set, which this version no longer imports. Only pool exports can be imported.');
  if (manifest.schema > SCHEMA_VERSION) throw new Error('This export comes from a newer Irelia Companion: update the app first.');
  if (!entries.some((e) => e.name === manifest.database)) throw new Error('This export is incomplete (its database is missing).');
  return { manifest, entries };
}

/** Copies every Irelia game of `from` into `to` (those not there yet), BATCH at a time. Returns how many were added. */
export function copyGames(from: Db, to: Db, o: { source?: PilotSource; progress?: (done: number, total: number) => void } = {}) {
  const ids = from.ireliaMatchIds();
  let added = 0;
  for (let i = 0; i < ids.length; i += BATCH) {
    const records: MatchRecord[] = ids.slice(i, i + BATCH).filter((id) => !to.hasMatch(id)).map((id) => from.matchRecord(id));
    if (records.length) added += to.importRecords(records, { source: o.source });
    o.progress?.(Math.min(ids.length, i + BATCH), ids.length);
  }
  return added;
}

/**
 * Imports a pool export into the pool: the games it has that this pool doesn't, and the pilots (one-tricks and mains)
 * this computer didn't follow yet. Nothing stored is replaced. The dashboard asks first (`confirmed`): it changes the
 * pool's builds.
 */
export async function importExport(dataDir: string, file: string, o: { confirmed?: boolean; progress: Progress }) {
  const { manifest, entries } = await inspectExport(file);
  if (!o.confirmed) throw new Error('Importing a pool export changes the pool: confirm it first.');
  const entry = entries.find((e) => e.name === manifest.database)!;
  await fsp.mkdir(dataDir, { recursive: true });
  await needSpace(dataDir, entry.size * 1.2, 'the import');
  const tmp = await scratch(dataDir);
  try {
    const copy = path.join(tmp.dir, 'import.db');
    await extractEntry(file, entry, copy, (n) => o.progress({ label: 'Unpacking', fraction: 0.4 * (n / Math.max(1, entry.size)) }));
    const from = openDb(copy);   // brings an older export's schema up to date
    try {
      const report = (done: number, total: number) => o.progress({ label: `Adding games (${done.toLocaleString('en-US')} of ${total.toLocaleString('en-US')})`, fraction: 0.4 + 0.6 * (done / Math.max(1, total)) });
      const pool = openDb(path.join(dataDir, 'irelia.db'), { migrate: false, busyTimeoutMs: 30_000 });
      try {
        const added = copyGames(from, pool, { progress: report });
        // The other computer's pilots, so this one can follow them too (rows it has already are left as they are).
        const pilots = from.raw.prepare("SELECT * FROM players WHERE pilot_class IN ('otp', 'main')").all() as { puuid: string }[];
        pool.tx(() => {
          for (const p of pilots) {
            if (pool.player(p.puuid)) continue;
            const row = from.player(p.puuid)!;
            pool.upsertPlayer(row.puuid, row.platform, row.tier, row.league_points);
            pool.setPilot(row.puuid, { pilotClass: row.pilot_class as 'otp' | 'main', ireliaPoints: row.irelia_points ?? 0, ireliaLastPlay: row.irelia_last_play ?? null, ireliaRank: row.irelia_rank ?? null });
          }
        });
        // Its evaluation (0.21.0), left for the main thread to take or turn down (model/evalTravel.ts): not here, where
        // an evaluation may be running and the settings it is checked against aren't known.
        let evaluation = false;
        if (manifest.evaluation && Object.values(EVAL_ENTRIES).every((n) => entries.some((e) => e.name === n))) {
          await stageIncomingEvaluation((n, dest) => extractEntry(file, entries.find((e) => e.name === n)!, dest), { machine: manifest.machine, exportedAt: manifest.exportedAt });
          evaluation = true;
        }
        return { name: POOL_EXPORT, added, games: manifest.games, exportedAt: manifest.exportedAt, evaluation };
      } finally { pool.close(); }
    } finally { from.close(); }
  } finally {
    await tmp.done();
  }
}

/** The computer an export was made on (its manifest's), or null when it can't be read as one of ours. */
export async function exportMachine(file: string): Promise<string | null> {
  try {
    const entries = await readZip(file);
    const m = entries.find((e) => e.name === 'manifest.json');
    if (!m) return null;
    const manifest = JSON.parse((await readEntry(file, m)).toString('utf8')) as Partial<ExportManifest>;
    return manifest.format === EXPORT_FORMAT && typeof manifest.machine === 'string' ? manifest.machine : null;
  } catch { return null; }
}

/**
 * Keeps the `keep` newest exports of `name` in `folder` (only files named as this app names them). With `machine`
 * (0.20.1), only that computer's exports count and go: two computers backing up into one folder each keep their own,
 * and one whose manifest can't be read is left alone. Before, a computer's weekly backups deleted the other's.
 */
export async function pruneExports(folder: string, name: string, keep: number, machine?: string) {
  const pattern = exportPattern(name);
  let files = (await fsp.readdir(folder).catch(() => [] as string[])).filter((f) => pattern.test(f)).sort().reverse();
  if (machine !== undefined) {
    const mine: string[] = [];
    for (const f of files) if (await exportMachine(path.join(folder, f)) === machine) mine.push(f);
    files = mine;
  }
  for (const f of files.slice(keep)) await fsp.rm(path.join(folder, f), { force: true });
  return files.slice(keep).length;
}

/** The name an export's file belongs to (the pool), or null for anything else. */
export function exportOwner(file: string): string | null {
  const m = /^(.+)_\d{4}-\d{2}-\d{2}_\d{4}\.zip$/.exec(path.basename(file));
  if (!m) return null;
  return m[1] === POOL_EXPORT ? m[1] : null;
}

