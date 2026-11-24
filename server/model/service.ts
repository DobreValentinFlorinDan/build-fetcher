// The draft-aware model inside the app (run by the model worker, model/worker.ts): built from the database when its
// data changes (at most every few minutes, never during champ select or a game), and asked for drafts, the live "Next"
// item and the personal layer.
import fs from 'node:fs';
import path from 'node:path';
import type { BuildMode, BuildRecommendation, DraftModelInfo, DraftResponse, Lane } from '../../shared/types.ts';
import { appConfigDir, readAppConfig, updateAppConfig, type AppConfig } from '../appConfig.ts';
import type { Db } from '../db.ts';
import type { StaticData } from '../ddragon.ts';
import type { ExclusivityIndex } from '../engine/exclusivity.ts';
import type { RegionPatchOf } from '../engine/recommend.ts';
import { regionPatches } from '../patches.ts';
import type { ScrapeScope } from '../scrapeScope.ts';
import { frameSlabs, prepareGames, scoreGames, trainState, type ItemChoice, type PreparedGame } from './dataset.ts';
import { buildDraftModel, recommendDraft, weighGames, type DraftModel, type DraftSettings, type Weighted } from './draft.ts';
import { ESTIMATE_DEFAULTS, type Observation } from './estimate.ts';
import type { EvalPath } from './evalBase.ts';
import type { TuningChoice } from './evaluate.ts';
import { crossFitted, EXPECTED_DEFAULTS, type AgeOf, type Penalty } from './expected.ts';
import { ChampionProfiles, type ChampionTotals } from './profiles.ts';
import { BuildCancelled, PARALLEL_FROM_GAMES, prepareOnThreads } from './prepareTeam.ts';
import { STATE_FEATURES, winProbabilityOf } from './state.ts';
import { MEMORY_PATCHES, patchAge, patchAger, patchOrder, WEIGHT_DEFAULTS, type WeightOptions } from './weights.ts';

/** The newest npm run eval:builds result (model/evaluate.ts writes it). */
export interface EvalSummary {
  evaluatedAt: number;
  gate: 'passed' | 'failed';
  bandwidth: number;
  k: number;
  lambda: number;
  dataVersion: string;
  /** Patch memory against today's and the previous patch alone (evaluate.ts): the verdict and the fade it chose. */
  memory?: { gate: 'passed' | 'failed'; decay: number };
  /** Runes and summoners against the draft (0.9.0): the verdict and the expected win rate's penalties it chose. */
  scoring?: { gate: 'passed' | 'failed'; penalty: Penalty | null };
  /** Fine-tuning (0.15.0): the verdict and what it chose; applied only when passed. */
  tuning?: { gate: 'passed' | 'failed'; choice: TuningChoice };
  /** Late-game swaps (0.17.0, docs/SWAPS.md): whether following the swap advice beat keeping every item. */
  swaps?: { gate: 'passed' | 'failed' };
  /** What the evaluation saw when it started, for the next automatic one (server/autoEval.ts; since 0.8.4). */
  mark?: { row: number | null; games: number; patch: string | null };
  /**
   * 0.16.0 (docs/EVAL-UPDATE.md): a full evaluation or a quick update of one, and the full evaluation its verdicts rest
   * on (for a full one, itself).
   */
  path?: EvalPath;
  full?: { evaluatedAt: number; mark: { row: number | null; games: number; patch: string | null } };
  /** Taken from another computer's backup (0.21.0, model/evalTravel.ts): which, and when that backup was made. */
  importedFrom?: { machine: string; exportedAt: number };
}

export const evalDir = () => path.join(appConfigDir, 'evals');
export function readEvaluation(): EvalSummary | null {
  try { return JSON.parse(fs.readFileSync(path.join(evalDir(), 'latest.json'), 'utf8')) as EvalSummary; } catch { return null; }
}

/**
 * The data a model is built from: every region's current and previous patch, plus Data Dragon's pair; with patch
 * memory, every stored patch within `window` patches of a region's current one (or Data Dragon's).
 */
export function modelPatches(db: Db, data: StaticData, window = 2): string[] {
  const keep = new Set([data.patch, data.previousPatch]);
  const versions = db.gameVersions();
  const regions = [...regionPatches(versions, data).values()];
  for (const r of regions) { keep.add(r.patch); keep.add(r.previousPatch); }
  if (window > 2) {
    const order = patchOrder(data);
    const currents = [data.patch, ...regions.map((r) => r.patch)];
    for (const p of new Set(versions.map((v) => v.patch))) {
      if (currents.some((c) => { const age = patchAge(order, p, c); return age !== null && age >= 0 && age < window; })) keep.add(p);
    }
  }
  return [...keep];
}

/**
 * Whether a patch is older than patch memory's window on every region (and on Data Dragon): what "Remove" and
 * "Archive old patches" and the cache cleanup may drop. null: can't be told (kept).
 */
export function beforeWindow(db: Db, data: StaticData, window = MEMORY_PATCHES): (patch: string) => boolean | null {
  const keep = new Set(modelPatches(db, data, window));
  const order = patchOrder(data);
  const currents = [data.patch, ...[...regionPatches(db.gameVersions(), data).values()].map((r) => r.patch)];
  return (patch) => {
    if (keep.has(patch)) return false;
    let known = false;
    for (const c of currents) {
      const age = patchAge(order, patch, c);
      if (age === null) continue;
      if (age < window) return false;
      known = true;
    }
    return known ? true : null;
  };
}

/** The fade per patch patch memory uses before an evaluation has chosen one. */
export const MEMORY_DEFAULT_DECAY = 0.75;

/**
 * Patch memory as it stands: the Data tab's On / Off when chosen, else on once an evaluation found it better than
 * today's and the previous patch alone. null: off (the 0.7.x behaviour).
 */
export function currentMemory(): WeightOptions['memory'] {
  const chosen = readAppConfig().patchMemory;
  const ev = readEvaluation()?.memory;
  if (chosen === 'off') return null;
  if (chosen === 'on' || ev?.gate === 'passed') return { decay: ev?.decay ?? MEMORY_DEFAULT_DECAY, patches: MEMORY_PATCHES };
  return null;
}

/**
 * Runes and summoners against the draft (model/expected.ts) as it stands: following the evaluation (Follow the
 * evaluation on, i.e. no patch memory choice of yours), on once an evaluation found it better; else the Data tab's
 * switch. The penalties to fit with (the evaluation's, else chosen by cross-validation); null: off.
 */
/** Fine-tuning's choice (0.15.0), when the last evaluation passed it; else null (today's settings). */
export function currentTuning(): TuningChoice | null {
  const t = readEvaluation()?.tuning;
  return t?.gate === 'passed' ? t.choice : null;
}

export function currentScoring(): Penalty[] | null {
  const cfg = readAppConfig();
  const ev = readEvaluation()?.scoring;
  const on = cfg.patchMemory === undefined ? ev?.gate === 'passed' : cfg.pregameScoring === 'on';
  if (!on) return null;
  return ev?.penalty ? [ev.penalty] : EXPECTED_DEFAULTS.penalties;
}

/**
 * Late-game swaps (docs/SWAPS.md, 0.17.0) as they stand: following the evaluation, on once an evaluation found following
 * the advice better than keeping every item; else the Data tab's switch. Only what the Items view shows: the model is
 * built the same either way.
 */
export function currentSwaps(): boolean { return swapsOn(readAppConfig(), readEvaluation()); }
const swapsOn = (cfg: AppConfig, ev: EvalSummary | null) => (cfg.patchMemory === undefined ? ev?.swaps?.gate === 'passed' : cfg.lateSwaps === 'on');

type Switches = {
  memory: Pick<DraftModelInfo['memory'], 'active' | 'choice' | 'gate' | 'decay'>; scoring: Omit<DraftModelInfo['scoring'], 'inModel'>;
  swaps: NonNullable<DraftModelInfo['swaps']>;
};

/** The Data tab's model switches as saved: whether each is on, whose choice it is ('auto': Follow the evaluation), the verdicts. */
function switches(memory: WeightOptions['memory'], scoring: Penalty[] | null): Switches {
  const ev = readEvaluation();
  const cfg = readAppConfig();
  const choice = cfg.patchMemory ?? 'auto';
  return {
    memory: { active: memory !== null, choice, gate: ev?.memory?.gate ?? 'not evaluated', decay: memory?.decay ?? ev?.memory?.decay ?? null },
    scoring: { active: scoring !== null, gate: ev?.scoring?.gate ?? 'not evaluated', choice: choice === 'auto' ? 'auto' : cfg.pregameScoring ?? 'off' },
    swaps: { active: swapsOn(cfg, ev), gate: ev?.swaps?.gate ?? 'not evaluated', choice: choice === 'auto' ? 'auto' : cfg.lateSwaps ?? 'off' },
  };
}

/**
 * The model's info with its switches as just saved, for the main thread to show at once. The model thread reads a
 * change only once a build it is in ends (35-45 s at 54,000 games): waiting for it, a switch went back after a few
 * seconds and over again when the build ended. What the model was built with (inModel) stays the model thread's.
 */
export function withSwitches(info: DraftModelInfo): DraftModelInfo {
  const sw = switches(currentMemory(), currentScoring());
  return { ...info, memory: { ...info.memory, ...sw.memory }, scoring: { ...info.scoring, ...sw.scoring }, swaps: sw.swaps };
}

export interface Built {
  version: string;
  model: DraftModel;
  /** Every prepared game, the user's own included (the personal layer reads them; the draft model weighs them 0). */
  prepared: PreparedGame[];
  profiles: ChampionProfiles;
  /** What the profiles were made from: the main thread rebuilds the same profiles for the live threat summary. */
  profileRows: { current: ChampionTotals[]; older: ChampionTotals[] };
  buildMs: number;
  builtAt: number;
  /** Your own games when it was built (0.15.0: a new one makes a build due, ModelService.needsBuild). */
  ownGames: number;
}

const BUILT: number[] = [];

type BuildScoring = { penalties: Penalty[]; ageOf: AgeOf } | null;

/** What a build reads before its games: the patches, the champion profiles and its sources, and the data version. */
function buildInputs(db: Db, data: StaticData, scope: Pick<ScrapeScope, 'draftSources'>, memory: WeightOptions['memory']) {
  // What the build reads, as it starts: games stored meanwhile are new data for the next one.
  const version = db.dataVersion(), ownGames = db.ownGameCount();
  const recent = modelPatches(db, data);
  const patches = memory ? modelPatches(db, data, memory.patches) : recent;
  const profileRows = { current: db.championTotals(recent), older: db.olderChampionTotals(recent) };
  const profiles = new ChampionProfiles(profileRows.current, profileRows.older, data);
  const sources = [...scope.draftSources, 'me'];
  return { version, ownGames, recent, patches, profileRows, profiles, sources };
}

/** The model from its prepared games: the state model, each decision's WPA and the draft model. */
function finishBuild(inputs: ReturnType<typeof buildInputs>, prepared: PreparedGame[], t0: number, lambda: number | undefined,
  memory: WeightOptions['memory'], scoring: BuildScoring): Built {
  const { version, ownGames, recent, profileRows, profiles } = inputs;
  const inRecent = new Set(recent);
  const state = trainState(memory ? prepared.filter((g) => inRecent.has(g.patch)) : prepared, lambda);
  // With scoring against the draft, pre-game decisions against each game's expected win rate (cross-fitted).
  const expected = scoring ? crossFitted(prepared, scoring.ageOf, { ...EXPECTED_DEFAULTS, penalties: scoring.penalties }).values : undefined;
  const model = buildDraftModel(scoreGames(prepared, state, expected), state, profiles);
  // Only the build reads each purchase's state features (scored into its WPA) and raw live context (standardised into
  // the model): they are dropped, so the model kept between builds is smaller. Of the state frames, the best buy for
  // your gold needs only the state model's win probability at each one.
  const built = (c: ItemChoice) => { c.x = null; c.live = BUILT; };
  const width = STATE_FEATURES.length;
  for (const g of prepared) {
    g.items.forEach(built);
    if (g.boots) built(g.boots);
    // Late-game swap decisions (0.17.0): their state features are scored into their WPA and their live context
    // standardised into the model, so they go too.
    g.swaps?.forEach(built);
    const f = g.frames;
    if (f && state) {
      g.pAt = new Float64Array(f.length / width);
      for (let i = 0; i < g.pAt.length; i++) g.pAt[i] = winProbabilityOf(state, f.subarray(i * width, (i + 1) * width));
    }
    g.frames = null;
  }
  return { version, model, prepared, profiles, profileRows, buildMs: Math.round(performance.now() - t0), builtAt: Date.now(), ownGames };
}

/**
 * Builds the model synchronously from the database; with patch memory, from every patch in its window. The champion
 * profiles and the game-state model come from the two-patch games either way (as the evaluation measured it).
 */
export function buildFromDb(db: Db, data: StaticData, scope: Pick<ScrapeScope, 'draftSources'>, startingItemWindowMs: number, lambda?: number,
  memory: WeightOptions['memory'] = null, scoring: BuildScoring = null): Built {
  const t0 = performance.now();
  const inputs = buildInputs(db, data, scope, memory);
  const prepared = prepareGames(db.modelGamesIter(inputs.patches, inputs.sources), db.modelParticipants(inputs.patches, inputs.sources), data, inputs.profiles,
    { startingItemWindowMs, frames: frameSlabs() });
  return finishBuild(inputs, prepared, t0, lambda, memory, scoring);
}

/** Where a build's games are prepared on several threads (0.15.0, model/prepareTeam.ts). */
export interface BuildTeam { dataDir: string; threads: number; stop: () => boolean; /** Default PARALLEL_FROM_GAMES. */ minGames?: number }

/**
 * The same model as buildFromDb (0.15.0), its games prepared on `team.threads` helper threads in the order they are
 * read; on one thread, or below PARALLEL_FROM_GAMES games, as buildFromDb. Rejects with BuildCancelled when
 * `team.stop` says so between batches.
 */
export async function buildFromDbAsync(db: Db, data: StaticData, scope: Pick<ScrapeScope, 'draftSources'>, startingItemWindowMs: number,
  lambda: number | undefined, memory: WeightOptions['memory'], scoring: BuildScoring, team: BuildTeam): Promise<Built> {
  const t0 = performance.now();
  const inputs = buildInputs(db, data, scope, memory);
  const games = db.modelGameCount(inputs.patches, inputs.sources);
  if (team.threads <= 1 || games < (team.minGames ?? PARALLEL_FROM_GAMES)) return buildFromDb(db, data, scope, startingItemWindowMs, lambda, memory, scoring);
  const prepared = await prepareOnThreads(db.modelGamesRawIter(inputs.patches, inputs.sources), db.modelParticipants(inputs.patches, inputs.sources), {
    dataDir: team.dataDir, versions: data.versions ?? [data.version], profileRows: inputs.profileRows, startingItemWindowMs, dicts: db.timelineDicts(),
    threads: team.threads, frames: frameSlabs(), stop: team.stop,
  });
  if (team.stop()) throw new BuildCancelled();
  return finishBuild(inputs, prepared, t0, lambda, memory, scoring);
}

export interface ModelEnv {
  db: Db;
  data: () => StaticData;
  excl: () => ExclusivityIndex;
  scope: () => ScrapeScope;
  regionPatch: () => RegionPatchOf;
  ownPlatform: () => string | null;
  /** Champ select or a game is on: new data waits (a build takes seconds on a large database). */
  busy: () => boolean;
  startingItemWindowMs: number;
}

/** A build is tried at most this often (a failed one too). */
const REBUILD_EVERY_MS = 5 * 60_000;

/**
 * When new data is worth a build (0.15.0). Each build reads every stored game (about a minute at 118,000 games), and
 * the 30-80 games fetched in a few minutes change nothing in it: new data waits until it is 1 % of what the model was
 * built from (at least 50 games) and the model is half an hour old, or six hours have passed. Your own new game, a new
 * patch and a moved Data tab switch don't wait.
 */
export const DATA_REBUILD = { share: 0.01, minGames: 50, minAgeMs: 30 * 60_000, maxAgeMs: 6 * 60 * 60_000 };

/** Whether stored data that went from version `built` to `now` (Db.dataVersion: games:last:timelines:captured) is due. */
export function dataDue(built: string, now: string, ageMs: number, o = DATA_REBUILD): boolean {
  if (built === now) return false;
  if (ageMs >= o.maxAgeMs) return true;
  if (ageMs < o.minAgeMs) return false;
  const [g0, , t0, c0] = built.split(':').map(Number), [g1, , t1, c1] = now.split(':').map(Number);
  const changed = Math.max(Math.abs(g1 - g0), Math.abs(t1 - t0), Math.abs(c1 - c0));
  if (!Number.isFinite(changed)) return true;
  return changed >= Math.max(o.minGames, Math.ceil(o.share * (Number.isFinite(g0) ? g0 : 0)));
}
/** The draft caches go this long after the last draft asked for (champ select is over by then). */
export const DRAFT_CACHE_IDLE_MS = 3 * 60_000;
const SETTINGS_TTL_MS = 5_000;

/** What the dashboard shows first: the user's choice, else the draft model once it passed the evaluation. */
export function currentMode(): BuildMode {
  const chosen = readAppConfig().buildMode;
  return chosen === 'draft' || chosen === 'baseline' ? chosen : readEvaluation()?.gate === 'passed' ? 'draft' : 'baseline';
}

export class ModelService {
  private built: Built | null = null;
  private failed: string | null = null;
  private builtFor = '';          // the Data Dragon version and patch memory window the model was built with
  private olderGames = 0;         // the model's games from before the previous patch (patch memory)
  private builtWithMemory = false;
  private builtWithScoring = false;
  private attemptedAt = 0;
  private modeCache: { at: number; value: BuildMode } | null = null;
  private settingsCache: { at: number; value: DraftSettings } | null = null;
  private scoringCache: { at: number; value: Penalty[] | null } | null = null;
  /**
   * Champ select asks for a draft at every pick: the games' weights and each decision's observations (with their
   * indexes) are kept between those calls, for the same model, weights and patches per region. At 55,000 games that
   * takes a draft from 1-3 s to tens of milliseconds after the first, with the same answers. The caches (about 230 MB
   * there with patch memory) are kept only for champ select (`keep`, 0.9.1: outside it new games refresh the build
   * every 30 s while fetching, which kept them for good) and go DRAFT_CACHE_IDLE_MS after its last draft (trimCaches).
   */
  private draftCache: { built: Built; key: string; weighted: Weighted[]; cache: Map<string, Observation[]>; usedAt: number } | null = null;

  constructor(private env: ModelEnv) {}

  /** The model built last (null until the first build). Never builds: the worker decides when (needsBuild). */
  current(): Built | null { return this.built; }

  /** Whether the current model was built with patch memory (it holds games from before the previous patch too). */
  holdsOlderPatches(): boolean { return this.builtWithMemory; }

  /**
   * Whether a build is due: there is none yet; or, with no champ select, game or queue on and the last try a few minutes
   * ago, the Data Dragon version or a Data Tab switch changed, your own games did, or enough new data came (dataDue).
   */
  needsBuild(): boolean {
    if (Date.now() - this.attemptedAt < REBUILD_EVERY_MS) return false;
    if (!this.built) return true;
    if (this.env.busy()) return false;
    if (this.forKey() !== this.builtFor) return true;
    if (this.env.db.ownGameCount() !== this.built.ownGames) return true;
    return dataDue(this.built.version, this.env.db.dataVersion(), Date.now() - this.built.builtAt);
  }

  /** Scoring against the draft as it stands (cached a few seconds, like the settings). */
  private scoring(): Penalty[] | null {
    if (this.scoringCache && Date.now() - this.scoringCache.at < SETTINGS_TTL_MS) return this.scoringCache.value;
    const value = currentScoring();
    this.scoringCache = { at: Date.now(), value };
    return value;
  }

  /**
   * What a build depends on besides the data: the Data Dragon version, the patches patch memory reaches, and scoring
   * against the draft with its penalties.
   */
  private forKey() {
    const scoring = this.scoring();
    return `${this.env.data().version}|${this.settings().weights.memory?.patches ?? 2}|${scoring ? JSON.stringify(scoring) : 'overall'}`;
  }

  rebuild() {
    this.attemptedAt = Date.now();
    try {
      const p = this.buildPlan();
      this.adopt(buildFromDb(this.env.db, this.env.data(), this.env.scope(), this.env.startingItemWindowMs, p.lambda, p.memory, p.scoring), p);
    } catch (err) { this.fail(err); }
    return this.built;
  }

  /**
   * The same build with its games prepared on helper threads (0.15.0, model/prepareTeam.ts): the model thread answers
   * requests with the model it has meanwhile. Champ select, a game or the queue stops it (`team.stop`): the model
   * stays as it was, and the next build is due a few minutes later.
   */
  async rebuildAsync(team: BuildTeam): Promise<Built | null> {
    this.attemptedAt = Date.now();
    try {
      const p = this.buildPlan();
      this.adopt(await buildFromDbAsync(this.env.db, this.env.data(), this.env.scope(), this.env.startingItemWindowMs, p.lambda, p.memory, p.scoring, team), p);
    } catch (err) {
      if (err instanceof BuildCancelled) console.log(`[model] ${err.message}: the model stays as it was`);
      else this.fail(err);
    }
    return this.built;
  }

  /**
   * What a build is made with, read once at its start: a switch moved while it runs (a minute at 118,000 games) is
   * what the next build is for, so it mustn't count as built (needsBuild compares builtFor).
   */
  private buildPlan() {
    const ev = readEvaluation();
    const scoring = this.scoring();
    return {
      key: this.forKey(), lambda: ev?.lambda, memory: this.settings().weights.memory,
      scoring: scoring ? { penalties: scoring, ageOf: patchAger(this.env.data(), this.env.regionPatch()) } : null,
    };
  }

  private adopt(built: Built, p: ReturnType<ModelService['buildPlan']>) {
    this.built = built;
    this.builtFor = p.key;
    this.builtWithMemory = p.memory !== null;
    this.builtWithScoring = p.scoring !== null;
    const ageOf = patchAger(this.env.data(), this.env.regionPatch());
    this.olderGames = built.model.scored.filter((x) => x.g.source !== 'me' && ageOf(x.g) >= 2).length;
    this.failed = null;
    console.log(`[model] built from ${built.prepared.length} games in ${built.buildMs} ms`);
  }

  private fail(err: unknown) {
    this.failed = err instanceof Error ? err.message : String(err);
    console.warn('[model] build failed', err);
  }

  /** Draft settings from the scope and the last evaluation (cached a few seconds: the live poll asks every second). */
  settings(regions: string[] | null = null): DraftSettings {
    if (regions === null && this.settingsCache && Date.now() - this.settingsCache.at < SETTINGS_TTL_MS) return this.settingsCache.value;
    const value = this.readSettings(regions);
    if (regions === null) this.settingsCache = { at: Date.now(), value };
    return value;
  }

  private readSettings(regions: string[] | null): DraftSettings {
    const scope = this.env.scope();
    const ev = readEvaluation();
    // Fine-tuning only once an evaluation found it better than the chosen model (0.15.0).
    const tuned = currentTuning();
    return {
      weights: {
        ...WEIGHT_DEFAULTS, regionWeights: scope.regionWeights, ownPlatform: this.env.ownPlatform(), ownRegionBoost: scope.ownRegionBoost,
        patchWeights: scope.patchWeights, hotfixes: scope.hotfixes, regions, memory: currentMemory(),
        ...(tuned ? { pilotStrength: tuned.pilotStrength, recencyDays: tuned.recencyDays } : {}),
      },
      estimate: { ...ESTIMATE_DEFAULTS, ...(ev ? { h: ev.bandwidth, k: ev.k } : {}) },
      ...(tuned?.itemK != null ? { itemK: tuned.itemK } : {}),
    };
  }

  info(): DraftModelInfo | null {
    const b = this.built;
    if (!b) return null;
    const ev = readEvaluation();
    const s = this.settings();
    const sw = switches(s.weights.memory, this.scoring());
    return {
      gate: ev?.gate ?? 'not evaluated', evaluatedAt: ev?.evaluatedAt ?? null, bandwidth: s.estimate.h, k: s.estimate.k,
      stateModel: b.model.state ? { games: b.model.state.games, rows: b.model.state.rows } : null, dataVersion: b.version,
      games: b.model.scored.filter((x) => x.g.source !== 'me').length,
      memory: { ...sw.memory, patches: MEMORY_PATCHES, olderGames: this.olderGames, inModel: this.builtWithMemory },
      scoring: { ...sw.scoring, inModel: this.builtWithScoring },
      tuning: { gate: ev?.tuning?.gate ?? 'not evaluated', ...(currentTuning() ?? { pilotStrength: 1, itemK: null, recencyDays: null }) },
      swaps: sw.swaps,
    };
  }

  /** What the dashboard shows first: the user's choice, else the draft model once it passed the evaluation. */
  mode(): BuildMode {
    if (this.modeCache && Date.now() - this.modeCache.at < SETTINGS_TTL_MS) return this.modeCache.value;
    const value = currentMode();
    this.modeCache = { at: Date.now(), value };
    return value;
  }

  setMode(mode: BuildMode | null) { updateAppConfig({ buildMode: mode ?? undefined }); this.modeCache = null; }

  /**
   * A new evaluation changes the default mode, the bandwidth and the prior, and may turn patch memory on or off: a
   * different window is built as soon as no champ select or game is on, not after the usual few minutes.
   */
  evaluated() { this.modeCache = null; this.settingsCache = null; this.scoringCache = null; this.attemptedAt = 0; }

  /**
   * A Data tab switch moved (patch memory, runes against the draft, Follow the evaluation), saved by the main thread,
   * the only one that writes them: a different model is due at once (needsBuild). Late-game swaps aren't in the build
   * (forKey): moving that one alone makes none due.
   */
  optionsChanged() { this.settingsCache = null; this.scoringCache = null; this.attemptedAt = 0; }

  error() { return this.failed; }

  draft(q: { lane: Lane; opponent: number | null; enemies: number[]; allies: number[]; regions: string[] | null }, baseline: BuildRecommendation,
    keep = true): DraftResponse {
    const b = this.current();
    let draft = null;
    if (b && b.model.scored.length && !keep) {
      // Outside champ select: no caches kept (the ones of the last champ select go now).
      this.draftCache = null;
      draft = recommendDraft(b.model, q, this.env.data(), this.env.excl(), this.env.regionPatch(), baseline, this.settings(q.regions));
    } else if (b && b.model.scored.length) {
      const settings = this.settings(q.regions);
      const regionPatch = this.env.regionPatch();
      // What the weights depend on besides the model: the settings, and each region's current and previous patch.
      const platforms = [...new Set(b.model.scored.map((s) => s.g.platform))].sort();
      const key = JSON.stringify([settings.weights, platforms.map((p) => [p, regionPatch(p)?.patch, regionPatch(p)?.previousPatch])]);
      let c = this.draftCache;
      if (!c || c.built !== b || c.key !== key) {
        c = { built: b, key, weighted: weighGames(b.model, this.env.data(), regionPatch, settings.weights), cache: new Map(), usedAt: 0 };
        this.draftCache = c;
      }
      c.usedAt = Date.now();
      draft = recommendDraft(b.model, q, this.env.data(), this.env.excl(), regionPatch, baseline, settings, { weighted: c.weighted, cache: c.cache });
    }
    return { baseline, draft, mode: this.mode(), model: this.info() };
  }

  /**
   * Lets the draft caches go once no draft was asked for in `idleMs` (the model thread checks every 30 s); whether it
   * let any go.
   */
  trimCaches(idleMs = DRAFT_CACHE_IDLE_MS): boolean {
    if (!this.draftCache || Date.now() - this.draftCache.usedAt < idleMs) return false;
    this.draftCache = null;
    return true;
  }

  /** Whether drafts are kept between calls right now (tests). */
  keepsDrafts(): boolean { return this.draftCache !== null; }
}
