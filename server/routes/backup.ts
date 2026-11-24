// Backing up the pool on the dashboard (0.18.0, pool only since 0.19.0): the backup folder (a Google Drive folder synced
// to this computer, say), "Back up now", importing a ZIP made on another computer. The state lives in Backup (main
// thread); the copying runs in the transfer thread (server/transfers.ts).
import fs from 'node:fs';
import path from 'node:path';
import express from 'express';
import type { BackupStatus } from '../../shared/types.ts';
import { AutoExport } from '../autoExport.ts';
import { appVersion } from '../model/evalBase.ts';
import { inspectExport } from '../transfer.ts';
import { Transfers, type TransferJob } from '../transfers.ts';

export interface BackupEnv {
  dataDir: () => string;
  versions: () => string[];
  broadcast: (status: BackupStatus) => void;
  /** The pool got games from an import: the build every window shows is made again (not mid-game). */
  rebuild: () => void;
  /**
   * An import left the other computer's evaluation (0.21.0, model/evalTravel.ts): take it now if it is valid here, and
   * say what came of it (null: nothing waiting; while an evaluation runs, it is taken once that one ends).
   */
  adoptEvaluation?: () => string | null;
}

const fmtMb = (b: number) => (b >= 1024 ** 3 ? `${(b / 1024 ** 3).toFixed(1)} GB` : `${Math.max(1, Math.round(b / 1024 ** 2))} MB`);

/** The sentence shown when a transfer finishes. */
function resultText(job: TransferJob) {
  return (r: unknown): string => {
    const x = r as { file?: string; bytes?: number; games?: number; added?: number; evaluation?: boolean };
    return job.kind === 'import'
      ? `Added ${(x.added ?? 0).toLocaleString('en-US')} new games to the pool`
      : `Saved ${path.basename(x.file ?? '')} (${fmtMb(x.bytes ?? 0)}, ${(x.games ?? 0).toLocaleString('en-US')} games${x.evaluation ? ', with this computer\'s last evaluation' : ''})`;
  };
}

export class Backup {
  readonly transfers: Transfers;
  readonly autoExport: AutoExport;

  constructor(private env: BackupEnv) {
    this.transfers = new Transfers(() => ({ dataDir: env.dataDir(), versions: env.versions(), app: appVersion() }));
    this.transfers.on('status', () => this.changed());
    this.autoExport = new AutoExport({ run: (job) => this.transfers.run(job, { wait: true, result: resultText(job) }), changed: () => this.changed() });
    this.autoExport.start();
  }

  private changed() { this.env.broadcast(this.status()); }

  status(): BackupStatus {
    return { transfer: this.transfers.status, autoExport: this.autoExport.view(), offers: this.autoExport.offers };
  }

  /**
   * Starts a transfer; the HTTP answer doesn't wait for it (its progress comes over /ws). What `after` returns is added
   * to the sentence shown when it finishes.
   */
  transfer(job: TransferJob, after?: (r: unknown) => string | null | void) {
    if (this.transfers.busy) throw new Error(`${this.transfers.status?.label ?? 'A transfer'} is still running.`);
    void this.transfers.run(job, { result: resultText(job) }).then((r) => {
      const more = after?.(r);
      if (more && this.transfers.status) this.transfers.status = { ...this.transfers.status, result: `${this.transfers.status.result ?? 'Done'}. ${more[0].toUpperCase()}${more.slice(1)}.` };
      this.changed();
    }, () => this.changed());
  }
}

const absolute = (v: unknown, what: string) => {
  if (typeof v !== 'string' || !path.isAbsolute(v)) throw new Error(`Choose ${what}.`);
  return path.resolve(v);
};

export function backupRoutes(b: Backup, env: Pick<BackupEnv, 'rebuild' | 'adoptEvaluation'>) {
  const router = express.Router();
  const act = (fn: (req: express.Request) => unknown) => (req: express.Request, res: express.Response) => {
    try { res.json(fn(req) ?? b.status()); } catch (err) { res.status(400).json({ error: err instanceof Error ? err.message : String(err) }); }
  };

  router.get('/backup', (_req, res) => { res.json(b.status()); });

  // The backup folder (null turns auto backup off).
  router.put('/backup/folder', act((req) => {
    const folder = req.body?.folder ? absolute(req.body.folder, 'a folder') : null;
    if (folder) fs.mkdirSync(folder, { recursive: true });
    b.autoExport.configure(folder);
  }));
  // Back up now (the answer doesn't wait; progress comes over /ws).
  router.post('/backup/now', act(() => { void b.autoExport.backUp().catch(() => {}); }));
  router.post('/backup/check', act(() => { void b.autoExport.check(); }));

  // What a ZIP holds, before importing it (the dashboard shows it and asks).
  router.get('/import/inspect', async (req, res) => {
    try {
      const file = absolute(req.query.file, 'a ZIP file');
      const { manifest } = await inspectExport(file);
      res.json({ file, name: manifest.name, exportedAt: manifest.exportedAt, games: manifest.games, patches: manifest.patches, machine: manifest.machine, bytes: fs.statSync(file).size });
    } catch (err) { res.status(400).json({ error: err instanceof Error ? err.message : String(err) }); }
  });
  router.post('/import', act((req) => {
    const file = absolute(req.body?.file, 'a ZIP file');
    if (req.body?.confirm !== true) throw new Error('Importing adds games to the pool and changes its builds: confirm it first.');
    b.transfer({ kind: 'import', file }, (r) => {
      const x = r as { exportedAt?: number; evaluation?: boolean };
      if (x.exportedAt) b.autoExport.seen(file, x.exportedAt);
      const note = x.evaluation ? env.adoptEvaluation?.() ?? null : null;
      env.rebuild();
      return note;
    });
  }));
  router.post('/import/dismiss', act((req) => { b.autoExport.seen(absolute(req.body?.file, 'a ZIP file'), Number(req.body?.exportedAt) || Date.now()); }));
  return router;
}
