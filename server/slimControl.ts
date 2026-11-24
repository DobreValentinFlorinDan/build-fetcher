// Slim stored data's controller (0.14.0): runs server/slimWorker.ts with fetching, jobs and the conversion paused,
// rolls back by itself when the check finds a difference or champ select interrupts it from the slimming on, and
// carries out Roll back and Keep the slimmer data. The Storage card shows its status (SlimStatus), phase by phase with
// the time left (0.14.2): from each phase's own pace, the check's from the time the same phase of the baseline took.
import type { Worker } from 'node:worker_threads';
import type { SlimCount, SlimManifest, SlimStatus } from '../shared/types.ts';
import type { Db } from './db.ts';
import { Progress } from './progress.ts';
import { backupBytes, copyCount, readManifest, removeBackup, restoreEntries, rollbackFile, rollbackRows, SLIM_META, slimDir, writeManifest } from './slim.ts';
import { startWorker } from './workers.ts';

export interface SlimDeps {
  dataDir: () => string;
  cacheDir: () => string;
  versions: () => string[];
  db: () => Db;
  /** Fetching, the jobs and the conversion stop for the run (the model keeps answering), and start again. */
  pauseWriters: () => Promise<void>;
  resumeWriters: () => void;
  /** For a rollback that replaces the database file: every thread with it open stops, and this thread's connection
   * is closed for `between` and opened again. */
  withDatabaseClosed: (between: () => Promise<void>) => Promise<void>;
  /** Champ select or a game. */
  busy: () => boolean;
  /** Another storage operation is running (a move, Compact, old patches, the conversion of earlier versions' data). */
  blocked: () => string | null;
  onStatus: (s: SlimStatus) => void;
  log?: (line: string) => void;
}

const message = (err: unknown) => (err instanceof Error ? err.message : String(err));

export class SlimControl {
  private worker: Worker | null = null;
  status: SlimStatus;

  constructor(private deps: SlimDeps) {
    this.status = { running: false, step: null, part: null, label: null, manifest: this.lastManifest(), error: null };
  }

  /** The run on record: the backup folder's (a verified run waiting for your choice), else the last one, from meta. */
  private lastManifest(): SlimManifest | null {
    const m = readManifest(this.deps.dataDir());
    if (m) return { ...m, backupBytes: safe(() => backupBytes(this.deps.dataDir())) ?? m.backupBytes };
    try {
      const v = this.deps.db().getMeta(SLIM_META);
      const last = v ? JSON.parse(v) as SlimManifest : null;
      // Verified, but its backup folder is gone (deleted by hand): nothing left to roll back to.
      return last?.step === 'verified' ? { ...last, step: 'kept', note: 'The backup folder is gone: the slimmer data is kept.' } : last;
    } catch { return null; }
  }

  /** The phase running (`step|phase`), its progress, and how long each finished phase took (the check's estimates). */
  private phase: { key: string; progress: Progress; copyTotal?: number } | null = null;
  private phaseMs = new Map<string, number>();
  private poll: ReturnType<typeof setInterval> | null = null;

  /** A worker's progress: a new phase starts its own clock; the check's phases expect what the baseline's took. */
  private onProgress(step: SlimStatus['step'], phase: string, part: number | null, label: string | null, count: SlimCount | null) {
    if (this.stopReason) return;   // stopping: its last messages say nothing anymore
    const key = `${step}|${phase}`;
    if (this.phase?.key !== key) {
      if (this.phase) this.phaseMs.set(this.phase.key, Date.now() - this.phase.progress.startedAt);
      const expected = step === 'verify' ? this.phaseMs.get(`baseline|${phase}`) ?? null : null;
      this.phase = { key, progress: new Progress(expected) };
      if (key === 'backup|copy') this.phase.copyTotal = safe(() => this.pagesInUse()) ?? undefined;
    }
    this.show({ step, part, label, count, progress: this.phase.progress.info(part) });
  }

  /** The database's bytes in use: about what VACUUM INTO writes. */
  private pagesInUse() {
    const raw = this.deps.db().raw;
    const n = (pragma: string) => Number(Object.values(raw.prepare(`PRAGMA ${pragma}`).get() as Record<string, unknown>)[0]);
    return (n('page_count') - n('freelist_count')) * n('page_size');
  }

  /** While the worker copies the database (it waits on VACUUM INTO), the copy's size says how far it is. */
  private watchCopy() {
    const p = this.phase;
    if (!p || p.key !== 'backup|copy' || !p.copyTotal) return;
    const { part, count } = copyCount(this.deps.dataDir(), p.copyTotal);
    this.show({ part, count, progress: p.progress.info(part) });
  }

  private endPhases() {
    if (this.poll) clearInterval(this.poll);
    this.poll = null;
    this.phase = null;
    this.phaseMs.clear();
  }

  private show(patch: Partial<SlimStatus>) {
    this.status = { ...this.status, ...patch };
    this.deps.onStatus(this.status);
  }

  /** The run's end: its manifest kept in meta (the backup folder may be gone), and the card told. */
  private finish(m: SlimManifest, error: string | null = null) {
    try { this.deps.db().setMeta(SLIM_META, JSON.stringify(m)); } catch { /* shown anyway */ }
    this.show({ running: false, stopping: false, step: null, part: null, label: null, count: null, progress: null, manifest: m, error });
  }

  /** Why a run can't start now, or null. */
  refusal(): string | null {
    if (this.worker || this.status.running) return 'Slim stored data is already running.';
    if (this.deps.busy()) return 'Not during champ select or a game: try again after it.';
    const blocked = this.deps.blocked();
    if (blocked) return blocked;
    if (readManifest(this.deps.dataDir())?.step === 'verified') return 'The last run\'s backup is still there: choose Roll back or Keep the slimmer data first.';
    return null;
  }

  async start(): Promise<SlimStatus> {
    const why = this.refusal();
    if (why) throw new Error(why);
    const m: SlimManifest = { version: 1, startedAt: Date.now(), updatedAt: Date.now(), step: 'backup', plan: null, db: null, cache: null };
    this.endPhases();
    this.show({ running: true, step: 'backup', part: null, label: 'Checking the free space…', count: null, progress: null, error: null, manifest: m });
    await this.deps.pauseWriters();
    let worker: Worker;
    try { worker = startWorker('slim', { dataDir: this.deps.dataDir(), versions: this.deps.versions(), manifest: m }); } catch (err) {
      this.deps.resumeWriters();
      this.finish({ ...m, step: 'cancelled', note: message(err) }, message(err));
      throw err;
    }
    this.worker = worker;
    this.stopReason = null;
    type Outcome = { type: string; step?: SlimStatus['step']; phase?: string; part?: number | null; label?: string; count?: SlimCount | null; manifest?: SlimManifest; identical?: boolean; differences?: string[]; error?: string; refused?: boolean };
    let outcome: Outcome | null = null;
    this.poll = setInterval(() => this.watchCopy(), 2000);
    this.poll.unref?.();
    worker.on('message', (msg: Outcome) => {
      if (msg.type === 'progress') this.onProgress(msg.step ?? null, msg.phase ?? '', msg.part ?? null, msg.label ?? null, msg.count ?? null);
      else outcome = msg;
    });
    // Which thread stopped (0.15.2): this one, or one it started (its own message then).
    worker.on('error', (err) => { outcome ??= { type: 'error', error: `its thread stopped: ${message(err)}` }; });
    // Only once the thread is gone (its database connections closed) is anything undone.
    worker.on('exit', () => {
      this.worker = null;
      this.endPhases();
      const o = outcome as Outcome | null;
      const latest = readManifest(this.deps.dataDir()) ?? o?.manifest ?? m;
      if (o?.type === 'result' && !this.stopReason) void this.ended(o.manifest!, o.identical ? null : o.differences ?? []);
      else void this.failed(o?.type === 'error' && o.manifest ? o.manifest : latest, this.stopReason ?? o?.error ?? 'Slim stored data stopped', o?.refused === true);
    });
    return this.status;
  }
  private stopReason: string | null = null;

  /** The check is done: identical (the backup stays for your choice) or not (rolled back at once). */
  private async ended(m: SlimManifest, diff: string[] | null) {
    if (diff === null) {
      // Freed: the timelines moved out, and what slimming saved on the rest.
      const verified: SlimManifest = { ...m, freedBytes: m.applied ? (m.plan?.timelineBytes ?? 0) + m.applied.savedBytes : undefined, backupBytes: safe(() => backupBytes(this.deps.dataDir())) ?? undefined };
      writeManifest(this.deps.dataDir(), verified, this.deps.db());
      this.deps.resumeWriters();
      this.finish(verified);
      return;
    }
    this.show({ step: 'rollback', part: null, label: 'A result differed: rolling back…', count: null, progress: null });
    await this.rollBackFile(m, `Rolled back by itself: ${diff.length} result${diff.length === 1 ? '' : 's'} differed (${diff.slice(0, 3).join('; ')}).`, diff);
  }

  /** The run stopped short (an error, champ select, a refusal): undone as far as it went. */
  private async failed(m: SlimManifest, error: string, refused: boolean) {
    if (refused) { this.deps.resumeWriters(); this.finish({ ...m, step: 'cancelled', note: error }, error); return; }
    if (m.step === 'apply' || m.step === 'verify') {
      await this.rollBackFile(m, `Stopped while slimming (${error}): rolled back.`);
      return;
    }
    // Before the slimming nothing changed: the entries moved out go back, and the backup goes.
    try { await restoreEntries(this.deps.dataDir(), this.deps.cacheDir()); removeBackup(this.deps.dataDir()); } catch (err) { this.deps.log?.(`[slim] clean-up: ${message(err)}`); }
    this.deps.resumeWriters();
    this.finish({ ...m, step: 'cancelled', note: `Stopped before slimming (${error}): nothing was changed.` }, null);
  }

  private async rollBackFile(m: SlimManifest, note: string, differences?: string[]) {
    try {
      let restored = { entries: 0, sha256: '' };
      await this.deps.withDatabaseClosed(async () => { restored = await rollbackFile(this.deps.dataDir(), this.deps.cacheDir(), m); });
      removeBackup(this.deps.dataDir());
      this.deps.resumeWriters();
      this.finish({ ...m, step: 'rolled-back', ...(differences ? { differences } : {}), note: `${note} The database matches its backup (SHA-256 ${restored.sha256.slice(0, 12)}…); ${restored.entries.toLocaleString('en-US')} cache entries are back.` });
    } catch (err) {
      // The backup stays: it is what a later start (or you) can restore from.
      this.deps.resumeWriters();
      this.finish(m, `The rollback failed: ${message(err)}. The backup is kept in ${slimDir(this.deps.dataDir())}.`);
    }
  }

  /** Stop (0.15.0, Settings → Storage): the run ends at once, undone as far as it went (rolled back from the slimming on). */
  stop(): SlimStatus {
    if (!this.worker) throw new Error('Slim stored data is not running.');
    if (this.stopReason) return this.status;
    this.stopReason = 'you stopped it';
    this.show({ stopping: true, part: null, count: null, progress: null, label: 'Stopping: undoing what it did so far…' });
    void this.worker.terminate();   // its exit undoes it
    return this.status;
  }

  /** Champ select or a game began: a run stops at once (and is undone as far as it went). */
  setBusy(busy: boolean) {
    if (!busy || !this.worker) return;
    this.stopReason = 'champ select started';
    void this.worker.terminate();   // its exit undoes it
  }

  /** Roll back a verified run (later, with the app having run since): row by row, so newer games stay. */
  async rollback(): Promise<SlimStatus> {
    const m = readManifest(this.deps.dataDir());
    if (!m || m.step !== 'verified') throw new Error('There is no verified run to roll back.');
    if (this.deps.busy()) throw new Error('Not during champ select or a game: try again after it.');
    this.show({ running: true, step: 'rollback', part: null, label: 'Rolling back…', count: null, progress: null, error: null });
    await this.deps.pauseWriters();
    try {
      const r = await rollbackRows(this.deps.dataDir(), this.deps.cacheDir(), this.deps.db());
      removeBackup(this.deps.dataDir());
      this.finish({ ...m, step: 'rolled-back', note: `Rolled back: ${r.entries.toLocaleString('en-US')} cache entries and ${r.archived.toLocaleString('en-US')} archived matches are as they were (checked by SHA-256)${r.restoredSince ? `; ${r.restoredSince} archived matches restored to the tables since keep their cleared columns` : ''}.` });
    } catch (err) {
      this.finish(m, `The rollback failed: ${message(err)}. The backup is kept.`);
    } finally { this.deps.resumeWriters(); }
    return this.status;
  }

  /** Keep the slimmer data: the backup goes. */
  keep(): SlimStatus {
    const m = readManifest(this.deps.dataDir());
    if (!m || m.step !== 'verified') throw new Error('There is no verified run to keep.');
    removeBackup(this.deps.dataDir());
    this.finish({ ...m, step: 'kept', note: 'Kept the slimmer data: the backup was deleted.' });
    return this.status;
  }
}

function safe<T>(f: () => T): T | null { try { return f(); } catch { return null; } }
