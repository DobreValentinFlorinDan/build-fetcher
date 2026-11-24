import { REMAKE_MAX_SECONDS } from '../capture.ts';
import type { IreliaGameRow } from '../db.ts';
import type { ItemMeta, StaticData } from '../ddragon.ts';
import { isBoots, isPurchasableItem, REMOVED_ITEMS } from './exclusivity.ts';

export const STAT_SHARDS = new Set([5001, 5005, 5007, 5008, 5010, 5011, 5013]);

/** A completed ("legendary") item: purchasable end-of-tree item worth building around. */
export function isCompletedItem(item: ItemMeta | undefined): boolean {
  return isPurchasableItem(item) && !isBoots(item) && item.into.length === 0 && item.goldTotal >= 2000
    && !item.tags.includes('Consumable') && !item.tags.includes('Trinket');
}

export function isTier2Boots(item: ItemMeta | undefined): boolean {
  return isPurchasableItem(item) && isBoots(item) && item.depth >= 2;
}

export function isStartingItem(item: ItemMeta | undefined): boolean {
  return isPurchasableItem(item) && !item.tags.includes('Trinket') && item.goldTotal > 0;
}

export interface ParsedGame {
  row: IreliaGameRow;
  weight: number;
  win: boolean;
  perks: number[];
  statPerks: number[];
  purchases: [number, number][];
  completed: number[];   // completed items in first-purchase order
  boots: number | null;
  start: number[];
}

export type Verdict = { ok: true; game: ParsedGame } | { ok: false; reason: string; stale: boolean };

/**
 * Patch-aware validation: a stored game only feeds recommendations if it was played on an admitted
 * patch AND every rune / build-relevant item it used still exists in the current patch's data.
 * Remakes and games where a player left are never learned from (build sourcing v2, A4).
 * `patchWeight` gives the game's patch weight on its own platform (patches differ between regions).
 */
export function validateGame(row: IreliaGameRow, data: StaticData, patchWeight: (row: IreliaGameRow) => number | undefined, startWindowMs: number): Verdict {
  const weight = patchWeight(row);
  if (!weight) return { ok: false, reason: `stale patch ${row.patch}`, stale: true };
  if (row.remake === 1 || row.game_duration < REMAKE_MAX_SECONDS) return { ok: false, reason: 'remake', stale: false };
  if (row.leaver === 1) return { ok: false, reason: 'a player left the game', stale: false };
  // Riot ended it early (e.g. Abort_AntiCheatExit): not a real game. Older matches without the field pass.
  if (row.end_result && row.end_result !== 'GameComplete') return { ok: false, reason: 'aborted game', stale: false };

  const perks = JSON.parse(row.perks) as number[];
  const statPerks = JSON.parse(row.stat_perks) as number[];
  if (perks.length !== 6 || !perks.every((p) => data.runeIds.has(p)) || !data.runeIds.has(row.primary_style) || !data.runeIds.has(row.sub_style)) {
    return { ok: false, reason: 'rune no longer exists', stale: false };
  }
  if (!statPerks.every((s) => STAT_SHARDS.has(s))) return { ok: false, reason: 'stat shard no longer exists', stale: false };

  const purchases = JSON.parse(row.purchases) as [number, number][];
  for (const [id] of purchases) {
    const item = data.items.get(id);
    if (REMOVED_ITEMS.has(id)) return { ok: false, reason: `removed item: ${item?.name ?? id}`, stale: false };
    if (!item) return { ok: false, reason: `unknown item #${id}`, stale: false };
    // A completed-tier item that can no longer be bought means the build path is obsolete.
    const { name } = item;
    if (item.into.length === 0 && item.goldTotal >= 2000 && !isPurchasableItem(item)) {
      return { ok: false, reason: `removed item: ${name}`, stale: false };
    }
  }

  const completed: number[] = [];
  let boots: number | null = null;
  const start: number[] = [];
  for (const [id, ts] of purchases) {
    const item = data.items.get(id);
    if (ts < startWindowMs && isStartingItem(item)) start.push(id);
    if (isCompletedItem(item) && !completed.includes(id)) completed.push(id);
    if (boots === null && isTier2Boots(item)) boots = id;
  }

  return {
    ok: true,
    game: { row, weight, win: row.win === 1, perks, statPerks, purchases, completed, boots, start: start.sort((a, b) => a - b) },
  };
}
