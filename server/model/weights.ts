// How much each stored game counts in the draft model: who played it (source, tier, LP, Irelia points), which patch
// it was played on (and whether that patch changed Irelia or the items involved), hotfixes, region, and a cap on any
// one pilot's share of a lane. docs/BUILD-SOURCING.md §9.3.
import type { Lane } from '../../shared/types.ts';
import { toPatch, type Fingerprints, type StaticData } from '../ddragon.ts';
import type { RegionPatchOf } from '../engine/recommend.ts';
import type { ManualHotfix } from '../patches.ts';
import type { PreparedGame } from './dataset.ts';
import type { Observation } from './estimate.ts';

export interface WeightOptions {
  /** Source weights. The user's own games never feed the draft model. */
  sources: { otp: number; main: number; harvested: number };
  /** Most of a lane's weight one pilot may hold (raised to 1/pilots when there are fewer than 1/cap pilots). */
  pilotCap: number;
  /** Platform weights; missing platforms count 1. */
  regionWeights: Record<string, number>;
  /** Only these platforms (null: all). */
  regions: string[] | null;
  ownPlatform: string | null;
  ownRegionBoost: number;
  /** Previous-patch weights by patch, overriding the Data Dragon diff ("16.18": 0.5). */
  patchWeights: Record<string, number>;
  hotfixes: ManualHotfix[];
  /** Games played before the newest hand-entered hotfix of their patch. */
  beforeHotfixWeight: number;
  /**
   * Patch memory: games of patches before the previous one count too, fading by `decay` per patch, up to `patches`
   * patches (today's included); what changed since then counts less, and an option whose current-patch games clearly
   * disagree with its older ones leans on the new ones (consistent()). null: today's and the previous patch only.
   */
  memory: { decay: number; patches: number } | null;
  /**
   * Fine-tuning (0.15.0, used only once an evaluation found it better: model/evaluate.ts): how strongly a pilot's
   * standing (source, tier, mastery, LP) counts, as a power of pilotWeight (1 or missing: as before; 0: every pilot
   * alike); and a fade by age within each game's current patch, halving every `recencyDays` days (null or missing: none).
   */
  pilotStrength?: number;
  recencyDays?: number | null;
}

export const WEIGHT_DEFAULTS: WeightOptions = {
  sources: { otp: 1, main: 0.8, harvested: 0.6 },
  pilotCap: 0.05,
  regionWeights: {},
  regions: null,
  ownPlatform: null,
  ownRegionBoost: 1.5,
  patchWeights: {},
  hotfixes: [],
  beforeHotfixWeight: 0.5,
  memory: null,
};

/** Patch memory's window: today's patch and the five before it (about three months). */
export const MEMORY_PATCHES = 6;

/**
 * Data Dragon patches whose item and Irelia files patch memory reads: the window plus one, since a region can be a
 * patch behind Data Dragon (on patch day) and its window then reaches one patch further back.
 */
export const HISTORY_PATCHES = MEMORY_PATCHES + 1;

/** Previous-patch weights from the Data Dragon diff: nothing involved changed, something did, or the diff is unknown. */
export const PREVIOUS_PATCH = { unchanged: 0.8, changed: 0.2, unknown: 0.35 };

const TIER_WEIGHT: Record<string, number> = { CHALLENGER: 1, GRANDMASTER: 1, MASTER: 0.9, DIAMOND: 0.75, EMERALD: 0.6, PLATINUM: 0.5 };
const APEX = new Set(['CHALLENGER', 'GRANDMASTER', 'MASTER']);

/** Who played the game: source × tier (unknown 0.5) × Irelia points × LP within the apex tiers. */
export function pilotWeight(g: PreparedGame, o: WeightOptions): number {
  if (g.source === 'me') return 0;
  const source = o.sources[g.source] ?? 0;
  const tier = g.tier && g.tier !== 'UNKNOWN' ? TIER_WEIGHT[g.tier] ?? 0.4 : 0.5;
  const points = g.mastery === null ? 0.9 : 0.85 + 0.15 * Math.min(1, g.mastery / 500_000);
  const lp = g.tier && APEX.has(g.tier) && g.lp !== null ? 0.95 + 0.05 * Math.min(1, g.lp / 1000) : 1;
  const w = source * tier * points * lp;
  const strength = o.pilotStrength ?? 1;
  return strength === 1 || w <= 0 ? w : w ** strength;
}

const DAY_MS = 86_400_000;

/**
 * Fine-tuning's fade within a patch (WeightOptions.recencyDays): a game of its region's current patch counts
 * 0.5^(days before the newest game / recencyDays); other games (patch memory fades them by patch) count 1.
 */
export function recencyWeigher(newest: number, o: Pick<WeightOptions, 'recencyDays'>, ageOf: (g: PreparedGame) => number | null) {
  const days = o.recencyDays;
  if (!days || !(days > 0)) return () => 1;
  return (g: PreparedGame) => (ageOf(g) === 0 ? 0.5 ** (Math.max(0, newest - g.created) / (days * DAY_MS)) : 1);
}

/** What the previous patch changed, from Data Dragon fingerprints (ddragon.ts). */
export function patchChanges(data: StaticData) {
  const prev = data.previous;
  const ireliaChanged = prev ? prev.champion !== data.fingerprints.champion : null;
  return {
    /** null: unknown (the previous version's files could not be read). */
    ireliaChanged,
    itemChanged: (id: number) => (prev ? prev.items.get(id) !== data.fingerprints.items.get(id) : null),
  };
}

/** Patches newest first, from Data Dragon's version list. */
export function patchOrder(data: Pick<StaticData, 'version' | 'versions'>): string[] {
  const seen = new Set<string>();
  for (const v of data.versions ?? [data.version]) seen.add(toPatch(v));
  return [...seen];
}

/** How many patches `patch` is before `current` (0: the same one); null when it can't be told. */
export function patchAge(order: string[], patch: string, current: string): number | null {
  const a = order.indexOf(patch), b = order.indexOf(current);
  if (a >= 0 && b >= 0) return a - b;
  const [ma, na] = patch.split('.').map(Number), [mb, nb] = current.split('.').map(Number);
  return ma === mb && Number.isFinite(na) && Number.isFinite(nb) ? nb - na : null;
}

/**
 * patchAge over one patch order, each pair worked out once (0.15.1): the patch weights ask for every observation, and
 * the two searches through the order were 1.8 % of an evaluation's time.
 */
function patchAges(order: string[]) {
  const byCurrent = new Map<string, Map<string, number | null>>();
  return (patch: string, current: string): number | null => {
    let ages = byCurrent.get(current);
    if (!ages) { ages = new Map(); byCurrent.set(current, ages); }
    let age = ages.get(patch);
    if (age === undefined) { age = patchAge(order, patch, current); ages.set(patch, age); }
    return age;
  };
}

/** A game's age in patches on its region (0: its current patch); unknown counts as too old for any window. */
export function patchAger(data: StaticData, regionPatch: RegionPatchOf) {
  const ageOf = patchAges(patchOrder(data));
  return (g: Pick<PreparedGame, 'platform' | 'patch'>) => {
    const rp = regionPatch(g.platform) ?? data;
    if (g.patch === rp.patch) return 0;
    if (g.patch === rp.previousPatch && rp.previousPatch !== rp.patch) return 1;
    return ageOf(g.patch, rp.patch) ?? Number.POSITIVE_INFINITY;
  };
}

/**
 * Patch weight of a game for a decision involving `items` (none for runes, summoners and skills): 1 on its region's
 * current patch (0.5 before a hand-entered hotfix), the diff-based weight on the previous patch, and with patch memory
 * the same diff-based weight (Irelia and `items` against today's) fading by `decay` per further patch; 0 otherwise.
 */
export function patchWeigher(data: StaticData, regionPatch: RegionPatchOf, o: WeightOptions) {
  const changes = patchChanges(data);
  const ageOf = patchAges(o.memory ? patchOrder(data) : []);
  const fingerprintsOf = (patch: string): Fingerprints | undefined => (patch === data.patch ? data.fingerprints : data.history?.get(patch));
  /** Did Irelia or any of `items` change between `from` and `to`? null: unknown (no Data Dragon files for one of them). */
  const changedSince = (from: string, to: string, items: number[]) => {
    const a = fingerprintsOf(from), b = fingerprintsOf(to);
    if (!a || !b) return null;
    return a.champion !== b.champion || items.some((id) => a.items.get(id) !== b.items.get(id));
  };
  const latestHotfix = new Map<string, number>();
  for (const h of o.hotfixes) {
    const at = Date.parse(h.date);
    if (Number.isFinite(at)) latestHotfix.set(h.patch, Math.max(latestHotfix.get(h.patch) ?? 0, at));
  }
  return (g: PreparedGame, items: number[] = []): number => {
    const rp = regionPatch(g.platform) ?? data;
    if (g.patch === rp.patch) {
      const hotfix = latestHotfix.get(g.patch);
      return hotfix && g.created < hotfix ? o.beforeHotfixWeight : 1;
    }
    if (g.patch === rp.previousPatch && rp.previousPatch !== rp.patch) {
      const override = o.patchWeights[g.patch];
      if (override !== undefined) return override;
      // The diff describes Data Dragon's previous patch; a region a patch behind has no diff to read.
      if (g.patch !== data.previousPatch || changes.ireliaChanged === null) return PREVIOUS_PATCH.unknown;
      const changed = changes.ireliaChanged || items.some((id) => changes.itemChanged(id) !== false);
      return changed ? PREVIOUS_PATCH.changed : PREVIOUS_PATCH.unchanged;
    }
    if (!o.memory) return 0;
    const age = ageOf(g.patch, rp.patch);
    if (age === null || age < 2 || age >= o.memory.patches) return 0;
    const fade = o.memory.decay ** (age - 1);
    const override = o.patchWeights[g.patch];
    if (override !== undefined) return override * fade;
    const changed = changedSince(g.patch, rp.patch, items);
    return fade * (changed === null ? PREVIOUS_PATCH.unknown : changed ? PREVIOUS_PATCH.changed : PREVIOUS_PATCH.unchanged);
  };
}

/**
 * Patch memory's data check: an option (in a lane) whose games on the current patch clearly disagree with its games
 * on older patches (mean WPA |z| > 2, at least `minGames` effective games on each side) keeps its older games at
 * (2/z)² of their weight, so today's results lead; older evidence that today's agrees with keeps its weight. This
 * catches what Data Dragon can't show: hidden changes, bug fixes, other champions' changes. `obs` carry their `age`.
 */
export function consistent(obs: Observation[], minGames = 30): Observation[] {
  type Side = { sw: number; sw2: number; swx: number };
  const groups = new Map<string, { now: Side; old: Side }>();
  let sq = 0, sqW = 0;
  for (const ob of obs) {
    if (ob.w <= 0) continue;
    const key = `${ob.lane}|${ob.option}`;
    let g = groups.get(key);
    if (!g) { g = { now: { sw: 0, sw2: 0, swx: 0 }, old: { sw: 0, sw2: 0, swx: 0 } }; groups.set(key, g); }
    const side = (ob.age ?? 0) === 0 ? g.now : g.old;
    side.sw += ob.w; side.sw2 += ob.w * ob.w; side.swx += ob.w * ob.wpa;
    sq += ob.w * ob.wpa * ob.wpa; sqW += ob.w;
  }
  const sigma = Math.max(0.3, sqW > 0 ? Math.sqrt(sq / sqW) : 0.5);
  const factor = new Map<string, number>();
  for (const [key, { now, old }] of groups) {
    const nNow = now.sw2 > 0 ? now.sw ** 2 / now.sw2 : 0, nOld = old.sw2 > 0 ? old.sw ** 2 / old.sw2 : 0;
    if (nNow < minGames || nOld < minGames) continue;
    const z = (now.swx / now.sw - old.swx / old.sw) / (sigma * Math.sqrt(1 / nNow + 1 / nOld));
    if (Math.abs(z) > 2) factor.set(key, 4 / (z * z));
  }
  if (!factor.size) return obs;
  return obs.map((ob) => {
    const f = (ob.age ?? 0) > 0 ? factor.get(`${ob.lane}|${ob.option}`) : undefined;
    return f === undefined ? ob : { ...ob, w: ob.w * f };
  });
}

export function regionWeight(g: PreparedGame, o: WeightOptions): number {
  if (o.regions && !o.regions.includes(g.platform)) return 0;
  return (o.regionWeights[g.platform] ?? 1) * (o.ownPlatform && g.platform === o.ownPlatform ? o.ownRegionBoost : 1);
}

/**
 * Per-pilot factors so no pilot holds more than `cap` of a lane's weight (water-filling): the heaviest pilots are
 * cut to a common level c with c = cap × (lane total after the cut), solved exactly.
 */
export function pilotCapFactors(games: { pilot: string; lane: Lane; w: number }[], cap: number): Map<string, number> {
  const factors = new Map<string, number>();
  for (const lane of new Set(games.map((g) => g.lane))) {
    const totals = new Map<string, number>();
    for (const g of games) if (g.lane === lane && g.w > 0) totals.set(g.pilot, (totals.get(g.pilot) ?? 0) + g.w);
    if (!totals.size) continue;
    const limit = Math.max(cap, 1 / totals.size);
    const sorted = [...totals].sort((a, b) => b[1] - a[1]);
    let rest = sorted.reduce((s, [, w]) => s + w, 0);
    let level = Infinity;
    for (let capped = 0; capped < sorted.length; capped++) {
      const c = (limit * rest) / (1 - limit * capped);
      if (sorted[capped][1] <= c) { level = c; break; }
      rest -= sorted[capped][1];
    }
    for (const [p, w] of sorted) factors.set(`${lane}:${p}`, w > level ? level / w : 1);
  }
  return factors;
}
