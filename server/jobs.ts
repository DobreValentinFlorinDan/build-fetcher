import { EventEmitter } from 'node:events';
import type { ActionKind, JobName, JobsStatus } from '../shared/types.ts';
import { readAppConfig } from './appConfig.ts';
import { freeStoppedEvaluation } from './gpu/service.ts';
import type { EvalPath } from './model/evalBase.ts';
import { controlFlags, setBusyFlag, setOtherHeap } from './model/evalTeam.ts';
import { expectedMs, lastTiming, Progress, recordTiming, stepFraction } from './progress.ts';
import { evalInProcesses } from './model/runEval.ts';
import { startProcess, startWorker } from './workers.ts';

/** A finished data audit (server/audit.ts): the printable report and where it was saved. */
export interface AuditResult { text: string; file: string }
/**
 * A finished build evaluation (server/model/evaluate.ts): the report, where it was saved, and the gate; the threads it
 * ran on and the memory one took per game read (null: one thread), for the next one's plan; what it ran, a quick update
 * or a full evaluation, and why (0.16.0).
 */
export interface EvaluationResult {
  text: string; file: string; gate: 'passed' | 'failed'; threads?: number; bytesPerGame?: number | null; heapPerGame?: number | null; layout?: number;
  path?: EvalPath; reason?: string;
}

/**
 * `auto`: an evaluation the app starts by itself (server/autoEval.ts), and why. `size`: what its time grows with (the
 * games an evaluation reads, the matches an audit reads), for its estimate and the next one's. `path` (0.16.0): 'auto'
 * runs what planEvaluation says as it starts (model/evalBase.ts), 'full' (the default) every game again; `planned`:
 * what the plan said when it was asked for, a quick update or a full evaluation, which its estimate goes by.
 */
type Job = ({ job: 'audit' } | { job: 'evaluate'; quick?: boolean; auto?: string; path?: 'auto' | 'full'; planned?: EvalPath }) & { size?: number };
type Result<J extends Job> = J extends { job: 'audit' } ? AuditResult : EvaluationResult;

/** What the job thread posts (server/jobWorker.ts): progress while it runs, then the result. */
type WorkerMessage =
  | { progress: { step: number; steps: number; label: string; part?: number } }
  | { threads: number }
  | ({ ok: true } & (AuditResult | EvaluationResult))
  | { ok: false; error: string };

const STOPPED: Record<JobName, string> = { audit: 'The audit stopped', evaluate: 'The evaluation stopped' };
/**
 * The job thread's heap at most (0.19.0): what reading 150,000 games and their rows took at its peak (about 1.4 GB on
 * 0.19.0's first build, less since its rows are kept as plain copies), with room to spare.
 */
const JOB_HEAP_MB = 2560;
/** ...and as a process of its own (0.22.0): within the 4 GB a process's heap can reach, nothing else in it. */
const JOB_PROCESS_HEAP_MB = 3900;
/** How often the other threads' heap is measured while an evaluation runs. */
const OTHER_HEAP_EVERY_MS = 2000;
/** The error a cancelled job's caller gets. */
export const CANCELLED = 'Cancelled';

/**
 * Runs one job in its own thread (server/jobWorker.ts). The same job asked for again while it runs joins the running
 * one (a click joining an automatic evaluation makes it yours). `cancel` stops one; `cancelAll` stops them all (the
 * data folder is moving). Emits 'status' (JobsStatus) when a job starts, moves on a step or ends, for the dashboard.
 */
export class Jobs extends EventEmitter {
  private running = new Map<JobName, {
    promise: Promise<unknown>; stop: (reason: string, pause?: boolean) => Promise<unknown>; info: JobsStatus['running'][number];
    tell?: (msg: { type: 'busy'; on: boolean }) => void;
  }>();
  private last: JobsStatus['last'] = {};
  /** Shared with every job's threads: [0] = 1 while champ select or a game is on (the evaluation's other threads stop). */
  private control = controlFlags();

  /**
   * `threads(games)`: the threads an evaluation of that many games would run on now (its estimate goes by them).
   * `otherHeap`: what the middleware's other threads hold of the heap they share with the evaluation's (0.15.1),
   * measured every few seconds while one runs, for its plan (model/evalTeam.ts otherThreadsHeap).
   */
  constructor(private boot: () => { dataDir: string; versions: string[] }, private threads: (games: number) => number | undefined = () => undefined,
    private otherHeap: (() => Promise<number>) | null = null) { super(); }

  /** Measures the other threads' heap until `running()` says the evaluation ended. */
  private watchOtherHeap(running: () => boolean) {
    if (!this.otherHeap) return;
    const measure = async () => {
      if (!running()) return;
      try { setOtherHeap(this.control, await this.otherHeap!()); } catch { /* a thread that just exited: next time */ }
      if (running()) setTimeout(() => void measure(), OTHER_HEAP_EVERY_MS).unref();
    };
    void measure();
  }

  /** Champ select or a game began or ended: a running evaluation's other threads stop, and it finishes on one. */
  setBusy(on: boolean) {
    setBusyFlag(this.control, on);
    // A job running as a process (0.22.0) keeps its own flag: told here.
    for (const r of this.running.values()) r.tell?.({ type: 'busy', on });
  }
  /** Champ select or a game (setBusy). */
  get busy() { return Atomics.load(new Int32Array(this.control), 0) === 1; }

  get status(): JobsStatus {
    return { running: [...this.running.values()].map((r) => ({ ...r.info })), last: { ...this.last } };
  }

  run<J extends Job>(job: J): Promise<Result<J>> {
    const name: JobName = job.job;
    const auto = job.job === 'evaluate' ? job.auto : undefined;
    const existing = this.running.get(name);
    if (existing) {
      // A full evaluation asked for while a quick update runs (0.16.0) doesn't join it: the quick update stops as
      // Cancel stops it (its callers get "Cancelled"), and the full one starts once it has.
      if (job.job === 'evaluate' && job.path === 'full' && existing.info.path === 'update') {
        return existing.stop(CANCELLED).then(() => existing.promise.catch(() => {})).then(() => this.run(job));
      }
      if (!auto && existing.info.auto) { delete existing.info.auto; this.emit('status', this.status); }
      return existing.promise as Promise<Result<J>>;
    }
    const startedAt = Date.now();
    let worker: JobHandle;
    // An evaluation's heap capped (0.19.0): past JOB_HEAP_MB its thread alone stops, where it shared the app's one 4 GB
    // heap with the model and fetching threads and could take them down with it (ERR_WORKER_OUT_OF_MEMORY in each).
    // In the desktop app an evaluation runs as a process of its own (0.22.0): a 4 GB heap of its own, and its members
    // processes too (model/evalTeam.ts), so the app's heap no longer limits its threads.
    try {
      worker = name === 'evaluate' && evalInProcesses()
        ? processJob({ ...this.boot(), ...job, auto: auto !== undefined, busy: this.busy },
          () => startWorker('job', { ...this.boot(), ...job, auto: auto !== undefined, control: this.control }, [], JOB_HEAP_MB))
        : threadJob(startWorker('job', { ...this.boot(), ...job, auto: auto !== undefined, control: this.control }, [], name === 'evaluate' ? JOB_HEAP_MB : undefined));
    } catch (err) {
      this.last[name] = { startedAt, finishedAt: Date.now(), ok: false, error: err instanceof Error ? err.message : String(err) };
      this.emit('status', this.status);
      return Promise.reject(err);
    }
    // Its time left: how long the last run here took, scaled to this one's size, until its own pace says more. Each
    // step weighs what it took last time (the evaluation's steps differ a hundredfold). A quick update goes by the
    // last quick update (0.16.0): its time and steps are another evaluation's.
    const quick = job.job === 'evaluate' && job.quick === true;
    const planned = job.job === 'evaluate' ? (job.planned ?? 'full') : null;
    const kind: ActionKind = planned === 'update' ? 'evaluate-update' : name;
    const timed = !quick && job.size !== undefined && job.size > 0;
    const tracker = new Progress(timed ? expectedMs(kind, job.size!, { threads: name === 'evaluate' ? this.threads(job.size!) : undefined }) : null, startedAt);
    const weights = lastTiming(kind)?.steps ?? null;
    const stepStarts: number[] = [];
    const info: JobsStatus['running'][number] = {
      job: name, startedAt, step: null, steps: null, label: null, ...(auto ? { auto } : {}), ...(planned ? { path: planned } : {}),
      progress: tracker.info(0, startedAt),
    };
    let stopReason: string | null = null;
    let paused = false;
    let ended = false;
    if (name === 'evaluate') this.watchOtherHeap(() => !ended);
    const promise = new Promise<Result<J>>((resolve, reject) => {
      worker.on('message', (msg: WorkerMessage) => {
        if ('progress' in msg) {
          const { part, ...p } = msg.progress;
          if (p.step >= 1 && stepStarts[p.step - 1] === undefined) stepStarts[p.step - 1] = Date.now();
          Object.assign(info, p);
          info.progress = tracker.info(stepFraction(p.step, p.steps, part ?? 0, weights));
          this.emit('status', this.status);
          return;
        }
        if ('threads' in msg && !('ok' in msg)) {
          // Its threads, now that it knows: its time left goes by them.
          info.threads = msg.threads;
          if (timed) tracker.expect(expectedMs(kind, job.size!, { threads: msg.threads }));
          this.emit('status', this.status);
          return;
        }
        if (msg.ok) { const { ok: _, ...result } = msg; resolve(result as Result<J>); } else reject(new Error(msg.error ?? 'The job failed'));
      });
      worker.once('error', reject);
      worker.once('exit', () => reject(new Error(stopReason ?? STOPPED[name])));
    });
    // Whether it is still the app's own when it ends (a click may have joined it).
    const own = () => (info.auto ? { auto: info.auto } : {});
    const settled = promise.then(
      (result) => {
        const finishedAt = Date.now();
        const { bytesPerGame: _, heapPerGame: _h, layout: _l, ...report } = result as Partial<EvaluationResult> & AuditResult;
        this.last[name] = { startedAt, finishedAt, ok: true, ...report, ...own() };
        // Kept for the next estimate: the whole time and each step's (not a `quick` run's, one bandwidth and prior: another
        // job), under what it ran. Only when that is what it was timed as: a quick update that went on to a full
        // evaluation (it would change a verdict) took both, and says little about either.
        const ran = (result as Partial<EvaluationResult>).path ?? 'full';
        if (timed && (planned === null || ran === planned)) {
          const steps = info.steps !== null && stepStarts.length === info.steps && Array.from(stepStarts).every((t) => t !== undefined)
            ? stepStarts.map((t, i) => (stepStarts[i + 1] ?? finishedAt) - t) : undefined;
          const { threads, bytesPerGame, heapPerGame, layout } = result as Partial<EvaluationResult>;
          recordTiming(kind, {
            size: job.size!, ms: finishedAt - startedAt, at: finishedAt, ...(steps ? { steps } : {}),
            ...(threads ? { threads } : {}), ...(bytesPerGame ? { bytesPerGame, ...(heapPerGame ? { heapPerGame } : {}), layout } : {}),
          });
        }
      },
      (err: Error) => {
        // Its threads couldn't free what they put on the GPU (Use GPU): freed now.
        if (name === 'evaluate' && readAppConfig().useGpu) void freeStoppedEvaluation();
        this.last[name] = stopReason === CANCELLED
          ? { startedAt, finishedAt: Date.now(), ok: false, cancelled: true, ...own(), ...(paused ? { paused } : {}) }
          : { startedAt, finishedAt: Date.now(), ok: false, error: err.message, ...own() };
      },
    ).finally(() => {
      ended = true;
      this.running.delete(name);
      this.emit('status', this.status);
    });
    // Callers get the result once the status says it ended.
    const result = settled.then(() => promise);
    this.running.set(name, {
      promise: result, info,
      stop: (reason, pause = false) => { stopReason = reason; paused = pause; return worker.terminate(); },
      tell: worker.tell,
    });
    this.emit('status', this.status);
    return result;
  }

  /** Stops a running job; its caller gets "Cancelled". `paused`: an automatic evaluation champ select or a game stopped. */
  async cancel(name: JobName, paused = false) { await this.running.get(name)?.stop(CANCELLED, paused); }

  /** The running job of that name, if any (its `auto` says whether the app started it). */
  runningJob(name: JobName) { return this.running.get(name)?.info ?? null; }

  async cancelAll(what = 'the data folder moves') { await Promise.all([...this.running.values()].map((j) => j.stop(`Stopped while ${what}`))); }
}

/** A job's thread or process (0.22.0), as Jobs.run talks to it. */
interface JobHandle {
  on(event: 'message', fn: (msg: WorkerMessage) => void): unknown;
  once(event: 'error', fn: (err: Error) => void): unknown;
  once(event: 'exit', fn: () => void): unknown;
  terminate(): Promise<unknown>;
  /** A message to a process (its busy flag); a thread reads the shared flag instead. */
  tell?: (msg: { type: 'busy'; on: boolean }) => void;
}

const threadJob = (worker: ReturnType<typeof startWorker>): JobHandle => worker as unknown as JobHandle;

/**
 * The job's program (server/jobWorker.ts) as a process: its boot as the first message, a heap of its own. One that ends
 * (or can't start) before it says anything runs as a thread instead (`asThread`), as before 0.22.0.
 */
function processJob(boot: object, asThread: () => ReturnType<typeof startWorker>): JobHandle {
  const out = new EventEmitter();
  let heard = false, stopping = false, fellBack = false, ended = false;
  let thread: ReturnType<typeof startWorker> | null = null;
  const proc = startProcess('job', JOB_PROCESS_HEAP_MB);
  const fallBack = (why: string) => {
    if (fellBack || heard || stopping) return false;
    fellBack = true;
    console.warn(`[eval] the evaluation's process ${why}: it runs as a thread instead`);
    thread = asThread();
    thread.on('message', (m) => out.emit('message', m));
    thread.once('error', (e) => out.emit('error', e));
    thread.once('exit', () => { ended = true; out.emit('exit'); });
    return true;
  };
  proc.on('message', (m) => { if (!fellBack) { heard = true; out.emit('message', m); } });
  proc.on('error', (err) => { if (!fallBack(`couldn't start (${err.message})`) && !fellBack) out.emit('error', err); });
  proc.once('exit', (code) => { if (fallBack(`ended before it began (exit ${code})`) || fellBack) return; ended = true; out.emit('exit'); });
  try { proc.send({ type: 'boot', ...boot }); } catch (err) { fallBack(`couldn't be reached (${err instanceof Error ? err.message : String(err)})`); }
  return {
    on: (event, fn) => out.on(event, fn as (msg: unknown) => void),
    once: (event, fn) => out.once(event, fn as () => void),
    terminate: () => {
      stopping = true;
      if (thread) return thread.terminate();
      return new Promise<void>((resolve) => {
        if (ended || proc.exitCode !== null) { resolve(); return; }
        out.once('exit', () => resolve());
        proc.kill();
      });
    },
    tell: (msg) => { if (!thread && !ended && proc.connected) proc.send(msg); },
  };
}
