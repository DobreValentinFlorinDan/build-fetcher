// The background conversion's controller (0.14.0): runs server/convertWorker.ts while the app is idle, stops it in
// champ select and games and while the data folder moves, and starts it again after. It reports to the Storage card.
import fs from 'node:fs';
import path from 'node:path';
import type { Worker } from 'node:worker_threads';
import type { ConversionStatus } from '../shared/types.ts';
import { CACHE_TTL } from './riot/diskCache.ts';
import { startWorker } from './workers.ts';

/** How far the timelines' conversion got (a rowid, in meta). */
export const TIMELINE_CURSOR = 'timelines_z_cursor';

export interface ConversionDeps {
  dataDir: () => string;
  cacheDir: () => string;
  /** Plain timelines are left in the database (db.ts lastTimelineRow against the cursor). */
  timelinesLeft: () => boolean;
  onStatus: (s: ConversionStatus) => void;
  log?: (line: string) => void;
}

/** Whether the cache folder still holds files of an earlier version. */
export function legacyCacheLeft(cacheDir: string) {
  return Object.keys(CACHE_TTL).some((k) => fs.statSync(path.join(cacheDir, k), { throwIfNoEntry: false })?.isDirectory());
}

export class Conversion {
  private worker: Worker | null = null;
  private busy = false;
  private paused = false;
  status: ConversionStatus | null = null;

  constructor(private deps: ConversionDeps) {}

  /**
   * Whether there is anything to convert. Old files it couldn't read are left as they are (and read by the cache as
   * before): once a run has been through them all, they don't make it run again in this session.
   */
  needed() {
    const unreadableOnly = this.status !== null && !this.status.running && this.status.cache !== null && this.status.cache.done === this.status.cache.total
      && this.status.cache.skipped > 0;
    try { return (legacyCacheLeft(this.deps.cacheDir()) && !unreadableOnly) || this.deps.timelinesLeft(); } catch { return false; }
  }

  /** Whether earlier versions' data is still being converted (Slim stored data waits for it). */
  get working() { return this.worker !== null || (this.needed() && !(this.status?.error)); }

  /** Starts it when there is work, the app is idle and the folder isn't moving. */
  check() {
    if (this.worker || this.busy || this.paused || !this.needed()) return;
    const worker = startWorker('convert', { dataDir: this.deps.dataDir() });
    this.worker = worker;
    worker.on('message', (msg: { type: string; status?: ConversionStatus }) => {
      if (msg.type === 'status' && msg.status) this.show(msg.status);
    });
    worker.on('error', (err) => { this.deps.log?.(`[convert] ${err instanceof Error ? err.message : String(err)}`); });
    worker.on('exit', () => {
      if (this.worker === worker) this.worker = null;
      if (this.status?.error) this.deps.log?.(`[convert] stopped: ${this.status.error}`);
    });
  }

  private shownAt = 0;
  private show(s: ConversionStatus) {
    this.status = { ...s, ...(this.busy ? { paused: true } : {}) };
    // The card twice a second at most, and always the last word.
    if (s.running && !s.paused && Date.now() - this.shownAt < 500) return;
    this.shownAt = Date.now();
    this.deps.onStatus(this.status);
  }

  /** Champ select or a game (true): it stops at once; after (false), it carries on. */
  setBusy(busy: boolean) {
    if (busy === this.busy) return;
    this.busy = busy;
    if (busy) {
      void this.stop();
      if (this.status && !this.status.done && !this.status.error) this.show({ ...this.status, running: false });
    } else this.check();
  }

  private async stop() {
    const w = this.worker;
    this.worker = null;
    if (w) await w.terminate();
  }

  /** The data folder is moving: stopped until resume(). */
  async pause() { this.paused = true; await this.stop(); }
  resume() { this.paused = false; this.check(); }
}
