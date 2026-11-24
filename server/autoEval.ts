// Evaluate builds by itself (Data tab). With Follow the evaluation on, the last evaluation decides patch memory (and,
// unless you chose one, the default build mode), so its verdict shouldn't go stale as games come in. What it runs is
// the plan's (model/evalBase.ts planEvaluation, 0.16.0, docs/EVAL-UPDATE.md): while the last full evaluation still
// describes the stored games, a quick update after AUTO_EVAL.newGames new games (or 3 % of the games the last one saw,
// whichever is more, as in 0.15.0), at most every hour; when the plan says the next one is full, a full evaluation once
// games of a new patch are stored or as many games came in since the last full one, at most every few hours. Either only
// after a quiet spell without champ select or a game. Champ select or a game stopping by meanwhile pauses it
// (cancelled: nothing changes); it starts over after the next quiet spell. A click on Evaluate builds while it runs
// makes it yours (server/jobs.ts), and then nothing stops it. With Follow the evaluation off, it runs only when you
// click. The plan is worked out here either way, every minute and after each evaluation: the Data tab shows it.
import type { AutoEvalStatus, EvaluationPlan } from '../shared/types.ts';
import type { Db } from './db.ts';
import { comparePatch } from './ddragon.ts';
import type { EvalSummary } from './model/service.ts';

export const AUTO_EVAL = {
  /**
   * New stored games that start the next evaluation: since the last one (full or quick) for a quick update, since the
   * last full one when the plan says full (0.16.0)...
   */
  newGames: 300,
  /**
   * ...or this share of the games that one saw, when that is more (0.15.0): 300 new games among 100,000 change nothing
   * it measures. A quick update reads every stored game, so it costs minutes too: one every 300 new games would cost
   * more than the full evaluations it saves (measured on 82,000 games, docs/EVAL-UPDATE.md).
   */
  newShare: 0.03,
  /** Between two full evaluations (yours or the app's) at least... */
  spacingMs: 6 * 3600_000,
  /** ...and between a quick update and the last evaluation, full or quick (0.16.0). */
  updateSpacingMs: 3600_000,
  /** No champ select or game for this long first (also after the app starts). */
  quietMs: 10 * 60_000,
  /** Automatic evaluations' reports kept next to the settings (the newest; yours are never deleted). */
  keepReports: 20,
};

/** What an evaluation saw: the newest stored game's row (null before 0.8.4), the number of games, the newest patch. */
export type EvalMark = NonNullable<EvalSummary['mark']>;
type MarkDb = Pick<Db, 'gameMark' | 'gamesAfterRow' | 'gameVersions'>;
type EvalPath = Exclude<EvaluationPlan['path'], 'none'>;

/** The newest patch among the stored games' builds, first seen up to `before`. */
export function newestPatch(versions: { patch: string; first_seen: number }[], before = Infinity): string | null {
  let best: string | null = null;
  for (const v of versions) if (v.first_seen <= before && (best === null || comparePatch(v.patch, best) > 0)) best = v.patch;
  return best;
}

/** The mark an evaluation records as it starts (model/runEval.ts). */
export function evalMark(db: MarkDb): EvalMark {
  const { row, games } = db.gameMark();
  return { row, games, patch: newestPatch(db.gameVersions()) };
}

/**
 * The last evaluation's mark. One saved before 0.8.4 has none: its game count is in its dataVersion
 * ("games:last:timelines:captured") and its patch is the newest one first seen before it ran.
 */
export function markOf(summary: EvalSummary | null, versions: { patch: string; first_seen: number }[]): EvalMark | null {
  if (!summary) return null;
  if (summary.mark) return summary.mark;
  const games = Number(summary.dataVersion?.split(':')[0]);
  return { row: null, games: Number.isFinite(games) ? games : 0, patch: newestPatch(versions, summary.evaluatedAt) };
}

/**
 * Games stored since `mark`: by row (archiving older games doesn't hide new ones), or by count when that says more
 * (Compact database renumbers the rows). Never evaluated: every stored game.
 */
export function newGamesSince(db: MarkDb, mark: EvalMark | null): number {
  const now = db.gameMark();
  if (!mark) return now.games;
  return Math.max(0, now.games - mark.games, mark.row !== null ? db.gamesAfterRow(mark.row) : 0);
}

export interface AutoEvalDeps {
  /** Follow the evaluation is on (patch memory not chosen by hand). */
  following(): boolean;
  /** No champ select or game now. */
  quiet(): boolean;
  /** The data folder is moving or compacting, or old patches are being archived. */
  busy(): boolean;
  summary(): EvalSummary | null;
  db(): MarkDb;
  /** What Evaluate builds would run now (model/evalBase.ts planEvaluation; 0.16.0). */
  plan(): EvaluationPlan;
  /** The evaluation running now, if any (`auto` when the app started it and nobody clicked since). */
  running(): { auto?: string } | null;
  /**
   * Starts an automatic evaluation that runs what the plan says as it starts (path 'auto'); `path`: what it says now,
   * for its estimate. Settles when it ends.
   */
  start(why: string, path: EvalPath): Promise<unknown>;
  /** Stops the running automatic evaluation (champ select or a game began). */
  pause(): Promise<void>;
  changed?(status: AutoEvalStatus): void;
  now?(): number;
  log?(line: string): void;
}

export class AutoEvaluation {
  private quietSince: number | null;
  /** The last automatic start (a paused one doesn't count): spacing applies from it too, after a failure, say. */
  private attemptedAt = 0;
  /** The last automatic start of a full evaluation: a full one's spacing applies from it (0.16.0). */
  private fullAttemptedAt = 0;
  private pausing = false;
  /** The running automatic evaluation was paused (champ select or a game): it doesn't count as an attempt. */
  private paused = false;
  private current: AutoEvalStatus;
  /** What Evaluate builds would run now (null: not worked out yet), and why the last try to work it out failed. */
  private planned: EvaluationPlan | null = null;
  private planError = '';
  private sent = '';
  private readonly now: () => number;

  constructor(private deps: AutoEvalDeps) {
    this.now = deps.now ?? Date.now;
    this.quietSince = this.now();
    this.current = this.off();
  }

  status(): AutoEvalStatus { return this.current; }

  /** What Evaluate builds would run now, Follow the evaluation on or off (the Data tab's; null: not worked out yet). */
  plan(): EvaluationPlan | null { return this.planned; }

  /** Called every minute, after each evaluation, and whenever champ select or a game begins or ends. */
  check() {
    const now = this.now();
    const quiet = this.deps.quiet();
    if (!quiet) this.quietSince = null;
    else this.quietSince ??= now;
    const running = this.deps.running();
    if (running?.auto && !quiet && !this.pausing) {
      this.pausing = true;
      this.paused = true;
      this.deps.log?.('champ select or a game began: the automatic evaluation stops and runs again later');
      void this.deps.pause().finally(() => { this.pausing = false; });
    }
    if (this.deps.busy()) {
      // The database may be swapping (the data folder moving) or archiving: nothing is read or started meanwhile.
      if (this.current.on && !running && this.due(this.current)) this.current = { ...this.current, waiting: 'busy', notBefore: null };
    } else {
      // While an evaluation runs, its base and summary are being written: the plan is worked out again once it ends.
      if (!running) this.readPlan();
      this.current = this.compute(now, running !== null);
      if (this.current.on && !running && this.due(this.current) && this.current.waiting === null) this.begin(now);
    }
    const json = JSON.stringify([this.current, this.planned]);
    if (json !== this.sent) { this.sent = json; this.deps.changed?.(this.current); }
  }

  /** The plan now; on a failure the last one stays (and the failure is logged once). */
  private readPlan() {
    try {
      this.planned = this.deps.plan();
      this.planError = '';
    } catch (err) {
      const message = err instanceof Error ? err.message : String(err);
      if (message !== this.planError) this.deps.log?.(`what Evaluate builds would run could not be worked out: ${message}`);
      this.planError = message;
    }
  }

  private off(): AutoEvalStatus {
    return {
      on: false, path: null, reason: null, newGames: 0, sinceFull: 0, needed: AUTO_EVAL.newGames, updateGames: AUTO_EVAL.newGames, newPatch: null,
      spacingHours: AUTO_EVAL.spacingMs / 3600_000, updateSpacingHours: AUTO_EVAL.updateSpacingMs / 3600_000,
      quietMinutes: AUTO_EVAL.quietMs / 60_000, waiting: null, notBefore: null,
    };
  }

  /**
   * A quick update: enough new games since the last evaluation; a full one: enough since the last full one. A new patch
   * either way. Up to date (nothing to add): never.
   */
  private due(s: AutoEvalStatus) {
    if (s.path === 'none') return false;
    return s.newPatch !== null || (s.path === 'update' ? s.newGames : s.sinceFull) >= s.needed;
  }

  private compute(now: number, running: boolean): AutoEvalStatus {
    if (!this.deps.following()) return this.off();
    const db = this.deps.db();
    const summary = this.deps.summary();
    const versions = db.gameVersions();
    const mark = markOf(summary, versions);
    const newest = newestPatch(versions);
    // The full evaluation a quick update rests on; any other evaluation was a full one (before 0.16.0, all were).
    const full = summary?.path === 'update' ? summary.full ?? null : null;
    const fullAt = full?.evaluatedAt ?? summary?.evaluatedAt ?? 0;
    const newGames = newGamesSince(db, mark);
    // No plan worked out yet: a full evaluation's rules, as before 0.16.0.
    const update = this.planned?.path === 'update';
    const s: AutoEvalStatus = {
      ...this.off(), on: true, path: this.planned?.path ?? null, reason: this.planned?.reason ?? null,
      newGames, sinceFull: full ? newGamesSince(db, full.mark) : newGames,
      updateGames: Math.max(AUTO_EVAL.newGames, Math.ceil(AUTO_EVAL.newShare * (mark?.games ?? 0))),
      // As many new games as 0.15 waited for (a quick update costs minutes, mostly reading every game: one every 300 new
      // games would cost more than the full evaluations it saves), since the last evaluation for a quick update and
      // since the last full one for a full one.
      needed: Math.max(AUTO_EVAL.newGames, Math.ceil(AUTO_EVAL.newShare * ((update ? mark : full?.mark ?? mark)?.games ?? 0))),
      newPatch: newest && mark?.patch && comparePatch(newest, mark.patch) > 0 ? newest : null,
    };
    if (running || !this.due(s)) return s;
    // A quick update an hour after the last evaluation (or try), a full one hours after the last full one (or try): a
    // quick update in between doesn't put a full one off.
    const notBefore = update
      ? Math.max(summary?.evaluatedAt ?? 0, this.attemptedAt) + AUTO_EVAL.updateSpacingMs
      : Math.max(fullAt, this.fullAttemptedAt) + AUTO_EVAL.spacingMs;
    if (now < notBefore) return { ...s, waiting: 'spacing', notBefore };
    if (this.quietSince === null || now - this.quietSince < AUTO_EVAL.quietMs) {
      return { ...s, waiting: 'quiet', notBefore: this.quietSince === null ? null : this.quietSince + AUTO_EVAL.quietMs };
    }
    return s;
  }

  private begin(now: number) {
    const s = this.current;
    const path: EvalPath = s.path === 'update' ? 'update' : 'full';
    const count = (n: number) => n.toLocaleString('en-US');
    const why = !this.deps.summary() ? 'first evaluation'
      : s.newPatch ? `new patch ${s.newPatch}`
        : path === 'update' || s.sinceFull === s.newGames ? `${count(path === 'update' ? s.newGames : s.sinceFull)} new games`
          : `${count(s.sinceFull)} games since the last full evaluation`;
    const before = { at: this.attemptedAt, full: this.fullAttemptedAt };
    this.attemptedAt = now;
    if (path === 'full') this.fullAttemptedAt = now;
    this.paused = false;
    this.deps.log?.(`starting ${path === 'update' ? 'a quick update' : 'a full evaluation'} (${why})`);
    this.deps.start(why, path).then(
      () => this.deps.log?.('finished'),
      (err: unknown) => {
        // Paused by champ select or a game: it hasn't run, so the spacing doesn't start from it.
        if (this.paused) {
          this.attemptedAt = before.at;
          this.fullAttemptedAt = before.full;
        } else this.deps.log?.(`stopped: ${err instanceof Error ? err.message : String(err)}`);
      },
    ).finally(() => { this.paused = false; this.check(); });
  }
}
