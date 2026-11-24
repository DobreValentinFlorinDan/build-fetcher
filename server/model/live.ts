// B5 live re-ranking: in game, the next completed item is re-scored from the Live Client Data API alone (scoreboard
// items, levels, CS, kills): the enemies' damage mix weighted by how fed each is, the armour, magic resist and anti-heal
// they bought, and the gold and kill situation. Only unbought slots are scored; owned items, components in progress
// and the shop's exclusivity rules are respected. "Next" only changes when another item leads it by δ for several
// polls in a row. Build advice only: never "fight", "recall" or a timer. docs/BUILD-SOURCING.md §9.5.
import { toLane } from '../../shared/lanes.ts';
import type { LiveNext, PickStats } from '../../shared/types.ts';
import type { StaticData } from '../ddragon.ts';
import { identify, type AllGameData, type LiveApiPlayer } from '../engine/adaptive.ts';
import { isPurchasableItem, type ExclusivityIndex } from '../engine/exclusivity.ts';
import type { RegionPatchOf } from '../engine/recommend.ts';
import { isCompletedItem } from '../engine/validator.ts';
import { draftContext, ITEM_DIMS, liveContext, standardise } from './context.ts';
import { drivingDimension, observed, weighGames, type DraftModel, type DraftSettings } from './draft.ts';
import { estimateDecision, type Observation, type OptionEstimate } from './estimate.ts';

const IRELIA = 'Irelia';

export interface StabilityOptions {
  /** A challenger must lead "Next" by this much win-probability-added... */
  delta: number;
  /** ...on this many polls in a row. */
  polls: number;
}
export const STABILITY_DEFAULTS: StabilityOptions = { delta: 0.01, polls: 3 };

const toStats = (e: OptionEstimate): PickStats => ({
  lift: e.theta, low: e.low, high: e.high, nEff: e.nEff, games: e.games, pilots: e.pilots, winRate: e.winRate, matchupGames: e.matchupGames,
});

/** Every component (recursively) an item is built from. */
function componentsOf(data: StaticData, id: number, out = new Set<number>()): Set<number> {
  for (const c of data.items.get(id)?.from ?? []) { out.add(c); componentsOf(data, c, out); }
  return out;
}

/** The live state the re-ranker reads, in the model's terms. */
export interface LiveQuery {
  lane: 'TOP' | 'MIDDLE';
  slot: number;                 // 0-based index of the next completed item
  owned: number[];
  opponent: number | null;
  z: number[];
  raw: number[];
  fedEnemy: { name: string; ap: boolean } | null;
}

/** The app's matchup in game (server/matchup.ts): who you face and your lane. Without it, your position decides. */
export interface LiveHint { opponentName?: string | null; lane?: 'TOP' | 'MIDDLE' | null }

export function liveQuery(raw: AllGameData, model: DraftModel, data: StaticData, hint: LiveHint = {}): LiveQuery | null {
  const { me, opponent } = identify(raw, hint.opponentName);
  if (!me || data.championsByKey.get(me.championName)?.key !== IRELIA) return null;
  const lane = hint.lane ?? toLane(me.position) ?? 'TOP';
  const owned = me.items.map((i) => i.itemID).filter((id) => id > 0);
  const slot = owned.filter((id) => isCompletedItem(data.items.get(id))).length;
  if (slot >= 6) return null;
  const champ = (p: LiveApiPlayer) => data.championsByKey.get(p.championName)?.id ?? null;
  const enemies = raw.allPlayers.filter((p) => p.team !== me.team);
  const allies = raw.allPlayers.filter((p) => p.team === me.team && p !== me);
  const ids = (ps: LiveApiPlayer[]) => ps.map(champ).filter((id): id is number => id !== null);
  const oppId = opponent ? champ(opponent) : null;
  const draft = draftContext(model.profiles, ids(enemies).map((id) => ({ id })), ids(allies).map((id) => ({ id })),
    oppId !== null ? { id: oppId, position: lane } : null);
  const value = (p: LiveApiPlayer) => p.items.reduce((s, it) => s + (data.items.get(it.itemID)?.goldTotal ?? 0) * Math.max(1, it.count ?? 1), 0);
  const team = (ps: LiveApiPlayer[]) => ps.reduce((s, p) => s + value(p), 0);
  const kills = (ps: LiveApiPlayer[]) => ps.reduce((s, p) => s + p.scores.kills, 0);
  const mine = [me, ...allies];
  const enemyNow = enemies.filter((p) => champ(p) !== null).map((p) => ({ profile: model.profiles.get(champ(p)!), items: p.items.map((i) => i.itemID) }));
  const live = liveContext(enemyNow, (id) => data.items.get(id), {
    laneGold: opponent ? value(me) - value(opponent) : NaN,
    teamGold: team(mine) - team(enemies), kills: kills(mine) - kills(enemies), minute: raw.gameData.gameTime / 60,
  });
  // "Fed" only when one enemy's items are worth a quarter more than the enemy average.
  const avg = enemies.reduce((sum, p) => sum + value(p), 0) / Math.max(1, enemies.length);
  const top = [...enemies].sort((a, b) => value(b) - value(a))[0];
  const fed = top && avg > 0 && value(top) >= 1.25 * avg ? top : undefined;
  const fedProfile = fed && champ(fed) !== null ? model.profiles.get(champ(fed)!) : null;
  return {
    lane, slot, owned, opponent: oppId,
    z: [...standardise(draft, model.draftScale), ...standardise(live, model.liveScale)],
    raw: [...draft, ...live],
    fedEnemy: fed && fedProfile ? { name: data.championsByKey.get(fed.championName)?.name ?? fed.championName, ap: fedProfile.magicShare > 0.5 } : null,
  };
}

/**
 * The stability rule for "Next": a new slot (or a "Next" that can no longer be bought) takes the item in progress, else
 * the best; afterwards another item must lead by δ on `polls` polls in a row, and an item in progress is never displaced.
 */
export class NextStability {
  current: string | null = null;
  private slot = -1;
  private challenger: { option: string; polls: number } | null = null;

  constructor(private o: StabilityOptions = STABILITY_DEFAULTS) {}

  reset() { this.current = null; this.slot = -1; this.challenger = null; }

  /** `options` best first. Returns true when "Next" changed. */
  step(slot: number, options: { option: string; theta: number }[], inProgress: string | null): boolean {
    if (!options.length) { const had = this.current !== null; this.reset(); return had; }
    const top = options[0];
    const current = slot === this.slot ? options.find((x) => x.option === this.current) : undefined;
    if (!current) {
      this.current = inProgress && options.some((x) => x.option === inProgress) ? inProgress : top.option;
      this.slot = slot;
      this.challenger = null;
      return true;
    }
    if (top.option !== current.option && top.theta - current.theta >= this.o.delta && inProgress !== current.option) {
      this.challenger = this.challenger?.option === top.option ? { option: top.option, polls: this.challenger.polls + 1 } : { option: top.option, polls: 1 };
      if (this.challenger.polls >= this.o.polls) {
        this.current = top.option;
        this.challenger = null;
        return true;
      }
      return false;
    }
    this.challenger = null;
    return false;
  }
}

/** How a reason chip names a live dimension; the threat-weighted damage mix names the most fed enemy. */
function reasonText(dim: number, q: LiveQuery): string {
  const d = ITEM_DIMS[dim];
  if (d.key === 'eMagicThreat' && q.fedEnemy) return `fed ${q.fedEnemy.name} (${q.fedEnemy.ap ? 'AP' : 'AD'})`;
  return `${d.label} ${d.format(q.raw[dim])}`;
}

/**
 * Re-scores the next item slot on every poll and keeps "Next" stable. Observations are cached per model and settings;
 * the reason is searched only when "Next" changes (it costs one estimate per context dimension).
 */
export class LiveReranker {
  private stable: NextStability;
  private reasonText: string | null = null;
  private obsCache: { key: string; bySlot: Map<number, Observation[]> } | null = null;
  /** Milliseconds the last re-rank took (the target is < 50 ms at ten times today's data). */
  lastMs = 0;

  constructor(stability: StabilityOptions = STABILITY_DEFAULTS) { this.stable = new NextStability(stability); }

  reset() { this.stable.reset(); this.reasonText = null; }

  /** Lets go of the observations kept for the game (their first poll builds them again); whether there were any. */
  release(): boolean {
    const had = this.obsCache !== null;
    this.obsCache = null;
    return had;
  }

  private observations(model: DraftModel, key: string, data: StaticData, regionPatch: RegionPatchOf, s: DraftSettings, slot: number) {
    if (this.obsCache?.key !== key) this.obsCache = { key, bySlot: new Map() };
    let obs = this.obsCache.bySlot.get(slot);
    if (!obs) {
      obs = observed(weighGames(model, data, regionPatch, s.weights).flatMap(({ i, s: sg, base, patch, age }) => {
        const c = sg.g.items[slot];
        if (!c) return [];
        const w = base * patch([c.item]);
        return w > 0 ? [{ option: String(c.item), wpa: sg.itemWpa[slot], win: sg.g.win, w, lane: sg.g.lane, opponent: sg.g.opponent, pilot: sg.g.pilot, zs: model.zItems, zRow: model.itemRow[i] + slot, age }] : [];
      }), s.weights);
      this.obsCache.bySlot.set(slot, obs);
    }
    return obs;
  }

  /** `version` identifies the model build (the observation cache is kept while it holds). */
  rerank(model: DraftModel, version: string, raw: AllGameData, data: StaticData, excl: ExclusivityIndex,
    regionPatch: RegionPatchOf, s: DraftSettings, hint: LiveHint = {}): LiveNext | null {
    const t0 = performance.now();
    try {
      const q = liveQuery(raw, model, data, hint);
      if (!q) { this.reset(); return null; }
      const obs = this.observations(model, `${version}|${JSON.stringify(s)}`, data, regionPatch, s, q.slot);
      const query = { lane: q.lane, opponent: q.opponent, z: q.z };
      const est = estimateDecision(obs, query, s.itemK !== undefined ? { ...s.estimate, k: s.itemK } : s.estimate);
      const ok = (o: OptionEstimate) => isPurchasableItem(data.items.get(Number(o.option))) && excl.blocker(q.owned, Number(o.option)) === null;
      const options = est.options.filter(ok);
      if (!options.length) { this.reset(); return null; }
      const byItem = (id: number) => options.find((o) => Number(o.option) === id);

      // Components in progress: the candidate the owned components build into (most component gold wins).
      const ownedSet = new Set(q.owned);
      let inProgress: { item: number; gold: number } | null = null;
      for (const o of options) {
        const gold = [...componentsOf(data, Number(o.option))].filter((c) => ownedSet.has(c)).reduce((sum, c) => sum + (data.items.get(c)?.goldTotal ?? 0), 0);
        if (gold > 0 && (!inProgress || gold > inProgress.gold)) inProgress = { item: Number(o.option), gold };
      }

      const changed = this.stable.step(q.slot, options, inProgress ? String(inProgress.item) : null);
      if (changed) this.reasonText = null;
      const chosen = byItem(Number(this.stable.current))!;
      if (changed) this.reasonText = this.reason(chosen, options, obs, query, q, s);
      return {
        itemId: Number(chosen.option), slot: q.slot + 1, reason: this.reasonText, stats: toStats(chosen),
        inProgress: inProgress?.item === Number(chosen.option),
        alternatives: options.filter((o) => o !== chosen).slice(0, 3).map((o) => ({ itemId: Number(o.option), stats: toStats(o) })),
      };
    } finally {
      this.lastMs = performance.now() - t0;
    }
  }

  /** The context dimension that alone gives the pick its largest lead over the runner-up. */
  private reason(chosen: OptionEstimate, options: OptionEstimate[], obs: Observation[], query: { lane: 'TOP' | 'MIDDLE'; opponent: number | null; z: number[] }, q: LiveQuery, s: DraftSettings): string | null {
    const rival = options.find((o) => o !== chosen);
    return rival ? drivingDimension(chosen.option, rival.option, obs, query, s.itemK !== undefined ? { ...s.estimate, k: s.itemK } : s.estimate, (j) => reasonText(j, q)) : null;
  }
}
