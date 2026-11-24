import { EventEmitter } from 'node:events';
import type { Worker } from 'node:worker_threads';
import type { ModelStatus } from '../../shared/types.ts';
import { startWorker } from '../workers.ts';
import type { ChampionTotals } from './profiles.ts';
import { lastTiming, recordTiming } from '../progress.ts';
import { withSwitches } from './service.ts';

/** A worker that lived this long before exiting was healthy: its restart waits the shortest time again. */
const STABLE_MS = 60_000;

type Message =
  | { type: 'reply'; id: number; value?: unknown; error?: string }
  | { type: 'status'; status: ModelStatus; profiles?: { current: ChampionTotals[]; older: ChampionTotals[] }; built?: { ms: number; games: number } }
  | { type: 'data' }
  | { type: 'gc-done' };

/**
 * Main-thread handle for the model worker (model/worker.ts). Requests never block the main thread and time out
 * instead of hanging; the worker restarts by itself if it ever exits. Emits 'status' (ModelStatus, plus the champion
 * profile rows after a build) and 'data' (the stored games changed).
 */
export class ModelClient extends EventEmitter {
  status: ModelStatus = { state: 'loading', info: null, mode: 'baseline', error: null };
  private worker: Worker | null = null;
  private seq = 0;
  private pending = new Map<number, { resolve: (v: unknown) => void; reject: (e: Error) => void; timer: NodeJS.Timeout }>();
  private paused = false;
  private failures = 0;
  private startedAt = 0;
  private env: { ownPlatform?: string | null; busy?: boolean; queued?: boolean; evaluating?: boolean } = {};
  private gcDone: (() => void)[] = [];

  constructor(private boot: () => { dataDir: string; versions: string[] }) {
    super();
    this.respawn();
  }

  /** Starts the worker on the current data folder (after a move, or by itself after an exit). */
  respawn() {
    this.paused = false;
    if (this.worker) return;
    // The last build's time (kept on disk): the first build of a session has an estimate too.
    const worker = startWorker('model', { ...this.boot(), lastBuildMs: lastTiming('model')?.ms ?? null });
    worker.on('message', (msg: Message) => {
      if (msg.type === 'reply') {
        const p = this.pending.get(msg.id);
        if (!p) return;
        this.pending.delete(msg.id);
        clearTimeout(p.timer);
        if (msg.error) p.reject(new Error(msg.error)); else p.resolve(msg.value);
      } else if (msg.type === 'status') {
        if (msg.built) recordTiming('model', { size: msg.built.games, ms: msg.built.ms, at: Date.now() });
        this.status = msg.status;
        this.emit('status', msg.status, msg.profiles);
      } else if (msg.type === 'data') this.emit('data');
      else if (msg.type === 'gc-done') this.gcDone.splice(0).forEach((f) => f());
    });
    worker.on('error', (err) => console.error('[model] worker crashed:', err));
    worker.on('exit', () => {
      if (this.worker !== worker) return;
      this.worker = null;
      for (const p of this.pending.values()) { clearTimeout(p.timer); p.reject(new Error('The model worker stopped')); }
      this.pending.clear();
      this.status = { ...this.status, state: 'loading' };
      this.emit('status', this.status);
      if (this.paused) return;
      // A worker that ran a while before exiting starts over at the shortest wait; one that keeps exiting right after
      // starting waits longer each time (its first status message arrives at once, so it can't be the signal).
      if (Date.now() - this.startedAt > STABLE_MS) this.failures = 0;
      // Unexpected: start again, waiting longer after each failure in a row (up to a minute).
      const wait = Math.min(60_000, 1000 * 2 ** this.failures++);
      console.warn(`[model] worker exited; restarting in ${wait / 1000} s`);
      setTimeout(() => { if (!this.paused) this.respawn(); }, wait).unref();
    });
    this.worker = worker;
    this.startedAt = Date.now();
    if (Object.keys(this.env).length) worker.postMessage({ type: 'env', ...this.env });
  }

  /** Stops the worker (the data folder is moving): its database connection closes with it. */
  async terminate() {
    this.paused = true;
    const w = this.worker;
    this.worker = null;
    for (const p of this.pending.values()) { clearTimeout(p.timer); p.reject(new Error('The model worker stopped')); }
    this.pending.clear();
    if (w) await w.terminate();
  }

  request<T>(kind: string, args: unknown = {}, timeoutMs = 30_000): Promise<T> {
    const worker = this.worker;
    if (!worker) return Promise.reject(new Error('The model is restarting. Try again in a moment.'));
    const id = ++this.seq;
    return new Promise<T>((resolve, reject) => {
      const timer = setTimeout(() => {
        this.pending.delete(id);
        reject(new Error('The model is busy (building from new data). Try again in a moment.'));
      }, timeoutMs);
      this.pending.set(id, { resolve: resolve as (v: unknown) => void, reject, timer });
      worker.postMessage({ type: 'request', id, kind, args });
    });
  }

  private post(msg: object) { this.worker?.postMessage(msg); }
  /** A full garbage collection in the worker; resolves when it is done (or after `timeoutMs`, or at once without one). */
  collectGarbage(timeoutMs = 5000): Promise<void> {
    if (!this.worker) return Promise.resolve();
    return new Promise((resolve) => {
      const timer = setTimeout(resolve, timeoutMs);
      this.gcDone.push(() => { clearTimeout(timer); resolve(); });
      this.worker!.postMessage({ type: 'gc' });
    });
  }
  /** Lets go of what the worker keeps for requests (Slim stored data starts, 0.15.2); the model stays. */
  release() { this.post({ type: 'release' }); }
  /** The worker's heap (bytes it holds, free space included; 0 without a worker): an evaluation's plan counts it. */
  async heapSize(): Promise<number> {
    const w = this.worker;
    return w ? (await w.getHeapStatistics()).total_heap_size : 0;
  }
  /**
   * Your platform (region boost) and whether champ select or a game is on, the queue, or an evaluation (builds wait).
   * Sent again after a restart.
   */
  setEnv(env: { ownPlatform?: string | null; busy?: boolean; queued?: boolean; evaluating?: boolean }) {
    const changed = Object.entries(env).some(([k, v]) => this.env[k as keyof typeof env] !== v);
    if (!changed) return;
    Object.assign(this.env, env);
    this.post({ type: 'env', ...env });
  }
  setPatch(versions: string[]) { this.post({ type: 'patch', versions }); }
  /** A new evaluation (or your Draft-adjusted / Lane baseline choice) changes the mode and settings. */
  evaluated() { this.post({ type: 'evaluated' }); }
  rebuild() { this.post({ type: 'rebuild' }); }
  /**
   * A Data tab switch was saved: the model rebuilds with it. Returns the status with the switches as saved now (kept
   * until the model thread's next status, which reads the same), to show at once: the model thread answers only after
   * a build it may be in.
   */
  optionsChanged(): ModelStatus {
    this.post({ type: 'options' });
    if (this.status.info) this.status = { ...this.status, info: withSwitches(this.status.info) };
    return this.status;
  }
  liveReset() { this.post({ type: 'live-reset' }); }
}
