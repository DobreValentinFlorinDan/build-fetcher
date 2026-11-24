// B1 champion profiles: what each champion does in a game, from every stored player (`participants`) and the totals
// kept for purged patches (`champion_patch_stats`). The draft model (model/draft.ts) and the live counters read them
// instead of a hand-made healer list and Data Dragon's 1-10 attack / magic ratings. docs/BUILD-SOURCING.md §9.1.
import type { ChampionMeta, StaticData } from '../ddragon.ts';

/** Summed whole-game stats of one champion in one position (db.championTotals). */
export interface ChampionTotals {
  champion_id: number;
  position: string;
  games: number;
  seconds: number;
  dmg_physical: number; dmg_magic: number; dmg_true: number;
  dmg_taken: number; dmg_mitigated: number;
  heal: number; heal_allies: number; shield_allies: number;
  cc_time: number;
}

export interface ChampionProfile {
  championId: number;
  /** Position the profile is for; null pools every position. */
  position: string | null;
  /** Games behind it, after older patches' discount. 0: the class prior alone. */
  games: number;
  cls: string;              // Data Dragon's first tag (Fighter, Mage, ...)
  range: number;            // Data Dragon attack range
  ranged: boolean;
  /** Shares of damage dealt to champions. */
  physShare: number; magicShare: number; trueShare: number;
  /** Per minute of game time. */
  dmgPerMin: number;
  healPerMin: number;       // healing on themselves (totalHeal minus healing on allies)
  allyHealPerMin: number;
  shieldPerMin: number;     // shields on allies
  ccPerMin: number;         // seconds of crowd control on enemies (timeCCingOthers)
  tankPerMin: number;       // damage taken plus damage mitigated
}

const RATE_KEYS = ['dmgPerMin', 'healPerMin', 'allyHealPerMin', 'shieldPerMin', 'ccPerMin', 'tankPerMin'] as const;
const SHARE_KEYS = ['physShare', 'magicShare', 'trueShare'] as const;
type StatKey = typeof RATE_KEYS[number] | typeof SHARE_KEYS[number];
type Stats = Record<StatKey, number>;

/** Ranged champions' attack range starts above this (melee is 125-250). */
export const RANGED_FROM = 300;

export interface ProfileOptions {
  /** Pseudo-games of the prior: a champion with k games is half its own data, half its class's. */
  k: number;
  /** Weight of a purged patch's games (champion_patch_stats) against the current and previous patch. */
  olderWeight: number;
}
export const PROFILE_DEFAULTS: ProfileOptions = { k: 50, olderWeight: 0.5 };

interface Sums { games: number; minutes: number; phys: number; magic: number; true_: number; taken: number; heal: number; healAllies: number; shield: number; cc: number }
const zero = (): Sums => ({ games: 0, minutes: 0, phys: 0, magic: 0, true_: 0, taken: 0, heal: 0, healAllies: 0, shield: 0, cc: 0 });
function addTo(s: Sums, t: ChampionTotals, w: number) {
  s.games += t.games * w; s.minutes += (t.seconds / 60) * w;
  s.phys += t.dmg_physical * w; s.magic += t.dmg_magic * w; s.true_ += t.dmg_true * w;
  s.taken += (t.dmg_taken + t.dmg_mitigated) * w;
  s.heal += Math.max(0, t.heal - t.heal_allies) * w; s.healAllies += t.heal_allies * w; s.shield += t.shield_allies * w;
  s.cc += t.cc_time * w;
}
function merge(a: Sums, b: Sums) { for (const k of Object.keys(a) as (keyof Sums)[]) a[k] += b[k]; }

function statsOf(s: Sums): Stats {
  const dmg = s.phys + s.magic + s.true_;
  const per = (x: number) => (s.minutes > 0 ? x / s.minutes : 0);
  return {
    physShare: dmg > 0 ? s.phys / dmg : 0, magicShare: dmg > 0 ? s.magic / dmg : 0, trueShare: dmg > 0 ? s.true_ / dmg : 0,
    dmgPerMin: per(dmg), healPerMin: per(s.heal), allyHealPerMin: per(s.healAllies), shieldPerMin: per(s.shield),
    ccPerMin: per(s.cc), tankPerMin: per(s.taken),
  };
}

/** x̂ = (n·x̄ + k·prior) / (n + k), stat by stat. */
function shrink(own: Stats, n: number, prior: Stats, k: number): Stats {
  const out = {} as Stats;
  for (const key of [...RATE_KEYS, ...SHARE_KEYS]) out[key] = (n * own[key] + k * prior[key]) / (n + k);
  return out;
}

/** With no stored games at all: Data Dragon's attack / magic ratings for the damage mix, typical rates otherwise. */
function noDataPrior(c: ChampionMeta | undefined): Stats {
  const attack = c?.info.attack ?? 5, magic = c?.info.magic ?? 5;
  const magicShare = attack + magic > 0 ? magic / (attack + magic) : 0.5;
  return {
    physShare: 1 - magicShare, magicShare, trueShare: 0,
    dmgPerMin: 700, healPerMin: 120, allyHealPerMin: 0, shieldPerMin: 0, ccPerMin: 0.8, tankPerMin: 1500,
  };
}

export class ChampionProfiles {
  private byChampion = new Map<number, { sums: Sums; positions: Map<string, Sums> }>();
  private classStats = new Map<string, { stats: Stats; games: number }>();
  private global: { stats: Stats; games: number } | null = null;
  private cache = new Map<string, ChampionProfile>();
  private healerFrom = Infinity;
  private tankFrom = Infinity;
  /** Games behind every profile (current and previous patch, plus older patches' discounted totals). */
  readonly games: number;

  constructor(current: ChampionTotals[], older: ChampionTotals[], private data: StaticData, private o: ProfileOptions = PROFILE_DEFAULTS) {
    const add = (t: ChampionTotals, w: number) => {
      const c = this.byChampion.get(t.champion_id) ?? { sums: zero(), positions: new Map<string, Sums>() };
      addTo(c.sums, t, w);
      const pos = c.positions.get(t.position) ?? zero();
      addTo(pos, t, w);
      c.positions.set(t.position, pos);
      this.byChampion.set(t.champion_id, c);
    };
    for (const t of current) add(t, 1);
    for (const t of older) add(t, o.olderWeight);

    const classes = new Map<string, Sums>();
    const all = zero();
    for (const [id, c] of this.byChampion) {
      const cls = this.clsOf(id);
      const s = classes.get(cls) ?? zero();
      merge(s, c.sums);
      classes.set(cls, s);
      merge(all, c.sums);
    }
    this.games = all.games / 10;   // ten participants per match
    if (all.games > 0) this.global = { stats: statsOf(all), games: all.games };
    for (const [cls, s] of classes) {
      // A class with few games leans on every champion's average.
      const own = statsOf(s);
      this.classStats.set(cls, { stats: this.global ? shrink(own, s.games, this.global.stats, o.k) : own, games: s.games });
    }

    // Thresholds from champions with real samples: a healer heals at least twice the median champion; a tank is in
    // the top fifth for damage taken plus mitigated.
    const solid = [...this.byChampion.keys()].map((id) => this.get(id)).filter((p) => p.games >= 20);
    if (solid.length >= 10) {
      const heals = solid.map((p) => p.healPerMin + p.allyHealPerMin).sort((a, b) => a - b);
      const tanks = solid.map((p) => p.tankPerMin).sort((a, b) => a - b);
      this.healerFrom = 2 * heals[Math.floor(heals.length / 2)];
      this.tankFrom = tanks[Math.floor(tanks.length * 0.8)];
    }
  }

  private clsOf(id: number) { return this.data.champions.get(id)?.tags[0] ?? 'Unknown'; }

  /** The profile of a champion, in a position when one is given (shrunk toward the champion's pooled profile). */
  get(championId: number, position?: string | null): ChampionProfile {
    const key = `${championId}:${position ?? ''}`;
    const hit = this.cache.get(key);
    if (hit) return hit;
    const meta = this.data.champions.get(championId);
    const cls = this.clsOf(championId);
    const prior = this.classStats.get(cls)?.stats ?? this.global?.stats ?? noDataPrior(meta);
    const c = this.byChampion.get(championId);
    let stats = c ? shrink(statsOf(c.sums), c.sums.games, prior, this.o.k) : prior;
    let games = c?.sums.games ?? 0;
    const pos = position ? c?.positions.get(position) : undefined;
    if (pos) { stats = shrink(statsOf(pos), pos.games, stats, this.o.k); games = pos.games; }
    const range = meta?.attackRange ?? 0;
    const profile: ChampionProfile = { championId, position: position ?? null, games, cls, range, ranged: range > RANGED_FROM, ...stats };
    this.cache.set(key, profile);
    return profile;
  }

  /** Heals itself or its allies more than twice as much as the median champion (with stored games). */
  isHealer(p: ChampionProfile) { return p.healPerMin + p.allyHealPerMin > this.healerFrom; }
  /** Above the top-fifth mark of champions for damage taken plus mitigated per minute (ties don't count). */
  isTank(p: ChampionProfile) { return p.tankPerMin > this.tankFrom; }
  /** False until enough champions have games to set the healer and tank thresholds. */
  get calibrated() { return Number.isFinite(this.healerFrom); }
}

/**
 * A team's damage mix: each champion's magic share weighted by the damage it deals (a support's mix counts less
 * than a carry's), and by `threat` when given (in game: how fed each enemy is).
 */
export function teamMagicShare(team: ChampionProfile[], threat?: number[]): number {
  let dmg = 0, magic = 0;
  team.forEach((p, i) => {
    const w = p.dmgPerMin * (threat?.[i] ?? 1);
    dmg += w; magic += w * p.magicShare;
  });
  return dmg > 0 ? magic / dmg : 0.5;
}
