// How long the app's long actions take, and how long one that is running has left (0.10.0): every progress bar in the
// dashboard shows an estimate.
//
// Each finished action's duration and size (games, matches, bytes or files) are kept next to the settings
// (timings.json, this computer only, never sent anywhere), so the next estimate is this computer's own: the last run
// scaled to the size it has now. While an action runs, its own pace takes over as it goes. Before an action has run
// here once, the evaluation and the compaction start from what they took on the test database; the others say nothing
// until their own pace does (a few seconds).
import fs from 'node:fs';
import path from 'node:path';
import type { ActionKind, ProgressInfo } from '../shared/types.ts';
import { appConfigDir } from './appConfig.ts';

export type { ActionKind } from '../shared/types.ts';

/**
 * How an action's time grows with its size: time ∝ size^exponent (1 unless listed). The evaluation compares every
 * held-out game with every training game of its fold, so it grows faster than its games: 0.5 min at 4,400 games and
 * 10 at 54,000 on the test databases, about games^1.25.
 *
 * A quick update (0.16.0, 'evaluate-update') is timed apart, and its size is the same as a full one's: the games it
 * reads, not the new games. It reads and prepares every stored game and rebuilds each fold's models from all of them,
 * as a full one does, then measures only the new held-out games against them and decides the gates again on every
 * game's outcomes: about linear in the games read (no settings grids, no game against every other). Counted in new
 * games, 600 new games would take twice what 300 did, which reading 80,000 games each time doesn't; counted in games
 * read, one update's time is a close estimate of the next one's (growth 1, the default here).
 */
const GROWTH: Partial<Record<ActionKind, number>> = { evaluate: 1.3 };

/** The evaluation's two kinds (0.16.0): both run on the evaluation's threads, so their estimates go by them. */
const ON_THREADS = new Set<ActionKind>(['evaluate', 'evaluate-update']);

/**
 * Before an action has run on this computer: its time at a size, measured on the 54,000-game test database (Linux, one
 * core of a cloud machine). A first guess: the run's own pace soon replaces it, and its time is kept for the next. A
 * quick update has none yet: until one has run here, its own pace says.
 */
const DEFAULT_MS: Partial<Record<ActionKind, (size: number) => number>> = {
  evaluate: (games) => 10 * 60_000 * (Math.max(1, games) / 54_000) ** 1.3,
  compact: (bytes) => bytes / (58 * 1024 * 1024) * 1000,
};

export interface Timing {
  size: number;
  ms: number;
  at: number;
  /** Each step's ms (the evaluation): how the next run weighs its steps. */
  steps?: number[];
  /** The evaluation's threads (0.11.0; 1 when missing) and the memory one took per game it read (bytes). */
  threads?: number;
  bytesPerGame?: number;
  /** ...of which heap (0.19.0): what counts where the threads share one. */
  heapPerGame?: number;
  /** How the threads held their data when it was measured (model/runEval.ts THREAD_LAYOUT; 1 when missing: 0.11.0). */
  layout?: number;
}

/**
 * The evaluation's share that runs on one thread whatever the threads (reading and preparing the games, fitting the
 * models, putting the results back in order). The rest is shared out: on W threads it takes serial + (1 − serial) / W
 * of its time on one (Amdahl). Measured on the 54,000-game test database: 12.2 min on one thread, 4.5 on four.
 */
export const EVAL_SERIAL_SHARE = 0.16;
const onThreads = (threads: number) => EVAL_SERIAL_SHARE + (1 - EVAL_SERIAL_SHARE) / Math.max(1, threads);

/** Runs kept per action (the newest first). */
const KEEP = 5;

const timingsFile = () => path.join(appConfigDir, 'timings.json');

function readTimings(): Partial<Record<ActionKind, Timing[]>> {
  try { return JSON.parse(fs.readFileSync(timingsFile(), 'utf8')) as Partial<Record<ActionKind, Timing[]>>; } catch { return {}; }
}

/** Keeps a finished action's time (only complete runs: a cancelled one says little about the next). */
export function recordTiming(kind: ActionKind, t: Timing) {
  if (!(t.ms > 0) || !(t.size > 0)) return;
  const all = readTimings();
  all[kind] = [t, ...(all[kind] ?? [])].slice(0, KEEP);
  try {
    fs.mkdirSync(appConfigDir, { recursive: true });
    const tmp = `${timingsFile()}.${process.pid}.tmp`;
    fs.writeFileSync(tmp, `${JSON.stringify(all)}\n`);
    fs.renameSync(tmp, timingsFile());
  } catch { /* an estimate isn't worth failing an action over */ }
}

export function lastTiming(kind: ActionKind): Timing | null {
  return readTimings()[kind]?.[0] ?? null;
}

/**
 * The memory an evaluation thread took per game, as the newest run on several threads measured it with the threads
 * holding their data as `layout` says (null: none yet). A measure of another layout says little about this one.
 * Measured by full evaluations only: a quick update plans its threads by it too (they build the same folds' models from
 * the same games).
 */
export function threadMeasure(layout: number): number | null {
  return readTimings().evaluate?.find((t) => t.bytesPerGame && (t.layout ?? 1) === layout)?.bytesPerGame ?? null;
}

/** ...and its heap (0.19.0), from the same run. */
export function heapMeasure(layout: number): number | null {
  return readTimings().evaluate?.find((t) => t.bytesPerGame && (t.layout ?? 1) === layout)?.heapPerGame ?? null;
}

/**
 * How long an action of this size should take here: the last run's time scaled to the size; null: nothing to go on.
 * `threads` (the evaluation, full or quick): on that many threads, from the last run's own.
 */
export function expectedMs(kind: ActionKind, size: number, o: { threads?: number } = {}): number | null {
  const last = lastTiming(kind);
  const scale = (ms: number) => (o.threads === undefined || !ON_THREADS.has(kind) ? ms : ms / onThreads(last?.threads ?? 1) * onThreads(o.threads));
  if (last) return scale(last.ms * (Math.max(1, size) / last.size) ** (GROWTH[kind] ?? 1));
  const first = DEFAULT_MS[kind]?.(size);
  return first === undefined ? null : scale(first);
}

/** Before its pace says anything: at least this much done and this long running. */
const PACE_AFTER = { fraction: 0.01, ms: 3000 };

/**
 * A running action's progress. `info(fraction)`: how far it is (null when its work can't be counted) and its time
 * left. That is the expected time minus what has passed, until its own pace (time so far over the fraction done) can
 * say more. The pace counts more the further it gets, fully from halfway.
 */
export class Progress {
  readonly startedAt: number;

  constructor(private expected: number | null, now = Date.now()) { this.startedAt = now; }

  /** A better expected time, once the action knows more (the evaluation, its threads). */
  expect(ms: number | null) { if (ms !== null) this.expected = ms; }

  info(fraction: number | null, now = Date.now()): ProgressInfo {
    const f = fraction === null ? null : Math.min(1, Math.max(0, fraction));
    const elapsed = now - this.startedAt;
    const left = this.expected !== null ? Math.max(0, this.expected - elapsed) : null;
    let eta = left;
    if (f !== null && f >= PACE_AFTER.fraction && elapsed >= PACE_AFTER.ms) {
      const pace = elapsed * (1 - f) / f;
      const w = left === null ? 1 : Math.min(1, f * 2);
      eta = w * pace + (1 - w) * left!;
    }
    return { fraction: f, etaMs: eta === null ? null : Math.round(eta), startedAt: this.startedAt, at: now };
  }
}

/**
 * How far a run of steps is, each step weighed by how long it took last time (`weights`, ms; when there are as many),
 * else equally: the steps before `step` (1-based) and `part` (0-1) of it.
 */
export function stepFraction(step: number, steps: number, part: number, weights?: number[] | null): number {
  if (steps <= 0) return 0;
  const w = weights && weights.length === steps && weights.every((x) => x >= 0) && weights.some((x) => x > 0) ? weights : null;
  const total = w ? w.reduce((s, x) => s + x, 0) : steps;
  let done = 0;
  for (let i = 0; i < Math.min(step - 1, steps); i++) done += w ? w[i] : 1;
  done += (w ? w[step - 1] ?? 0 : 1) * Math.min(1, Math.max(0, part));
  return done / total;
}
