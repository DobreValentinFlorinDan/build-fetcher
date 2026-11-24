import fs from 'node:fs/promises';
import express from 'express';
import type { ActionKind, ConversionStatus, MigrationMode, ServerEvent, StorageAction, StorageProgress } from '../../shared/types.ts';
import { TIMELINE_CURSOR } from '../convert.ts';
import { appConfigFile, updateAppConfig } from '../appConfig.ts';
import { config, dataDirLockedBy, dataPaths, defaultDataDir, setDataDir } from '../config.ts';
import { openDb, type SwappableDb } from '../db.ts';
import { measure } from '../fsUtil.ts';
import { expectedMs, Progress, recordTiming } from '../progress.ts';
import { closeCacheStores, DiskCache } from '../riot/diskCache.ts';
import {
  copyDataDir, fmtBytes, inspectTarget, pruneDdragon, removeOwnedData, samePath, storageStats, verifyCopy,
} from '../storage.ts';

export interface StorageDeps {
  store: SwappableDb;               // the live database; swapped to the new folder after a move
  /** Stops every thread with the database or the Riot cache open (scraper, model, jobs) before a move or Compact
   * (`what`: "the data folder moves", said on the Data tab)... */
  pause: (what: string) => Promise<void>;
  /** ...and starts them again on the (new) data folder afterwards. */
  resume: () => void;
  ddragonVersion: () => string;
  /** Older Data Dragon versions still read (patch memory's item and Irelia files), kept by "Remove old Data Dragon patches". */
  ddragonHistory?: () => string[];
  broadcast: (ev: ServerEvent) => void;
  /** Another data-folder operation (old patches: archiving, restoring, trimming the cache) running: moves and compaction wait. */
  blocked?: () => string | null;
  /** The background conversion's status (0.14.0, server/convert.ts). */
  conversion?: () => ConversionStatus | null;
}

const MODES: MigrationMode[] = ['move', 'copy', 'adopt'];
const CLEAR_TARGETS = ['riot-cache', 'riot-expired', 'ddragon-old', 'compact-db'] as const;
type ClearTarget = (typeof CLEAR_TARGETS)[number];

/**
 * /api/storage: disk usage of the data folder, cache cleanup and moving the folder.
 *
 *   GET  /stats                      sizes per category + free disk space
 *   GET  /inspect?dir=D:\x           pre-flight check of a new folder
 *   POST /clear   {target}           riot-cache | riot-expired | ddragon-old | compact-db
 *   POST /migrate {targetDir, mode}  move | copy | adopt (use the data already in that folder)
 *
 * While a move runs, `busy()` is true and index.ts answers other /api routes with 503.
 */
export function storageRoutes(deps: StorageDeps) {
  const router = express.Router();
  let busy = false;
  /** The API cache's files when the folder was last measured: what clearing it goes through (no second walk). */
  let cacheFiles = 0;
  // A move's progress, with its time left (bytes copied against all of them; the last move's pace before its own).
  let moving: { tracker: Progress; bytes: number } | null = null;
  const progress = (p: StorageProgress) => deps.broadcast({
    type: 'storage', progress: moving ? { ...p, progress: moving.tracker.info(p.totalBytes ? p.doneBytes / p.totalBytes : null) } : p,
  });
  const stats = async () => {
    const s = await storageStats(config.dataDir, {
      defaultDataDir, configFile: appConfigFile, lockedBy: dataDirLockedBy, ddragonVersion: deps.ddragonVersion(), busy,
    });
    const bytesOf = (key: string) => s.categories.find((c) => c.key === key);
    cacheFiles = bytesOf('riotCache')?.files ?? cacheFiles;
    // How long each action would take now: this computer's last run, scaled to the folder as it is.
    const estimates = {
      compact: expectedMs('compact', bytesOf('database')?.bytes ?? 0),
      'clear-cache': expectedMs('clear-cache', bytesOf('riotCache')?.files ?? 0),
      'clear-expired': expectedMs('clear-expired', 1),
      'ddragon-old': expectedMs('ddragon-old', 1),
      move: expectedMs('move', s.totalBytes),
    };
    const conversion = deps.conversion?.() ?? null;
    // Once the conversion is through, a database from before 0.14.0 needs one full VACUUM to give space back by itself.
    const db = deps.store.handle;
    const suggestCompact = !busy && !db.autoVacuum() && (!conversion || conversion.done) ? { freeBytes: db.freeBytes() } : null;
    return { ...s, estimates, tables: busy ? undefined : db.tableStats(), conversion, suggestCompact };
  };
  /**
   * A Storage action, shown on the Storage card with its time left (from its last run here, scaled to `size`; `tick`
   * tells how far it is when that can be counted), and its time kept for the next estimate once it's done.
   */
  const asAction = async <T>(action: StorageAction['action'] & ActionKind, text: string, size: number, work: (tick: (f: number) => void) => Promise<T>) => {
    const tracker = new Progress(expectedMs(action, size));
    let fraction: number | null = null;
    const show = () => deps.broadcast({ type: 'storage-action', action: { action, text, progress: tracker.info(fraction) } });
    show();
    // Out to the dashboard before work that holds the thread (VACUUM) begins.
    await new Promise((r) => setTimeout(r, 30));
    try {
      const result = await work((f) => { fraction = f; show(); });
      recordTiming(action, { size, ms: Date.now() - tracker.startedAt, at: Date.now() });
      return result;
    } finally {
      deps.broadcast({ type: 'storage-action', action: null });
    }
  };
  const conflict = () => (busy ? 'The data folder is being moved or the database compacted.' : deps.blocked?.() ?? null);

  router.get('/stats', async (_req, res) => { res.json(await stats()); });

  router.get('/inspect', async (req, res) => {
    res.json(await inspectTarget(String(req.query.dir ?? ''), config.dataDir));
  });

  router.post('/clear', async (req, res) => {
    const target = String(req.body?.target) as ClearTarget;
    if (!CLEAR_TARGETS.includes(target)) { res.status(400).json({ error: `target must be one of ${CLEAR_TARGETS.join(', ')}` }); return; }
    const blocked = conflict();
    if (blocked) { res.status(409).json({ error: blocked }); return; }

    let freed = { files: 0, bytes: 0 };
    if (target === 'riot-cache') {
      freed = await asAction('clear-cache', 'Clearing the API cache…', Math.max(1, cacheFiles), (tick) => DiskCache.prune(config.riotCacheDir, { onProgress: tick }));
    } else if (target === 'riot-expired') {
      freed = await asAction('clear-expired', 'Clearing expired API answers…', 1, () => DiskCache.prune(config.riotCacheDir, { expiredOnly: true }));
    } else if (target === 'ddragon-old') {
      freed = await asAction('ddragon-old', 'Removing old Data Dragon patches…', 1,
        () => pruneDdragon(config.ddragonCacheDir, deps.ddragonVersion(), deps.ddragonHistory?.()));
    } else {
      const files = [config.dbPath, `${config.dbPath}-wal`];
      const before = (await Promise.all(files.map(measure))).reduce((s, m) => s + m.bytes, 0);
      // VACUUM needs the database to itself: the other threads close it meanwhile. In WAL mode VACUUM writes the
      // rebuilt file through the WAL: checkpoint afterwards to fold it back in.
      // Busy meanwhile, as during a move: other /api routes answer 503 instead of reaching paused threads.
      busy = true;
      await asAction('compact', 'Compacting the database…', before, async () => {
        await deps.pause('the database is compacted');
        try {
          // Participants by ids, when they are most of the database and it makes them smaller (0.14.0, db.ts).
          const ids = deps.store.handle.normalizeParticipants();
          // Incremental auto-vacuum from now on (0.14.0): this VACUUM is the one a database from before needs.
          deps.store.handle.raw.exec('PRAGMA auto_vacuum = INCREMENTAL; VACUUM; PRAGMA wal_checkpoint(TRUNCATE);');
          // VACUUM may renumber the timelines' rowids: the conversion looks through them again (quickly: done ones are passed over).
          deps.store.handle.setMeta(TIMELINE_CURSOR, '0');
          // Its statements named the old participants table: this thread's connection opens again.
          if (ids.done) deps.store.swap(openDb(config.dbPath)).close();
        } finally { busy = false; deps.resume(); }
      });
      const after = (await Promise.all(files.map(measure))).reduce((s, m) => s + m.bytes, 0);
      freed = { files: 0, bytes: Math.max(0, before - after) };
    }
    res.json({ freedBytes: freed.bytes, files: freed.files, stats: await stats() });
  });

  router.post('/migrate', async (req, res) => {
    const mode = String(req.body?.mode) as MigrationMode;
    if (!MODES.includes(mode)) { res.status(400).json({ error: `mode must be one of ${MODES.join(', ')}` }); return; }
    if (dataDirLockedBy) { res.status(409).json({ error: `The data folder is pinned by the ${dataDirLockedBy}.` }); return; }
    const blocked = conflict();
    if (blocked) { res.status(409).json({ error: blocked }); return; }

    const from = config.dataDir;
    const check = await inspectTarget(String(req.body?.targetDir ?? ''), from);
    if (check.problems.length) { res.status(400).json({ error: check.problems.join(' ') }); return; }
    if (mode !== 'adopt' && check.hasData) {
      res.status(409).json({ error: 'That folder already contains Irelia Companion data. Use it as-is ("Use existing data") or pick an empty folder.' });
      return;
    }
    const to = check.path;

    busy = true;
    moving = { tracker: new Progress(mode === 'adopt' ? null : expectedMs('move', check.requiredBytes)), bytes: check.requiredBytes };
    progress({ phase: 'preparing', doneBytes: 0, totalBytes: check.requiredBytes, text: 'Pausing fetching and the model…' });
    await deps.pause('the data folder moves');
    // The API cache's database is copied, then deleted from the old folder: no connection of this thread may hold it.
    closeCacheStores();
    let copyStarted = false, switched = false;
    try {
      let copiedBytes = 0;
      if (mode === 'adopt') {
        await fs.mkdir(to, { recursive: true });
      } else {
        const rows = deps.store.handle.rowCounts();
        copyStarted = true;
        const copy = await copyDataDir(from, to, (file) => deps.store.handle.vacuumInto(file), progress);
        copiedBytes = copy.totalBytes;
        progress({ phase: 'verifying', doneBytes: copiedBytes, totalBytes: copiedBytes, text: 'Verifying the copy…' });
        await verifyCopy(from, to, copy.files, rows, copy.cacheEntries);
      }

      progress({ phase: 'switching', doneBytes: copiedBytes, totalBytes: copiedBytes, text: 'Switching to the new folder…' });
      const next = openDb(dataPaths(to).dbPath);
      updateAppConfig({ dataDir: samePath(to, defaultDataDir) ? undefined : to });
      setDataDir(to);
      deps.store.swap(next).close();
      switched = true;

      let leftovers: string[] = [];
      if (mode === 'move') {
        progress({ phase: 'cleaning', doneBytes: copiedBytes, totalBytes: copiedBytes, text: 'Removing the old copy…' });
        leftovers = await removeOwnedData(from).catch(() => ['(could not remove the old files; delete them by hand)']);
      }
      const text = mode === 'adopt' ? `Now using the data in ${to}` : `${mode === 'move' ? 'Moved' : 'Copied'} ${fmtBytes(copiedBytes)} to ${to}`;
      if (mode !== 'adopt' && moving) recordTiming('move', { size: copiedBytes, ms: Date.now() - moving.tracker.startedAt, at: Date.now() });
      progress({ phase: 'done', doneBytes: copiedBytes, totalBytes: copiedBytes, text });
      res.json({ ok: true, mode, from, dataDir: to, copiedBytes, leftovers, stats: await stats() });
    } catch (err) {
      const msg = err instanceof Error ? err.message : String(err);
      // Before the switch nothing changed for the app; drop the partial copy (the target held none of our files).
      if (copyStarted && !switched) await removeOwnedData(to).catch(() => {});
      progress({ phase: 'error', doneBytes: 0, totalBytes: 0, text: msg });
      res.status(500).json({ error: switched ? `Switched to ${to}, but: ${msg}` : `Nothing was changed: ${msg}` });
    } finally {
      busy = false;
      moving = null;
      deps.resume();
    }
  });

  return { router, busy: () => busy };
}
