// The context a build is chosen in: the draft (B3) and, for items, the game at the moment of the purchase (B5).
// Each dimension can be computed the same way from a stored game and from champ select or the Live Client, and is
// NaN when unknown (an enemy not picked yet, the lane opponent not known).
import type { ItemMeta } from '../ddragon.ts';
import { teamMagicShare, type ChampionProfile, type ChampionProfiles } from './profiles.ts';

export interface Dim {
  key: string;
  /** How a reason chip names it: "enemy magic damage 62%". */
  label: string;
  format: (v: number) => string;
}

const pct = (v: number) => `${Math.round(v * 100)}%`;
const perMin = (v: number) => `${Math.round(v).toLocaleString('en-US')}/min`;
const one = (v: number) => (Math.round(v * 10) / 10).toString();
const gold = (v: number) => `${v >= 0 ? '+' : ''}${(Math.round(v * 10) / 10).toFixed(1)}k`;
const signed = (v: number) => `${v >= 0 ? '+' : ''}${Math.round(v)}`;

/** Known from the draft. */
export const DRAFT_DIMS: Dim[] = [
  { key: 'eMagic', label: 'enemy magic damage', format: pct },
  { key: 'eHeal', label: 'enemy healing', format: perMin },
  { key: 'eShield', label: 'enemy shielding', format: perMin },
  { key: 'eCc', label: 'enemy crowd control', format: (v) => `${one(v)} s/min` },
  { key: 'eTank', label: 'enemy tankiness', format: perMin },
  { key: 'eRanged', label: 'ranged enemies', format: pct },
  { key: 'aMagic', label: 'ally magic damage', format: pct },
  { key: 'aTank', label: 'ally frontline', format: perMin },
  { key: 'oMagic', label: 'lane opponent magic damage', format: pct },
  { key: 'oTank', label: 'lane opponent tankiness', format: perMin },
  { key: 'oRanged', label: 'lane opponent', format: (v) => (v >= 0.5 ? 'ranged' : 'melee') },
  { key: 'oHeal', label: 'lane opponent healing', format: perMin },
];

/** Known in game, when an item is bought. */
export const LIVE_DIMS: Dim[] = [
  { key: 'eMagicThreat', label: 'magic damage of the enemies ahead', format: pct },
  { key: 'eArmor', label: 'enemy armour from items', format: one },
  { key: 'eMr', label: 'enemy magic resist from items', format: one },
  { key: 'eAntiHeal', label: 'enemies with anti-heal', format: one },
  { key: 'laneGold', label: 'lane item gold', format: gold },
  { key: 'teamGold', label: 'team item gold', format: gold },
  { key: 'kills', label: 'kill difference', format: signed },
  { key: 'minute', label: 'game time', format: (v) => `${Math.round(v)} min` },
];

export const ITEM_DIMS = [...DRAFT_DIMS, ...LIVE_DIMS];

const mean = (xs: number[]) => (xs.length ? xs.reduce((a, b) => a + b, 0) / xs.length : NaN);

/**
 * The draft context: enemy and ally team profiles, and the lane opponent's. Unknown parts are NaN. Champ select hides
 * enemy roles, so teams use each champion's pooled profile (stored games too, to compare like with like); only the lane
 * opponent's is for Irelia's lane.
 */
export function draftContext(profiles: ChampionProfiles, enemies: { id: number; position?: string | null }[],
  allies: { id: number; position?: string | null }[], opponent: { id: number; position?: string | null } | null): number[] {
  const e = enemies.map((c) => profiles.get(c.id, c.position));
  const a = allies.map((c) => profiles.get(c.id, c.position));
  const o = opponent ? profiles.get(opponent.id, opponent.position) : null;
  const ifAny = (ps: ChampionProfile[], f: (ps: ChampionProfile[]) => number) => (ps.length ? f(ps) : NaN);
  return [
    ifAny(e, (ps) => teamMagicShare(ps)),
    ifAny(e, (ps) => mean(ps.map((p) => p.healPerMin + p.allyHealPerMin))),
    ifAny(e, (ps) => mean(ps.map((p) => p.shieldPerMin))),
    ifAny(e, (ps) => mean(ps.map((p) => p.ccPerMin))),
    ifAny(e, (ps) => mean(ps.map((p) => p.tankPerMin))),
    ifAny(e, (ps) => mean(ps.map((p) => (p.ranged ? 1 : 0)))),
    ifAny(a, (ps) => teamMagicShare(ps)),
    ifAny(a, (ps) => mean(ps.map((p) => p.tankPerMin))),
    o ? o.magicShare : NaN,
    o ? o.tankPerMin : NaN,
    o ? (o.ranged ? 1 : 0) : NaN,
    o ? o.healPerMin + o.allyHealPerMin : NaN,
  ];
}

/** What the live part of the context needs about each enemy at one moment. */
export interface EnemyNow { profile: ChampionProfile; items: number[] }

/**
 * The live context when an item is bought: the enemies' damage mix weighted by how fed each is (inventory value),
 * the armour, magic resist and anti-heal they bought, and the gold, kill and time situation.
 */
export function liveContext(enemies: EnemyNow[], itemOf: (id: number) => ItemMeta | undefined,
  o: { laneGold: number; teamGold: number; kills: number; minute: number }): number[] {
  const value = (items: number[]) => items.reduce((s, id) => s + (itemOf(id)?.goldTotal ?? 0), 0);
  const values = enemies.map((e) => value(e.items));
  const avg = mean(values) || 1;
  const threat = values.map((v) => Math.max(0.25, v / avg));
  const sum = (items: number[], f: (it: ItemMeta) => number) => items.reduce((s, id) => { const it = itemOf(id); return s + (it ? f(it) : 0); }, 0);
  return [
    enemies.length ? teamMagicShare(enemies.map((e) => e.profile), threat) : NaN,
    enemies.length ? mean(enemies.map((e) => sum(e.items, (it) => it.armor))) : NaN,
    enemies.length ? mean(enemies.map((e) => sum(e.items, (it) => it.magicResist))) : NaN,
    enemies.length ? enemies.filter((e) => e.items.some((id) => itemOf(id)?.antiHeal)).length : NaN,
    o.laneGold / 1000, o.teamGold / 1000, o.kills, o.minute,
  ];
}

/** Mean and spread of each dimension over the stored games, to compare dimensions on one scale. */
export interface Scale { mean: number[]; sd: number[] }

export function scaleOf(vectors: number[][], dims: number): Scale {
  const m = new Array(dims).fill(0), s = new Array(dims).fill(0), n = new Array(dims).fill(0);
  for (const v of vectors) for (let i = 0; i < dims; i++) if (Number.isFinite(v[i])) { m[i] += v[i]; n[i]++; }
  for (let i = 0; i < dims; i++) m[i] = n[i] ? m[i] / n[i] : 0;
  for (const v of vectors) for (let i = 0; i < dims; i++) if (Number.isFinite(v[i])) s[i] += (v[i] - m[i]) ** 2;
  // A dimension that (almost) never varies says nothing about similarity: its spread is infinite, and rounding noise
  // can't make one draft look far from every stored game.
  return { mean: m, sd: s.map((x, i) => { const sd = n[i] > 1 ? Math.sqrt(x / (n[i] - 1)) : 0; return sd > 1e-4 * (1 + Math.abs(m[i])) ? sd : Infinity; }) };
}

/**
 * Standard deviations from the stored games' mean, held within ±4 so one extreme value can't dominate a distance. NaN
 * when unknown, or when the stored games never differ in that dimension (it can't tell drafts apart, so the kernel
 * leaves it out instead of counting it as "same").
 */
export const standardise = (v: number[], sc: Scale) => v.map((x, i) => standardOne(x, sc, i));
const standardOne = (x: number, sc: Scale, i: number) => (Number.isFinite(x) && Number.isFinite(sc.sd[i])
  ? Math.max(-4, Math.min(4, (x - sc.mean[i]) / sc.sd[i]))
  : NaN);

/**
 * Standardised contexts of one width, one row after another in one array (0.14.0): row r is flat[r·d] to
 * flat[r·d + d − 1]. A draft model holds its games' contexts this way instead of an array per game.
 */
export interface Contexts { flat: Float64Array; d: number; rows: number; anyNaN: boolean }

/** `rows` contexts `d` wide, filled by `fill(out, at)` writing row r from `at` (left NaN: unknown). */
export function contextsOf(rows: number, d: number, fill: (out: Float64Array, row: number, at: number) => void): Contexts {
  const flat = new Float64Array(rows * d).fill(NaN);
  for (let r = 0; r < rows; r++) fill(flat, r, r * d);
  let anyNaN = false;
  for (let i = 0; i < flat.length; i++) if (Number.isNaN(flat[i])) { anyNaN = true; break; }
  return { flat, d, rows, anyNaN };
}

/** Contexts from arrays (tests, and contexts gathered row by row): longer ones cut to `d`, shorter ones NaN-padded. */
export const contextsFrom = (zs: ArrayLike<number>[], d = zs[0]?.length ?? 0): Contexts =>
  contextsOf(zs.length, d, (out, r, at) => { const z = zs[r]; for (let j = 0; j < d && j < z.length; j++) out[at + j] = z[j]; });

/** standardise(v, sc) written into `out` from `at` (at most `d` values; NaN-padded as contextsOf leaves it). */
export function standardiseInto(v: number[], sc: Scale, out: Float64Array, at: number, d: number) {
  for (let i = 0; i < v.length && i < d; i++) out[at + i] = standardOne(v[i], sc, i);
}

/**
 * Similarity of a stored game's context to the query's: a Gaussian kernel on the mean squared standardised distance
 * over the dimensions both know. `only` uses one dimension alone (to find which one drove a pick). 1 when none is known.
 */
export function kernel(game: ArrayLike<number>, query: ArrayLike<number>, h: number, only = -1): number {
  if (!Number.isFinite(h)) return 1;
  let d2 = 0, n = 0;
  for (let i = only >= 0 ? only : 0; i < (only >= 0 ? only + 1 : query.length); i++) {
    const q = query[i], g = game[i];
    if (Number.isNaN(q) || Number.isNaN(g)) continue;
    d2 += (g - q) ** 2; n++;
  }
  return n ? Math.exp(-(d2 / n) / (2 * h * h)) : 1;
}

/**
 * kernel() for row `row` of `c`: the same number as kernel() on that row as an array (a dimension past the row's
 * width is unknown, as an array's missing entry is).
 */
export function kernelAt(c: Contexts, row: number, query: ArrayLike<number>, h: number, only = -1): number {
  if (!Number.isFinite(h)) return 1;
  const { flat, d } = c, at = row * d;
  let d2 = 0, n = 0;
  for (let i = only >= 0 ? only : 0; i < (only >= 0 ? only + 1 : query.length) && i < d; i++) {
    const q = query[i], g = flat[at + i];
    if (Number.isNaN(q) || Number.isNaN(g)) continue;
    d2 += (g - q) ** 2; n++;
  }
  return n ? Math.exp(-(d2 / n) / (2 * h * h)) : 1;
}
