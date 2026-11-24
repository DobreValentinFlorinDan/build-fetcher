// Pure formatting and parsing for the dashboard (unit-tested in format.test.ts, no DOM).
import type { BudgetStatus, BuildOption, OptionKind, PairVolume, Role } from './types.ts';

export function pct(x: number | null | undefined, digits = 1): string {
  if (x == null || !Number.isFinite(x)) return '–';
  return `${(x * 100).toFixed(digits)}%`;
}

/** WPA (a fraction) as signed percentage points: +2.1 pts. */
export function pts(x: number | null | undefined): string {
  if (x == null || !Number.isFinite(x)) return '–';
  const v = x * 100;
  return `${v > 0 ? '+' : v < 0 ? '−' : '±'}${Math.abs(v).toFixed(1)} pts`;
}

export function count(n: number): string {
  return n.toLocaleString('en-US');
}

export function bytes(n: number | null | undefined): string {
  if (n == null || !Number.isFinite(n)) return '–';
  const units = ['B', 'KB', 'MB', 'GB', 'TB'];
  let v = n;
  let i = 0;
  while (v >= 1000 && i < units.length - 1) {
    v /= 1000;
    i += 1;
  }
  return `${i === 0 ? v.toFixed(0) : v.toFixed(v < 10 ? 1 : 0)} ${units[i]}`;
}

/** How long ago, coarse: "just now", "12 min ago", "3 h ago", "2 days ago". */
export function ago(ms: number, now: number): string {
  const s = Math.max(0, (now - ms) / 1000);
  if (s < 60) return 'just now';
  if (s < 3600) return `${Math.floor(s / 60)} min ago`;
  if (s < 2 * 86400) return `${Math.floor(s / 3600)} h ago`;
  return `${Math.floor(s / 86400)} days ago`;
}

export const ROLE_NAMES: Record<Role, string> = { TOP: 'Top', JUNGLE: 'Jungle', MIDDLE: 'Mid', BOTTOM: 'Bot', UTILITY: 'Support' };

export const KIND_TITLES: Record<OptionKind, string> = {
  runePage: 'Runes',
  keystone: 'Keystone',
  summoners: 'Summoner spells',
  skillMax: 'Skill max order',
  skillOrder: 'First skill points',
  startingItems: 'Starting items',
  firstItem: 'First item',
  coreItems: 'Core items',
  boots: 'Boots',
  lateItem: 'Late-game items and swaps',
};

/** The order sections are shown in. */
export const KIND_ORDER: OptionKind[] = ['runePage', 'keystone', 'summoners', 'skillMax', 'skillOrder', 'startingItems', 'firstItem', 'coreItems', 'boots', 'lateItem'];

export type RunePage = { style: number; perks: number[]; subStyle: number; subPerks: number[]; shards: number[] };

/** `8000:8010,9111,9104,8299/8400:8444,8473/5008,5008,5011` → its parts (null when malformed). */
export function parseRunePage(key: string): RunePage | null {
  const parts = key.split('/');
  if (parts.length !== 3) return null;
  const tree = (p: string) => {
    const [s, rest] = p.split(':');
    const ids = (rest ?? '').split(',').map(Number);
    return { s: Number(s), ids };
  };
  const a = tree(parts[0]);
  const b = tree(parts[1]);
  const shards = parts[2].split(',').map(Number);
  const all = [a.s, b.s, ...a.ids, ...b.ids, ...shards];
  if (all.some((x) => !Number.isInteger(x)) || a.ids.length !== 4 || b.ids.length !== 2 || shards.length !== 3) return null;
  return { style: a.s, perks: a.ids, subStyle: b.s, subPerks: b.ids, shards };
}

/** What an option is made of, for drawing it. */
export type OptionParts =
  | { kind: 'items'; ids: number[]; ordered: boolean }
  | { kind: 'runes'; page: RunePage }
  | { kind: 'keystone'; id: number }
  | { kind: 'summoners'; ids: number[] }
  | { kind: 'skills'; letters: string[]; ordered: boolean }
  | { kind: 'text'; text: string };

export function optionParts(o: BuildOption): OptionParts {
  const nums = (sep: string) => o.key.split(sep).map(Number).filter((x) => Number.isInteger(x) && x > 0);
  switch (o.kind) {
    case 'runePage': {
      const page = parseRunePage(o.key);
      return page ? { kind: 'runes', page } : { kind: 'text', text: o.key };
    }
    case 'keystone':
      return { kind: 'keystone', id: Number(o.key) };
    case 'summoners':
      return { kind: 'summoners', ids: nums(',') };
    case 'skillMax':
      return { kind: 'skills', letters: o.key.split('>'), ordered: true };
    case 'skillOrder':
      return { kind: 'skills', letters: o.key.split(','), ordered: true };
    case 'startingItems':
      return { kind: 'items', ids: nums(','), ordered: false };
    case 'coreItems':
      return { kind: 'items', ids: nums('>'), ordered: true };
    case 'firstItem':
    case 'boots':
    case 'lateItem':
      return { kind: 'items', ids: nums(','), ordered: false };
    default:
      return { kind: 'text', text: o.key };
  }
}

/** Stat shards aren't in Data Dragon's runesReforged. */
export const SHARDS: Record<number, { name: string; icon: string }> = {
  5001: { name: 'Health scaling', icon: 'perk-images/StatMods/StatModsHealthPlusIcon.png' },
  5005: { name: 'Attack speed', icon: 'perk-images/StatMods/StatModsAttackSpeedIcon.png' },
  5007: { name: 'Ability haste', icon: 'perk-images/StatMods/StatModsCDRScalingIcon.png' },
  5008: { name: 'Adaptive force', icon: 'perk-images/StatMods/StatModsAdaptiveForceIcon.png' },
  5010: { name: 'Move speed', icon: 'perk-images/StatMods/StatModsMovementSpeedIcon.png' },
  5011: { name: 'Health', icon: 'perk-images/StatMods/StatModsHealthScalingIcon.png' },
  5013: { name: 'Tenacity and slow resist', icon: 'perk-images/StatMods/StatModsTenacityIcon.png' },
};

/** A budget's meter: the share Riot counted and the share that was ours (0-1 each). */
export function meter(b: BudgetStatus): { used: number; ours: number; other: number } {
  const limit = b.limit > 0 ? b.limit : 1;
  const used = Math.min(1, Math.max(0, b.used / limit));
  const ours = Math.min(used, Math.max(0, b.ours / limit));
  return { used, ours, other: used - ours };
}

export type ChampionAge = { champion: number; games: number; newestMs: number; roles: Role[] };

/** Data age per champion: games over all roles and patches, the newest game, its roles by games. */
export function dataAge(volumes: PairVolume[]): ChampionAge[] {
  const by = new Map<number, { games: number; newestMs: number; roles: Map<Role, number> }>();
  for (const v of volumes) {
    const e = by.get(v.champion) ?? { games: 0, newestMs: 0, roles: new Map() };
    e.games += v.games;
    e.newestMs = Math.max(e.newestMs, v.newestMs);
    e.roles.set(v.role, (e.roles.get(v.role) ?? 0) + v.games);
    by.set(v.champion, e);
  }
  return [...by.entries()]
    .map(([champion, e]) => ({ champion, games: e.games, newestMs: e.newestMs, roles: [...e.roles.entries()].sort((a, b) => b[1] - a[1]).map((r) => r[0]) }))
    .sort((a, b) => b.newestMs - a.newestMs || b.games - a.games);
}

/** A rough Riot key shape check before sending (the server checks again). */
export function looksLikeKey(s: string): boolean {
  return /^RGAPI-[A-Za-z0-9-]{10,94}$/.test(s.trim());
}

/** Bytes for the disk budget picker, in GB steps. */
export function gb(n: number): number {
  return n * 1024 * 1024 * 1024;
}
