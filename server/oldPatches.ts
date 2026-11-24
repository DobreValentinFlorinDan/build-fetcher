// Old patches: the games and cached Riot files of patches older than patch memory's six-patch window
// (docs/BUILD-SOURCING.md §9.8). Two switches, both off by default:
//  - "Archive old patches" (Data → Patches): their games go into the database's archive, compressed whole; switched
//    off, the archive is restored.
//  - "Keep the API cache to the window" (Settings → Storage): their cached matches and timelines are deleted.
// Each applies when switched on, and again whenever a patch leaves the window (the hourly check and at startup).
// One operation at a time, in batches, so the app stays responsive.
import type { ActionKind, OldPatchesStatus } from '../shared/types.ts';
import { readAppConfig, updateAppConfig } from './appConfig.ts';
import type { Db } from './db.ts';
import { toPatch, type StaticData } from './ddragon.ts';
import { beforeWindow, modelPatches } from './model/service.ts';
import { MEMORY_PATCHES } from './model/weights.ts';
import { expectedMs, Progress, recordTiming } from './progress.ts';
import { DiskCache } from './riot/diskCache.ts';

/** The patches the cache was last trimmed to (meta key): trimming again is due once the window moves. */
const TRIMMED_TO = 'cache_trimmed_to';

export interface OldPatchesDeps {
  db: () => Db;
  data: () => StaticData;
  cacheDir: () => string;
  /** Another data-folder operation (a move, compaction) is running: wait for the next check. */
  blocked: () => boolean;
  /** No champ select or game is on: the automatic catch-up waits for that (its batches run on the main thread). */
  idle?: () => boolean;
  /** The status changed (an operation started or ended). */
  changed?: (status: OldPatchesStatus) => void;
}

/**
 * Between two batches the write lock stays free this long. A write waiting in another thread (fetching) retries at
 * least every 100 ms (SQLite's busy handler), so it gets in instead of timing out behind batch after batch.
 */
export const BATCH_GAP_MS = 150;

/** Runs `step` (one batch per call, returning how many it did) until it does none, pausing between batches. */
export async function inBatches(step: () => number, gapMs = BATCH_GAP_MS, onBatch?: (done: number) => void) {
  let done = 0;
  for (;;) {
    const n = step();
    if (!n) return done;
    done += n;
    onBatch?.(done);
    await new Promise((r) => setTimeout(r, gapMs));
  }
}

const KIND: Record<NonNullable<OldPatchesStatus['working']>, ActionKind> = { archiving: 'archive', restoring: 'restore', trimming: 'trim' };

export class OldPatches {
  private working: OldPatchesStatus['working'] = null;
  private error: string | null = null;
  private last: string | null = null;
  private queue: Promise<unknown> = Promise.resolve();
  /** The operation running: its progress (how far, when it can be counted) and when it last told the dashboard. */
  private progress: { tracker: Progress; fraction: number | null; told: number } | null = null;

  constructor(private deps: OldPatchesDeps) {}

  status(): OldPatchesStatus {
    const archived = this.deps.db().archiveSummary();
    const p = this.working && this.progress ? { progress: this.progress.tracker.info(this.progress.fraction) } : {};
    return {
      archive: this.archiveOn(archived.length > 0), trimCache: readAppConfig().trimCacheToWindow === true, working: this.working, ...p,
      archived, last: this.last, error: this.error,
    };
  }

  /** How far the running operation is (0-1), told to the dashboard at most every half second. */
  private tick(fraction: number | null) {
    if (!this.progress) return;
    this.progress.fraction = fraction;
    const now = Date.now();
    if (now - this.progress.told < 500) return;
    this.progress.told = now;
    this.deps.changed?.(this.status());
  }

  /** Never switched: on when there is an archive already (0.8.0 archived with a button, once). */
  private archiveOn(hasArchive: boolean) { return readAppConfig().archiveOldPatches ?? hasArchive; }

  busy() { return this.working !== null; }

  /** The archive switch: on archives what is older than the window now; off restores the whole archive. */
  setArchive(on: boolean) {
    updateAppConfig({ archiveOldPatches: on });
    return on ? this.run('archiving', () => this.archiveNow()) : this.run('restoring', () => this.restoreAll());
  }

  /** How long each operation would take now (estimates: this computer's last run, scaled to what there is now). */
  estimates() {
    const db = this.deps.db();
    return {
      archive: expectedMs('archive', db.staleCount(this.window())),
      restore: expectedMs('restore', db.archiveSummary().reduce((s, p) => s + p.games, 0)),
      trim: expectedMs('trim', 1),
    };
  }

  /** The cache switch: on deletes the cached files of patches older than the window now. */
  setTrimCache(on: boolean) {
    updateAppConfig({ trimCacheToWindow: on });
    return on ? this.run('trimming', () => this.trimNow()) : Promise.resolve(this.status());
  }

  /**
   * At startup and hourly: what left the window since, for the switches that are on, unless champ select or a game is
   * on (then the next check does it). Resolves when done.
   */
  async maintain() {
    if (this.deps.blocked() || this.deps.idle?.() === false) return;
    const db = this.deps.db();
    const window = this.window();
    const runs: Promise<unknown>[] = [];
    if (this.archiveOn(db.archiveSummary().length > 0) && db.hasArchivable(window)) runs.push(this.run('archiving', () => this.archiveNow(), true));
    if (readAppConfig().trimCacheToWindow && db.getMeta(TRIMMED_TO) !== window.join(',')) runs.push(this.run('trimming', () => this.trimNow(), true));
    await Promise.all(runs);
  }

  /** The patches kept: every one within the window of a region's current patch, plus Data Dragon's pair. */
  private window() { return modelPatches(this.deps.db(), this.deps.data(), MEMORY_PATCHES).sort(); }

  /** One operation at a time; `quiet` (the automatic ones) skips while another data-folder operation runs. */
  private run(kind: NonNullable<OldPatchesStatus['working']>, work: () => Promise<string>, quiet = false): Promise<OldPatchesStatus> {
    const next = this.queue.then(async () => {
      if (this.deps.blocked()) {
        if (!quiet) this.error = 'The data folder is being moved or the database compacted. Try again in a moment.';
        return this.status();
      }
      // What its time grows with: the matches to archive or restore; the cache's trim walks every cached match (its
      // last time is the estimate).
      const db = this.deps.db();
      const size = kind === 'archiving' ? db.staleCount(this.window()) : kind === 'restoring' ? db.archiveSummary().reduce((s, p) => s + p.games, 0) : 1;
      const startedAt = Date.now();
      this.progress = { tracker: new Progress(expectedMs(KIND[kind], size), startedAt), fraction: kind === 'trimming' ? null : 0, told: 0 };
      this.size = size;
      this.working = kind;
      this.deps.changed?.(this.status());
      try {
        this.last = await work();
        this.error = null;
        recordTiming(KIND[kind], { size, ms: Date.now() - startedAt, at: Date.now() });
      } catch (err) {
        this.error = err instanceof Error ? err.message : String(err);
        console.warn(`[old patches] ${kind} failed`, err);
      } finally {
        this.working = null;
        this.progress = null;
      }
      const s = this.status();
      this.deps.changed?.(s);
      return s;
    });
    this.queue = next.catch(() => {});
    return next;
  }

  /** What the running operation goes through (its progress counts against it). */
  private size = 0;

  private async archiveNow() {
    const keep = this.window();
    const n = await inBatches(() => this.deps.db().archiveBatch(keep), BATCH_GAP_MS, (done) => this.tick(done / Math.max(1, this.size)));
    // The archive holds them compressed: the pages they left go back to the file system (0.14.0).
    if (n) this.deps.db().reclaim();
    return n ? `Archived ${n.toLocaleString('en-US')} matches of patches older than the window.` : 'Nothing older than the window to archive.';
  }

  private async restoreAll() {
    const n = await inBatches(() => this.deps.db().restoreBatch(null), BATCH_GAP_MS, (done) => this.tick(done / Math.max(1, this.size)));
    return n ? `Restored ${n.toLocaleString('en-US')} archived matches.` : 'The archive was empty.';
  }

  private async trimNow() {
    const db = this.deps.db();
    const window = this.window();
    const old = beforeWindow(db, this.deps.data());
    const patchOf = db.matchPatches();
    const r = await DiskCache.pruneMatches(this.deps.cacheDir(), async (id, version) => {
      const patch = patchOf.get(id) ?? await version().then((v) => (v ? toPatch(v) : undefined));
      return patch !== undefined && old(patch) === true;
    });
    db.setMeta(TRIMMED_TO, window.join(','));
    return r.files ? `Deleted ${r.files.toLocaleString('en-US')} cached files of old patches (${(r.bytes / 1e6).toFixed(1)} MB).` : 'No cached files of old patches.';
  }
}
