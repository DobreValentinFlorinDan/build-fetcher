// B6 npm run eval:builds: is the draft-aware model better than the lane baseline? Grouped cross-validation by pilot
// (no pilot's games in both training and test), offline, on the stored games. docs/BUILD-SOURCING.md §9.6.
//
//  - State model (B2): held-out Brier score, log loss and calibration, against always predicting the training win rate;
//    the L2 penalty is chosen by held-out log loss.
//  - Builds (B3): for each held-out game and decision, did the pilot do what the model recommends for that draft? The
//    lift of a model is the state-adjusted outcome (WPA) of games that followed it minus those that didn't, averaged
//    over decisions; the draft model is compared with the lane baseline on the same games, with a 95 % interval from
//    resampling pilots. The kernel bandwidth and the shrinkage strength are chosen on the same folds.
//  - Brier of each model's outcome prediction for the option the pilot actually took.
//  - Live stability: how often "Next" flips within one item slot while replaying held-out timelines minute by minute
//    (at most maxLiveGames of them, evenly spread).
//  - Yours: games where you followed the shown build against those where you didn't, once rec_log has 20 linked games.
//  - Patch memory: the draft model that also learns from older patches (model/weights.ts) against the one that reads
//    today's and the previous patch only, on the same held-out games: with all of today's games, and as early in a
//    patch (a quarter of the current patch's training games). The fade per patch is chosen on the same folds.
//
// Gate: the draft model becomes the default only if its lift over the baseline has a 95 % interval above zero and its
// Brier score is no worse. Patch memory turns on by itself only if it beats the two-patch model with 95 % confidence
// (with all games or early in a patch), is no worse with all games, and its Brier score is no worse either.
import crypto from 'node:crypto';
import v8 from 'node:v8';
import type { BuildRecommendation, Lane } from '../../shared/types.ts';
import type { RecLogRow } from '../db.ts';
import type { StaticData } from '../ddragon.ts';
import { buildExclusivityIndex, isPurchasableItem, type ExclusivityIndex } from '../engine/exclusivity.ts';
import { collectGarbage } from '../gc.ts';
import { recommend, type EngineSettings, type RegionPatchOf, type ValidatedRows } from '../engine/recommend.ts';
import { isCompletedItem } from '../engine/validator.ts';
import { contextsOf, standardise, type Contexts } from './context.ts';
import { frameSlabs, gameTimeline, momentsAt, prepareGames, stateRows, type ModelGameRow, type ParticipantLite, type PreparedGame, type ScoredGame } from './dataset.ts';
import {
  adviseSwap, buildDraftModel, itemQueryOf, observed, recommendDraft, similarities, weighGames, type DraftModel, type DraftSettings, type Weighted,
} from './draft.ts';
import { estimateDecision, ESTIMATE_DEFAULTS, type EstimateOptions, type Observation } from './estimate.ts';
import { KEEP as SWAP_KEEP, SELL as SWAP_SELL } from './swaps.ts';
import { crossFitted, EXPECTED_DEFAULTS, expectedOf, fitExpected, type Penalty } from './expected.ts';
import { NextStability, STABILITY_DEFAULTS } from './live.ts';
import { calibration, scoreProbabilities } from './logistic.ts';
import { ChampionProfiles, type ChampionTotals } from './profiles.ts';
import { STATE_FEATURES, trainStateModel, winProbabilityOf, type StateModel } from './state.ts';
import { solo, type Team } from './evalTeam.ts';
import { assignDraftGroups, autoGroups, type GroupingStats } from './groups.ts';
import { MEMORY_PATCHES, patchAger, WEIGHT_DEFAULTS, type WeightOptions } from './weights.ts';

export interface EvalOptions {
  folds: number;
  lambdas: number[];
  bandwidths: number[];
  ks: number[];
  bootstrap: number;
  seed: number;
  /** The lane baseline's sources (scope buildSources). */
  buildSources: string[];
  engine: EngineSettings;
  weights: WeightOptions;
  /** Replay held-out timelines for the live stability measure. */
  live: boolean;
  /** Fewer than this many held-out games: the gate fails for lack of data. */
  minGames: number;
  /** The bandwidth / shrinkage search uses at most this many held-out games (an even subsample); the final run all. */
  maxGridGames: number;
  /** The live stability replay uses at most this many held-out games (an even subsample). */
  maxLiveGames: number;
  /**
   * Patch memory against today's and the previous patch alone: its window in patches, the fades per patch tried, and
   * the games from older patches it needs to be judged. null: not evaluated.
   */
  memory: { patches: number; decays: number[]; minOlderGames: number } | null;
  /**
   * Runes, summoners, the start and the skill order scored against each game's expected win rate before it started
   * (model/expected.ts, 0.9.0), against today's scoring by the overall win rate. false: not evaluated.
   */
  scoring: boolean;
  /**
   * Fine-tuning (0.15.0) against the chosen draft model: pilot standing's strength, the item decisions' own prior (from
   * `ks`) and a fade within the current patch, each tried on the grid's subsample, the best of each together then
   * confirmed on every held-out game. null: not evaluated.
   */
  tuning: { pilotStrengths: number[]; recencyDays: number[] } | null;
  /** Late-game swaps (0.17.0, docs/SWAPS.md): the draft model's swap advice against keeping every item. */
  swaps: boolean;
  /**
   * A quick update (0.16.0, docs/EVAL-UPDATE.md): the base's settings, and which held-out games it measured. Only the
   * others are worked out; the grids and the live replay are skipped (EvalInput.base holds the rest). Unset: full.
   */
  update?: EvalUpdate;
  /**
   * Grouped drafts (0.20.0, model/groups.ts; docs/EVAL-SCALING.md): the stored games' drafts put into `groups` groups
   * (0: about 40 games each) and every comparison made per group. A trial, not a full evaluation's report: its numbers
   * are close to the exact ones, not the same. Unset: exact, as before.
   */
  grouping?: { groups: number } | null;
}

/** What a quick update takes from its base (model/evalBase.ts), in every thread of the team. */
export interface EvalUpdate {
  chosen: { h: number; k: number };
  lambda: number;
  /** Patch memory's fade as the base chose it (null: not compared). */
  decay: number | null;
  /** Fine-tuning's choice, when the base confirmed one on its held-out games (null: nothing to confirm). */
  tuning: TuningChoice | null;
  /** The salt of the base's hashes (baseHash), and those of the held-out games it measured (by match ID). */
  salt: string;
  known: Float64Array;
  /** Each of those games' fingerprint (gamePrint) when measured: a game rewritten since (backfilled, say) is counted. */
  prints: Float64Array;
  /** The expected win rate's penalty each fold chose (null: no model): kept, as the other settings are. */
  penalties: (Penalty | null)[] | null;
  /** One held-out game in so many: the grid's subsample the base compared early in a patch on. */
  gridStride: number;
}

export const EVAL_DEFAULTS: Omit<EvalOptions, 'engine'> = {
  folds: 5, lambdas: [0.3, 1, 3, 10], bandwidths: [0.5, 1, Infinity], ks: [30, 100, 300], bootstrap: 1000, seed: 1,
  buildSources: ['otp'], weights: WEIGHT_DEFAULTS, live: true, minGames: 200, maxGridGames: 10_000,
  maxLiveGames: 500, memory: { patches: MEMORY_PATCHES, decays: [0.5, 0.75, 0.9], minOlderGames: 200 }, scoring: true,
  tuning: { pilotStrengths: [0, 0.5, 2], recencyDays: [7, 21] }, swaps: true,
};

/** Where an evaluation is: step `step` of `steps`, doing `label`. */
/**
 * Where an evaluation is: step `step` of `steps`, doing `label`, and (0.10.0) `part` of that step done when its work can be
 * counted (held-out games compared, stored games read). Step 0: reading the stored games, before the first step.
 */
export interface EvalProgress { step: number; steps: number; label: string; part?: number }

/** How long one step of an evaluation took (0.20.0): its label as the progress showed it, in milliseconds. */
export interface StepTime { label: string; ms: number }

/**
 * How many progress steps an evaluation with these options reports: penalties, folds, final, interval, patch memory
 * (its folds, all games, early in a patch), scoring against the draft (the expected win rate, the comparison), replay.
 */
export const evalSteps = (o: Pick<EvalOptions, 'lambdas' | 'folds' | 'live' | 'memory' | 'scoring'> & Partial<Pick<EvalOptions, 'tuning' | 'update' | 'swaps'>>) =>
  (o.update
    // A quick update: one penalty, the new games, the interval, patch memory's two, scoring's two, fine-tuning's one.
    ? 3 + (o.memory ? 2 : 0) + (o.scoring ? 2 : 0) + (o.tuning ? 1 : 0)
    : o.lambdas.length + o.folds + 2 + (o.memory ? o.folds + 2 : 0) + (o.scoring ? 2 : 0) + (o.tuning ? o.folds + 1 : 0) + (o.live ? 1 : 0))
  // Late-game swaps (0.17.0): one.
  + (o.swaps ? 1 : 0);

export interface EvalInput {
  /**
   * The stored games, read once (an iterator over the database is enough). evaluate() drops their raw timelines (most
   * of their memory): with `timelinesOf`, each as soon as its game is prepared; without, all but the live replay's.
   */
  rows: Iterable<ModelGameRow>;
  /** How many `rows` there are, when known: the reading's progress. */
  total?: number;
  /** Reads the raw timelines of these games again (the live replay's sample), so none has to be kept meanwhile. */
  timelinesOf?: (matchIds: string[]) => Map<string, string>;
  /**
   * The patches the draft model reads without patch memory (service.ts modelPatches): everything but patch memory is
   * evaluated on their games only, as without it. Games of other patches within the memory window are what patch memory
   * adds. Unset: every game is one of them.
   */
  classicPatches?: string[];
  /** Every stored game's players. evaluate() empties the array once it has read it. */
  participants: ParticipantLite[];
  /**
   * Prepares the games on other threads (0.15.1, model/prepareTeam.ts): the same games in the order read, each row's
   * timeline dropped once it is sent (with `timelinesOf`). Unset: prepareGames, here.
   */
  prepare?: (rows: Iterable<ModelGameRow>, participants: ParticipantLite[], frames: (length: number) => Float32Array) => Promise<PreparedGame[]>;
  data: StaticData;
  excl: ExclusivityIndex;
  profiles: ChampionProfiles;
  regionPatch: RegionPatchOf;
  recLog?: RecLogRow[];
  startingItemWindowMs: number;
  /** Progress lines while it runs. */
  log?: (line: string) => void;
  /** The step it starts, before each one (evalSteps in all), and how far into it, every half second or so. */
  progress?: (p: EvalProgress) => void;
  /**
   * A full garbage collection (0.12.0), called before a fold's models are built when no step starts there: the last
   * fold's models are garbage by then, and the heap would otherwise grow to hold both.
   */
  collect?: () => void;
  /**
   * Share the comparisons out over a team of threads (0.11.0, model/evalTeam.ts): `start` gets their copy of what was
   * read and returns the team (or one thread alone). The other threads rebuild the champion profiles and each region's
   * patch from `profileRows` and `regions` (functions don't cross threads).
   */
  team?: {
    start: (snapshot: EvalSnapshot) => Promise<Team>;
    profileRows: { current: ChampionTotals[]; older: ChampionTotals[] };
    regions: Map<string, { patch: string; previousPatch: string }>;
  };
  /** A quick update's base (model/evalBase.ts): the outcomes the new games' are added to, and its last report. */
  base?: EvalBaseData;
  /** What was measured (with the base's, on a quick update), to be saved as the next base. Called once, here. */
  onBase?: (base: EvalBaseData) => void;
  /** A full evaluation's new base: the salt of its hashes (a quick update keeps its base's). */
  salt?: string;
  /** How long each step took (0.20.0), the first being the reading: called once, when the evaluation ends or fails. */
  onTimings?: (timings: StepTime[]) => void;
}

/** Decisions the gate compares. */
export const EVAL_DECISIONS = ['keystone', 'secondaryTree', 'summoners', 'start', 'maxFirst', 'core1', 'core2', 'core3', 'boots'] as const;
type DecisionKey = typeof EVAL_DECISIONS[number];
/** recommendDraft estimates only these for the evaluation (picksOf and the Brier sums read nothing else). */
const SCORED: ReadonlySet<string> = new Set(EVAL_DECISIONS);
/** The scored decisions an item prior changes (draft.ts: core items and boots); the others don't depend on them. */
const SCORED_ITEMS: ReadonlySet<string> = new Set(['core1', 'core2', 'core3', 'boots']);
/** The scored decisions made before the game: all that scoring against the expected win rate changes (0.9.0). */
const PREGAME: ReadonlySet<string> = new Set(['keystone', 'secondaryTree', 'summoners', 'start', 'maxFirst']);

export interface DecisionLift { key: string; draft: number; baseline: number; followedDraft: number; followedBaseline: number; n: number }

export interface EvalReport {
  evaluatedAt: number;
  games: number;
  pilots: number;
  folds: number;
  state: {
    lambda: number;
    brier: number; logLoss: number;
    baseBrier: number; baseLogLoss: number;
    frames: number;
    byLambda: { lambda: number; brier: number; logLoss: number }[];
    calibration: { from: number; to: number; n: number; predicted: number; observed: number }[];
  } | null;
  grid: { h: number; k: number; draft: number; baseline: number; delta: number }[];
  /** Grouped drafts (0.20.0): set on a trial with grouping on (EvalOptions.grouping), what the grouping made. */
  grouping?: GroupingStats;
  chosen: { h: number; k: number };
  decisions: DecisionLift[];
  lift: {
    draft: number; baseline: number; delta: number; low: number; high: number;
    /**
     * 0.15.0, reported only: the same lifts compared within each pilot (followed against not followed by the same
     * pilot), so a model isn't credited for the pilots who tend to follow it being better players.
     */
    withinPilot?: { draft: number; baseline: number; delta: number };
  };
  brier: { draft: number; baseline: number; n: number };
  stability: { games: number; minutes: number; flipsPerGame: number; slotChangesPerGame: number } | null;
  yours: { linked: number; followed: { games: number; wins: number }; other: { games: number; wins: number } } | null;
  /**
   * Patch memory against the two-patch draft model on the same held-out games (lifts as above, `classic` the two-patch
   * model's): with every training game (`full`), and early in a patch (`early`: a quarter of the current patch's
   * training games). `olderGames`: training games from before the previous patch, within the window.
   */
  memory: {
    patches: number;
    olderGames: number;
    decay: number;
    grid: { decay: number; memory: number; classic: number; delta: number }[];
    full: MemoryLift & { brier: number; classicBrier: number };
    early: MemoryLift & { currentGames: number; keptGames: number };
    gate: 'passed' | 'failed';
    gateReason: string;
  } | null;
  /** Pre-game decisions scored against the expected win rate, against today's scoring (0.9.0). */
  scoring: ScoringReport | null;
  /** Fine-tuning against the chosen draft model (0.15.0); missing in reports from before. */
  tuning?: TuningReport | null;
  /** Late-game swaps against keeping every item (0.17.0, docs/SWAPS.md); missing in reports from before. */
  swaps?: SwapReport | null;
  gate: 'passed' | 'failed';
  gateReason: string;
  /**
   * A quick update (0.16.0, docs/EVAL-UPDATE.md): the full evaluation of `fullAt` with `newGames` new held-out games
   * added, and why a quick update was enough. Missing: a full evaluation.
   */
  update?: {
    fullAt: number; newGames: number; reason: string;
    /** The base's held-out games no longer held out now, and those stored since in another form: their outcomes stand. */
    missing?: number; changed?: number;
    /** Verdicts the new games moved too little to change: the full evaluation's stand (model/evalBase.ts verdictShift). */
    close?: string[];
  };
}

/**
 * Pre-game decisions scored against each game's expected win rate before it started (`fair`), against today's scoring
 * by the overall win rate (`classic`), on the same held-out games. Both are measured by the same yardstick: how much
 * more often than its expected win rate a game that followed the pick was won (`adjusted`), so neither is credited for
 * the matchups its picks fall into; `raw` is the old yardstick (the result minus the overall win rate).
 */
export interface ScoringReport {
  /** The expected win rate on held-out games, against always predicting the training win rate. null: too few games. */
  expected: { brier: number; logLoss: number; baseBrier: number; baseLogLoss: number; games: number; penalty: Penalty | null } | null;
  adjusted: { fair: number; classic: number; delta: number; low: number; high: number };
  raw: { fair: number; classic: number; delta: number };
  /** Each model's chance to win for the option taken, against the result. */
  brier: { fair: number; classic: number; n: number };
  /** Per decision: both lifts by the fair yardstick, and how often the two picks differ. */
  decisions: { key: string; fair: number; classic: number; changed: number }[];
  gate: 'passed' | 'failed';
  gateReason: string;
}

/**
 * Late-game swaps (0.17.0): at the held-out games' late full-inventory shop visits, the outcomes (state-adjusted) of
 * those that did what the draft model advises for each held item (keep it, or sell it for another) against those that
 * didn't, compared with keeping every item; a pilot bootstrap interval. `situations`: the visits and items compared;
 * `swapped`: the share of them where the item was sold; `advised`: where the advice was to sell it.
 */
export interface SwapReport {
  lift: { advice: number; keep: number; delta: number; low: number; high: number };
  situations: number;
  games: number;
  swapped: number;
  advised: number;
  gate: 'passed' | 'failed';
  gateReason: string;
}

/** What fine-tuning chose (0.15.0): pilot standing's strength, the item decisions' prior and the fade within a patch. */
export interface TuningChoice { pilotStrength: number; itemK: number | null; recencyDays: number | null }
export const UNTUNED: TuningChoice = { pilotStrength: 1, itemK: null, recencyDays: null };

/**
 * Fine-tuning against the chosen draft model, on the same held-out games: each setting tried alone on the grid's
 * subsample (`grid`), the best of each that did better together on every held-out game (`full`, lifts as above,
 * `current` the chosen model's), with its Brier against the chosen model's.
 */
export interface TuningReport {
  grid: { setting: keyof TuningChoice; value: number; tuned: number; current: number; delta: number }[];
  choice: TuningChoice;
  full: { tuned: number; current: number; delta: number; low: number; high: number; brier: number; currentBrier: number } | null;
  gate: 'passed' | 'failed';
  gateReason: string;
}

/**
 * The lifts compared within each pilot (0.15.0): per decision and pilot, the mean WPA of the games that followed a
 * model's pick minus those that didn't, weighted by n₁n₀/(n₁+n₀); pilots with no game on either side say nothing.
 */
export function withinPilot(outcomes: Outcome[]) {
  const cells = new Map<string, { d: number[]; b: number[] }>();
  for (const o of outcomes) {
    const id = `${o.key}\u0000${o.pilot}`;
    let c = cells.get(id);
    if (!c) { c = { d: [0, 0, 0, 0], b: [0, 0, 0, 0] }; cells.set(id, c); }
    if (o.followedDraft) { c.d[0] += o.wpa; c.d[1]++; } else { c.d[2] += o.wpa; c.d[3]++; }
    if (o.followedBaseline) { c.b[0] += o.wpa; c.b[1]++; } else { c.b[2] += o.wpa; c.b[3]++; }
  }
  const sums = new Map<string, { d: [number, number]; b: [number, number] }>();
  for (const [id, c] of cells) {
    const key = id.slice(0, id.indexOf('\u0000'));
    const s = sums.get(key) ?? { d: [0, 0], b: [0, 0] };
    const add = (a: number[], into: [number, number]) => {
      if (!a[1] || !a[3]) return;
      const w = (a[1] * a[3]) / (a[1] + a[3]);
      into[0] += w * (a[0] / a[1] - a[2] / a[3]); into[1] += w;
    };
    add(c.d, s.d); add(c.b, s.b);
    sums.set(key, s);
  }
  const keys = EVAL_DECISIONS.filter((k) => sums.has(k));
  const mean = (f: (s: { d: [number, number]; b: [number, number] }) => [number, number]) =>
    keys.length ? keys.reduce((t, k) => { const [x, w] = f(sums.get(k)!); return t + (w ? x / w : 0); }, 0) / keys.length : 0;
  const draft = mean((s) => s.d), baseline = mean((s) => s.b);
  return { draft, baseline, delta: draft - baseline };
}

/** Patch memory's lift against the two-patch model's, with the 95 % interval of the difference. */
export interface MemoryLift { memory: number; classic: number; delta: number; low: number; high: number }

/** FNV-1a: a pilot's fold, stable across runs. */
function hash(s: string) {
  let h = 0x811c9dc5;
  for (let i = 0; i < s.length; i++) { h ^= s.charCodeAt(i); h = Math.imul(h, 0x01000193) >>> 0; }
  return h;
}

/** What a build (either model's, in the baseline's shape) recommends for each decision. */
export function picksOf(b: BuildRecommendation): Partial<Record<DecisionKey, string>> {
  const out: Partial<Record<DecisionKey, string>> = {};
  if (b.runes) { out.keystone = String(b.runes.value.perks[0]); out.secondaryTree = String(b.runes.value.subStyle); }
  if (b.summoners) out.summoners = [...b.summoners.value].sort((x, y) => x - y).join(',');
  if (b.startingItems[0]) out.start = b.startingItems[0].value.join(',');
  if (b.skills) out.maxFirst = b.skills.value.priority[0];
  const core = b.coreBuild[0]?.value ?? [];
  core.slice(0, 3).forEach((id, i) => { out[`core${i + 1}` as DecisionKey] = String(id); });
  if (b.boots[0]) out.boots = String(b.boots[0].value);
  return out;
}

/** What the pilot did, and the WPA of it. */
function actual(s: ScoredGame): Partial<Record<DecisionKey, { option: string; wpa: number }>> {
  const g = s.g;
  const out: Partial<Record<DecisionKey, { option: string; wpa: number }>> = {
    keystone: { option: String(g.perks[0]), wpa: s.wpa0 },
    secondaryTree: { option: String(g.sub), wpa: s.wpa0 },
    summoners: { option: g.summoners, wpa: s.wpa0 },
  };
  if (g.start) out.start = { option: g.start, wpa: s.wpa0 };
  if (g.maxFirst) out.maxFirst = { option: g.maxFirst, wpa: s.wpa0 };
  g.items.slice(0, 3).forEach((c, i) => { out[`core${i + 1}` as DecisionKey] = { option: String(c.item), wpa: s.itemWpa[i] }; });
  if (g.boots && s.bootsWpa !== null) out.boots = { option: String(g.boots.item), wpa: s.bootsWpa };
  return out;
}

/** `w`: the outcome's weight (late-game swaps: its share of the game's decision points); unset counts it once. */
interface Outcome { pilot: string; key: DecisionKey | 'swap'; wpa: number; followedDraft: boolean; followedBaseline: boolean; w?: number }
/** Late-game swaps' outcomes (0.17.0): one decision, every held item's together. */
const SWAP_KEYS = ['swap'] as const;
type Picks = Partial<Record<DecisionKey, string>>;
/** Decisions whose outcome prediction is scored (Brier). */
const BRIER_KEYS: readonly string[] = ['keystone', 'core1', 'core2', 'core3', 'boots'];

/** Outcomes by decision, in order, with pilots as indices: the bootstrap reweights pilots a thousand times. */
export interface GroupedOutcomes {
  /** Pilots in the order first seen. */
  pilots: string[];
  /** Per decision (`keys`); `w`: each outcome's weight, when they have one (unset: each counts once). */
  byKey: { pilot: Int32Array; wpa: Float64Array; draft: Uint8Array; baseline: Uint8Array; w?: Float64Array }[];
  /** The decisions, when not the gate's (EVAL_DECISIONS): late-game swaps' (SWAP_KEYS). */
  keys?: readonly string[];
}

export function groupOutcomes(outcomes: Outcome[], keys: readonly string[] = EVAL_DECISIONS): GroupedOutcomes {
  const at = new Map<string, number>();
  const pilots: string[] = [];
  for (const o of outcomes) if (!at.has(o.pilot)) { at.set(o.pilot, pilots.length); pilots.push(o.pilot); }
  const byKey = keys.map((key) => {
    const of = outcomes.filter((o) => o.key === key);
    return {
      pilot: Int32Array.from(of, (o) => at.get(o.pilot)!), wpa: Float64Array.from(of, (o) => o.wpa),
      draft: Uint8Array.from(of, (o) => +o.followedDraft), baseline: Uint8Array.from(of, (o) => +o.followedBaseline),
      ...(of.some((o) => o.w !== undefined) ? { w: Float64Array.from(of, (o) => o.w ?? 1) } : {}),
    };
  });
  return { pilots, byKey, ...(keys !== EVAL_DECISIONS ? { keys } : {}) };
}

/**
 * What a full evaluation measured, or a quick update with its base's added: saved as the next base (model/evalBase.ts).
 * Pilots are hashes (baseHash), never IDs. `known`: the held-out games measured, hashed.
 */
export interface EvalBaseData {
  salt: string;
  known: Float64Array;
  /** Each known game's fingerprint (gamePrint), in the same order. */
  prints: Float64Array;
  report: EvalReport;
  final: { g: GroupedOutcomes; brier: { d: number; b: number; n: number } };
  memory: { full: GroupedOutcomes; brier: { d: number; n: number }; early: GroupedOutcomes } | null;
  scoring: {
    adjusted: GroupedOutcomes; raw: GroupedOutcomes; brier: { fair: number; classic: number; n: number };
    changed: [string, { n: number; changed: number }][]; expected: { p: Float64Array; y: Float64Array; pb: Float64Array };
    /** Each fold's penalty for the expected win rate. */
    penalties: (Penalty | null)[];
  } | null;
  tuning: { full: GroupedOutcomes; brier: { d: number; n: number } } | null;
  /** Late-game swaps' outcomes and counts (0.17.0). */
  swaps?: { g: GroupedOutcomes; situations: number; games: number; swapped: number; advised: number } | null;
}

/** A 48-bit hash of `s` with `salt`: how a base keeps match and pilot IDs (it never holds the IDs themselves). */
export function baseHash(salt: string, s: string): number {
  return crypto.createHash('sha256').update(salt).update(s).digest().readUIntBE(0, 6);
}

/**
 * What a game's measurement depends on, hashed: when it differs from the base's, the stored game changed since (a
 * backfill rewrote its timeline or lane opponent, say).
 */
export function gamePrint(salt: string, g: PreparedGame): number {
  return baseHash(salt, JSON.stringify([
    g.platform, g.patch, g.created, g.pilot, g.source, g.tier, g.lp, g.mastery, g.lane, g.win, g.opponent, g.enemies, g.allies,
    g.primary, g.sub, g.perks, g.shards, g.summoners, g.start, g.maxFirst, g.early, g.items.map((c) => [c.item, c.t]),
    g.boots ? [g.boots.item, g.boots.t] : null, g.duration, g.lane10, g.lane15,
  ]));
}

/**
 * `base`'s outcomes, then `more`'s, as groupOutcomes groups them one after the other: a pilot keeps its index, one
 * first seen in `more` comes after. `more`'s pilots are named as the base's are (`pilotKey`).
 */
export function mergeOutcomes(base: GroupedOutcomes, more: GroupedOutcomes, pilotKey: (p: string) => string): GroupedOutcomes {
  const at = new Map(base.pilots.map((p, i) => [p, i]));
  const pilots = [...base.pilots];
  const index = Int32Array.from(more.pilots, (p) => {
    const key = pilotKey(p);
    let i = at.get(key);
    if (i === undefined) { i = pilots.length; at.set(key, i); pilots.push(key); }
    return i;
  });
  const join = <T extends Int32Array | Float64Array | Uint8Array>(a: T, b: ArrayLike<number>, out: T): T => { out.set(a); out.set(b, a.length); return out; };
  const byKey = base.byKey.map((b, d) => {
    const m = more.byKey[d], n = b.pilot.length + m.pilot.length;
    const ones = (k: number) => new Float64Array(k).fill(1);
    return {
      pilot: join(b.pilot, Int32Array.from(m.pilot, (p) => index[p]), new Int32Array(n)), wpa: join(b.wpa, m.wpa, new Float64Array(n)),
      draft: join(b.draft, m.draft, new Uint8Array(n)), baseline: join(b.baseline, m.baseline, new Uint8Array(n)),
      ...(b.w || m.w ? { w: join(b.w ?? ones(b.pilot.length), m.w ?? ones(m.pilot.length), new Float64Array(n)) } : {}),
    };
  });
  return { pilots, byKey, ...(base.keys ? { keys: base.keys } : {}) };
}

/** withinPilot() from grouped outcomes: the same sums in the same order, so the same numbers. */
export function withinPilotOf(g: GroupedOutcomes) {
  const sums = new Map<string, { d: [number, number]; b: [number, number] }>();
  const add = (a: number[], into: [number, number]) => {
    if (!a[1] || !a[3]) return;
    const w = (a[1] * a[3]) / (a[1] + a[3]);
    into[0] += w * (a[0] / a[1] - a[2] / a[3]); into[1] += w;
  };
  EVAL_DECISIONS.forEach((key, d) => {
    const { pilot, wpa, draft, baseline } = g.byKey[d];
    if (!pilot.length) return;
    const cells = new Map<number, { d: number[]; b: number[] }>();
    for (let j = 0; j < pilot.length; j++) {
      let c = cells.get(pilot[j]);
      if (!c) { c = { d: [0, 0, 0, 0], b: [0, 0, 0, 0] }; cells.set(pilot[j], c); }
      if (draft[j]) { c.d[0] += wpa[j]; c.d[1]++; } else { c.d[2] += wpa[j]; c.d[3]++; }
      if (baseline[j]) { c.b[0] += wpa[j]; c.b[1]++; } else { c.b[2] += wpa[j]; c.b[3]++; }
    }
    const s = { d: [0, 0] as [number, number], b: [0, 0] as [number, number] };
    for (const c of cells.values()) { add(c.d, s.d); add(c.b, s.b); }
    sums.set(key, s);
  });
  const keys = EVAL_DECISIONS.filter((k) => sums.has(k));
  const mean = (f: (s: { d: [number, number]; b: [number, number] }) => [number, number]) =>
    keys.length ? keys.reduce((t, k) => { const [x, w] = f(sums.get(k)!); return t + (w ? x / w : 0); }, 0) / keys.length : 0;
  const draft = mean((s) => s.d), baseline = mean((s) => s.b);
  return { draft, baseline, delta: draft - baseline };
}

/**
 * Lift of following a model per decision (0 when either group has fewer than `minGroup` games), and their mean.
 * `weights`: per pilot (by index), for the bootstrap; none counts every outcome once. `only`: these decisions alone.
 */
function lifts(g: GroupedOutcomes, weights: Float64Array | null = null, minGroup = 10, only?: ReadonlySet<string>) {
  const per: DecisionLift[] = [];
  (g.keys ?? EVAL_DECISIONS).forEach((key, d) => {
    if (only && !only.has(key)) return;
    const { pilot, wpa, draft, baseline, w: ws } = g.byKey[d];
    const acc = { d: [0, 0, 0, 0], b: [0, 0, 0, 0], n: 0 };   // [sum followed, n followed, sum not, n not]
    for (let j = 0; j < pilot.length; j++) {
      const w = (weights ? weights[pilot[j]] : 1) * (ws ? ws[j] : 1);
      if (!w) continue;
      acc.n += w;
      if (draft[j]) { acc.d[0] += w * wpa[j]; acc.d[1] += w; } else { acc.d[2] += w * wpa[j]; acc.d[3] += w; }
      if (baseline[j]) { acc.b[0] += w * wpa[j]; acc.b[1] += w; } else { acc.b[2] += w * wpa[j]; acc.b[3] += w; }
    }
    const lift = (a: number[]) => (a[1] >= minGroup && a[3] >= minGroup ? a[0] / a[1] - a[2] / a[3] : 0);
    per.push({ key, draft: lift(acc.d), baseline: lift(acc.b), followedDraft: acc.n ? acc.d[1] / acc.n : 0, followedBaseline: acc.n ? acc.b[1] / acc.n : 0, n: acc.n });
  });
  const mean = (f: (d: DecisionLift) => number) => per.reduce((s, d) => s + f(d), 0) / per.length;
  return { per, draft: mean((d) => d.draft), baseline: mean((d) => d.baseline) };
}

const clip = (p: number) => Math.min(0.99, Math.max(0.01, p));

const NO_PURCHASES: [number, number][] = [];

/** Purchases a game is scored on: its six completed items, then its boots (slot 6). */
const SLOTS = 7;

/**
 * Each game's purchases' win probability under each fold's state model (scoreGames' `p`: the model's base rate when
 * there is no model or no state), at ((game × folds) + fold) × SLOTS + slot, in memory every thread can read.
 */
export function purchaseChances(games: PreparedGame[], models: (StateModel | null)[]): Float64Array {
  const out = new Float64Array(new SharedArrayBuffer(games.length * models.length * SLOTS * 8));
  models.forEach((m, f) => {
    const base = m?.base ?? 0.5;
    const p = (x: number[] | null) => (m && x ? winProbabilityOf(m, x) : base);
    games.forEach((g, k) => {
      const at = (k * models.length + f) * SLOTS;
      g.items.forEach((c, slot) => { out[at + slot] = p(c.x); });
      if (g.boots) out[at + SLOTS - 1] = p(g.boots.x);
    });
  });
  return out;
}

/** The positions 0, stride, 2 × stride… below n: the games a loop goes over (an even subsample when stride > 1). */
const strided = (n: number, stride: number) => { const at: number[] = []; for (let i = 0; i < n; i += stride) at.push(i); return at; };

export async function evaluate(input: EvalInput, opts: EvalOptions): Promise<EvalReport> {
  // Each step's time (0.20.0): what the report's "Time by step" lists, the last one closed however it ends.
  const clock: StepClock = { timings: [], at: performance.now(), label: 'Reading the stored games' };
  try {
    return await evaluateTimed(input, opts, clock);
  } finally {
    clock.timings.push({ label: clock.label, ms: performance.now() - clock.at });
    input.onTimings?.(clock.timings);
  }
}

/** The steps timed so far, and the one under way since `at`. */
interface StepClock { timings: StepTime[]; at: number; label: string }

async function evaluateTimed(input: EvalInput, opts: EvalOptions, clock: StepClock): Promise<EvalReport> {
  const { data, excl, profiles, regionPatch } = input;
  const log = input.log ?? (() => {});
  const steps = evalSteps(opts);
  let at = 0, current = clock.label, lastPart = 0;
  /** Ends the timed part under way and starts `label`'s, without a progress step (0.20.0). */
  const lap = (label: string) => {
    const now = performance.now();
    clock.timings.push({ label: clock.label, ms: now - clock.at });
    clock.at = now; clock.label = current = label;
  };
  let lastFrac = 0;
  const step = (label: string) => { lap(label); lastPart = 0; lastFrac = 0; input.progress?.({ step: ++at, steps, label }); };
  /**
   * What the step is doing now, beside its label (0.22.0): "fold 3 of 5: building its models", work the step's games
   * don't count, so the progress isn't still for minutes. The bar stays where its games left it.
   */
  const phase = (text: string) => {
    // A step whose label names its fold already ("Comparing builds on fold 2 of 5") isn't told it again.
    const said = text.replace(/^fold \d+ of \d+: /, '');
    input.progress?.({ step: at, steps, label: `${current} · ${/\bfold \d+ of \d+/.test(current) ? said : text}`, part: lastFrac });
  };
  /** How far into the current step (`done` of `of`), at most every half second: a clock read per call, nothing more. */
  const part = (done: number, of: number) => {
    if (!input.progress || of <= 0) return;
    const now = Date.now();
    if (now - lastPart < 500) return;
    lastPart = now;
    lastFrac = Math.min(1, done / of);
    input.progress({ step: at, steps, label: current, part: lastFrac });
  };
  /** The same, through fold `f` of a step that goes over the folds in turn. */
  const foldPart = (f: number, done: number, of: number) => part(f + (of > 0 ? done / of : 1), opts.folds);
  // The stored games are prepared as they are read. With `timelinesOf`, each raw timeline goes right after its game is
  // prepared, so they are never all in memory at once (they are most of a row).
  const rows: ModelGameRow[] = [];
  const streamed = input.timelinesOf !== undefined;
  // On other threads (input.prepare) a row is sent with others before it is prepared: the preparer drops its timeline.
  const dropHere = streamed && !input.prepare;
  function* reading() {
    for (const r of input.rows) {
      rows.push(r); yield r; if (dropHere) r.timeline = null;
      if (input.total && rows.length % 200 === 0) part(rows.length, input.total);
    }
  }
  // Frames in large blocks (dataset.ts frameSlabs): dropped once the state model is fitted, they go back to the system.
  const frames = frameSlabs();
  const prepared = input.prepare
    ? await input.prepare(reading(), input.participants, frames)
    : prepareGames(reading(), input.participants, data, profiles, { startingItemWindowMs: input.startingItemWindowMs, frames });
  const classic = input.classicPatches ? new Set(input.classicPatches) : null;
  const isClassic = (g: PreparedGame) => !classic || classic.has(g.patch);
  // Everything but patch memory is evaluated on the two-patch games, as without it.
  shareGameTexts([prepared]);
  const games = prepared.filter((g) => g.source !== 'me' && isClassic(g));
  const ageOf = patchAger(data, regionPatch);
  const memoryWindow = opts.memory?.patches ?? 0;
  /** Games only patch memory reads: before the patches above, within its window (their state frames aren't needed). */
  const olderGames = prepared.filter((g) => {
    if (g.source === 'me' || isClassic(g)) return false;
    g.frames = null;
    const age = ageOf(g);
    return age >= 2 && age < memoryWindow;
  });
  // Grouped drafts (0.20.0, an evaluation trial): every game evaluated or trained on gets its draft group, which each
  // draft model built from them then uses (draft.ts buildDraftModel). Not on a quick update: it adds to an exact base.
  let grouping: GroupingStats | undefined;
  if (opts.grouping && !opts.update) {
    lap('Grouping the drafts');
    const all = [...games, ...olderGames];
    grouping = assignDraftGroups(all, opts.grouping.groups > 0 ? opts.grouping.groups : autoGroups(all.length), opts.seed);
    log(`grouped drafts: ${grouping.groups.toLocaleString('en-US')} groups of ${grouping.games.toLocaleString('en-US')} games, `
      + `spread ${grouping.spread.toFixed(3)} sd, ${(grouping.ms / 1000).toFixed(1)} s`);
    lap('Setting up the folds');
  }
  const foldOf = (g: PreparedGame) => hash(`${opts.seed}:${g.pilot}`) % opts.folds;
  const folds = Array.from({ length: opts.folds }, (_, f) => ({ train: games.filter((g) => foldOf(g) !== f), test: games.filter((g) => foldOf(g) === f) }));
  const heldOutGames = folds.reduce((n, f) => n + f.test.length, 0);
  const pilots = new Set(games.map((g) => g.pilot)).size;
  log(`${games.length} games from ${pilots} pilots, ${opts.folds} folds by pilot`);

  // The live replay reads the timelines of an even sample of held-out games: without `timelinesOf`, the others are
  // dropped now (the raw timelines are the bulk of the rows). Only the replayed games' participants are kept.
  const liveStride = Math.max(1, Math.ceil(heldOutGames / opts.maxLiveGames));
  const replayIds = new Set(opts.live && !opts.update ? folds.flatMap(({ test }) => test.filter((_, i) => i % liveStride === 0).map((g) => g.matchId)) : []);
  if (!streamed) for (const r of rows) if (!replayIds.has(r.match_id)) r.timeline = null;
  // Only the evaluated games' rows are read again (their lane baselines, the live replay), kept as plain copies (0.19.0,
  // db.ts compactRows: node:sqlite's rows hold their columns in a dictionary, three times the size); the rest go.
  const rowsById = new Map<string, ModelGameRow>();
  {
    const byId = new Map(rows.map((r) => [r.match_id, r]));
    for (const g of games) { const r = byId.get(g.matchId); if (r) rowsById.set(g.matchId, { ...r }); }
  }
  rows.length = 0;
  const partsByMatch = new Map<string, ParticipantLite[]>();
  for (const p of input.participants) {
    if (!replayIds.has(p.match_id)) continue;
    let list = partsByMatch.get(p.match_id);
    if (!list) { list = []; partsByMatch.set(p.match_id, list); }
    list.push(p);
  }
  input.participants = [];

  // ---- B2: the state model's penalty, by held-out log loss --------------------------------------------
  const width = STATE_FEATURES.length;
  const heldOut = (model: StateModel | null, test: PreparedGame[], base: number) => {
    const p: number[] = [], y: number[] = [], pb: number[] = [];
    for (const g of test) {
      if (!g.frames) continue;
      for (let f = 0; f < g.frames.length / width; f++) {
        p.push(model ? winProbabilityOf(model, g.frames.subarray(f * width, (f + 1) * width)) : base);
        pb.push(base); y.push(g.win);
      }
    }
    return { p, y, pb };
  };
  type StateFit = { lambda: number; models: (StateModel | null)[]; s: { brier: number; logLoss: number }; all: { p: number[]; y: number[]; pb: number[] } };
  /** Every penalty on the same per-fold frames; only the best one's models and held-out predictions are kept. */
  const fitStates = () => {
    const foldRows = folds.map(({ train }) => stateRows(train));
    const foldBase = folds.map(({ train }) => train.reduce((s, g) => s + g.win, 0) / Math.max(1, train.length));
    let first: StateFit | null = null, best: StateFit | null = null;
    const byLambda: { lambda: number; brier: number; logLoss: number }[] = [];
    // A quick update fits its base's penalty alone.
    const lambdas = opts.update ? [opts.update.lambda] : opts.lambdas;
    for (const [i, lambda] of lambdas.entries()) {
      step(`Testing the game-state model (${i + 1} of ${lambdas.length})`);
      const all = { p: [] as number[], y: [] as number[], pb: [] as number[] };
      const models = folds.map(({ test }, f) => {
        const r = foldRows[f];
        const m = r && trainStateModel(r.rows, r.games, lambda);
        const h = heldOut(m, test, foldBase[f]);
        for (let j = 0; j < h.p.length; j++) { all.p.push(h.p[j]); all.y.push(h.y[j]); all.pb.push(h.pb[j]); }
        return m;
      });
      const s = all.p.length ? scoreProbabilities(all.p, all.y) : { brier: NaN, logLoss: NaN, n: 0 };
      log(`state model λ=${lambda}: Brier ${s.brier.toFixed(4)}, log loss ${s.logLoss.toFixed(4)} on ${all.p.length} held-out frames`);
      const fit: StateFit = { lambda, models, s, all };
      first ??= fit;
      // The lowest finite log loss; the first of equals.
      if (Number.isFinite(s.logLoss) && (!best || s.logLoss < best.s.logLoss)) best = fit;
      byLambda.push({ lambda, brier: s.brier, logLoss: s.logLoss });
    }
    return { byLambda, best: (best ?? first)! };
  };
  const { byLambda, best: bestState } = fitStates();
  const stateModels = bestState.models;
  const base = bestState.all.pb.length ? scoreProbabilities(bestState.all.pb, bestState.all.y) : null;
  // The report's part on it now, so the held-out predictions can go; and the state frames, which only these fits read.
  const stateReport: EvalReport['state'] = base && Number.isFinite(bestState.s.brier) ? {
    lambda: bestState.lambda, brier: bestState.s.brier, logLoss: bestState.s.logLoss, baseBrier: base.brier, baseLogLoss: base.logLoss,
    frames: bestState.all.p.length,
    byLambda,
    calibration: calibration(bestState.all.p, bestState.all.y),
  } : null;
  bestState.all = { p: [], y: [], pb: [] };
  for (const g of prepared) g.frames = null;
  // Each purchase's win probability under each fold's state model, once (0.12.0): what scoring a game reads of its
  // purchases (scoreGames), the same numbers. Their state features then go: every thread would hold them otherwise.
  const itemP = purchaseChances([...games, ...olderGames], stateModels);
  // The late-game swaps' too (0.17.0), with each decision: they are fewer, and vary in number from game to game.
  if (opts.swaps) {
    for (const g of [...games, ...olderGames]) {
      for (const c of g.swaps ?? []) c.p = stateModels.map((m) => (m && c.x ? winProbabilityOf(m, c.x) : m?.base ?? 0.5));
    }
  }
  // The purchase log goes too: only the personal layer reads it, never the evaluation.
  for (const g of prepared) {
    for (const c of g.items) c.x = null;
    if (g.boots) g.boots.x = null;
    for (const c of g.swaps ?? []) c.x = null;
    if (!opts.swaps) g.swaps = [];
    g.purchases = NO_PURCHASES;
  }

  // ---- The rest on every core (0.11.0) ------------------------------------------------------------------------
  // The comparisons below go over held-out games, each on its own: with a team (input.team), its threads share them
  // out, each from a copy of what they read (no frames, no timelines but the replay's), and the results come back in
  // game order. Alone, the same code, game after game.
  const ctx: EvalContext = {
    opts, data, excl, profiles, regionPatch, games, olderGames, rowsById, partsByMatch, replayIds, stateModels, stateReport, itemP,
    recLog: input.recLog ?? [], step, part, phase, log, collect: input.collect ?? (() => {}),
    base: input.base, onBase: input.onBase, salt: input.salt,
    replayTimelines: () => (streamed ? input.timelinesOf!([...replayIds]) : null),
    dropRows: () => { rows.length = 0; },
  };
  let team: Team = solo;
  if (input.team) {
    // The replay's timelines are read once, here, for every thread.
    const timelines = ctx.replayTimelines();
    ctx.replayTimelines = () => timelines;
    // The frames and purchases dropped above are garbage: collected first, as the team's plan measures this thread's
    // heap (0.15.1; at 54,000 games 873 MB before, 227 MB after, which left the other threads 650 MB less).
    ctx.collect();
    team = await input.team.start({
      opts, data, profileRows: input.team.profileRows, regions: [...input.team.regions],
      games, olderGames, rows: games.map((g) => rowsById.get(g.matchId)).filter((r): r is ModelGameRow => !!r),
      parts: [...partsByMatch], replayIds: [...replayIds], replayTimelines: timelines ? [...timelines] : null, stateModels, itemP,
    });
  }
  try {
    const report = await evaluateWith(ctx, team);
    if (grouping) report.grouping = grouping;
    return report;
  } finally {
    await team.close();
  }
}

/**
 * What the comparisons need, in every thread of a team: rank 0's from what it read, the others' from its copy
 * (contextOf). `step`, `part` and `log` report in rank 0 and do nothing elsewhere (a worker collects garbage at a step).
 */
export interface EvalContext {
  opts: EvalOptions;
  data: StaticData;
  excl: ExclusivityIndex;
  profiles: ChampionProfiles;
  regionPatch: RegionPatchOf;
  games: PreparedGame[];
  olderGames: PreparedGame[];
  rowsById: Map<string, ModelGameRow>;
  partsByMatch: Map<string, ParticipantLite[]>;
  replayIds: Set<string>;
  stateModels: (StateModel | null)[];
  stateReport: EvalReport['state'];
  /** Each purchase's win probability by fold (purchaseChances), for games then older games in their order here. */
  itemP: Float64Array;
  recLog: RecLogRow[];
  replayTimelines: () => Map<string, string> | null;
  step: (label: string) => void;
  part: (done: number, of: number) => void;
  /** What the step does now beside its label (0.22.0; rank 0 only, unset elsewhere). */
  phase?: (text: string) => void;
  log: (line: string) => void;
  collect: () => void;
  /** Lets go of this thread's own references to the stored rows (0.19.0; rowsById is pruned by the caller). */
  dropRows?: () => void;
  /** A quick update's base, where the next base goes, and a full evaluation's salt for it: rank 0 only (EvalInput). */
  base?: EvalBaseData;
  onBase?: (base: EvalBaseData) => void;
  salt?: string;
}

/** What a team's other threads get (plain data: it is copied to each), and rebuild their context from. */
export interface EvalSnapshot {
  opts: EvalOptions;
  data: StaticData;
  profileRows: { current: ChampionTotals[]; older: ChampionTotals[] };
  regions: [string, { patch: string; previousPatch: string }][];
  games: PreparedGame[];
  olderGames: PreparedGame[];
  rows: ModelGameRow[];
  parts: [string, ParticipantLite[]][];
  replayIds: string[];
  replayTimelines: [string, string][] | null;
  stateModels: (StateModel | null)[];
  /** In memory every thread reads (a SharedArrayBuffer's): handed over beside the copy, not in it (runEval.ts). */
  itemP: Float64Array;
}

/**
 * One copy of each repeated text of the games (0.19.0): a deserialised copy, like the rows read, holds the pilot's PUUID
 * (78 characters), the platform, patch, source, tier and the summoner, start and skill strings once per game. The same
 * values; about 300 bytes a game less.
 */
export function shareGameTexts(lists: PreparedGame[][]) {
  const seen = new Map<string, string>();
  const one = <T extends string | null>(v: T): T => {
    if (v === null) return v;
    const known = seen.get(v);
    if (known !== undefined) return known as T;
    seen.set(v, v);
    return v;
  };
  for (const list of lists) {
    for (const g of list) {
      g.pilot = one(g.pilot); g.platform = one(g.platform); g.patch = one(g.patch); g.source = one(g.source);
      g.tier = one(g.tier); g.summoners = one(g.summoners); g.start = one(g.start); g.early = one(g.early);
      g.maxFirst = one(g.maxFirst);
    }
  }
}

/** A team thread's context from its copy (model/evalWorker.ts). */
export function contextOf(s: EvalSnapshot, step: (label: string) => void, collect: () => void = () => {}): EvalContext {
  shareGameTexts([s.games, s.olderGames]);
  const regions = new Map(s.regions);
  const timelines = s.replayTimelines ? new Map(s.replayTimelines) : null;
  return {
    opts: s.opts, data: s.data, excl: buildExclusivityIndex(s.data), profiles: new ChampionProfiles(s.profileRows.current, s.profileRows.older, s.data),
    regionPatch: (p) => regions.get(p), games: s.games, olderGames: s.olderGames, rowsById: new Map(s.rows.map((r) => [r.match_id, r])),
    partsByMatch: new Map(s.parts), replayIds: new Set(s.replayIds), stateModels: s.stateModels, stateReport: null, itemP: s.itemP, recLog: [],
    replayTimelines: () => timelines, step, part: () => {}, log: () => {}, collect,
    dropRows: () => { s.rows = []; },
  };
}

/**
 * Memory diagnostics (IRELIA_EVAL_MEMORY=1; changes nothing it measures): each thread's heap after a full collection
 * (where it can force one: the app's threads, or Node with --expose-gc; else as it stands), the process's buffers, and
 * the size of each part of what a thread holds as v8 would serialise it. On stderr, with the thread's number.
 */
const memoryDiagnostics = process.env.IRELIA_EVAL_MEMORY === '1';
function logMemory(rank: number, what: string, parts: Record<string, unknown> = {}) {
  if (!memoryDiagnostics) return;
  const collected = typeof (globalThis as { gc?: unknown }).gc === 'function';
  collectGarbage();
  const mb = (n: number) => `${(n / 2 ** 20).toFixed(0)} MB`;
  const m = process.memoryUsage();
  const heap = v8.getHeapStatistics().used_heap_size;
  const sizes = Object.entries(parts).map(([k, v]) => `${k} ${mb(v8.serialize(v).byteLength)}`).join(', ');
  console.error(`  [memory] thread ${rank + 1}, ${what}: heap ${mb(heap)}${collected ? '' : ' (not collected)'}, buffers ${mb(m.arrayBuffers)}, process ${mb(m.rss)}${sizes ? `; serialised: ${sizes}` : ''}`);
}

/** The comparisons, after the state model: in one thread, or in each of a team's (`team`). */
export async function evaluateWith(ctx: EvalContext, team: Team): Promise<EvalReport> {
  const { opts, data, excl, profiles, regionPatch, games, olderGames, rowsById, partsByMatch, replayIds, stateModels, stateReport, itemP, step, part, log, collect } = ctx;
  /** Fold f's draft model, its building shown beside the step's label (0.22.0): minutes on a large pool, uncounted. */
  const buildFold = (f: number, s: ScoredGame[], state: StateModel | null) => {
    ctx.phase?.(`fold ${f + 1} of ${opts.folds}: building its models`);
    return buildDraftModel(s, state, profiles);
  };
  const ageOf = patchAger(data, regionPatch);
  logMemory(team.rank, 'a thread holds', {
    games, olderGames, rows: [...rowsById.values()], participants: [...partsByMatch.values()], stateModels, itemP, replayIds: [...replayIds],
  });
  /** Games scored with fold f's state model, as scoreGames scores them, from the purchases' chances computed once. */
  const indexOf = new Map<PreparedGame, number>();
  [...games, ...olderGames].forEach((g, k) => indexOf.set(g, k));
  const scored = (list: PreparedGame[], f: number, expected?: ArrayLike<number>): ScoredGame[] => {
    const base = stateModels[f]?.base ?? 0.5;
    return list.map((g, i) => {
      const at = (indexOf.get(g)! * opts.folds + f) * SLOTS;
      return {
        g,
        wpa0: g.win - (expected && Number.isFinite(expected[i]) ? expected[i] : base),
        itemWpa: g.items.map((_, slot) => g.win - itemP[at + slot]),
        bootsWpa: g.boots ? g.win - itemP[at + SLOTS - 1] : null,
        swapWpa: (g.swaps ?? []).map((c) => g.win - (c.p ? c.p[f] : base)),
      };
    });
  };
  const foldOf = (g: PreparedGame) => hash(`${opts.seed}:${g.pilot}`) % opts.folds;
  const folds = Array.from({ length: opts.folds }, (_, f) => ({ train: games.filter((g) => foldOf(g) !== f), test: games.filter((g) => foldOf(g) === f) }));
  const heldOutGames = folds.reduce((n, f) => n + f.test.length, 0);
  const pilots = new Set(games.map((g) => g.pilot)).size;
  const liveStride = Math.max(1, Math.ceil(heldOutGames / opts.maxLiveGames));
  /** How far through fold `f` of a step that goes over the folds in turn. */
  const foldPart = (f: number, done: number, of: number) => part(f + (of > 0 ? done / of : 1), opts.folds);

  // ---- A quick update (0.16.0, docs/EVAL-UPDATE.md) -----------------------------------------------------------
  // It works out the held-out games its base hasn't measured, with the base's settings and models rebuilt from every
  // training game, and adds their outcomes to the base's: each gate is decided on all of them.
  const up = opts.update ?? null;
  const base = ctx.base ?? null;
  const salt = up?.salt ?? ctx.salt ?? '';
  const pilotKey = (p: string) => baseHash(salt, p).toString(36);
  /** The held-out games a quick update works out (null: a full evaluation, every one). */
  const fresh = up ? (() => {
    const known = new Set(up.known);
    return new Set(games.filter((g) => !known.has(baseHash(salt, g.matchId))));
  })() : null;
  /** The base's held-out games no longer held out now (archived, say), and those whose stored game changed since. */
  const missing = up && fresh ? up.known.length - (games.length - fresh.size) : 0;
  const changedGames = up && fresh ? (() => {
    const printOf = new Map<number, number>();
    up.known.forEach((h, i) => printOf.set(h, up.prints[i]));
    let n = 0;
    for (const g of games) {
      if (fresh.has(g)) continue;
      if (printOf.get(baseHash(salt, g.matchId)) !== gamePrint(salt, g)) n++;
    }
    return n;
  })() : 0;
  /** A fold a quick update has no new held-out game in: its models aren't built (every thread skips it alike). */
  const idle = (f: number) => !!fresh && !folds[f].test.some((g) => fresh.has(g));
  /** The positions of a pass over every held-out game: all of them, or a quick update's new ones. */
  const detailAt = (list: ScoredGame[]) => (fresh ? list.flatMap((s, i) => (fresh.has(s.g) ? [i] : [])) : strided(list.length, 1));
  /** ...and of the grid's subsample (early in a patch's): every gridStride-th, or new ones one in the base's stride. */
  const subsampleAt = (list: ScoredGame[]) => (fresh && up
    ? list.flatMap((s, i) => (fresh.has(s.g) && baseHash(salt, `sample:${s.g.matchId}`) % up.gridStride === 0 ? [i] : []))
    : strided(list.length, gridStride));
  /** A quick update's outcomes with its base's (pilots by their hash); a full evaluation's as they are. */
  const mergeIn = (b: GroupedOutcomes | undefined, m: GroupedOutcomes): GroupedOutcomes =>
    (up ? (b ? mergeOutcomes(b, m, pilotKey) : { ...m, pilots: m.pilots.map(pilotKey) }) : m);
  /** Outcomes as the next base keeps them: pilots by their hash (a quick update's are already). */
  const forBase = (g: GroupedOutcomes): GroupedOutcomes => (up ? g : { ...g, pilots: g.pilots.map(pilotKey) });
  /** "all 82,069 held-out games", or a quick update's "312 new held-out games". */
  const heldOutLabel = () => (fresh ? `${fresh.size.toLocaleString('en-US')} new held-out games` : `all ${heldOutGames.toLocaleString('en-US')} held-out games`);
  const next: Omit<EvalBaseData, 'known' | 'prints' | 'report'> = { salt, final: null!, memory: null, scoring: null, tuning: null, swaps: null };

  // ---- B3: draft model vs lane baseline on held-out games ------------------------------------------------
  // One fold at a time: a fold's draft model, weighted games and observation caches are most of the memory. The
  // bandwidth / prior search, the final comparison and the live replay each go over the folds in turn, rebuilding the
  // one they need (a fraction of the time the comparisons take).
  /** A fold's draft model and its held-out games, scored with its state model. */
  const foldModel = (f: number) => {
    const { train, test } = folds[f];
    const m = stateModels[f];
    const model = buildFold(f, scored(train, f), m);
    return { m, model, weighted: weighGames(model, data, regionPatch, opts.weights), testScored: scored(test, f) };
  };
  /**
   * A fold's lane baselines and their win rates: built once, kept for every pass over the folds. At its first pass the
   * baseline of each held-out game's lane and opponent is worked out once over the team and handed to every thread
   * (0.15.2): each thread built those its own games needed before, mostly the same ones, and each read every training
   * game again for each (the validated games are kept now, as the model thread keeps them). Every thread calls this at
   * the same points (it shares).
   */
  const baselinesByFold = new Map<number, ReturnType<typeof foldBaselines>>();
  const baselinesOf = async (f: number) => {
    let b = baselinesByFold.get(f);
    if (!b) {
      b = foldBaselines(f);
      baselinesByFold.set(f, b);
      const pairs = new Map<string, [Lane, number | null]>();
      // A quick update's: those of its new games (a baseline is still worked out on demand for any other).
      for (const g of folds[f].test) if (!fresh || fresh.has(g)) pairs.set(`${g.lane}:${g.opponent}`, [g.lane, g.opponent]);
      const list = [...pairs.values()];
      const made: BuildRecommendation[] = new Array(list.length);
      const fold = b;
      await team.forEach(list.length, (j) => fold.baselineFor(...list[j]), (j, r) => { made[j] = r; });
      const all = await team.share(() => made);
      for (const [j, [lane, opponent]] of list.entries()) fold.baselines.set(`${lane}:${opponent}`, all[j]);
      // The validated training games go now (0.19.0): kept with the fold for the whole evaluation, they were a third of
      // a thread's memory (673 bytes a training row, every fold). A baseline asked for later validates them again.
      fold.dropValidated();
    }
    return b;
  };
  const foldBaselines = (f: number) => {
    const m = stateModels[f];
    const { train } = folds[f];
    let trainRows: ModelGameRow[] | null = train.filter((g) => opts.buildSources.includes(g.source)).map((g) => rowsById.get(g.matchId)!);
    const baselines = new Map<string, BuildRecommendation>();
    let validated: Map<string, ValidatedRows> | undefined = new Map();
    const baselineFor = (lane: Lane, opponent: number | null) => {
      const key = `${lane}:${opponent}`;
      let b = baselines.get(key);
      if (!b) {
        // A full evaluation makes every held-out game's baseline before its rows go (dropRows): none is missing then.
        if (!trainRows) throw new Error(`No lane baseline for ${key} in fold ${f + 1}, and the stored rows are gone`);
        b = recommend(trainRows, data, excl, opponent, opts.engine, lane, regionPatch, validated);
        baselines.set(key, b);
      }
      return b;
    };
    // Raw lane win rates of each option among the baseline's games, for its outcome predictions (Brier).
    const winRates = new Map<string, { w: number; n: number }>();
    const laneRate = new Map<Lane, { w: number; n: number }>();
    for (const s of scored(train.filter((g) => opts.buildSources.includes(g.source)), f)) {
      const lr = laneRate.get(s.g.lane) ?? { w: 0, n: 0 };
      lr.w += s.g.win; lr.n++;
      laneRate.set(s.g.lane, lr);
      for (const [key, a] of Object.entries(actual(s))) {
        const k = `${s.g.lane}:${key}:${a!.option}`;
        const e = winRates.get(k) ?? { w: 0, n: 0 };
        e.w += s.g.win; e.n++;
        winRates.set(k, e);
      }
    }
    const baseRate = (lane: Lane, key: string, option: string) => {
      const lr = laneRate.get(lane) ?? { w: 1, n: 2 };
      const e = winRates.get(`${lane}:${key}:${option}`) ?? { w: 0, n: 0 };
      return (e.w + 10 * (lr.w / lr.n)) / (e.n + 10);
    };
    return { baselines, baselineFor, baseRate, dropValidated: () => { validated = undefined; }, dropRows: () => { trainRows = null; } };
  };
  /**
   * A full evaluation (0.19.0): every fold's lane baselines first, then the stored rows go but the live replay's, in
   * every thread. Only the baselines read them, and they were about a fifth of what a thread held to the end (1 KB a
   * game at 112,000 games). A quick update keeps them: it works out other baselines when it needs them.
   */
  const baselinesThenDropRows = async () => {
    for (let f = 0; f < opts.folds; f++) await baselinesOf(f);
    for (const b of baselinesByFold.values()) b.dropRows();
    for (const id of [...rowsById.keys()]) if (!replayIds.has(id)) rowsById.delete(id);
    ctx.dropRows?.();
    logMemory(team.rank, 'every fold\'s lane baselines made, the stored rows dropped');
  };
  /** ...with what comparing builds on it needs: its lane baselines, their win rates and the observation cache. */
  const foldToCompare = async (f: number) => {
    const { baselineFor, baseRate } = await baselinesOf(f);
    const made = { ...foldModel(f), baselineFor, baseRate, cache: new Map<string, Observation[]>() };
    logMemory(team.rank, `fold ${f + 1}'s models built`);
    return made;
  };

  /** The grid searches compare an even subsample of held-out games. */
  const gridStride = Math.max(1, Math.ceil(heldOutGames / opts.maxGridGames));
  /**
   * Fold f's held-out games under bandwidth h and prior k: their outcomes (and, in detail, the Brier sums and the
   * draft model's picks by game, into `picks`). Each fold's work is a call of its own, here and below (0.12.0): its
   * models go when the call ends, where a loop suspended at an await would hold them until the next fold's were built.
   */
  const compareFold = async (f: number, h: number, k: number, detail: boolean, outcomes: Outcome[],
    brier: { d: number; b: number; n: number }, picks?: Map<string, Picks>, thetas?: Map<string, Partial<Record<DecisionKey, number>>>) => {
    collect();
    if (detail && idle(f)) return;
    const fold = await foldToCompare(f);
    const settings: DraftSettings = { weights: opts.weights, estimate: { ...ESTIMATE_DEFAULTS, h, k } };
    const at = detail ? detailAt(fold.testScored) : strided(fold.testScored.length, gridStride);
    await team.forEach(at.length, (j) => {
      // A game's picks and (in detail) the estimate of each option taken, wherever the team works it out...
      const s = fold.testScored[at[j]];
      const baseline = fold.baselineFor(s.g.lane, s.g.opponent);
      const rec = recommendDraft(fold.model, { lane: s.g.lane, opponent: s.g.opponent, enemies: s.g.enemies, allies: s.g.allies },
        data, excl, regionPatch, baseline, settings, { reasons: false, detail, weighted: fold.weighted, cache: fold.cache, decisions: SCORED });
      const theta: Partial<Record<DecisionKey, number>> = {};
      if (detail) for (const [key, a] of Object.entries(actual(s)) as [DecisionKey, { option: string }][]) theta[key] = rec.estimates?.[key]?.[a.option] ?? 0;
      return { pd: picksOf(rec.build), theta };
    }, (j, { pd, theta: estimate }) => {
      // ...and added up here, in game order.
      const s = fold.testScored[at[j]];
      const pb = picksOf(fold.baselineFor(s.g.lane, s.g.opponent));
      picks?.set(s.g.matchId, pd);
      const theta: Partial<Record<DecisionKey, number>> = {};
      thetas?.set(s.g.matchId, theta);
      for (const [key, a] of Object.entries(actual(s)) as [DecisionKey, { option: string; wpa: number }][]) {
        outcomes.push({ pilot: s.g.pilot, key, wpa: a.wpa, followedDraft: pd[key] === a.option, followedBaseline: pb[key] === a.option });
        if (thetas && PREGAME.has(key)) theta[key] = estimate[key] ?? 0;
        if (detail && BRIER_KEYS.includes(key)) {
          const pState = s.g.win - a.wpa;
          brier.d += (clip(pState + (estimate[key] ?? 0)) - s.g.win) ** 2;
          brier.b += (clip(fold.baseRate(s.g.lane, key, a.option)) - s.g.win) ** 2;
          brier.n++;
        }
      }
    }, (done, of) => foldPart(f, done, of));
    logMemory(team.rank, `fold ${f + 1} compared (${fold.cache.size} observation lists)`);
  };

  // The bandwidth / prior grid: every setting on each fold in turn, game by game. A draft's sums at one bandwidth are
  // the same for every prior (only the shrinkage differs), so they are added up once per bandwidth (Query.memo). Each
  // setting's outcomes still come in game order, as when the settings went one after another.
  const settingsGrid = opts.bandwidths.flatMap((h) => opts.ks.map((k) => ({ h, k })));
  const gridOutcomes = settingsGrid.map(() => [] as Outcome[]);
  const gridFold = async (f: number) => {
    const fold = await foldToCompare(f);
    const at = strided(fold.testScored.length, gridStride);
    await team.forEach(at.length, (j) => {
      const s = fold.testScored[at[j]];
      const baseline = fold.baselineFor(s.g.lane, s.g.opponent);
      const draft = { lane: s.g.lane, opponent: s.g.opponent, enemies: s.g.enemies, allies: s.g.allies };
      const pds: Picks[] = new Array(settingsGrid.length);
      for (const h of opts.bandwidths) {
        const memo = new Map<Observation[], unknown>();
        settingsGrid.forEach((setting, g) => {
          if (setting.h !== h) return;
          const settings: DraftSettings = { weights: opts.weights, estimate: { ...ESTIMATE_DEFAULTS, h, k: setting.k } };
          const rec = recommendDraft(fold.model, draft, data, excl, regionPatch, baseline, settings,
            { reasons: false, detail: false, weighted: fold.weighted, cache: fold.cache, memo, decisions: SCORED });
          pds[g] = picksOf(rec.build);
        });
      }
      return pds;
    }, (j, pds) => {
      const s = fold.testScored[at[j]];
      const pb = picksOf(fold.baselineFor(s.g.lane, s.g.opponent));
      settingsGrid.forEach((_, g) => {
        for (const [key, a] of Object.entries(actual(s)) as [DecisionKey, { option: string; wpa: number }][]) {
          gridOutcomes[g].push({ pilot: s.g.pilot, key, wpa: a.wpa, followedDraft: pds[g][key] === a.option, followedBaseline: pb[key] === a.option });
        }
      });
    }, (done, of) => part(done, of));
  };
  // A quick update keeps its base's grid and choice.
  const grid: EvalReport['grid'] = up ? [...(base?.report.grid ?? [])] : [];
  let chosen: { h: number; k: number };
  if (up) chosen = up.chosen;
  else {
    for (let f = 0; f < opts.folds; f++) {
      // The step's start collects what the last fold left (its call is over).
      step(`Comparing builds on fold ${f + 1} of ${opts.folds} (${settingsGrid.length} settings)`);
      if (f === 0) await baselinesThenDropRows();
      await gridFold(f);
    }
    let best: { h: number; k: number; delta: number } | null = null;
    for (const [i, { h, k }] of settingsGrid.entries()) {
      const l = lifts(groupOutcomes(gridOutcomes[i]));
      gridOutcomes[i] = [];
      grid.push({ h, k, draft: l.draft, baseline: l.baseline, delta: l.draft - l.baseline });
      log(`h=${h} k=${k}: lift draft ${(l.draft * 100).toFixed(2)} pts, baseline ${(l.baseline * 100).toFixed(2)} pts`);
      if (!best || l.draft - l.baseline > best.delta) best = { h, k, delta: l.draft - l.baseline };
    }
    // Only rank 0 holds the grid's outcomes: the others get its choice.
    chosen = await team.share(() => ({ h: best?.h ?? ESTIMATE_DEFAULTS.h, k: best?.k ?? ESTIMATE_DEFAULTS.k }));
  }
  step(`Comparing builds on ${heldOutLabel()}`);
  const final = { outcomes: [] as Outcome[], brier: { d: 0, b: 0, n: 0 } };
  // The draft model's picks, for patch memory and scoring against the draft to be compared with; for the latter also
  // its estimate for each pre-game option taken (its Brier).
  const classicPicks = opts.memory || opts.scoring || opts.tuning ? new Map<string, Picks>() : undefined;
  const classicThetas = opts.scoring ? new Map<string, Partial<Record<DecisionKey, number>>>() : undefined;
  for (let f = 0; f < opts.folds; f++) await compareFold(f, chosen.h, chosen.k, true, final.outcomes, final.brier, classicPicks, classicThetas);
  const grouped = mergeIn(base?.final.g, groupOutcomes(final.outcomes));
  final.outcomes = [];
  const finalBrier = base ? { d: base.final.brier.d + final.brier.d, b: base.final.brier.b + final.brier.b, n: base.final.brier.n + final.brier.n } : final.brier;
  next.final = { g: forBase(grouped), brier: finalBrier };
  const l = lifts(grouped);

  /**
   * Pilot bootstrap of the lift difference (draft minus baseline): its 95 % interval (`only`: of these decisions). The
   * resamples are shared out (0.12.0): resample b draws the pilots its turn in the one sequence would (the generator
   * adds a constant per draw, so b's draws start b × pilots draws in), and they are sorted together as before.
   */
  const interval = async (g: GroupedOutcomes, only?: ReadonlySet<string>) => {
    // Rank 0's outcomes (only it holds them), in every thread.
    const { pilotCount, byKey, keys } = await team.share(() => ({ pilotCount: g.pilots.length, byKey: g.byKey, keys: g.keys }));
    const resampled: GroupedOutcomes = { pilots: [], byKey, ...(keys ? { keys } : {}) };
    const n = pilotCount > 1 ? opts.bootstrap : 0;
    const seed0 = opts.seed * 2654435761 >>> 0;
    const deltas: number[] = new Array(n);
    const w = new Float64Array(pilotCount);
    await team.forEach(n, (b) => {
      let seed = (seed0 + Math.imul(b * pilotCount, 0x6d2b79f5)) >>> 0;
      const rand = () => { seed = (seed + 0x6d2b79f5) >>> 0; let t = seed; t = Math.imul(t ^ (t >>> 15), t | 1); t ^= t + Math.imul(t ^ (t >>> 7), t | 61); return ((t ^ (t >>> 14)) >>> 0) / 4294967296; };
      w.fill(0);
      for (let i = 0; i < pilotCount; i++) w[Math.floor(rand() * pilotCount)]++;
      const r = lifts(resampled, w, 10, only);
      return r.draft - r.baseline;
    }, (b, delta) => { deltas[b] = delta; });
    deltas.sort((a, b) => a - b);
    const q = (p: number) => (deltas.length ? deltas[Math.min(deltas.length - 1, Math.floor(p * deltas.length))] : NaN);
    return { low: q(0.025), high: q(0.975) };
  };
  step('Confidence interval');
  const lift = { draft: l.draft, baseline: l.baseline, delta: l.draft - l.baseline, ...(await interval(grouped)), withinPilot: withinPilotOf(grouped) };
  const brier = { draft: finalBrier.n ? finalBrier.d / finalBrier.n : NaN, baseline: finalBrier.n ? finalBrier.b / finalBrier.n : NaN, n: finalBrier.n };
  // A quick update's: its base's and the new ones (the base's games no longer stored still count: their outcomes stand).
  const testGames = up && fresh ? up.known.length + fresh.size : folds.reduce((s, f) => s + f.test.length, 0);

  // ---- Patch memory against the two-patch draft model -----------------------------------------------------
  let memory: EvalReport['memory'] = null;
  if (opts.memory) memory = await evaluateMemory(opts.memory);
  const scoring = opts.scoring ? await evaluateScoring() : null;
  const tuning = opts.tuning ? await evaluateTuning(opts.tuning) : null;
  const swaps = opts.swaps ? await evaluateSwaps() : null;
  // The comparisons are done: the folds' lane baselines go before the replay.
  baselinesByFold.clear();
  classicPicks?.clear();
  classicThetas?.clear();

  /**
   * Pre-game decisions scored against each game's expected win rate before it started (model/expected.ts), against
   * today's scoring, on the same held-out games with the same bandwidth, prior, state models and lane baselines. Items
   * are scored as before, so only the pre-game picks can differ and only they are compared. Each fold's expected win
   * rate is fitted on its training games: cross-fitted by pilot for the training games the model learns from, fitted on
   * all of them for the held-out games it is measured on.
   */
  async function evaluateScoring(): Promise<ScoringReport> {
    const none = { fair: 0, classic: 0, delta: 0, low: NaN, high: NaN };
    const failed = (gateReason: string, expected: ScoringReport['expected'] = null): ScoringReport => ({
      expected, adjusted: none, raw: { fair: 0, classic: 0, delta: 0 }, brier: { fair: NaN, classic: NaN, n: 0 }, decisions: [],
      gate: 'failed', gateReason,
    });
    if (testGames < opts.minGames) {
      step('Expected win rate: not enough games');
      step('Runes and summoners against the draft: not enough games');
      return failed(`only ${testGames} held-out games (needs ${opts.minGames})`);
    }
    step('Expected win rate before the game');
    // The penalty is chosen per fold on its training games alone (cross-fitted log loss). The folds are fitted side by
    // side (0.12.0); every thread gets all of them.
    type FoldExpected = { train: Float64Array; penalty: Penalty | null; test: Float64Array };
    const fitted: FoldExpected[] = new Array(folds.length);
    await team.forEach(folds.length, (f) => {
      const { train, test } = folds[f];
      // A quick update keeps the penalty each fold chose (a fold that had no model searches again).
      const kept = up?.penalties?.[f];
      const cf = crossFitted(train, ageOf, kept ? { ...EXPECTED_DEFAULTS, penalties: [kept] } : undefined);
      const full = cf.penalty !== null ? fitExpected(train, ageOf, cf.penalty) : null;
      return { train: cf.values, penalty: cf.penalty, test: Float64Array.from(test, (g) => (full ? expectedOf(full, g, ageOf) : NaN)) };
    }, (f, r) => { fitted[f] = r; });
    const perFold = await team.share(() => fitted);
    // The app uses the penalty most folds chose (the more rigid on a tie: the more cautious).
    const votes = new Map<string, { penalty: Penalty; n: number }>();
    for (const x of perFold) {
      if (!x.penalty) continue;
      const key = JSON.stringify(x.penalty);
      const v = votes.get(key) ?? { penalty: x.penalty, n: 0 };
      v.n++;
      votes.set(key, v);
    }
    const penalty = [...votes.values()].sort((a, b) => b.n - a.n || b.penalty.lambda - a.penalty.lambda)[0]?.penalty ?? null;
    let p: number[] = [], pb: number[] = [], y: number[] = [];
    folds.forEach(({ train, test }, f) => {
      const rate = train.reduce((s, g) => s + g.win, 0) / Math.max(1, train.length);
      test.forEach((g, i) => {
        if (fresh && !fresh.has(g)) return;
        const e = perFold[f].test[i];
        if (Number.isFinite(e)) { p.push(e); pb.push(rate); y.push(g.win); }
      });
    });
    // A quick update: the base's predictions first.
    const expectedSeen = base?.scoring?.expected;
    if (expectedSeen) { p = [...expectedSeen.p, ...p]; pb = [...expectedSeen.pb, ...pb]; y = [...expectedSeen.y, ...y]; }
    if (!p.length) {
      step('Runes and summoners against the draft: not enough games');
      return failed('too few games to fit the expected win rate');
    }
    const se = scoreProbabilities(p, y), sb = scoreProbabilities(pb, y);
    const expected = { brier: se.brier, logLoss: se.logLoss, baseBrier: sb.brier, baseLogLoss: sb.logLoss, games: p.length, penalty };
    log(`expected win rate before the game: Brier ${se.brier.toFixed(4)} (overall win rate ${sb.brier.toFixed(4)}), log loss ${se.logLoss.toFixed(4)} (${sb.logLoss.toFixed(4)}) on ${p.length} held-out games`);

    step(`Runes and summoners against the draft on ${heldOutLabel()}`);
    const adjusted: Outcome[] = [], raw: Outcome[] = [];
    const brierSum = { fair: 0, classic: 0, n: 0 };
    const changed = new Map<string, { n: number; changed: number }>();
    const settings: DraftSettings = { weights: opts.weights, estimate: { ...ESTIMATE_DEFAULTS, h: chosen.h, k: chosen.k } };
    /** Runes and summoners against the draft: fold f, in a call of its own (compareFold). */
    const scoringFold = async (f: number) => {
      collect();
      if (idle(f)) return;
      const m = stateModels[f];
      const model = buildFold(f, scored(folds[f].train, f, perFold[f].train), m);
      const weighted = weighGames(model, data, regionPatch, opts.weights);
      const cache = new Map<string, Observation[]>();
      const { baselineFor } = await baselinesOf(f);
      const testScored = scored(folds[f].test, f);
      const at = detailAt(testScored);
      await team.forEach(at.length, (j) => {
        const s = testScored[at[j]];
        const baseline = baselineFor(s.g.lane, s.g.opponent);
        const rec = recommendDraft(model, { lane: s.g.lane, opponent: s.g.opponent, enemies: s.g.enemies, allies: s.g.allies },
          data, excl, regionPatch, baseline, settings, { reasons: false, detail: true, weighted, cache, decisions: PREGAME });
        const est: Partial<Record<DecisionKey, number>> = {};
        for (const [key, x] of Object.entries(actual(s)) as [DecisionKey, { option: string }][]) if (PREGAME.has(key)) est[key] = rec.estimates?.[key]?.[x.option] ?? 0;
        return { pa: picksOf(rec.build), est };
      }, (j, { pa, est }) => {
        const i = at[j];
        const s = testScored[i];
        const e = perFold[f].test[i];
        const pc = classicPicks!.get(s.g.matchId) ?? {}, tc = classicThetas!.get(s.g.matchId) ?? {};
        for (const [key, x] of Object.entries(actual(s)) as [DecisionKey, { option: string; wpa: number }][]) {
          if (!PREGAME.has(key)) continue;
          const followedDraft = pa[key] === x.option, followedBaseline = pc[key] === x.option;
          const overall = s.g.win - x.wpa;
          const exp = Number.isFinite(e) ? e : overall;
          adjusted.push({ pilot: s.g.pilot, key, wpa: s.g.win - exp, followedDraft, followedBaseline });
          raw.push({ pilot: s.g.pilot, key, wpa: x.wpa, followedDraft, followedBaseline });
          brierSum.fair += (clip(exp + (est[key] ?? 0)) - s.g.win) ** 2;
          brierSum.classic += (clip(overall + (tc[key] ?? 0)) - s.g.win) ** 2;
          brierSum.n++;
          const c = changed.get(key) ?? { n: 0, changed: 0 };
          c.n++;
          if (pa[key] !== pc[key]) c.changed++;
          changed.set(key, c);
        }
      }, (done, of) => foldPart(f, done, of));
    };
    for (let f = 0; f < opts.folds; f++) await scoringFold(f);
    const prior = base?.scoring;
    const ga = mergeIn(prior?.adjusted, groupOutcomes(adjusted)), gr = mergeIn(prior?.raw, groupOutcomes(raw));
    if (prior) {
      brierSum.fair += prior.brier.fair; brierSum.classic += prior.brier.classic; brierSum.n += prior.brier.n;
      for (const [key, c] of prior.changed) {
        const now = changed.get(key) ?? { n: 0, changed: 0 };
        changed.set(key, { n: now.n + c.n, changed: now.changed + c.changed });
      }
    }
    next.scoring = {
      adjusted: forBase(ga), raw: forBase(gr), brier: { ...brierSum }, changed: [...changed], penalties: perFold.map((x) => x.penalty),
      expected: { p: Float64Array.from(p), y: Float64Array.from(y), pb: Float64Array.from(pb) },
    };
    const la = lifts(ga, null, 10, PREGAME), lr = lifts(gr, null, 10, PREGAME);
    const adj = { fair: la.draft, classic: la.baseline, delta: la.draft - la.baseline, ...(await interval(ga, PREGAME)) };
    const brierOf = { fair: brierSum.n ? brierSum.fair / brierSum.n : NaN, classic: brierSum.n ? brierSum.classic / brierSum.n : NaN, n: brierSum.n };
    log(`runes and summoners against the draft: ${(adj.delta * 100).toFixed(2)} pts (${(adj.low * 100).toFixed(2)} to ${(adj.high * 100).toFixed(2)}) over today's scoring`);
    const informative = expected.logLoss < expected.baseLogLoss;
    const gateReason = !informative
      ? `the expected win rate predicts held-out results no better than the overall win rate (log loss ${expected.logLoss.toFixed(4)} against ${expected.baseLogLoss.toFixed(4)})`
      : !(adj.delta >= 0) ? `worse than today's scoring (${pts(adj.delta)})`
        : !(brierOf.fair <= brierOf.classic) ? `Brier ${brierOf.fair.toFixed(4)} is worse than today's scoring (${brierOf.classic.toFixed(4)})`
          : !(adj.low > 0) ? `not better with 95 % confidence (${pts(adj.low)} to ${pts(adj.high)})`
            : 'better with 95 % confidence, Brier no worse';
    const passed = informative && adj.low > 0 && brierOf.fair <= brierOf.classic;
    return {
      expected, adjusted: adj, raw: { fair: lr.draft, classic: lr.baseline, delta: lr.draft - lr.baseline }, brier: brierOf,
      decisions: la.per.map((d) => ({ key: d.key, fair: d.draft, classic: d.baseline, changed: (changed.get(d.key)?.changed ?? 0) / Math.max(1, changed.get(d.key)?.n ?? 0) })),
      gate: passed ? 'passed' : 'failed', gateReason,
    };
  }

  /**
   * Patch memory on the same held-out games as the draft model above, with its bandwidth and prior and each fold's
   * state model (trained on two-patch games, as in the app). Each fold's patch-memory model adds the fold's training
   * pilots' older games. The fade per patch is chosen on an even subsample, like the bandwidth.
   */
  async function evaluateMemory(mo: NonNullable<EvalOptions['memory']>): Promise<NonNullable<EvalReport['memory']>> {
    const older = (f: number) => olderGames.filter((g) => foldOf(g) !== f);
    // Two-patch games from before the previous patch (a region a patch behind) count with patch memory too.
    const olderCount = olderGames.length + games.filter((g) => { const a = ageOf(g); return a >= 2 && a < mo.patches; }).length;
    const withMemory = (decay: number): WeightOptions => ({ ...opts.weights, memory: { decay, patches: mo.patches } });
    type Side = { model: DraftModel; weighted: Weighted[]; cache: Map<string, Observation[]>; settings: DraftSettings };
    const sideOf = (model: DraftModel, weights: WeightOptions): Side => ({
      model, weighted: weighGames(model, data, regionPatch, weights), cache: new Map(),
      settings: { weights, estimate: { ...ESTIMATE_DEFAULTS, h: chosen.h, k: chosen.k } },
    });
    const draftOf = (side: Side, s: ScoredGame, baseline: BuildRecommendation, detail: boolean) =>
      recommendDraft(side.model, { lane: s.g.lane, opponent: s.g.opponent, enemies: s.g.enemies, allies: s.g.allies },
        data, excl, regionPatch, baseline, side.settings, { reasons: false, detail, weighted: side.weighted, cache: side.cache, decisions: SCORED });
    /**
     * One fold's held-out games: patch memory (the side `sideA` builds) against the two-patch model (`b`: its picks,
     * from the final comparison or sidePicks). followedDraft is patch memory's pick, followedBaseline the two-patch
     * model's. In detail every game and patch memory's Brier sums; else the grid's subsample. The side is built here, so
     * it goes when this call ends.
     */
    const compareSides = async (f: number, sideA: () => Side, b: Map<string, Picks>, testScored: ScoredGame[], detail: boolean, outcomes: Outcome[],
      brier = { d: 0, n: 0 }, progress: (done: number, of: number) => void = (done, of) => foldPart(f, done, of)) => {
      const a = sideA();
      const { baselineFor } = await baselinesOf(f);
      const at = detail ? detailAt(testScored) : subsampleAt(testScored);
      await team.forEach(at.length, (j) => {
        const s = testScored[at[j]];
        const baseline = baselineFor(s.g.lane, s.g.opponent);
        const rec = draftOf(a, s, baseline, detail);
        const est: Partial<Record<DecisionKey, number>> = {};
        if (detail) for (const [key, x] of Object.entries(actual(s)) as [DecisionKey, { option: string }][]) if (BRIER_KEYS.includes(key)) est[key] = rec.estimates?.[key]?.[x.option] ?? 0;
        return { pa: picksOf(rec.build), est };
      }, (j, r) => {
        const s = testScored[at[j]];
        const pa = r.pa, pb = b.get(s.g.matchId) ?? {};
        for (const [key, x] of Object.entries(actual(s)) as [DecisionKey, { option: string; wpa: number }][]) {
          outcomes.push({ pilot: s.g.pilot, key, wpa: x.wpa, followedDraft: pa[key] === x.option, followedBaseline: pb[key] === x.option });
          if (detail && BRIER_KEYS.includes(key)) {
            brier.d += (clip(s.g.win - x.wpa + (r.est[key] ?? 0)) - s.g.win) ** 2;
            brier.n++;
          }
        }
      }, progress);
      return brier;
    };
    /**
     * The picks of the side `sideOf` builds on a fold's held-out games, the grid's subsample (compareSides' `b`, kept
     * by rank 0): the first half of the fold. The side is built here, so it goes when this call ends.
     */
    const sidePicks = async (f: number, sideOf: () => Side, testScored: ScoredGame[]) => {
      const side = sideOf();
      const { baselineFor } = await baselinesOf(f);
      const at = subsampleAt(testScored);
      const picks = new Map<string, Picks>();
      await team.forEach(at.length, (j) => {
        const s = testScored[at[j]];
        return picksOf(draftOf(side, s, baselineFor(s.g.lane, s.g.opponent), false).build);
      }, (j, p) => { picks.set(testScored[at[j]].g.matchId, p); }, (done, of) => part(f + 0.5 * (of > 0 ? done / of : 1), opts.folds));
      return picks;
    };
    const memoryLift = async (g: GroupedOutcomes): Promise<MemoryLift> => {
      const r = lifts(g);
      return { memory: r.draft, classic: r.baseline, delta: r.draft - r.baseline, ...(await interval(g)) };
    };
    const none: MemoryLift = { memory: 0, classic: 0, delta: 0, low: NaN, high: NaN };
    const report = (decay: number, grid: NonNullable<EvalReport['memory']>['grid'], full: NonNullable<EvalReport['memory']>['full'],
      early: NonNullable<EvalReport['memory']>['early'], gateReason: string, passed: boolean) => ({
      patches: mo.patches, olderGames: olderCount, decay, grid, full, early, gate: passed ? 'passed' as const : 'failed' as const, gateReason,
    });

    if (olderCount < mo.minOlderGames || testGames < opts.minGames) {
      for (let i = 0; i < (up ? 2 : opts.folds + 2); i++) step('Patch memory: not enough games to compare');
      const reason = olderCount < mo.minOlderGames ? `only ${olderCount} games from older patches (needs ${mo.minOlderGames})`
        : `only ${testGames} held-out games (needs ${opts.minGames})`;
      return report(mo.decays[0] ?? 0, [], { ...none, brier: NaN, classicBrier: NaN }, { ...none, currentGames: 0, keptGames: 0 }, reason, false);
    }
    log(`patch memory: ${olderCount} games from older patches (within ${mo.patches} patches)`);

    // The fade per patch, on the grid's subsample.
    const gridOutcomes = mo.decays.map(() => [] as Outcome[]);
    /** Patch memory's grid: fold f, in a call of its own (compareFold); its step's start collects what the last one left. */
    const memoryGridFold = async (f: number) => {
      step(`Patch memory: fold ${f + 1} of ${opts.folds} (${mo.decays.length} fades per patch)`);
      const m = stateModels[f];
      const model = buildFold(f, scored([...folds[f].train, ...older(f)], f), m);
      const testScored = scored(folds[f].test, f);
      for (const [i, decay] of mo.decays.entries()) {
        // The fades share the model; each its own weights and observations, the last one's collected first.
        if (i > 0) collect();
        await compareSides(f, () => sideOf(model, withMemory(decay)), classicPicks!, testScored, false, gridOutcomes[i], undefined,
          (done, of) => part(i + done / of, mo.decays.length));
      }
    };
    // A quick update keeps its base's fades and choice.
    let grid: NonNullable<EvalReport['memory']>['grid'], decay: number;
    if (up) { grid = base?.report.memory?.grid ?? []; decay = up.decay ?? mo.decays[0]; }
    else {
      for (let f = 0; f < opts.folds; f++) await memoryGridFold(f);
      grid = mo.decays.map((decay, i) => {
        const r = lifts(groupOutcomes(gridOutcomes[i]));
        gridOutcomes[i] = [];
        log(`patch memory, fade ${decay} per patch: lift ${(r.draft * 100).toFixed(2)} pts, two patches ${(r.baseline * 100).toFixed(2)} pts`);
        return { decay, memory: r.draft, classic: r.baseline, delta: r.draft - r.baseline };
      });
      // Only rank 0 holds the outcomes: the others get its choice.
      const fades = grid;
      decay = await team.share(() => fades.reduce((b, g) => (g.delta > b.delta ? g : b), fades[0]).decay);
    }

    // Every held-out game, with every training game.
    step(`Patch memory on ${heldOutLabel()}`);
    const fullOutcomes: Outcome[] = [];
    const fullBrier = { d: 0, n: 0 };
    /** Patch memory on every held-out game: fold f, in a call of its own (compareFold). */
    const memoryFullFold = async (f: number) => {
      collect();
      if (idle(f)) return;
      const m = stateModels[f];
      const model = buildFold(f, scored([...folds[f].train, ...older(f)], f), m);
      await compareSides(f, () => sideOf(model, withMemory(decay)), classicPicks!, scored(folds[f].test, f), true, fullOutcomes, fullBrier);
    };
    for (let f = 0; f < opts.folds; f++) await memoryFullFold(f);
    const fullG = mergeIn(base?.memory?.full, groupOutcomes(fullOutcomes));
    if (base?.memory) { fullBrier.d += base.memory.brier.d; fullBrier.n += base.memory.brier.n; }
    const full = { ...(await memoryLift(fullG)), brier: fullBrier.n ? fullBrier.d / fullBrier.n : NaN, classicBrier: brier.draft };

    // Early in a patch: a quarter of the current patch's training games (the same quarter for both models; the same
    // state model and lane baselines), the grid's subsample.
    step('Patch memory early in a patch');
    const earlyOutcomes: Outcome[] = [];
    let currentGames = 0, keptGames = 0;
    /** Early in a patch: fold f, in a call of its own (compareFold). */
    const earlyFold = async (f: number) => {
      collect();
      const m = stateModels[f];
      const train = folds[f].train.filter((g) => {
        if (ageOf(g) !== 0) return true;
        currentGames++;
        const kept = hash(`${opts.seed}:${g.matchId}`) % 4 === 0;
        if (kept) keptGames++;
        return kept;
      });
      // A quick update with none of its new games in this fold's subsample: nothing to compare.
      if (fresh && up && !folds[f].test.some((g) => fresh.has(g) && baseHash(salt, `sample:${g.matchId}`) % up.gridStride === 0)) return;
      // One model at a time (0.12.0): the two-patch model's picks first, then patch memory's against them.
      const testScored = scored(folds[f].test, f);
      const twoPatchPicks = await sidePicks(f, () => sideOf(buildFold(f, scored(train, f), m), opts.weights), testScored);
      collect();
      await compareSides(f, () => sideOf(buildFold(f, scored([...train, ...older(f)], f), m), withMemory(decay)), twoPatchPicks, testScored, false, earlyOutcomes, undefined,
        (done, of) => part(f + 0.5 + 0.5 * (of > 0 ? done / of : 1), opts.folds));
    };
    for (let f = 0; f < opts.folds; f++) await earlyFold(f);
    const earlyG = mergeIn(base?.memory?.early, groupOutcomes(earlyOutcomes));
    next.memory = { full: forBase(fullG), brier: { ...fullBrier }, early: forBase(earlyG) };
    const early = { ...(await memoryLift(earlyG)), currentGames: Math.round(currentGames / Math.max(1, opts.folds - 1)), keptGames: Math.round(keptGames / Math.max(1, opts.folds - 1)) };
    log(`patch memory: all games ${(full.delta * 100).toFixed(2)} pts (${(full.low * 100).toFixed(2)} to ${(full.high * 100).toFixed(2)}), early in a patch ${(early.delta * 100).toFixed(2)} pts (${(early.low * 100).toFixed(2)} to ${(early.high * 100).toFixed(2)})`);

    const ci = (x: MemoryLift) => `${(x.low * 100).toFixed(2)} to ${(x.high * 100).toFixed(2)} pts`;
    const gateReason = !(full.delta >= 0) ? `worse than two patches alone with all games (${(full.delta * 100).toFixed(2)} pts)`
      : !(full.brier <= full.classicBrier) ? `Brier ${full.brier.toFixed(4)} is worse than two patches alone (${full.classicBrier.toFixed(4)})`
        : !(full.low > 0 || early.low > 0) ? `not better with 95 % confidence (all games ${ci(full)}, early in a patch ${ci(early)})`
          : `better with 95 % confidence ${full.low > 0 ? 'with all games' : 'early in a patch'}, no worse otherwise, Brier no worse`;
    const passed = full.delta >= 0 && full.brier <= full.classicBrier && (full.low > 0 || early.low > 0);
    return report(decay, grid, full, early, gateReason, passed);
  }

  /**
   * Late-game swaps (0.17.0, docs/SWAPS.md): at the held-out games' late full-inventory shop visits, did those that did
   * what the draft model advises for the item they held (from their draft, as the Items view shows it before the game)
   * do better than those that didn't, more than keeping every item does? Each game's decision points count once
   * together (their weights); the pilot bootstrap gives the interval. It passes when the interval is above zero.
   */
  async function evaluateSwaps(): Promise<SwapReport> {
    step(`Late-game swaps on ${heldOutLabel()}`);
    const outcomes: Outcome[] = [];
    const counts = { situations: 0, games: 0, swapped: 0, advised: 0 };
    const o: EstimateOptions = { ...ESTIMATE_DEFAULTS, h: chosen.h, k: chosen.k };
    /** Late-game swaps: fold f, in a call of its own (compareFold). */
    const swapsFold = async (f: number) => {
      collect();
      if (idle(f)) return;
      const testScored = scored(folds[f].test, f);
      const at = detailAt(testScored).filter((i) => testScored[i].g.swaps.length > 0);
      if (!at.length) return;
      const model = buildFold(f, scored(folds[f].train, f), stateModels[f]);
      const weighted = weighGames(model, data, regionPatch, opts.weights);
      const cache = new Map<string, Observation[]>();
      await team.forEach(at.length, (j) => {
        // A game's advice for each item it held, once per item, wherever the team works it out...
        const g = testScored[at[j]].g;
        const q = itemQueryOf(model, { lane: g.lane, opponent: g.opponent, enemies: g.enemies, allies: g.allies }, o.h);
        const advice = new Map<number, string>();
        return g.swaps.map((d) => {
          let a = advice.get(d.held);
          if (a === undefined) {
            const r = adviseSwap(model, weighted, q, d.held, o, cache);
            a = !r || r.pick.kind === 'keep' ? String(SWAP_KEEP) : r.pick.kind === 'sell' ? String(SWAP_SELL) : String(r.pick.item);
            advice.set(d.held, a);
          }
          return a;
        });
      }, (j, picks) => {
        // ...and added up here, in game order.
        const s = testScored[at[j]];
        counts.games++;
        s.g.swaps.forEach((d, k) => {
          counts.situations++;
          if (d.item !== SWAP_KEEP) counts.swapped++;
          if (picks[k] !== String(SWAP_KEEP)) counts.advised++;
          outcomes.push({ pilot: s.g.pilot, key: 'swap', wpa: s.swapWpa![k], followedDraft: picks[k] === String(d.item), followedBaseline: d.item === SWAP_KEEP, w: d.w });
        });
      }, (done, of) => foldPart(f, done, of));
    };
    for (let f = 0; f < opts.folds; f++) await swapsFold(f);
    const prior = base?.swaps;
    if (prior) for (const k of ['situations', 'games', 'swapped', 'advised'] as const) counts[k] += prior[k];
    const g = mergeIn(prior?.g, groupOutcomes(outcomes, SWAP_KEYS));
    next.swaps = { g: forBase(g), ...counts };
    const failed = (gateReason: string, lift = { advice: 0, keep: 0, delta: 0, low: NaN, high: NaN }): SwapReport => ({
      lift, situations: counts.situations, games: counts.games,
      swapped: counts.situations ? counts.swapped / counts.situations : 0, advised: counts.situations ? counts.advised / counts.situations : 0,
      gate: 'failed', gateReason,
    });
    // Only rank 0 counted (it adds the outcomes up): every thread goes the same way from here, the interval being shared.
    const counted = await team.share(() => counts.games);
    if (counted < opts.minGames) return failed(`only ${counted} held-out games with a late full-inventory shop visit (needs ${opts.minGames})`);
    const r = lifts(g);
    const lift = { advice: r.draft, keep: r.baseline, delta: r.draft - r.baseline, ...(await interval(g)) };
    log(`late-game swaps: ${pts(lift.delta)} (${pts(lift.low)} to ${pts(lift.high)}) over keeping every item, ${counts.situations} situations in ${counts.games} games`);
    if (!(lift.delta >= 0)) return failed(`worse than keeping every item (${pts(lift.delta)})`, lift);
    if (!(lift.low > 0)) return failed(`not better than keeping every item with 95 % confidence (${pts(lift.low)} to ${pts(lift.high)})`, lift);
    return { ...failed('', lift), gate: 'passed', gateReason: 'better than keeping every item with 95 % confidence' };
  }

  /**
   * Fine-tuning against the chosen draft model (0.15.0), on the same held-out games, state models and lane baselines:
   * pilot standing's strength, the item decisions' own prior and a fade within the current patch. Each setting alone
   * on the grid's subsample; the best of each that did better there, together, on every held-out game. It passes with
   * a lift above the chosen model's at 95 % confidence and a Brier no worse; until then the app keeps today's settings.
   */
  async function evaluateTuning(t: NonNullable<EvalOptions['tuning']>): Promise<TuningReport> {
    const failed = (gateReason: string, grid: TuningReport['grid'] = [], choice = UNTUNED, full: TuningReport['full'] = null): TuningReport =>
      ({ grid, choice, full, gate: 'failed', gateReason });
    if (testGames < opts.minGames || !classicPicks) {
      for (let i = 0; i < opts.folds + 1; i++) step('Fine-tuning: not enough games');
      return failed(`only ${testGames} held-out games (needs ${opts.minGames})`);
    }
    const settingsOf = (c: TuningChoice): DraftSettings => ({
      weights: { ...opts.weights, pilotStrength: c.pilotStrength, recencyDays: c.recencyDays },
      estimate: { ...ESTIMATE_DEFAULTS, h: chosen.h, k: chosen.k },
      ...(c.itemK !== null ? { itemK: c.itemK } : {}),
    });
    const variants: { setting: keyof TuningChoice; value: number }[] = [
      ...t.pilotStrengths.filter((v) => v !== 1).map((value) => ({ setting: 'pilotStrength' as const, value })),
      ...opts.ks.filter((k) => k !== chosen.k).map((value) => ({ setting: 'itemK' as const, value })),
      ...t.recencyDays.map((value) => ({ setting: 'recencyDays' as const, value })),
    ];
    /**
     * One fold's held-out games under `settings` against the chosen model's picks (classicPicks): followedDraft is the
     * tuned pick, followedBaseline the chosen model's. In detail every game and the tuned Brier sums; else the grid's
     * subsample. Its weights and observations go when the call ends. `only` (0.15.1, an item prior alone): just these
     * decisions are worked out; the others are the chosen model's picks, which the same weights and prior give them.
     */
    const compareTuned = async (f: number, model: DraftModel, settings: DraftSettings, testScored: ScoredGame[], detail: boolean,
      outcomes: Outcome[], brierSum = { d: 0, n: 0 }, progress: (done: number, of: number) => void, only: ReadonlySet<string> = SCORED) => {
      const weighted = weighGames(model, data, regionPatch, settings.weights);
      const cache = new Map<string, Observation[]>();
      const { baselineFor } = await baselinesOf(f);
      const at = detail ? detailAt(testScored) : subsampleAt(testScored);
      await team.forEach(at.length, (j) => {
        const s = testScored[at[j]];
        const rec = recommendDraft(model, { lane: s.g.lane, opponent: s.g.opponent, enemies: s.g.enemies, allies: s.g.allies },
          data, excl, regionPatch, baselineFor(s.g.lane, s.g.opponent), settings, { reasons: false, detail, weighted, cache, decisions: only });
        const est: Partial<Record<DecisionKey, number>> = {};
        if (detail) for (const [key, x] of Object.entries(actual(s)) as [DecisionKey, { option: string }][]) if (BRIER_KEYS.includes(key)) est[key] = rec.estimates?.[key]?.[x.option] ?? 0;
        return { pa: picksOf(rec.build), est };
      }, (j, r) => {
        const s = testScored[at[j]];
        const pb = classicPicks.get(s.g.matchId) ?? {};
        for (const [key, x] of Object.entries(actual(s)) as [DecisionKey, { option: string; wpa: number }][]) {
          const tuned = only.has(key) ? r.pa[key] : pb[key];
          outcomes.push({ pilot: s.g.pilot, key, wpa: x.wpa, followedDraft: tuned === x.option, followedBaseline: pb[key] === x.option });
          if (detail && BRIER_KEYS.includes(key)) { brierSum.d += (clip(s.g.win - x.wpa + (r.est[key] ?? 0)) - s.g.win) ** 2; brierSum.n++; }
        }
      }, progress);
      return brierSum;
    };

    // A quick update confirms its base's choice again with the new games, when the base confirmed one; else it keeps
    // the base's verdict as it is.
    if (up && !(up.tuning && base?.report.tuning?.full)) {
      step('Fine-tuning: as the full evaluation left it');
      return base?.report.tuning ?? failed('not evaluated by the full evaluation');
    }
    // Each setting alone, on the grid's subsample. A fold's draft model doesn't depend on the weights: one per fold.
    const gridOutcomes = variants.map(() => [] as Outcome[]);
    const tuningFold = async (f: number) => {
      step(`Fine-tuning: fold ${f + 1} of ${opts.folds} (${variants.length} settings)`);
      const model = buildFold(f, scored(folds[f].train, f), stateModels[f]);
      const testScored = scored(folds[f].test, f);
      for (const [i, v] of variants.entries()) {
        if (i > 0) collect();
        await compareTuned(f, model, settingsOf({ ...UNTUNED, [v.setting]: v.value }), testScored, false, gridOutcomes[i], undefined,
          (done, of) => part(i + done / of, variants.length), v.setting === 'itemK' ? SCORED_ITEMS : SCORED);
      }
    };
    let grid: TuningReport['grid'] = [], choice: TuningChoice;
    if (up) { grid = base!.report.tuning!.grid; choice = up.tuning!; }
    else {
    for (let f = 0; f < opts.folds; f++) await tuningFold(f);
    grid = variants.map((v, i) => {
      const r = lifts(groupOutcomes(gridOutcomes[i]));
      gridOutcomes[i] = [];
      log(`fine-tuning, ${v.setting} ${v.value}: lift ${(r.draft * 100).toFixed(2)} pts, chosen model ${(r.baseline * 100).toFixed(2)} pts`);
      return { ...v, tuned: r.draft, current: r.baseline, delta: r.draft - r.baseline };
    });
    // The best value of each setting, where it did better than today's; only rank 0 holds the outcomes.
    const tried = grid;
    choice = await team.share(() => {
      const c: TuningChoice = { ...UNTUNED };
      for (const setting of ['pilotStrength', 'itemK', 'recencyDays'] as const) {
        const best = tried.filter((g) => g.setting === setting).reduce<(typeof grid)[number] | null>((b, g) => (g.delta > 0 && (!b || g.delta > b.delta) ? g : b), null);
        if (best) c[setting] = best.value;
      }
      return c;
    });
    if (choice.pilotStrength === 1 && choice.itemK === null && choice.recencyDays === null) {
      step('Fine-tuning: no setting did better');
      return failed('no setting did better than the chosen model on the subsample', grid);
    }
    }

    step(`Fine-tuning on ${heldOutLabel()}`);
    const fullOutcomes: Outcome[] = [];
    const fullBrier = { d: 0, n: 0 };
    const fullFold = async (f: number) => {
      collect();
      if (idle(f)) return;
      const model = buildFold(f, scored(folds[f].train, f), stateModels[f]);
      await compareTuned(f, model, settingsOf(choice), scored(folds[f].test, f), true, fullOutcomes, fullBrier, (done, of) => foldPart(f, done, of));
    };
    for (let f = 0; f < opts.folds; f++) await fullFold(f);
    const g = mergeIn(base?.tuning?.full, groupOutcomes(fullOutcomes));
    if (base?.tuning) { fullBrier.d += base.tuning.brier.d; fullBrier.n += base.tuning.brier.n; }
    next.tuning = { full: forBase(g), brier: { ...fullBrier } };
    const r = lifts(g);
    const full = { tuned: r.draft, current: r.baseline, delta: r.draft - r.baseline, ...(await interval(g)), brier: fullBrier.n ? fullBrier.d / fullBrier.n : NaN, currentBrier: brier.draft };
    log(`fine-tuning: ${(full.delta * 100).toFixed(2)} pts (${(full.low * 100).toFixed(2)} to ${(full.high * 100).toFixed(2)})`);
    const passed = full.low > 0 && full.brier <= full.currentBrier;
    const gateReason = !(full.low > 0) ? `not better than the chosen model with 95 % confidence (${(full.low * 100).toFixed(2)} to ${(full.high * 100).toFixed(2)} pts)`
      : !(full.brier <= full.currentBrier) ? `Brier ${full.brier.toFixed(4)} is worse than the chosen model's ${full.currentBrier.toFixed(4)}`
        : 'better than the chosen model with 95 % confidence, Brier no worse';
    return passed ? { grid, choice, full, gate: 'passed', gateReason } : failed(gateReason, grid, choice, full);
  }

  // ---- Live stability: replay held-out timelines minute by minute ----------------------------------------
  let stability: EvalReport['stability'] = up ? base?.report.stability ?? null : null;
  if (opts.live && !up) {
    const settings: DraftSettings = { weights: opts.weights, estimate: { ...ESTIMATE_DEFAULTS, h: chosen.h, k: chosen.k } };
    // Every game is replayed against every training game, minute by minute: an even subsample keeps it to seconds.
    step(`Replaying ${replayIds.size.toLocaleString('en-US')} held-out games minute by minute`);
    const replayTimelines = ctx.replayTimelines();
    let flips = 0, slotChanges = 0, replayed = 0, minutes = 0;
    /** The replay: fold f, in a call of its own (compareFold). */
    const replayFold = async (f: number) => {
      collect();
      const fold = foldModel(f);
      // Per item slot: its observations, and the contexts they're compared with (an observation's row is its context's
      // index), so each minute's similarities are one pass over a flat array (Query.sims), with the same numbers.
      const obsBySlot = new Map<number, { obs: Observation[]; zs: Contexts }>();
      const obsFor = (slot: number) => {
        let o = obsBySlot.get(slot);
        if (!o) {
          const { zItems, itemRow } = fold.model;
          const at: number[] = [];
          const obs = observed(fold.weighted.flatMap(({ i, s, base, patch, age }) => {
            const c = s.g.items[slot];
            if (!c) return [];
            const w = base * patch([c.item]);
            if (w <= 0) return [];
            at.push(itemRow[i] + slot);
            return [{ option: String(c.item), wpa: s.itemWpa[slot], win: s.g.win, w, lane: s.g.lane, opponent: s.g.opponent, pilot: s.g.pilot, zs: zItems, zRow: itemRow[i] + slot, row: at.length - 1, age }];
          }), opts.weights);
          // Their contexts in their order, one flat array: each minute's similarities are one pass over it.
          const zs = contextsOf(at.length, zItems.d, (out, r, to) => out.set(zItems.flat.subarray(at[r] * zItems.d, at[r] * zItems.d + zItems.d), to));
          o = { obs, zs };
          obsBySlot.set(slot, o);
        }
        return o;
      };
      const at = strided(fold.testScored.length, liveStride);
      await team.forEach(at.length, (j) => {
        // One game replayed minute by minute: how often "Next" changed within a slot, and the slots it went through.
        const s = fold.testScored[at[j]];
        const row = rowsById.get(s.g.matchId);
        const raw = replayTimelines ? replayTimelines.get(s.g.matchId) ?? null : row?.timeline ?? null;
        const tl = row && raw ? gameTimeline({ ...row, timeline: raw }, partsByMatch.get(s.g.matchId) ?? [], (id) => data.items.get(id)?.goldTotal ?? 0) : null;
        if (!tl) return null;
        const times = tl.tg.cap.t.filter((t) => t >= 3 * 60_000);
        const moments = momentsAt(tl, times, profiles, data);
        const zDraft = standardise(s.g.draft, fold.model.draftScale);
        const stable = new NextStability(STABILITY_DEFAULTS);
        let lastSlot = -1;
        const r = { minutes: 0, flips: 0, slotChanges: 0 };
        for (const mo of moments) {
          const slot = mo.owned.filter((id) => isCompletedItem(data.items.get(id))).length;
          if (slot >= 6) break;
          r.minutes++;
          const { obs, zs } = obsFor(slot);
          const z = [...zDraft, ...standardise(mo.live, fold.model.liveScale)];
          const sims = Number.isFinite(chosen.h) ? { h: chosen.h, values: similarities(zs, z, chosen.h) } : undefined;
          const est = estimateDecision(obs, { lane: s.g.lane, opponent: s.g.opponent, z, sims }, settings.estimate);
          const options = est.options.filter((o) => isPurchasableItem(data.items.get(Number(o.option))) && excl.blocker(mo.owned, Number(o.option)) === null);
          const changed = stable.step(slot, options, null);
          if (changed && slot === lastSlot) r.flips++;
          if (slot !== lastSlot) { if (lastSlot >= 0) r.slotChanges++; lastSlot = slot; }
        }
        return r;
      }, (_, r) => {
        if (!r) return;
        replayed++; minutes += r.minutes; flips += r.flips; slotChanges += r.slotChanges;
      }, (done, of) => foldPart(f, done, of));
    };
    for (let f = 0; f < opts.folds; f++) await replayFold(f);
    stability = { games: replayed, minutes, flipsPerGame: replayed ? flips / replayed : 0, slotChangesPerGame: replayed ? slotChanges / replayed : 0 };
  }

  // ---- Yours: rec_log ---------------------------------------------------------------------------------------
  let yours: EvalReport['yours'] = null;
  const linked = ctx.recLog.filter((r) => r.match_id && r.bought && r.phase === 'draft');
  if (linked.length) {
    const followed = { games: 0, wins: 0 }, other = { games: 0, wins: 0 };
    for (const r of linked) {
      const shown = JSON.parse(r.shown) as { keystone: number | null; core: number[] };
      const bought = JSON.parse(r.bought!) as { keystone: number; purchases: number[] };
      const did = shown.keystone === bought.keystone && shown.core.length > 0 && bought.purchases.includes(shown.core[0]);
      const g = did ? followed : other;
      g.games++; g.wins += r.win ?? 0;
    }
    yours = { linked: linked.length, followed, other };
  }

  const gateReason = testGames < opts.minGames ? `only ${testGames} held-out games (needs ${opts.minGames})`
    : !(lift.low > 0) ? `the lift over the baseline's 95 % interval reaches ${(lift.low * 100).toFixed(2)} pts (needs > 0)`
      : !(brier.draft <= brier.baseline) ? `Brier ${brier.draft.toFixed(4)} is worse than the baseline's ${brier.baseline.toFixed(4)}`
        : 'lift above zero with 95 % confidence, Brier no worse';
  const passed = testGames >= opts.minGames && lift.low > 0 && brier.draft <= brier.baseline;

  const report: EvalReport = {
    evaluatedAt: Date.now(), games: games.length, pilots, folds: opts.folds,
    state: stateReport,
    grid, chosen, decisions: l.per, lift, brier, stability, yours, memory, scoring, tuning, swaps,
    gate: passed ? 'passed' : 'failed', gateReason,
    ...(fresh ? { update: { fullAt: 0, newGames: fresh.size, reason: '', missing, changed: changedGames } } : {}),
  };
  if (ctx.onBase) {
    // The held-out games measured: the base's and the new ones (a full evaluation's: all of them).
    const list = [...(fresh ?? games)];
    const measuredNow = Float64Array.from(list, (g) => baseHash(salt, g.matchId));
    const printsNow = Float64Array.from(list, (g) => gamePrint(salt, g));
    const join = (a: Float64Array | undefined, b: Float64Array) => {
      if (!a) return b;
      const out = new Float64Array(a.length + b.length);
      out.set(a); out.set(b, a.length);
      return out;
    };
    ctx.onBase({ ...next, known: join(up?.known, measuredNow), prints: join(up?.prints, printsNow), report });
  }
  return report;
}

const pts = (x: number) => `${x >= 0 ? '+' : ''}${(x * 100).toFixed(2)} pts`;

/** The report as text (what npm run eval:builds prints and saves). */
export function formatEval(r: EvalReport): string {
  const lines: string[] = [];
  lines.push(`Irelia Companion build evaluation · ${new Date(r.evaluatedAt).toISOString().slice(0, 16)}`);
  lines.push(`${r.games} games from ${r.pilots} pilots, ${r.folds}-fold cross-validation grouped by pilot`, '');
  if (r.grouping) {
    lines.push(`Grouped drafts (a trial, docs/EVAL-SCALING.md): ${r.grouping.groups.toLocaleString('en-US')} groups of ${r.grouping.games.toLocaleString('en-US')} games, `
      + `spread ${r.grouping.spread.toFixed(3)} sd around their centre. Close to an exact evaluation's numbers, not the same.`, '');
  }
  lines.push('Game-state model (B2)', '---------------------');
  if (r.state) {
    lines.push(`λ = ${r.state.lambda}: Brier ${r.state.brier.toFixed(4)} (always the base rate: ${r.state.baseBrier.toFixed(4)}), log loss ${r.state.logLoss.toFixed(4)} (${r.state.baseLogLoss.toFixed(4)}) on ${r.state.frames} held-out frames`);
    lines.push('  calibration: ' + r.state.calibration.map((b) => `${Math.round(b.from * 100)}-${Math.round(b.to * 100)} %: ${Math.round(b.observed * 100)} % of ${b.n}`).join(' · '));
  } else lines.push('not enough timelines to fit it');
  lines.push('', 'Draft-adjusted vs lane baseline (B3)', '------------------------------------');
  lines.push('Lift = WPA of held-out games that followed a model\'s pick minus those that didn\'t (estimates).');
  for (const g of r.grid) lines.push(`  h=${g.h} k=${g.k}: draft ${pts(g.draft)}, baseline ${pts(g.baseline)}, difference ${pts(g.delta)}`);
  lines.push(`chosen: h=${r.chosen.h}, k=${r.chosen.k}`);
  for (const d of r.decisions) {
    lines.push(`  ${d.key.padEnd(14)} draft ${pts(d.draft).padStart(11)} (followed ${Math.round(d.followedDraft * 100)} %) · baseline ${pts(d.baseline).padStart(11)} (followed ${Math.round(d.followedBaseline * 100)} %)`);
  }
  lines.push(`lift: draft ${pts(r.lift.draft)}, baseline ${pts(r.lift.baseline)}, difference ${pts(r.lift.delta)} (95 % CI ${pts(r.lift.low)} to ${pts(r.lift.high)}, pilot bootstrap)`);
  lines.push(`Brier on the option taken: draft ${r.brier.draft.toFixed(4)}, baseline ${r.brier.baseline.toFixed(4)} (${r.brier.n} decisions)`);
  if (r.lift.withinPilot) {
    const w = r.lift.withinPilot;
    lines.push(`within each pilot (reported only): draft ${pts(w.draft)}, baseline ${pts(w.baseline)}, difference ${pts(w.delta)}`);
  }
  if (r.stability) lines.push('', 'Live stability (B5)', '-------------------', `${r.stability.games} held-out timelines, ${r.stability.minutes} minutes: "Next" flips ${r.stability.flipsPerGame.toFixed(2)} times per game within a slot (${r.stability.slotChangesPerGame.toFixed(1)} slot changes per game)`);
  if (r.memory) {
    const m = r.memory;
    lines.push('', 'Patch memory', '------------');
    lines.push(`Games of up to ${m.patches} patches (today's included), fading per patch, against today's and the previous patch alone; ${m.olderGames} games from older patches.`);
    if (m.grid.length) {
      for (const g of m.grid) lines.push(`  fade ${g.decay} per patch: patch memory ${pts(g.memory)}, two patches ${pts(g.classic)}, difference ${pts(g.delta)}`);
      lines.push(`chosen: fade ${m.decay} per patch`);
      lines.push(`all games: patch memory ${pts(m.full.memory)}, two patches ${pts(m.full.classic)}, difference ${pts(m.full.delta)} (95 % CI ${pts(m.full.low)} to ${pts(m.full.high)})`);
      lines.push(`early in a patch (${m.early.keptGames} of ${m.early.currentGames} current-patch games): patch memory ${pts(m.early.memory)}, two patches ${pts(m.early.classic)}, difference ${pts(m.early.delta)} (95 % CI ${pts(m.early.low)} to ${pts(m.early.high)})`);
      lines.push(`Brier on the option taken: patch memory ${m.full.brier.toFixed(4)}, two patches ${m.full.classicBrier.toFixed(4)}`);
    }
    lines.push(`Patch memory: ${m.gate.toUpperCase()} — ${m.gateReason}`);
  }
  if (r.scoring) {
    const c = r.scoring;
    lines.push('', 'Runes and summoners against the draft', '-------------------------------------');
    lines.push('Keystone, secondary tree, summoners, start and skill maxed first scored against each game\'s expected win rate before it started (draft, lane, patch, region, pilot standing, lane opponent), against today\'s scoring by the overall win rate.');
    if (c.expected) lines.push(`expected win rate on ${c.expected.games} held-out games: Brier ${c.expected.brier.toFixed(4)} (overall win rate ${c.expected.baseBrier.toFixed(4)}), log loss ${c.expected.logLoss.toFixed(4)} (${c.expected.baseLogLoss.toFixed(4)})${c.expected.penalty ? `; penalties ${c.expected.penalty.lambda} (draft, pilot, patch, region) and ${c.expected.penalty.opponentLambda} (lane opponent)` : ''}`);
    if (c.decisions.length) {
      lines.push('Lift = how much more often than its expected win rate a held-out game that followed the pick was won, minus those that didn\'t.');
      for (const d of c.decisions) lines.push(`  ${d.key.padEnd(14)} against the draft ${pts(d.fair).padStart(11)} · today ${pts(d.classic).padStart(11)} · picks differ in ${Math.round(d.changed * 100)} % of games`);
      lines.push(`lift: against the draft ${pts(c.adjusted.fair)}, today ${pts(c.adjusted.classic)}, difference ${pts(c.adjusted.delta)} (95 % CI ${pts(c.adjusted.low)} to ${pts(c.adjusted.high)}, pilot bootstrap)`);
      lines.push(`by the overall win rate instead: against the draft ${pts(c.raw.fair)}, today ${pts(c.raw.classic)}, difference ${pts(c.raw.delta)}`);
      lines.push(`Brier on the option taken: against the draft ${c.brier.fair.toFixed(4)}, today ${c.brier.classic.toFixed(4)} (${c.brier.n} decisions)`);
    }
    lines.push(`Runes and summoners against the draft: ${c.gate.toUpperCase()} — ${c.gateReason}`);
  }
  if (r.tuning) {
    const t = r.tuning;
    const name = { pilotStrength: 'pilot standing\'s strength', itemK: 'item decisions\' prior', recencyDays: 'fade within a patch (days to half)' } as const;
    lines.push('', 'Fine-tuning', '-----------');
    lines.push('Each setting against the chosen draft model on the same held-out games; the better ones together on all of them.');
    for (const g of t.grid) lines.push(`  ${name[g.setting]} ${g.value}: tuned ${pts(g.tuned)}, chosen ${pts(g.current)}, difference ${pts(g.delta)}`);
    lines.push(`chosen: strength ${t.choice.pilotStrength}, item prior ${t.choice.itemK ?? 'as the rest'}, fade ${t.choice.recencyDays ?? 'none'}`);
    if (t.full) {
      lines.push(`all games: tuned ${pts(t.full.tuned)}, chosen ${pts(t.full.current)}, difference ${pts(t.full.delta)} (95 % CI ${pts(t.full.low)} to ${pts(t.full.high)}, pilot bootstrap)`);
      lines.push(`Brier on the option taken: tuned ${t.full.brier.toFixed(4)}, chosen ${t.full.currentBrier.toFixed(4)}`);
    }
    lines.push(`Fine-tuning: ${t.gate.toUpperCase()} — ${t.gateReason}`);
  }
  if (r.swaps) {
    const w = r.swaps;
    lines.push('', 'Late-game swaps', '---------------');
    lines.push('At shop visits from 20:00 with every slot full: those that did what the draft model advises for the item they held (keep it, or sell it for another) against those that didn\'t, compared with keeping every item; each game\'s visits count once together.');
    lines.push(`${w.situations} situations in ${w.games} held-out games; sold in ${Math.round(w.swapped * 100)} %, advised to sell in ${Math.round(w.advised * 100)} %`);
    if (Number.isFinite(w.lift.low)) lines.push(`lift: the advice ${pts(w.lift.advice)}, keeping ${pts(w.lift.keep)}, difference ${pts(w.lift.delta)} (95 % CI ${pts(w.lift.low)} to ${pts(w.lift.high)}, pilot bootstrap)`);
    lines.push(`Late-game swaps: ${w.gate.toUpperCase()} — ${w.gateReason}`);
  }
  lines.push('', 'Yours', '-----');
  if (!r.yours) lines.push('no game of yours is linked to a shown build yet');
  else lines.push(`${r.yours.linked} linked games${r.yours.linked < 20 ? ' (needs 20 to compare)' : ''}: followed the draft build ${r.yours.followed.games} (${r.yours.followed.wins} won), didn't ${r.yours.other.games} (${r.yours.other.wins} won)`);
  lines.push('', `Gate: ${r.gate.toUpperCase()} — ${r.gateReason}`);
  lines.push(r.gate === 'passed' ? 'The app now shows the draft-adjusted build by default (you can still switch).' : 'The app keeps the lane baseline by default (you can still switch to the draft-adjusted build).');
  return lines.join('\n');
}
