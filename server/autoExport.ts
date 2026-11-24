// Auto backup (0.18.0, pool only since 0.19.0): a ZIP export of the high-elo pool into a folder the user picks, for
// example a Google Drive folder synced to this computer, so another computer pointed at the same folder can import it.
//   - once a week (and "Back up now" in the Data tab); the 3 newest exports are kept.
// The folder is also read for an export newer than what this computer has, made on another computer: it is offered on
// the Data tab, and only imported when the user says so.
import os from 'node:os';
import path from 'node:path';
import fsp from 'node:fs/promises';
import type { ImportOffer } from '../shared/types.ts';
import { readAppConfig, updateAppConfig, type AutoExportSettings } from './appConfig.ts';
import { exportOwner, inspectExport, POOL_EXPORT, pruneExports } from './transfer.ts';
import type { TransferJob } from './transfers.ts';

export const KEEP_POOL_EXPORTS = 3;
const WEEK = 7 * 24 * 3600_000;

export interface AutoExportEnv {
  run: (job: TransferJob) => Promise<unknown>;
  /** Something changed that the dashboard should show (an offer, the last backup time). */
  changed: () => void;
  now?: () => number;
  machine?: string;
}

export class AutoExport {
  private timer: NodeJS.Timeout | null = null;
  private offerCache = new Map<string, { mtimeMs: number; offer: ImportOffer | null }>();
  offers: ImportOffer[] = [];
  /** The last backup's failure, shown on the Data tab until the next one works. */
  lastError: string | null = null;

  constructor(private env: AutoExportEnv) {}

  private now() { return this.env.now?.() ?? Date.now(); }
  settings(): AutoExportSettings | null {
    const s = readAppConfig().autoExport;
    return s?.folder ? s : null;
  }

  /** Sets the backup folder (null turns auto backup off) and checks it right away. */
  configure(folder: string | null) {
    const cur = readAppConfig().autoExport;
    updateAppConfig({ autoExport: folder ? { folder: path.resolve(folder), last: cur?.last, seen: cur?.seen ?? {} } : undefined });
    this.offerCache.clear();
    this.lastError = null;
    void this.check();
  }

  /** Every hour: a backup when one is due, and the folder read again for offers. */
  start() {
    if (this.timer) return;
    this.timer = setInterval(() => void this.check(), 3600_000);
    this.timer.unref();
    setTimeout(() => void this.check(), 30_000).unref();
  }
  stop() { if (this.timer) clearInterval(this.timer); this.timer = null; }

  /** Backs up the pool now (the button, or a week since the last). */
  async backUp() {
    const s = this.settings();
    if (!s) throw new Error('Choose a backup folder first.');
    try {
      await this.env.run({ kind: 'export-pool', folder: s.folder });
      this.lastError = null;
      updateAppConfig({ autoExport: { ...s, last: this.now() } });
      // Only this computer's: another one backing up into the same folder keeps its own (0.20.1).
      await pruneExports(s.folder, POOL_EXPORT, KEEP_POOL_EXPORTS, this.env.machine ?? os.hostname());
    } catch (err) {
      this.lastError = err instanceof Error ? err.message : String(err);
      throw err;
    } finally {
      this.env.changed();
    }
  }

  /** A backup when one is due, then the offers. */
  async check() {
    const s = this.settings();
    if (!s) { this.offers = []; this.env.changed(); return; }
    if (this.now() - (s.last ?? 0) >= WEEK) {
      try { await this.backUp(); } catch (err) { console.warn('[auto-backup]', err instanceof Error ? err.message : err); }
    }
    await this.scan();
  }

  /**
   * The newest pool export in the folder when another computer made it and this one hasn't imported or dismissed it
   * yet. A file still syncing (not a readable ZIP yet) is looked at again the next time.
   */
  async scan() {
    const s = this.settings();
    if (!s) { this.offers = []; this.env.changed(); return; }
    const files = (await fsp.readdir(s.folder).catch(() => [] as string[])).filter((f) => exportOwner(f) !== null).sort().reverse();
    const offers: ImportOffer[] = [];
    if (files[0]) {
      const file = path.join(s.folder, files[0]);
      const stat = await fsp.stat(file).catch(() => null);
      if (stat) {
        let hit = this.offerCache.get(file);
        if (!hit || hit.mtimeMs !== stat.mtimeMs) {
          let offer: ImportOffer | null = null;
          try {
            const { manifest } = await inspectExport(file);
            if (manifest.machine !== (this.env.machine ?? os.hostname())) {
              offer = { file, kind: 'pool', name: manifest.name, exportedAt: manifest.exportedAt, games: manifest.games, bytes: stat.size };
            }
          } catch { /* still syncing, an older format, or not ours */ }
          hit = { mtimeMs: stat.mtimeMs, offer };
          this.offerCache.set(file, hit);
        }
        const o = hit.offer;
        const seen = readAppConfig().autoExport?.seen?.[path.basename(file)];
        if (o && !(seen !== undefined && seen >= o.exportedAt)) offers.push(o);
      }
    }
    this.offers = offers;
    this.env.changed();
  }

  /** Imported or dismissed: not offered again (a newer export of it is). */
  seen(file: string, exportedAt: number) {
    const s = readAppConfig().autoExport;
    if (!s) return;
    updateAppConfig({ autoExport: { ...s, seen: { ...(s.seen ?? {}), [path.basename(file)]: exportedAt } } });
    this.offers = this.offers.filter((o) => o.file !== file);
    this.env.changed();
  }

  /** What the dashboard shows. */
  view() {
    const s = this.settings();
    return s ? { folder: s.folder, last: s.last ?? null, error: this.lastError } : null;
  }
}
