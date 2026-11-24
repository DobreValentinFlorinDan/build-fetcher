import { toLane } from '../../shared/lanes.ts';
import { comparePatch } from '../ddragon.ts';
import { platformOfMatch } from '../riot/routing.ts';
import type { BuildRecommendation, Lane, Option, RunePage, Skill, SkillPlan } from '../../shared/types.ts';
import type { IreliaGameRow } from '../db.ts';
import type { StaticData } from '../ddragon.ts';
import type { ExclusivityIndex } from './exclusivity.ts';
import { validateGame, type ParsedGame } from './validator.ts';

export interface EngineSettings {
  minMatchupGames: number;
  generalPriorWeight: number;
  minLaneGames: number;
  otherLaneWeight: number;
  previousPatchWeight: number;
  minCurrentPatchGames: number;
  startingItemWindowMs: number;
}

/** The weighted median of whole numbers; null without data. */
function weightedMedian(xs: { n: number; w: number }[]): number | null {
  const total = xs.reduce((s, x) => s + x.w, 0);
  if (!total) return null;
  let acc = 0;
  for (const x of [...xs].sort((a, b) => a.n - b.n)) { acc += x.w; if (acc >= total / 2) return x.n; }
  return null;
}

/** Wilson score lower bound on (weighted) win rate - favours options that are both good and well-sampled. */
export function wilson(wins: number, n: number, z = 1.0): number {
  if (n <= 0) return 0;
  const p = wins / n;
  const denom = 1 + (z * z) / n;
  const centre = p + (z * z) / (2 * n);
  const margin = z * Math.sqrt((p * (1 - p)) / n + (z * z) / (4 * n * n));
  return (centre - margin) / denom;
}

class Tally<T> {
  private m = new Map<string, { value: T; w: number; wins: number; n: number }>();
  add(value: T, weight: number, win: boolean, key = JSON.stringify(value)) {
    const e = this.m.get(key) ?? { value, w: 0, wins: 0, n: 0 };
    e.w += weight; e.n += 1; if (win) e.wins += weight;
    this.m.set(key, e);
  }
  /** Options ranked by Wilson score, ignoring ones under `minShare` of the total weight (noise). */
  options(minShare = 0.05, filter: (v: T) => boolean = () => true): Option<T>[] {
    const total = [...this.m.values()].reduce((s, e) => s + e.w, 0);
    return [...this.m.values()]
      .filter((e) => e.w >= Math.max(0.5, total * minShare) && filter(e.value))
      .map((e) => ({ value: e.value, games: round(e.w), rawGames: e.n, winRate: e.wins / e.w, score: wilson(e.wins, e.w) }))
      .sort((a, b) => b.score - a.score || b.games - a.games);
  }
}

const round = (x: number) => Math.round(x * 100) / 100;

/** Q, W and E in the order they are maxed along a skill path (ties: more points first). */
export function derivePriority(path: Skill[]): Skill[] {
  const counts: Record<string, number> = { Q: 0, W: 0, E: 0 };
  const maxedAt: Record<string, number> = { Q: 99, W: 99, E: 99 };
  path.forEach((s, i) => { if (s !== 'R' && ++counts[s] === 5) maxedAt[s] = i; });
  return (['Q', 'W', 'E'] as Skill[]).sort((a, b) => maxedAt[a] - maxedAt[b] || counts[b] - counts[a]);
}

/** Levels 1-18: `prefix`, then R at 6/11/16 and the rest by `priority`. */
export function completePath(prefix: Skill[], priority: Skill[]): Skill[] {
  const path = [...prefix];
  const count = (s: Skill) => path.filter((x) => x === s).length;
  while (path.length < 18) {
    const lvl = path.length + 1;
    if ([6, 11, 16].includes(lvl) && count('R') < 3) { path.push('R'); continue; }
    path.push(priority.find((s) => count(s) < 5) ?? 'R');
  }
  return path;
}

/** A platform's current and previous patch (server/patches.ts); without one, Data Dragon's pair is used. */
export type RegionPatchOf = (platform: string) => { patch: string; previousPatch: string } | undefined;

/**
 * `lane` limits the build to games Irelia played in that lane (the other lane only blends in while this one
 * is thin); null mixes every position, as before lanes existed. `regionPatch` gives each platform its own current
 * patch: a region still on the old patch keeps its games current until it moves.
 */
/** Stored games validated for a build: the usable ones, and the others with why. */
export interface ValidatedRows { games: ParsedGame[]; discarded: { row: IreliaGameRow; stale: boolean; reason: string }[] }

function validate(rows: IreliaGameRow[], data: StaticData, excl: ExclusivityIndex, patchWeight: (r: IreliaGameRow) => number | undefined,
  startingItemWindowMs: number): ValidatedRows {
  const games: ParsedGame[] = [];
  const discarded: ValidatedRows['discarded'] = [];
  for (const row of rows) {
    const v = validateGame(row, data, patchWeight, startingItemWindowMs);
    if (!v.ok) { discarded.push({ row, stale: v.stale, reason: v.reason }); continue; }
    // A real game can't hold conflicting items, but corrupted/parsed data could - never learn from it.
    const conflicts = excl.conflicts([...v.game.completed, ...(v.game.boots ? [v.game.boots] : [])]);
    if (conflicts.length) { discarded.push({ row, stale: false, reason: 'exclusive-item conflict' }); continue; }
    games.push(v.game);
  }
  return { games, discarded };
}

export function recommend(
  rows: IreliaGameRow[],
  data: StaticData,
  excl: ExclusivityIndex,
  enemyChampionId: number | null,
  s: EngineSettings,
  lane: Lane | null = null,
  regionPatch: RegionPatchOf = () => undefined,
  /**
   * The validated games of these same `rows` (0.15.0): the model thread keeps them per data version, so the next
   * opponent in champ select doesn't validate every stored game again. They depend only on the patch admission.
   */
  memo?: Map<string, ValidatedRows>,
): BuildRecommendation {
  const inLane = (r: IreliaGameRow) => lane === null || toLane(r.position) === lane;
  const patchOf = (r: IreliaGameRow) => regionPatch(platformOfMatch(r.match_id)) ?? data;

  // ---- 1. Patch admission (counted on the requested lane's own games) ----------------------
  const currentCount = rows.filter((r) => r.patch === patchOf(r).patch && inLane(r)).length;
  const usePrevious = currentCount < s.minCurrentPatchGames;
  const patchWeight = (r: IreliaGameRow) => {
    const p = patchOf(r);
    if (r.patch === p.patch) return 1;
    return usePrevious && p.previousPatch !== p.patch && r.patch === p.previousPatch ? s.previousPatchWeight : undefined;
  };
  // The newest current patch among the regions with games (Data Dragon's when there are none).
  const regionPatches = [...new Set(rows.map((r) => patchOf(r).patch))];
  const patch = regionPatches.reduce((a, b) => (comparePatch(b, a) > 0 ? b : a), regionPatches[0] ?? data.patch);
  const admitted = [...new Set(rows.flatMap((r) => { const p = patchOf(r); return usePrevious && p.previousPatch !== p.patch ? [p.patch, p.previousPatch] : [p.patch]; }))];

  // ---- 2. Validation (stale patch, remakes, leavers, removed items/runes) ------------------
  const validated = memo?.get(String(usePrevious)) ?? validate(rows, data, excl, patchWeight, s.startingItemWindowMs);
  memo?.set(String(usePrevious), validated);
  const { games, discarded } = validated;

  // ---- 3. Lane + matchup weighting -------------------------------------------------------
  // weight = patch weight × lane weight × matchup weight. The other lane only blends in (otherLaneWeight)
  // while this lane has fewer than minLaneGames; jungle / bot / support / unknown games never feed a lane build.
  const laneGames = games.filter((g) => inLane(g.row));
  const usedLaneFallback = lane !== null && laneGames.length < s.minLaneGames;
  const feedsBuild = (r: IreliaGameRow) => inLane(r) || (usedLaneFallback && toLane(r.position) !== null);
  const laneWeight = (g: ParsedGame) => (inLane(g.row) ? 1 : feedsBuild(g.row) ? s.otherLaneWeight : 0);
  const isMatchup = (g: ParsedGame) => !enemyChampionId || g.row.opponent_champion_id === enemyChampionId;
  const matchup = laneGames.filter(isMatchup);
  const usedFallback = !!enemyChampionId && matchup.length < s.minMatchupGames;
  const weighted = games
    .map((g) => {
      const w = isMatchup(g) ? 1 : usedFallback ? s.generalPriorWeight : 0;
      return { g, w: w * laneWeight(g) * g.weight };
    })
    .filter((x) => x.w > 0);
  const otherLaneGames = weighted.filter((x) => !inLane(x.g.row)).length;

  // The validation report covers the games this build can use: its lane's, plus the other lane's while they
  // blend in. A lane build never reports jungle, bot or support games as used or discarded.
  const validation: BuildRecommendation['validation'] = {
    currentPatch: patch, freshGames: 0, previousPatchGames: 0, staleDiscarded: 0, invalidDiscarded: 0,
    invalidReasons: {}, conflictsRejected: [],
  };
  for (const { stale, reason } of discarded.filter((d) => feedsBuild(d.row))) {
    if (stale) validation.staleDiscarded++;
    else { validation.invalidDiscarded++; validation.invalidReasons[reason] = (validation.invalidReasons[reason] ?? 0) + 1; }
  }
  for (const { row } of games.filter((g) => feedsBuild(g.row))) {
    if (row.patch === patchOf(row).patch) validation.freshGames++; else validation.previousPatchGames++;
  }

  // ---- 4. Tallies ------------------------------------------------------------------------
  const runes = new Tally<RunePage>();
  const summs = new Tally<number[]>();
  const starts = new Tally<number[]>();
  const cores = new Tally<number[]>();
  const boots = new Tally<number>();
  const slots = [new Tally<number>(), new Tally<number>(), new Tally<number>()];
  const skills = new Tally<Skill[]>();
  const bootsAt: { n: number; w: number }[] = [];   // completed items finished before the tier-2 boots, per game

  for (const { g, w } of weighted) {
    const r = g.row;
    runes.add({ primaryStyle: r.primary_style, subStyle: r.sub_style, perks: g.perks, statPerks: g.statPerks }, w, g.win);
    summs.add([r.summoner1, r.summoner2].sort((a, b) => a - b), w, g.win);
    if (g.start.length) starts.add(g.start, w, g.win);
    if (g.completed.length >= 3) cores.add(g.completed.slice(0, 3), w, g.win);
    if (g.boots) {
      boots.add(g.boots, w, g.win);
      const first = (id: number) => g.purchases.find(([x]) => x === id)?.[1] ?? Infinity;
      const t = first(g.boots);
      bootsAt.push({ n: Math.min(3, g.completed.filter((id) => first(id) < t).length), w });
    }
    g.completed.slice(3, 6).forEach((id, i) => slots[i].add(id, w, g.win));
    const order = r.skill_order;
    const len = order.length >= 15 ? 15 : order.length >= 9 ? 9 : 0;
    if (len) skills.add(order.slice(0, len).split('') as Skill[], w, g.win);
  }

  // ---- 5. Exclusivity-safe assembly ------------------------------------------------------
  const coreOptions = cores.options(0.03, (ids) => {
    const c = excl.conflicts(ids);
    if (c.length) validation.conflictsRejected.push(...c.map(excl.describe));
    return c.length === 0;
  }).slice(0, 3);
  const core = coreOptions[0]?.value ?? [];
  const bootOptions = boots.options(0.05, (id) => excl.blocker(core, id) === null).slice(0, 3);
  const owned = [...core, ...(bootOptions[0] ? [bootOptions[0].value] : [])];

  const slotOptions = slots.map((t) =>
    t.options(0.03, (id) => {
      const block = excl.blocker(owned, id);
      if (block && block !== 'Duplicate') validation.conflictsRejected.push(`${data.items.get(id)?.name} blocked by ${block}`);
      return block === null;
    }).slice(0, 4),
  );

  const skillOpt = skills.options(0.03)[0];
  let skillPlan: Option<SkillPlan> | null = null;
  if (skillOpt) {
    const priority = derivePriority(completePath(skillOpt.value, derivePriority(skillOpt.value)));
    skillPlan = { ...skillOpt, value: { priority, path: completePath(skillOpt.value, priority) } };
  }

  const matchupWins = matchup.filter((g) => g.win).length;
  return {
    patch,
    ddragonVersion: data.version,
    enemyChampionId,
    lane,
    sample: {
      matchupGames: matchup.length,
      totalGames: games.length,
      patches: admitted.length ? admitted.sort((a, b) => comparePatch(b, a)) : usePrevious && data.previousPatch !== data.patch ? [data.patch, data.previousPatch] : [data.patch],
      usedFallback,
      laneGames: laneGames.length,
      otherLaneGames,
      usedLaneFallback,
    },
    winRate: matchup.length ? matchupWins / matchup.length : null,
    runes: runes.options(0.03)[0] ?? null,
    summoners: summs.options(0.05)[0] ?? null,
    startingItems: starts.options(0.05).slice(0, 2),
    coreBuild: coreOptions,
    boots: bootOptions,
    bootsAfter: weightedMedian(bootsAt),
    situational: { slot4: slotOptions[0], slot5: slotOptions[1], slot6: slotOptions[2] },
    skills: skillPlan,
    validation: { ...validation, conflictsRejected: [...new Set(validation.conflictsRejected)] },
  };
}
