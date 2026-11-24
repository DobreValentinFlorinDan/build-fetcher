// Quick updates between full evaluations (0.16.0, docs/EVAL-UPDATE.md). A full evaluation saves its base next to its
// report: every held-out game's outcomes, the settings it chose and which games it held out (a hash of each match ID
// and of what the game held). A quick update adds the new held-out games' outcomes to it and decides every gate again.
// planEvaluation says which one Evaluate builds runs now. Nothing here is written to the data folder.
import crypto from 'node:crypto';
import fs from 'node:fs';
import path from 'node:path';
import v8 from 'node:v8';
import { CAPTURE_VERSION } from '../capture.ts';
import { config } from '../config.ts';
import { comparePatch, type StaticData } from '../ddragon.ts';
import type { Db } from '../db.ts';
import { regionPatches } from '../patches.ts';
import { DEFAULT_SCOPE, loadScope, type ScrapeScope } from '../scrapeScope.ts';
import { EVAL_DEFAULTS, type EvalBaseData, type EvalOptions, type EvalReport, type EvalUpdate } from './evaluate.ts';
import { evalDir, modelPatches, readEvaluation, type EvalSummary } from './service.ts';
import { WEIGHT_DEFAULTS } from './weights.ts';

/**
 * How evaluate.ts measures: a base saved by another version of it can't be added to. Bump when that changes. 2: late-
 * game swaps (0.17.0), whose outcomes a base of 1 lacks.
 */
export const EVAL_LOGIC = 2;

/**
 * When a quick update is no longer enough (docs/EVAL-UPDATE.md):
 *  - `maxGrowth`: stored games since the last full evaluation over this share of the games it held out;
 *  - `currentGrowth`: games of the current patch since then over this share of those it had. The models lean on the
 *    current patch, so its new games move every measurement, not just their own: measured on the user's games (16.21,
 *    docs/EVAL-UPDATE.md), 1,000 new current-patch games on 194 left a quick update 0.19 pts from the full evaluation.
 *    Early in a patch this keeps every evaluation full, as in 0.15; quick updates come once the patch has games enough;
 *  - `maxAgeDays`: the last full one older than this;
 *  - `maxMissing`: over this share of its held-out games no longer held out, or stored since in another form.
 * And when one would change a verdict by more than `margin` of its interval's half-width (verdictShift).
 */
export const UPDATE_RULES = { maxGrowth: 0.2, currentGrowth: 0.3, maxAgeDays: 7, maxMissing: 0.02, margin: 0.25 };

export type EvalPath = 'full' | 'update';

/** What Evaluate builds would run now, and why (the Data tab shows it). */
export interface EvalPlan {
  path: EvalPath | 'none';
  /** Why: "new patch 25.21", "1,240 new games since the full evaluation of 21 Oct", "up to date". */
  reason: string;
  /** Stored games since the last evaluation (full or quick), and since the last full one. */
  newGames: number;
  sinceFull: number;
  /** The last full evaluation (null: none). */
  fullAt: number | null;
}

type Mark = NonNullable<EvalSummary['mark']>;
type Regions = [string, { patch: string; previousPatch: string }][];

/** The base's header (base.json); its outcomes are in base.bin. */
export interface BaseHeader {
  logic: number;
  /** The full evaluation it rests on: when, what it saw, how many games it held out. */
  fullAt: number;
  fullMark: Mark;
  fullHeldOut: number;
  /** The newest patch then and its stored games (every source): the current patch's growth since. */
  fullPatch: string | null;
  fullCurrent: number;
  /** The last evaluation added to it (the full one, or the last quick update), and the held-out games it holds. */
  lastAt: number;
  lastMark: Mark;
  known: number;
  /** The patches it read, each region's current and previous patch, and what its settings were (currentKey). */
  patches: string[];
  regions: Regions;
  key: string;
  /** The key without the data folder (0.21.0, portableKey): what another computer checks before it takes this base. */
  portable?: string;
  /** Taken from another computer's backup (0.21.0, model/evalTravel.ts): which, and when that backup was made. */
  importedFrom?: { machine: string; exportedAt: number };
}

/**
 * What an evaluation reads: the two-patch model's patches, with patch memory evaluated those of the older patches in
 * its window too, of the draft and build sources and your own games.
 */
export function evalReads(db: Db, data: StaticData, quick = false) {
  const scope = (() => { try { return loadScope(); } catch { return DEFAULT_SCOPE; } })();
  const memory = quick ? null : EVAL_DEFAULTS.memory;
  const patches = modelPatches(db, data);
  const read = memory ? modelPatches(db, data, memory.patches) : patches;
  const sources = [...new Set([...scope.draftSources, ...scope.buildSources, 'me'])];
  return { scope, patches, read, sources };
}

/** The evaluation's options for this scope (scrape.config.json's sources and weights); `quick`: --quick's. */
export function evalOptions(scope: ScrapeScope, quick = false): EvalOptions {
  const q = quick ? { bandwidths: [0.7], ks: [100], lambdas: [1], live: false, bootstrap: 300, memory: null, scoring: false, tuning: null, swaps: false } : {};
  return {
    ...EVAL_DEFAULTS, engine: config, buildSources: scope.buildSources,
    weights: { ...WEIGHT_DEFAULTS, regionWeights: scope.regionWeights, ownRegionBoost: scope.ownRegionBoost, patchWeights: scope.patchWeights, hotfixes: scope.hotfixes },
    ...q,
  };
}

/**
 * The app's version: the desktop build's (scripts/bundle.mjs sets it), npm's, else this checkout's package.json. A
 * new version may measure differently, so its first evaluation is a full one.
 */
export function appVersion(): string {
  const set = process.env.IRELIA_APP_VERSION ?? process.env.npm_package_version;
  if (set) return set;
  try { return (JSON.parse(fs.readFileSync(path.resolve('package.json'), 'utf8')) as { version?: string }).version ?? 'unknown'; } catch { return 'unknown'; }
}

/**
 * Everything an evaluation's measurements depend on but the games, as one string: the options (every setting tried
 * and the weights), the sources, the lane baseline's engine settings, the app's version, Data Dragon's, how games are
 * captured, and the data folder. A base measured under other ones can't be added to.
 */
export function evalKey(opts: EvalOptions, sources: string[], data: Pick<StaticData, 'version'>): string {
  return keyOf(opts, sources, data, true);
}

/**
 * The same settings key without the data folder (0.21.0): equal on two computers with the same app, Data Dragon and
 * settings, so an evaluation that came with a backup can be checked against this computer's (model/evalTravel.ts).
 */
export function portableKey(opts: EvalOptions, sources: string[], data: Pick<StaticData, 'version'>): string {
  return keyOf(opts, sources, data, false);
}

function keyOf(opts: EvalOptions, sources: string[], data: Pick<StaticData, 'version'>, withFolder: boolean): string {
  const { engine: e, update: _, ...rest } = opts;
  const engine = {
    minMatchupGames: e.minMatchupGames, generalPriorWeight: e.generalPriorWeight, minLaneGames: e.minLaneGames, otherLaneWeight: e.otherLaneWeight,
    previousPatchWeight: e.previousPatchWeight, minCurrentPatchGames: e.minCurrentPatchGames, startingItemWindowMs: e.startingItemWindowMs,
  };
  const parts = {
    logic: EVAL_LOGIC, app: appVersion(), ddragon: data.version, capture: CAPTURE_VERSION, ...(withFolder ? { dataDir: path.resolve(config.dataDir) } : {}),
    options: rest, engine, sources: [...sources].sort(),
  };
  return crypto.createHash('sha256').update(JSON.stringify(parts)).digest('hex').slice(0, 24);
}

/** The settings key an evaluation started now would have. */
export function currentKey(db: Db, data: StaticData): string {
  const { scope, sources } = evalReads(db, data);
  return evalKey(evalOptions(scope), sources, data);
}

/** ...and without the data folder (portableKey). */
export function currentPortableKey(db: Db, data: StaticData): string {
  const { scope, sources } = evalReads(db, data);
  return portableKey(evalOptions(scope), sources, data);
}

const headerFile = () => path.join(evalDir(), 'base.json');
export const baseFile = () => path.join(evalDir(), 'base.bin');

export function readBaseHeader(): BaseHeader | null {
  try {
    const h = JSON.parse(fs.readFileSync(headerFile(), 'utf8')) as BaseHeader;
    return h.logic === EVAL_LOGIC && Array.isArray(h.regions) && fs.existsSync(baseFile()) ? h : null;
  } catch { return null; }
}

/**
 * Saves `data` as the base, over the last one: its outcomes first (through a temporary file), then the header. A base
 * whose outcomes and header don't match (a save cut short) is never read (readBase).
 */
export function saveBase(data: EvalBaseData, header: Omit<BaseHeader, 'logic' | 'known'>) {
  fs.mkdirSync(evalDir(), { recursive: true });
  const tmp = `${baseFile()}.tmp`;
  fs.writeFileSync(tmp, v8.serialize(data));
  fs.renameSync(tmp, baseFile());
  fs.writeFileSync(headerFile(), `${JSON.stringify({ logic: EVAL_LOGIC, ...header, known: data.known.length }, null, 2)}\n`);
}

/** The base, when one is saved by this version and its outcomes are its header's. */
export function readBase(): { header: BaseHeader; data: EvalBaseData } | null {
  const header = readBaseHeader();
  if (!header) return null;
  try {
    const data = v8.deserialize(fs.readFileSync(baseFile())) as EvalBaseData;
    const whole = data?.report?.evaluatedAt === header.lastAt && data.known instanceof Float64Array && data.known.length === header.known
      && data.prints instanceof Float64Array && data.prints.length === data.known.length;
    return whole ? { header, data } : null;
  } catch { return null; }
}

/** What a quick update takes from `data` (evaluate.ts EvalOptions.update); `maxGridGames`: the evaluation's grid size. */
export function updateFrom(data: EvalBaseData, header: BaseHeader, maxGridGames: number): EvalUpdate {
  const r = data.report;
  return {
    chosen: { ...r.chosen }, lambda: r.state?.lambda ?? 1,
    decay: r.memory?.grid.length ? r.memory.decay : null, tuning: r.tuning?.full ? r.tuning.choice : null,
    salt: data.salt, known: data.known, prints: data.prints, penalties: data.scoring?.penalties ?? null,
    gridStride: Math.max(1, Math.ceil(header.fullHeldOut / maxGridGames)),
  };
}

/** An interval's low end clear of zero by `margin` of its half-width, on the side `passed` says. */
const clear = (x: { low: number; high: number }, passed: boolean) => {
  const m = UPDATE_RULES.margin * ((x.high - x.low) / 2);
  return passed ? x.low > m : x.low < -m;
};

/**
 * The verdicts a quick update would decide otherwise than its base. `decisive`: the first that the new games move
 * clearly across the line (its interval clear of zero by UPDATE_RULES.margin of its half-width; patch memory passes on
 * either of its two): a full evaluation decides it. `close`: those they only move onto it (or move by their Brier score
 * alone): the base's verdict stands until the next full evaluation, so a verdict on the line doesn't send every quick
 * update on to a full one.
 */
export function verdictShift(before: EvalReport, after: EvalReport): { decisive: string | null; close: string[] } {
  const gates: { name: string; was: string | null; now: string | null; intervals: { low: number; high: number }[]; any: boolean }[] = [
    { name: 'the draft-adjusted builds', was: before.gate, now: after.gate, intervals: [after.lift], any: false },
    { name: 'patch memory', was: before.memory?.gate ?? null, now: after.memory?.gate ?? null, intervals: after.memory?.grid.length ? [after.memory.full, after.memory.early] : [], any: true },
    { name: 'runes and summoners against the draft', was: before.scoring?.gate ?? null, now: after.scoring?.gate ?? null, intervals: after.scoring?.decisions.length ? [after.scoring.adjusted] : [], any: false },
    { name: 'fine-tuning', was: before.tuning?.gate ?? null, now: after.tuning?.gate ?? null, intervals: after.tuning?.full ? [after.tuning.full] : [], any: false },
    { name: 'late-game swaps', was: before.swaps?.gate ?? null, now: after.swaps?.gate ?? null, intervals: after.swaps ? [after.swaps.lift] : [], any: false },
  ];
  const close: string[] = [];
  for (const g of gates) {
    if (g.was === g.now) continue;
    const passed = g.now === 'passed';
    const finite = g.intervals.filter((x) => Number.isFinite(x.low) && Number.isFinite(x.high));
    // Passing needs one clear interval where either may pass it (patch memory), else every one; failing, every one.
    const decisive = finite.length > 0 && (passed && g.any ? finite.some((x) => clear(x, true)) : finite.every((x) => clear(x, passed)));
    if (decisive) return { decisive: g.name, close };
    close.push(g.name);
  }
  return { decisive: null, close };
}

/** `after` with `before`'s verdict on the gates named (verdictShift's `close`), each saying so. */
export function keepVerdicts(before: EvalReport, after: EvalReport, names: string[]): EvalReport {
  const note = (reason: string, was: string) => `${reason} (the new games moved it too little to change the full evaluation's ${was.toUpperCase()}: it stands)`;
  const r: EvalReport = structuredClone(after);
  if (names.includes('the draft-adjusted builds')) { r.gateReason = note(r.gateReason, before.gate); r.gate = before.gate; }
  if (names.includes('patch memory') && r.memory && before.memory) { r.memory.gateReason = note(r.memory.gateReason, before.memory.gate); r.memory.gate = before.memory.gate; }
  if (names.includes('runes and summoners against the draft') && r.scoring && before.scoring) {
    r.scoring.gateReason = note(r.scoring.gateReason, before.scoring.gate); r.scoring.gate = before.scoring.gate;
  }
  if (names.includes('fine-tuning') && r.tuning && before.tuning) { r.tuning.gateReason = note(r.tuning.gateReason, before.tuning.gate); r.tuning.gate = before.tuning.gate; }
  if (names.includes('late-game swaps') && r.swaps && before.swaps) { r.swaps.gateReason = note(r.swaps.gateReason, before.swaps.gate); r.swaps.gate = before.swaps.gate; }
  return r;
}

/** Stored games since `mark`, as server/autoEval.ts counts them (by row, or by count when that says more). */
function gamesSince(db: Pick<Db, 'gameMark' | 'gamesAfterRow'>, mark: Mark): number {
  const now = db.gameMark();
  return Math.max(0, now.games - mark.games, mark.row !== null ? db.gamesAfterRow(mark.row) : 0);
}

const day = (ms: number) => new Date(ms).toLocaleDateString('en-GB', { day: 'numeric', month: 'short' });
const count = (n: number) => n.toLocaleString('en-US');

/** Each region's current and previous patch now, sorted: compared with the base's. */
export function regionsNow(db: Pick<Db, 'gameVersions'>, data: StaticData): Regions {
  return [...regionPatches(db.gameVersions(), data)].sort((a, b) => (a[0] < b[0] ? -1 : a[0] > b[0] ? 1 : 0));
}

/**
 * What Evaluate builds runs now: a quick update while the last full evaluation still describes the stored games, else
 * a full one (docs/EVAL-UPDATE.md). `header`, `summary`, `key`: the base's, latest.json, currentKey (read when unset).
 */
export function planEvaluation(db: Db, data: StaticData, now = Date.now(), o: {
  key?: string; header?: BaseHeader | null; summary?: EvalSummary | null;
} = {}): EvalPlan {
  const header = o.header !== undefined ? o.header : readBaseHeader();
  const summary = o.summary !== undefined ? o.summary : readEvaluation();
  const lastMark = summary?.mark ?? null;
  const total = db.gameMark().games;
  const newGames = lastMark ? gamesSince(db, lastMark) : total;
  const full = (reason: string, sinceFull = newGames, fullAt: number | null = header?.fullAt ?? null): EvalPlan =>
    ({ path: 'full', reason, newGames, sinceFull, fullAt });
  if (!summary) return full('no evaluation yet', total, null);
  if (!header) return full('no full evaluation saved by this version yet');
  const sinceFull = gamesSince(db, header.fullMark);
  // The base is the last evaluation's: one run meanwhile without saving a base (a --quick check) leaves it stale.
  if (summary.evaluatedAt !== header.lastAt) return full('the last evaluation saved nothing to add to', sinceFull);
  const { read, sources } = evalReads(db, data);
  const newer = read.filter((p) => !header.patches.includes(p)).sort((a, b) => comparePatch(b, a));
  if (newer.length) return full(`new patch ${newer[0]}`, sinceFull);
  if (read.length !== header.patches.length) return full('the patches an evaluation reads changed', sinceFull);
  // A region moving to a patch already read (Data Dragon's, say) changes every one of its games' patch weights.
  const regions = regionsNow(db, data);
  const before = new Map(header.regions);
  const moved = regions.find(([platform, r]) => before.get(platform)?.patch !== r.patch || before.get(platform)?.previousPatch !== r.previousPatch);
  if (moved || regions.length !== header.regions.length) {
    return full(moved ? `${moved[0]} is on patch ${moved[1].patch} since the full evaluation` : 'the regions stored changed', sinceFull);
  }
  if ((o.key ?? currentKey(db, data)) !== header.key) return full('the app, Data Dragon, the data folder or settings the evaluation depends on changed', sinceFull);
  if (sinceFull > UPDATE_RULES.maxGrowth * header.fullHeldOut) {
    return full(`${count(sinceFull)} games since the full evaluation of ${day(header.fullAt)} (over ${Math.round(UPDATE_RULES.maxGrowth * 100)} % more)`, sinceFull);
  }
  if (header.fullPatch) {
    const current = db.modelGameCount([header.fullPatch], sources) - header.fullCurrent;
    if (current > UPDATE_RULES.currentGrowth * header.fullCurrent) {
      return full(`${count(current)} more games of patch ${header.fullPatch} since the full evaluation of ${day(header.fullAt)}, which had ${count(header.fullCurrent)}`, sinceFull);
    }
  }
  if (now - header.fullAt > UPDATE_RULES.maxAgeDays * 86_400_000) {
    return full(`the last full evaluation is over ${UPDATE_RULES.maxAgeDays} days old (${day(header.fullAt)})`, sinceFull);
  }
  if (newGames === 0) return { path: 'none', reason: 'up to date: no new games since the last evaluation', newGames, sinceFull, fullAt: header.fullAt };
  return {
    path: 'update', newGames, sinceFull, fullAt: header.fullAt,
    reason: `${count(newGames)} new games since the last evaluation (full evaluation of ${day(header.fullAt)})`,
  };
}
