// B3 draft-aware recommender. Every decision (keystone, each rune row, secondary tree and runes, shards, summoners,
// start, each core slot, boots, items 4-6, the skill maxed first, levels 1-3) is scored on its own by its options'
// win-probability-added in games like this one (estimate.ts), then assembled under the shop's exclusivity rules into
// the lane baseline's shape. Thin evidence keeps the lane baseline's pick and says so. docs/BUILD-SOURCING.md §9.3.
import type {
  BuildPick, BuildRecommendation, ContextChip, DraftRecommendation, Lane, Option, PickStats, RunePage, Skill, SkillPlan, SwapAdvice,
} from '../../shared/types.ts';
import type { StaticData } from '../ddragon.ts';
import { isPurchasableItem, type ExclusivityIndex } from '../engine/exclusivity.ts';
import { completePath, type RegionPatchOf } from '../engine/recommend.ts';
import { STAT_SHARDS } from '../engine/validator.ts';
import {
  DRAFT_DIMS, ITEM_DIMS, LIVE_DIMS, contextsOf, draftContext, kernelAt, scaleOf, standardise, standardiseInto, type Contexts, type Dim, type Scale,
} from './context.ts';
import type { PreparedGame, ScoredGame } from './dataset.ts';
import { estimateDecision, ESTIMATE_DEFAULTS, type EstimateOptions, type Observation, type OptionEstimate, type Query } from './estimate.ts';
import type { ChampionProfiles } from './profiles.ts';
import { draftGroupsOf, groupSimilarities, type DraftGroups } from './groups.ts';
import type { StateModel } from './state.ts';
import { KEEP as SWAP_KEEP, SELL as SWAP_SELL } from './swaps.ts';
import {
  consistent, patchAger, patchWeigher, pilotCapFactors, pilotWeight, recencyWeigher, regionWeight, WEIGHT_DEFAULTS, type WeightOptions,
} from './weights.ts';

/** Below this many effective similar games, a decision keeps the lane baseline's pick. */
export const MIN_EFFECTIVE_GAMES = 30;

/** Scored games with their standardised contexts: built once per data version, queried per draft. */
export interface DraftModel {
  scored: ScoredGame[];
  state: StateModel | null;
  profiles: ChampionProfiles;
  draftScale: Scale;
  liveScale: Scale;
  /**
   * Per game, its draft context (DRAFT_DIMS), row i for game i. One flat array since 0.14.0 (an array per game before,
   * and a flat copy of them all for similarities()): the same numbers in about two fifths of the memory.
   */
  zDraft: Contexts;
  /** Per game, per completed item: draft and live context together (ITEM_DIMS); game i's from row itemRow[i] on. */
  zItems: Contexts;
  /** Where each game's item rows start in zItems (one more entry than games: the last is the end). */
  itemRow: Int32Array;
  /** Per game with boots, their context (ITEM_DIMS): game i's at row bootsRow[i] (-1: no boots). */
  zBoots: Contexts;
  bootsRow: Int32Array;
  /**
   * Per late-game swap decision (0.17.0), the draft and live context at its visit (ITEM_DIMS): game i's from row
   * swapRow[i] on (one more entry than games: the last is the end).
   */
  zSwaps: Contexts;
  swapRow: Int32Array;
  /** Grouped drafts (0.20.0, model/groups.ts): set when every game carries a group (an evaluation trial), else null. */
  groups?: DraftGroups | null;
}

export function buildDraftModel(scored: ScoredGame[], state: StateModel | null, profiles: ChampionProfiles): DraftModel {
  const draftScale = scaleOf(scored.map((s) => s.g.draft), DRAFT_DIMS.length);
  const lives = scored.flatMap((s) => [...s.g.items.map((c) => c.live), ...(s.g.boots ? [s.g.boots.live] : [])]);
  const liveScale = scaleOf(lives, LIVE_DIMS.length);
  const D = DRAFT_DIMS.length, I = ITEM_DIMS.length;
  const zDraft = contextsOf(scored.length, D, (out, i, at) => standardiseInto(scored[i].g.draft, draftScale, out, at, D));
  // An item's context: the game's draft context, then the live one at the purchase.
  const itemRow = new Int32Array(scored.length + 1), bootsRow = new Int32Array(scored.length).fill(-1);
  let boots = 0;
  scored.forEach((s, i) => { itemRow[i + 1] = itemRow[i] + s.g.items.length; if (s.g.boots) bootsRow[i] = boots++; });
  const gameOf = (rowOf: Int32Array, rows: number, items: boolean) => {
    const at = new Int32Array(rows);
    for (let i = 0; i < scored.length; i++) {
      if (items) for (let r = rowOf[i]; r < rowOf[i + 1]; r++) at[r] = i;
      else if (rowOf[i] >= 0) at[rowOf[i]] = i;
    }
    return at;
  };
  const withLive = (rows: number, game: Int32Array, live: (i: number, r: number) => number[]) => contextsOf(rows, I, (out, r, at) => {
    const i = game[r];
    out.set(zDraft.flat.subarray(i * D, i * D + D), at);
    standardiseInto(live(i, r), liveScale, out, at + D, I - D);
  });
  const items = itemRow[scored.length];
  const zItems = withLive(items, gameOf(itemRow, items, true), (i, r) => scored[i].g.items[r - itemRow[i]].live);
  const zBoots = withLive(boots, gameOf(bootsRow, boots, false), (i) => scored[i].g.boots!.live);
  const swapRow = new Int32Array(scored.length + 1);
  scored.forEach((s, i) => { swapRow[i + 1] = swapRow[i] + (s.g.swaps?.length ?? 0); });
  const swaps = swapRow[scored.length];
  const zSwaps = withLive(swaps, gameOf(swapRow, swaps, true), (i, r) => scored[i].g.swaps[r - swapRow[i]].live);
  return { scored, state, profiles, draftScale, liveScale, zDraft, zItems, itemRow, zBoots, bootsRow, zSwaps, swapRow, groups: draftGroupsOf(scored, zDraft) };
}

/** Game i's context row for the item finished in `slot` (0-based; -1: boots) in zItems (zBoots for boots), or -1. */
export function itemContextRow(model: DraftModel, i: number, slot: number): number {
  if (slot < 0) return model.bootsRow[i];
  const at = model.itemRow[i] + slot;
  return at < model.itemRow[i + 1] ? at : -1;
}

export interface DraftQuery {
  lane: Lane;
  opponent: number | null;
  enemies: number[];
  allies: number[];
}

/**
 * Each stored game's similarity to the draft being recommended, reused from one draft to the next: the evaluation asks
 * for thousands of drafts, each a buffer as long as the model's games. recommendDraft is synchronous and reads it only
 * while it runs.
 */
let simScratch = new Float64Array(0);
export function similarities(zs: Contexts, zq: ArrayLike<number>, h: number): Float64Array {
  const rows = zs.rows;
  if (simScratch.length < rows) simScratch = new Float64Array(Math.max(rows, Math.ceil(simScratch.length * 1.25)));
  const out = simScratch.subarray(0, rows);
  const d = zq.length;
  // Contexts of another width than the query's: the general kernel, game by game.
  if (!Number.isFinite(h) || zs.d !== d) {
    for (let i = 0; i < rows; i++) out[i] = kernelAt(zs, i, zq, h);
    return out;
  }
  const q = Float64Array.from(zq);
  const denominator = 2 * h * h;
  // Use GPU (0.13.0, server/gpu/): the same numbers from the graphics card, or false and the loops below.
  if (accelerator?.(zs, rows, d, q, denominator, out)) return out;
  return flatSimilarities(zs, rows, d, q, denominator, out);
}

export type FlatDrafts = Pick<Contexts, 'flat' | 'anyNaN'>;
/** Fills `out` with the `rows` games' similarities (true) or leaves it to similarities() (false). */
export type SimilarityAccelerator = (stored: FlatDrafts, rows: number, d: number, q: Float64Array, denominator: number, out: Float64Array) => boolean;
let accelerator: SimilarityAccelerator | null = null;
/** The evaluation's threads with Use GPU on (server/gpu/accelerate.ts); null: the processor alone. */
export function setSimilarityAccelerator(a: SimilarityAccelerator | null) { accelerator = a; }

/** similarities() on the processor, over the flat contexts (`q` the draft's, `denominator` 2 h²). */
export function flatSimilarities(stored: FlatDrafts, rows: number, d: number, q: Float64Array, denominator: number, out: Float64Array): Float64Array {
  const { flat } = stored;
  // kernel() inlined: the same sums in the same order, so the same numbers.
  // A difference times itself is what ** 2 computes (IEEE: the same rounding), without the call.
  if (!stored.anyNaN) {
    // No game misses a dimension: only the query's unknown ones are left out, the same for every game.
    const known = Int32Array.from([...q.keys()].filter((j) => !Number.isNaN(q[j])));
    const n = known.length;
    if (n === d && n > 0) {
      // Every dimension known on both sides (the usual draft): one straight pass.
      for (let i = 0, at = 0; i < rows; i++, at += d) {
        let d2 = 0;
        for (let j = 0; j < d; j++) { const t = flat[at + j] - q[j]; d2 += t * t; }
        out[i] = Math.exp(-(d2 / n) / denominator);
      }
      return out;
    }
    for (let i = 0, at = 0; i < rows; i++, at += d) {
      let d2 = 0;
      for (let x = 0; x < n; x++) { const j = known[x]; const t = flat[at + j] - q[j]; d2 += t * t; }
      out[i] = n ? Math.exp(-(d2 / n) / denominator) : 1;
    }
    return out;
  }
  for (let i = 0; i < rows; i++) out[i] = rowSimilarity(flat, d, q, denominator, i);
  return out;
}

/** One game's similarity, any dimension unknown on either side left out: the same number as the loops above. */
export function rowSimilarity(flat: Float64Array, d: number, q: Float64Array, denominator: number, i: number): number {
  let d2 = 0, n = 0;
  for (let j = 0, at = i * d; j < d; j++) {
    const qj = q[j], g = flat[at + j];
    if (Number.isNaN(qj) || Number.isNaN(g)) continue;
    const t = g - qj;
    d2 += t * t; n++;
  }
  return n ? Math.exp(-(d2 / n) / denominator) : 1;
}

const memoSims = new WeakMap<Map<Observation[], unknown>, Float64Array>();
/** The same for grouped drafts: each group's similarity. */
const memoGroupSims = new WeakMap<Map<Observation[], unknown>, Float64Array>();


/** A game's weight before the patch weight of a specific decision. */
export interface Weighted { i: number; s: ScoredGame; base: number; patch: (items?: number[]) => number; age: number }

/** Pilot × region weights with the pilot cap applied per lane (the cap is computed on the decision-free patch weight). */
export function weighGames(model: DraftModel, data: StaticData, regionPatch: RegionPatchOf, o: WeightOptions): Weighted[] {
  const pw = patchWeigher(data, regionPatch, o);
  const ageOf = patchAger(data, regionPatch);
  const recency = recencyWeigher(o.recencyDays ? model.scored.reduce((m, s) => Math.max(m, s.g.created), 0) : 0, o, ageOf);
  const pre = model.scored.map((s, i) => ({ i, s, w: pilotWeight(s.g, o) * regionWeight(s.g, o) * recency(s.g), p0: pw(s.g) }))
    .filter((x) => x.w * x.p0 > 0);
  const caps = pilotCapFactors(pre.map((x) => ({ pilot: x.s.g.pilot, lane: x.s.g.lane, w: x.w * x.p0 })), o.pilotCap);
  return pre.map((x) => ({
    i: x.i, s: x.s, base: x.w * (caps.get(`${x.s.g.lane}:${x.s.g.pilot}`) ?? 1), patch: (items?: number[]) => pw(x.s.g, items), age: ageOf(x.s.g),
  }));
}

/** A decision's observations as the weights want them: with patch memory, after its data check (weights.ts). */
export function observed(obs: Observation[], o: Pick<WeightOptions, 'memory'>): Observation[] {
  return o.memory ? consistent(obs) : obs;
}

/**
 * The item query for draft `q` as recommendDraft asks it (the draft's context, the moment's unknown), with each
 * stored game's similarity to the draft at bandwidth `h` (a copy: similarities() reuses its buffer).
 */
export function itemQueryOf(model: DraftModel, q: DraftQuery, h: number): Query {
  const draftRaw = draftContext(model.profiles, q.enemies.map((id) => ({ id })), q.allies.map((id) => ({ id })),
    q.opponent !== null ? { id: q.opponent, position: q.lane } : null);
  const zq = standardise(draftRaw, model.draftScale);
  const values = Number.isFinite(h) ? similarities(model.zDraft, zq, h).slice() : undefined;
  return { lane: q.lane, opponent: q.opponent, z: [...zq, ...LIVE_DIMS.map(() => NaN)], sims: values ? { h, values } : undefined };
}

/**
 * The observations of what was done with `held` at late full-inventory shop visits (model/swaps.ts): the option (KEEP,
 * SELL or the item sold for, as strings), its WPA from the visit, the game's weight times its share of the game's
 * decision points, the visit's context. `minutes`: each one's minute, for the advice's typical minute.
 */
export function swapObservations(model: DraftModel, weighted: Weighted[], held: number): { obs: Observation[]; minutes: number[] } {
  const obs: Observation[] = [], minutes: number[] = [];
  for (const { i, s: sg, base, patch, age } of weighted) {
    const swaps = sg.g.swaps, wpa = sg.swapWpa;
    if (!swaps?.length || !wpa) continue;
    for (let j = 0; j < swaps.length; j++) {
      const d = swaps[j];
      if (d.held !== held) continue;
      const w = base * patch([held]) * d.w;
      if (w <= 0) continue;
      obs.push({
        option: String(d.item), wpa: wpa[j], win: sg.g.win, w, lane: sg.g.lane, opponent: sg.g.opponent, pilot: sg.g.pilot,
        zs: model.zSwaps, zRow: model.swapRow[i] + j, row: i, age,
      });
      minutes.push(d.t / 60_000);
    }
  }
  return { obs, minutes };
}

/** Each cached swap observation list's minutes (adviseSwap), kept as long as the list. */
const swapMinutes = new WeakMap<Observation[], number[]>();

/**
 * The swap advice for `held` in this query (an item query: the draft's context, the moment's where known): the option
 * with the best estimate (estimate.ts), with its lift over keeping; keep when the similar situations are too few
 * (MIN_EFFECTIVE_GAMES) or keeping is best. null: no game held it at a late full-inventory visit.
 */
export function adviseSwap(model: DraftModel, weighted: Weighted[], q: Query, held: number, o: EstimateOptions,
  cache?: Map<string, Observation[]>): SwapAdvice | null {
  const key = `swap:${held}`;
  let obs = cache?.get(key), minutes = obs && swapMinutes.get(obs);
  if (!obs || !minutes) {
    const made = swapObservations(model, weighted, held);
    // Keeping is the reference: each option's WPA counts from keeping's mean, so a rare swap shrinks towards keeping
    // (estimate.ts shrinks towards 0), not towards a lead the data doesn't show. Late in a game keeping's mean isn't 0.
    let sw = 0, swx = 0;
    for (const x of made.obs) if (x.option === String(SWAP_KEEP)) { sw += x.w; swx += x.w * x.wpa; }
    const keepMean = sw > 0 ? swx / sw : 0;
    obs = made.obs.map((x) => ({ ...x, wpa: x.wpa - keepMean }));
    minutes = made.minutes;
    swapMinutes.set(obs, minutes);
    cache?.set(key, obs);
  }
  if (!obs.length) return null;
  // Every option is offered, however rare: the shrinkage keeps a thin one near the lane's.
  const est = estimateDecision(obs, q, { ...o, minGames: 1, minShare: 0 });
  const keep = est.options.find((x) => x.option === String(SWAP_KEEP));
  const best = est.options[0];
  const thin = est.nEff < MIN_EFFECTIVE_GAMES || !keep || !best;
  const se = (e: OptionEstimate) => (e.high - e.low) / (2 * 1.96);
  const lift = (e: OptionEstimate) => e.theta - (keep?.theta ?? 0);
  const pickOf = (option: string): SwapAdvice['pick'] => (option === String(SWAP_KEEP) ? { kind: 'keep' }
    : option === String(SWAP_SELL) ? { kind: 'sell' } : { kind: 'swap', item: Number(option) });
  const halfOf = (e: OptionEstimate) => (keep ? 1.96 * Math.sqrt(se(e) ** 2 + se(keep) ** 2) : 0);
  // A swap only when its lead over keeping is clear (its 95 % range above zero): the best of many options with few
  // games each would otherwise beat keeping by chance (on the user's games it advised selling in half the situations
  // and did 7.2 pts worse than keeping).
  const chosen = thin ? null : est.options.find((x) => x.option !== String(SWAP_KEEP) && lift(x) - halfOf(x) > 0) ?? null;
  const minuteOf = (option: string) => {
    const ms = obs!.flatMap((x, k) => (x.option === option ? [minutes![k]] : [])).sort((a, b) => a - b);
    return ms.length ? Math.round(ms[Math.floor(ms.length / 2)]) : null;
  };
  const half = chosen ? halfOf(chosen) : 0;
  return {
    held, pick: chosen ? pickOf(chosen.option) : { kind: 'keep' },
    lift: chosen ? lift(chosen) : null, low: chosen ? lift(chosen) - half : null, high: chosen ? lift(chosen) + half : null,
    nEff: (chosen ?? keep ?? best)?.nEff ?? 0, games: (chosen ?? keep ?? best)?.games ?? 0, thin,
    alternatives: est.options.filter((x) => x.option !== String(SWAP_KEEP) && x !== chosen).slice(0, 3)
      .map((x) => ({ item: x.option === String(SWAP_SELL) ? null : Number(x.option), lift: lift(x), games: x.games })),
    minute: chosen ? minuteOf(chosen.option) : null,
  };
}

const toStats = (e: OptionEstimate): PickStats => ({
  lift: e.theta, low: e.low, high: e.high, nEff: e.nEff, games: e.games, pilots: e.pilots, winRate: e.winRate, matchupGames: e.matchupGames,
});

export interface DecisionResult { pick: BuildPick; chosen: OptionEstimate | null; options: OptionEstimate[] }

export interface DecideInput {
  key: string;
  label: string;
  obs: Observation[];
  q: Query;
  o: EstimateOptions;
  filter?: (option: string) => boolean;
  baseline: string | null;
  dims: Dim[];
  raw: number[];
  opponentName: string | null;
  /** Search the reason for a pick (one estimate per context dimension); evaluation turns it off. */
  reasons?: boolean;
}

/**
 * Why the draft (or the matchup) favoured `chosen` over the lane-level pick: the matchup when the pick only wins with it,
 * else the context dimension that, used alone, gives the pick its largest lead (dimensions often say the same thing,
 * e.g. the enemy team's and the lane opponent's damage type, so leaving one out would hide both).
 */
function reasonFor(chosen: OptionEstimate, laneBest: OptionEstimate | undefined, d: DecideInput): string | null {
  if (!laneBest || laneBest.option === chosen.option) return null;
  if (d.q.opponent !== null && chosen.matchupGames >= 3 && chosen.contextTheta <= laneBest.contextTheta) {
    return `vs ${d.opponentName ?? 'this opponent'} (${chosen.matchupGames} games)`;
  }
  return drivingDimension(chosen.option, laneBest.option, d.obs, d.q, d.o, (j) => `${d.dims[j].label} ${d.dims[j].format(d.raw[j])}`);
}

/** The context dimension that alone gives `a` its largest lead over `b`, named by `name`; null when none does. */
export function drivingDimension(a: string, b: string, obs: Observation[], q: Query, o: EstimateOptions, name: (dim: number) => string): string | null {
  const pair = new Set([a, b]);
  let best = -1, bestLead = 1e-6;
  for (let j = 0; j < q.z.length; j++) {
    if (Number.isNaN(q.z[j])) continue;
    const e = estimateDecision(obs, q, o, pair, j);
    const x = e.options.find((v) => v.option === a), y = e.options.find((v) => v.option === b);
    if (!x || !y) continue;
    const lead = x.contextTheta - y.contextTheta;
    if (lead > bestLead) { bestLead = lead; best = j; }
  }
  return best >= 0 ? name(best) : null;
}

export function decide(d: DecideInput): DecisionResult {
  const est = estimateDecision(d.obs, d.q, d.o);
  const ok = d.filter ?? (() => true);
  const options = est.options.filter((x) => ok(x.option));
  const thin = est.nEff < MIN_EFFECTIVE_GAMES || !options.length;
  const baselineEst = d.baseline !== null ? options.find((x) => x.option === d.baseline) : undefined;
  let chosen: OptionEstimate | null = options[0] ?? null;
  let value = chosen?.option ?? d.baseline ?? '';
  let fallback = false;
  if (thin && d.baseline !== null && ok(d.baseline)) {
    chosen = baselineEst ?? null;
    value = d.baseline;
    fallback = true;
  }
  const laneBest = [...options].sort((a, b) => b.laneTheta - a.laneTheta)[0];
  const reason = !fallback && chosen && d.reasons !== false ? reasonFor(chosen, laneBest, d) : null;
  return {
    chosen,
    options,
    pick: {
      key: d.key, label: d.label, value, stats: chosen ? toStats(chosen) : null, reason, fallback,
      changed: d.baseline !== null && value !== d.baseline,
      baseline: d.baseline !== null ? { value: d.baseline, lift: baselineEst?.theta ?? null } : null,
      alternatives: options.filter((x) => x.option !== value).slice(0, 3).map((x) => ({ value: x.option, stats: toStats(x) })),
    },
  };
}

/** Where a rune sits: its tree and row (0 = keystones). */
function runeIndex(data: StaticData) {
  const at = new Map<number, { tree: number; row: number }>();
  for (const tree of data.runes) tree.slots.forEach((slot, row) => slot.runes.forEach((r) => at.set(r.id, { tree: tree.id, row })));
  return at;
}

const optionOf = <T>(e: OptionEstimate | null, value: T): Option<T> => ({
  value, games: Math.round((e?.nEff ?? 0) * 100) / 100, rawGames: e?.games ?? 0, winRate: e?.winRate ?? 0, score: e?.theta ?? 0,
});

export interface DraftSettings {
  weights: WeightOptions;
  estimate: EstimateOptions;
  /**
   * Fine-tuning (0.15.0): the prior of item and boots decisions, when an evaluation found one better than `estimate.k`
   * for them (runes, summoners, the start and skills keep `estimate.k`). Missing: `estimate.k` for all.
   */
  itemK?: number;
}
export const DRAFT_DEFAULTS: DraftSettings = { weights: WEIGHT_DEFAULTS, estimate: ESTIMATE_DEFAULTS };

/**
 * The draft-adjusted build for `q`, next to the lane baseline `baseline` (its picks are the fallback and the
 * reference for "changed").
 */
export function recommendDraft(model: DraftModel, q: DraftQuery, data: StaticData, excl: ExclusivityIndex,
  regionPatch: RegionPatchOf, baseline: BuildRecommendation, s: DraftSettings = DRAFT_DEFAULTS,
  extra: {
    reasons?: boolean; detail?: boolean; weighted?: Weighted[]; cache?: Map<string, Observation[]>;
    /** Sums kept for the same draft at the same bandwidth (Query.memo): the evaluation's grid, one prior after another. */
    memo?: Map<Observation[], unknown>;
    /**
     * Only these decisions are estimated; the others keep the baseline's pick. The evaluation scores nine (EVAL_DECISIONS),
     * and none of them depends on the rest (rune rows, secondary runes, shards, levels 1-3, items 4-6).
     */
    decisions?: ReadonlySet<string>;
  } = {}): DraftRecommendation & { estimates?: Record<string, Record<string, number>> } {
  const weighted = extra.weighted ?? weighGames(model, data, regionPatch, s.weights);
  const estimates: Record<string, Record<string, number>> = {};
  const draftRaw = draftContext(model.profiles, q.enemies.map((id) => ({ id })), q.allies.map((id) => ({ id })),
    q.opponent !== null ? { id: q.opponent, position: q.lane } : null);
  const zq = standardise(draftRaw, model.draftScale);
  // Each game's similarity to this draft, once for every decision. Item decisions compare the draft dimensions only
  // (the live ones are unknown before the game), so they share it.
  const h = s.estimate.h;
  let values: Float64Array | undefined;
  let grouped: { of: Int32Array; sims: Float64Array } | undefined;
  if (Number.isFinite(h)) {
    // With a memo (the same draft and bandwidth), the similarities of its first call too.
    values = extra.memo ? memoSims.get(extra.memo) : undefined;
    if (model.groups) {
      // Grouped drafts (an evaluation trial, model/groups.ts): one similarity per group, each game its group's.
      const g = model.groups;
      const gs = (extra.memo ? memoGroupSims.get(extra.memo) : undefined) ?? groupSimilarities(g, zq, h);
      if (extra.memo) memoGroupSims.set(extra.memo, gs);
      if (!values) {
        values = new Float64Array(g.of.length);
        for (let i = 0; i < values.length; i++) values[i] = gs[g.of[i]];
        if (extra.memo) memoSims.set(extra.memo, values);
      }
      grouped = { of: g.of, sims: gs };
    } else if (!values) {
      values = similarities(model.zDraft, zq, h);
      if (extra.memo) { values = values.slice(); memoSims.set(extra.memo, values); }
    }
  }
  const sims = values ? { h, values, ...(grouped ? { groups: grouped } : {}) } : undefined;
  // Decisions on the same observations (the two secondary runes) add their sums up once; with extra.memo, so do the
  // next calls for this draft. The same memo serves both queries: their observations are different arrays.
  const memo = extra.memo ?? new Map<Observation[], unknown>();
  const itemQuery: Query = { lane: q.lane, opponent: q.opponent, z: [...zq, ...LIVE_DIMS.map(() => NaN)], sims, memo };
  const draftQuery: Query = { lane: q.lane, opponent: q.opponent, z: zq, sims, memo };
  const itemRaw = [...draftRaw, ...LIVE_DIMS.map(() => NaN)];
  const opponentName = q.opponent !== null ? data.champions.get(q.opponent)?.name ?? null : null;
  const runes = runeIndex(data);
  const picks: BuildPick[] = [];

  /** Observations of a pre-game decision. `option` returns one or more options per game (none: not applicable). */
  const pregame = (option: (g: PreparedGame) => string[] | string | null, items?: (g: PreparedGame) => number[]): Observation[] =>
    weighted.flatMap(({ i, s: sg, base, patch, age }) => {
      const v = option(sg.g);
      if (v === null || v === '') return [];
      const w = base * patch(items?.(sg.g));
      if (w <= 0) return [];
      return (Array.isArray(v) ? v : [v]).map((opt) => ({
        option: opt, wpa: sg.wpa0, win: sg.g.win, w, lane: sg.g.lane, opponent: sg.g.opponent, pilot: sg.g.pilot, zs: model.zDraft, zRow: i, row: i, age,
      }));
    });
  /** Observations of the item finished in `slot` (0-based), or of boots (slot -1). */
  const itemObs = (slot: number): Observation[] => weighted.flatMap(({ i, s: sg, base, patch, age }) => {
    const choice = slot < 0 ? sg.g.boots : sg.g.items[slot];
    const zRow = itemContextRow(model, i, slot);
    if (!choice || zRow < 0) return [];
    const w = base * patch([choice.item]);
    if (w <= 0) return [];
    const wpa = slot < 0 ? sg.bootsWpa! : sg.itemWpa[slot];
    const zs = slot < 0 ? model.zBoots : model.zItems;
    return [{ option: String(choice.item), wpa, win: sg.g.win, w, lane: sg.g.lane, opponent: sg.g.opponent, pilot: sg.g.pilot, zs, zRow, row: i, age }];
  });

  /** Observations depend on the games and weights only (plus the rune tree for rune rows): `extra.cache` keeps them. */
  const cached = (key: string, make: () => Observation[]) => {
    if (!extra.cache) return observed(make(), s.weights);
    let obs = extra.cache.get(key);
    if (!obs) { obs = observed(make(), s.weights); extra.cache.set(key, obs); }
    return obs;
  };
  const run = (key: string, label: string, obsOf: Observation[] | (() => Observation[]), baselineValue: string | null | undefined,
    filter?: (o: string) => boolean, item = false, cacheKey = key): DecisionResult => {
    if (extra.decisions && !extra.decisions.has(key)) {
      const value = baselineValue ?? '';
      const pick: BuildPick = {
        key, label, value, stats: null, reason: null, fallback: true, changed: false,
        baseline: baselineValue != null ? { value: baselineValue, lift: null } : null, alternatives: [],
      };
      picks.push(pick);
      return { chosen: null, options: [], pick };
    }
    const obs = typeof obsOf === 'function' ? cached(cacheKey, obsOf) : obsOf;
    const r = decide({
      key, label, obs, q: item ? itemQuery : draftQuery, o: item && s.itemK !== undefined ? { ...s.estimate, k: s.itemK } : s.estimate, filter, baseline: baselineValue ?? null,
      dims: item ? ITEM_DIMS : DRAFT_DIMS, raw: item ? itemRaw : draftRaw, opponentName, reasons: extra.reasons,
    });
    picks.push(r.pick);
    if (extra.detail) estimates[key] = Object.fromEntries(r.options.map((x) => [x.option, x.theta]));
    return r;
  };

  // ---- Runes ---------------------------------------------------------------------------------
  const bRunes = baseline.runes?.value;
  const keystone = run('keystone', 'Keystone', () => pregame((g) => String(g.perks[0])), bRunes ? String(bRunes.perks[0]) : null,
    (o) => runes.get(Number(o))?.row === 0);
  const tree = runes.get(Number(keystone.pick.value))?.tree ?? bRunes?.primaryStyle ?? 0;
  const inTree = (g: PreparedGame) => g.primary === tree;
  const sameTree = bRunes?.primaryStyle === tree;
  const rows = [1, 2, 3].map((row) => run(`row${row}`, `Primary row ${row}`, () => pregame((g) => (inTree(g) ? String(g.perks[row]) : null)),
    sameTree ? String(bRunes!.perks[row]) : null, (o) => runes.get(Number(o))?.tree === tree && runes.get(Number(o))?.row === row, false, `row${row}:${tree}`));
  const subTree = run('secondaryTree', 'Secondary tree', () => pregame((g) => (inTree(g) ? String(g.sub) : null)),
    sameTree ? String(bRunes!.subStyle) : null, (o) => Number(o) !== tree && data.runes.some((t) => t.id === Number(o)), false, `secondaryTree:${tree}`);
  const sub = Number(subTree.pick.value);
  const subObs = () => pregame((g) => (inTree(g) && g.sub === sub ? [String(g.perks[4]), String(g.perks[5])] : null));
  const sameSub = sameTree && bRunes?.subStyle === sub;
  const sec1 = run('secondary1', 'Secondary rune', subObs, sameSub ? String(bRunes!.perks[4]) : null,
    (o) => runes.get(Number(o))?.tree === sub && (runes.get(Number(o))?.row ?? 0) > 0, false, `secondary:${tree}:${sub}`);
  const row1 = runes.get(Number(sec1.pick.value))?.row;
  const sec2 = run('secondary2', 'Secondary rune', subObs, sameSub ? String(bRunes!.perks[5]) : null,
    (o) => runes.get(Number(o))?.tree === sub && (runes.get(Number(o))?.row ?? 0) > 0 && runes.get(Number(o))?.row !== row1, false, `secondary:${tree}:${sub}`);
  const shards = [0, 1, 2].map((i) => run(`shard${i + 1}`, ['Offense shard', 'Flex shard', 'Defense shard'][i],
    () => pregame((g) => String(g.shards[i])), bRunes ? String(bRunes.statPerks[i]) : null, (o) => STAT_SHARDS.has(Number(o))));

  // ---- Summoners, start, skills -----------------------------------------------------------------
  const summs = run('summoners', 'Summoner spells', () => pregame((g) => g.summoners), baseline.summoners ? [...baseline.summoners.value].sort((a, b) => a - b).join(',') : null);
  const start = run('start', 'Starting items', () => pregame((g) => g.start || null, (g) => g.start.split(',').map(Number)),
    baseline.startingItems[0] ? baseline.startingItems[0].value.join(',') : null,
    (o) => o.split(',').every((id) => isPurchasableItem(data.items.get(Number(id)))));
  const maxFirst = run('maxFirst', 'Ability maxed first', () => pregame((g) => g.maxFirst), baseline.skills?.value.priority[0] ?? null);
  const early = run('early', 'Levels 1-3', () => pregame((g) => g.early), baseline.skills ? baseline.skills.value.path.slice(0, 3).join('') : null,
    (o) => /^[QWE]{3}$/.test(o));

  // ---- Items: core slots, boots, items 4-6 -------------------------------------------------------
  const owned: number[] = [];
  const canBuy = (o: string) => isPurchasableItem(data.items.get(Number(o))) && excl.blocker(owned, Number(o)) === null;
  const bCore = baseline.coreBuild[0]?.value ?? [];
  const core = [0, 1, 2].map((slot) => {
    const r = run(`core${slot + 1}`, ['First item', 'Second item', 'Third item'][slot], () => itemObs(slot),
      bCore[slot] !== undefined ? String(bCore[slot]) : null, canBuy, true);
    if (r.pick.value) owned.push(Number(r.pick.value));
    return r;
  });
  const boots = run('boots', 'Boots', () => itemObs(-1), baseline.boots[0] ? String(baseline.boots[0].value) : null, canBuy, true);
  if (boots.pick.value) owned.push(Number(boots.pick.value));
  const bSlots = [baseline.situational.slot4, baseline.situational.slot5, baseline.situational.slot6];
  const later = [3, 4, 5].map((slot, i) => {
    const r = run(`item${slot + 1}`, `Item ${slot + 1}`, () => itemObs(slot), bSlots[i][0] ? String(bSlots[i][0].value) : null, canBuy, true);
    if (r.pick.value) owned.push(Number(r.pick.value));
    return r;
  });

  // ---- Assembly in the baseline's shape ----------------------------------------------------------
  const num = (r: DecisionResult) => Number(r.pick.value);
  const runePage: RunePage = {
    primaryStyle: tree, subStyle: sub,
    perks: [num(keystone), ...rows.map(num), num(sec1), num(sec2)],
    statPerks: shards.map(num),
  };
  const firstSkill = maxFirst.pick.value as Skill;
  const rest = (baseline.skills?.value.priority ?? ['Q', 'E', 'W']).filter((x) => x !== firstSkill);
  const priority = (firstSkill ? [firstSkill, ...rest] : rest) as Skill[];
  const earlyPath = (early.pick.value || 'QEW').split('') as Skill[];
  const skillPlan: SkillPlan = { priority, path: completePath(earlyPath, priority) };
  const coreValue = core.map(num).filter((id) => id > 0);
  // Alternatives for items 4-6 may not clash with the core, the boots or another slot's pick.
  const canBuyAfter = (o: string) => isPurchasableItem(data.items.get(Number(o))) && excl.blocker(owned, Number(o)) === null;
  const slotOptions = (r: DecisionResult) => [r.chosen && r.pick.value ? optionOf(r.chosen, num(r)) : null,
    ...r.options.filter((x) => x.option !== r.pick.value && canBuyAfter(x.option)).slice(0, 3).map((x) => optionOf(x, Number(x.option)))]
    .filter((x): x is Option<number> => x !== null);

  const sample = laneSample(model, weighted, q.lane);

  const chips: ContextChip[] = DRAFT_DIMS.map((d, i) => ({ key: d.key, label: d.label, value: Number.isFinite(draftRaw[i]) ? d.format(draftRaw[i]) : '', z: zq[i] }))
    .filter((c) => Number.isFinite(c.z) && Math.abs(c.z) >= 0.5)
    .sort((a, b) => Math.abs(b.z) - Math.abs(a.z)).slice(0, 4);

  const build: BuildRecommendation = {
    ...baseline,
    winRate: baseline.winRate,
    runes: optionOf(keystone.chosen, runePage),
    summoners: summs.pick.value ? optionOf(summs.chosen, summs.pick.value.split(',').map(Number)) : baseline.summoners,
    startingItems: start.pick.value ? [optionOf(start.chosen, start.pick.value.split(',').map(Number))] : baseline.startingItems,
    coreBuild: coreValue.length === 3 ? [optionOf(core[0].chosen, coreValue)] : baseline.coreBuild,
    boots: boots.pick.value ? [optionOf(boots.chosen, num(boots)), ...boots.options.filter((x) => x.option !== boots.pick.value)
      .filter((x) => excl.blocker(coreValue, Number(x.option)) === null).slice(0, 2).map((x) => optionOf(x, Number(x.option)))] : baseline.boots,
    situational: { slot4: slotOptions(later[0]), slot5: slotOptions(later[1]), slot6: slotOptions(later[2]) },
    skills: optionOf(maxFirst.chosen, skillPlan),
  };
  // Late-game swaps (0.17.0): for each item of this build (core, boots, items 4 and 5), from the same weighted games.
  const swaps = extra.decisions && !extra.decisions.has('swaps') ? undefined
    : [...coreValue, ...(boots.pick.value ? [num(boots)] : []), ...later.slice(0, 2).map(num).filter((id) => id > 0)]
      .flatMap((held) => adviseSwap(model, weighted, itemQuery, held, s.itemK !== undefined ? { ...s.estimate, k: s.itemK } : s.estimate, extra.cache) ?? []);

  // A slot with no pick of its own keeps the baseline's list.
  if (!build.situational.slot4.length) build.situational.slot4 = baseline.situational.slot4;
  if (!build.situational.slot5.length) build.situational.slot5 = baseline.situational.slot5;
  if (!build.situational.slot6.length) build.situational.slot6 = baseline.situational.slot6;

  return {
    ...(extra.detail ? { estimates } : {}),
    build,
    picks,
    context: chips,
    sample: { ...sample, regions: { ...sample.regions } },
    ...(swaps ? { swaps } : {}),
  };
}

const samples = new WeakMap<Weighted[], Map<Lane, DraftRecommendation['sample']>>();

/** The games behind a lane's builds: the same for every draft on these weighted games, so it is counted once. */
function laneSample(model: DraftModel, weighted: Weighted[], lane: Lane): DraftRecommendation['sample'] {
  let byLane = samples.get(weighted);
  if (!byLane) { byLane = new Map(); samples.set(weighted, byLane); }
  const known = byLane.get(lane);
  if (known) return known;
  const laneGames = weighted.filter((w) => w.s.g.lane === lane);
  // Every region with games in this lane (also the ones the query leaves out, so they can be switched back on).
  const regions: Record<string, number> = {};
  for (const sg of model.scored) if (sg.g.lane === lane && sg.g.source !== 'me') regions[sg.g.platform] = (regions[sg.g.platform] ?? 0) + 1;
  const kw = laneGames.map((w) => w.base);
  const sw = kw.reduce((a, b) => a + b, 0), sw2 = kw.reduce((a, b) => a + b * b, 0);
  const sample = { games: laneGames.length, nEff: sw2 > 0 ? (sw * sw) / sw2 : 0, pilots: new Set(laneGames.map((w) => w.s.g.pilot)).size, regions };
  byLane.set(lane, sample);
  return sample;
}
