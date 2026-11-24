import { EventEmitter } from 'node:events';
import type { Worker } from 'node:worker_threads';
import type { RateSnapshot, ScraperStatus } from '../../shared/types.ts';
import { startWorker } from '../workers.ts';
import type { ClientAccount } from './account.ts';

/** A worker that lived this long before exiting was healthy: its restart waits the shortest time again. */
const STABLE_MS = 60_000;

type WorkerMessage =
  | { type: 'status'; status: ScraperStatus }
  | { type: 'ratelimit'; snapshot: RateSnapshot }
  | { type: 'gc-done' };

/**
 * Main-thread handle for the scraper worker. Emits 'status' (ScraperStatus) and 'ratelimit' (RateSnapshot, the live
 * pacing state). The worker can be stopped and respawned (data-folder move) without listeners noticing, and starts
 * again by itself if it ever exits: Start and Stop never talk to a worker that is gone.
 */
export class ScraperController extends EventEmitter {
  private worker: Worker | null = null;
  status: ScraperStatus | null = null;
  rate: RateSnapshot | null = null;
  private paused = false;
  private failures = 0;
  private startedAt = 0;
  private me: ClientAccount | null = null;
  /** Champ select or a game is on, and Pause fetching during games (Settings) is on. */
  private inGame = false;
  private pauseInGame = true;
  private gcDone: (() => void)[] = [];

  constructor(private patch: string, private dataDir: () => string, private previousPatch = patch, private apiKey: () => string = () => '') {
    super();
    this.respawn();
  }

  /** Starts a fresh worker on the current data folder. It resumes the background loop if it was enabled. */
  respawn() {
    this.paused = false;
    if (this.worker) return;
    const worker = startWorker('scraper', { patch: this.patch, previousPatch: this.previousPatch, dataDir: this.dataDir(), gamePaused: this.gamePaused });
    worker.on('message', (msg: WorkerMessage) => {
      if (msg.type === 'status') { this.status = msg.status; this.emit('status', msg.status); }
      else if (msg.type === 'ratelimit') { this.rate = msg.snapshot; this.emit('ratelimit', msg.snapshot); }
      else if (msg.type === 'gc-done') this.gcDone.splice(0).forEach((f) => f());
    });
    worker.on('error', (err: Error) => {
      console.error('[scraper] worker crashed:', err);
      this.status = this.status && { ...this.status, running: false, phase: 'Restarting after an error', lastError: String(err) };
      if (this.status) this.emit('status', this.status);
    });
    worker.on('exit', () => {
      if (this.worker !== worker) return;
      this.worker = null;
      if (this.paused) return;
      // A worker that ran a while before exiting starts over at the shortest wait; one that keeps exiting right after
      // starting waits longer each time (its first status message arrives at once, so it can't be the signal).
      if (Date.now() - this.startedAt > STABLE_MS) this.failures = 0;
      // Unexpected: start again (the worker resumes fetching if it was on), waiting longer after each failure in a row.
      const wait = Math.min(60_000, 1000 * 2 ** this.failures++);
      console.warn(`[scraper] worker exited; restarting in ${wait / 1000} s`);
      setTimeout(() => { if (!this.paused && !this.worker) this.respawn(); }, wait).unref();
    });
    this.worker = worker;
    this.startedAt = Date.now();
    // A key saved in Settings and the signed-in account outlive the worker.
    const key = this.apiKey();
    if (key) worker.postMessage({ type: 'apikey', key });
    if (this.me) worker.postMessage({ type: 'me', account: this.me });
  }

  /**
   * Stops the worker, and with it every Riot API call, cache write and database write it owns. `what` is said on the
   * Data tab: "Paused while <what>".
   */
  async terminate(what = 'the data folder moves') {
    this.paused = true;
    const w = this.worker;
    this.worker = null;
    if (!w) return;
    await w.terminate();
    if (this.status) {
      this.status = { ...this.status, running: false, phase: `Paused while ${what}` };
      this.emit('status', this.status);
    }
  }

  /** Posts to the worker, starting one first when it is gone (and not paused for a data-folder move). */
  private post(msg: object) {
    if (!this.worker && !this.paused) this.respawn();
    this.worker?.postMessage(msg);
  }
  start() { this.post({ type: 'start' }); }
  stop() { this.post({ type: 'stop' }); }
  setPatch(patch: string, previousPatch: string) {
    this.patch = patch; this.previousPatch = previousPatch;
    this.post({ type: 'patch', patch, previousPatch });
  }
  setApiKey(key: string) { this.post({ type: 'apikey', key }); }
  /** Rebuilds the v3 capture from the disk cache (no API calls); `force` redoes matches already captured. */
  backfill(force = false) { this.post({ type: 'backfill', force }); }
  /** The signed-in account (League client): its Irelia games are fetched as source "me". */
  setMe(account: ClientAccount) {
    this.me = { gameName: account.gameName, tagLine: account.tagLine, platform: account.platform };
    this.post({ type: 'me', account: this.me });
  }
  /** Your server was chosen in Settings: look the account up again. */
  retryMe() { this.post({ type: 'me-retry' }); }
  /** A game just ended: fetch the user's new games once match-v5 has them. */
  gameEnded() { this.post({ type: 'game-ended' }); }

  private get gamePaused() { return this.inGame && this.pauseInGame; }
  /** Champ select or a game began or ended (server/index.ts), or the setting changed: fetching pauses or resumes. */
  setPauseInGame(on: boolean) { this.setInGame(this.inGame, on); }
  setInGame(inGame: boolean, pauseInGame = this.pauseInGame) {
    const was = this.gamePaused;
    this.inGame = inGame; this.pauseInGame = pauseInGame;
    if (this.gamePaused !== was) this.worker?.postMessage({ type: 'game-pause', on: this.gamePaused });
  }
  /** The worker's heap (bytes it holds, free space included; 0 without a worker): an evaluation's plan counts it. */
  async heapSize(): Promise<number> {
    const w = this.worker;
    return w ? (await w.getHeapStatistics()).total_heap_size : 0;
  }
  /** A full garbage collection in the worker; resolves when it is done (or after `timeoutMs`, or at once without one). */
  collectGarbage(timeoutMs = 5000): Promise<void> {
    if (!this.worker) return Promise.resolve();
    return new Promise((resolve) => {
      const timer = setTimeout(resolve, timeoutMs);
      this.gcDone.push(() => { clearTimeout(timer); resolve(); });
      this.worker!.postMessage({ type: 'gc' });
    });
  }
}
