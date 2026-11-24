import type { PlanItem } from '../../shared/buildPlan.ts';
import type { ItemMeta, StaticData } from '../ddragon.ts';
import { isBoots, type ExclusivityIndex } from './exclusivity.ts';
import { isCompletedItem } from './validator.ts';

export interface MetaItem extends PlanItem { name: string }

/** Recipe, or the item it transforms from when Data Dragon only gives `specialRecipe` (Manamune -> Muramana). */
const recipeOf = (it: ItemMeta) => (it.from.length ? it.from : it.specialRecipe ? [it.specialRecipe] : []);

/**
 * Every Summoner's Rift item the Live Client can report in an inventory, with what the overlay's build plan
 * (shared/buildPlan.ts) needs. Unlike the engine, this keeps items you can own but not buy (Muramana), so
 * an upgraded or transformed item can still be matched to the planned one.
 */
export function itemCatalog(data: Pick<StaticData, 'items'>, excl: ExclusivityIndex): Record<number, MetaItem> {
  const kinds = new Map<number, { boots: boolean; finished: boolean }>();
  const kind = (id: number, visiting = new Set<number>()): { boots: boolean; finished: boolean } => {
    const known = kinds.get(id);
    if (known) return known;
    const it = data.items.get(id);
    if (!it || visiting.has(id)) return { boots: false, finished: false };
    visiting.add(id);
    const from = recipeOf(it).map((c) => kind(c, visiting));
    // Tier-3 boots carry the Boots tag except Gunmetal Greaves, so boots are also recognised by what they upgrade.
    const boots = (isBoots(it) && it.depth >= 2) || from.some((c) => c.boots);
    const k = { boots, finished: boots || isCompletedItem(it) || from.some((c) => c.finished) };
    kinds.set(id, k);
    return k;
  };

  const out: Record<number, MetaItem> = {};
  for (const it of data.items.values()) {
    if (!it.onSummonersRift || it.id >= 100_000) continue;
    out[it.id] = { name: it.name, gold: it.goldTotal, groups: excl.groupsOf(it.id), from: recipeOf(it), ...kind(it.id) };
  }
  return out;
}
