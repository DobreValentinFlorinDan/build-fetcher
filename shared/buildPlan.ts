// The overlay's build plan (core items, boots, then 4th / 5th / 6th) and which entries your inventory already
// covers. Pure functions, shared so they can be tested against real Data Dragon data next to the engine
// (server/engine/buildPlan.test.ts), like shared/rateView.ts.
import type { BuildRecommendation } from './types.ts';

/** What the plan needs to know about an item. /api/meta serves it for every Summoner's Rift item. */
export interface PlanItem {
  gold: number;        // total cost
  from: number[];      // recipe, or the item it upgrades / transforms from (tier-3 boots, Muramana)
  groups: string[];    // unique-passive groups (server/engine/exclusivity.ts): owning one blocks the rest
  boots: boolean;      // finished boots: tier 2, or an upgrade of them (Gunmetal Greaves has no Boots tag)
  finished: boolean;   // a completed item or finished boots, so it can stand in for a planned item
}
export type ItemCatalog = Record<number, PlanItem | undefined>;

export type PlanSlot = 'Core' | 'Boots' | '4th' | '5th' | '6th';

export interface PlanEntry {
  id: number;              // planned item
  slot: PlanSlot;
  alternatives: number[];  // other items the recommendation lists for this slot
}

/**
 * done: you own the planned item.  upgraded: you own an upgrade of it (Armored Advance for Plated Steelcaps).
 * swap: another item fills the slot (other boots, an item of the same unique group, or an alternative the
 * recommendation lists for the slot).  building: you own some of its components.  todo: nothing yet.
 */
export type EntryState = 'done' | 'upgraded' | 'swap' | 'building' | 'todo';

export interface PlanStatus extends PlanEntry {
  state: EntryState;
  owned: number | null;   // the item filling the slot (done, upgraded, swap)
  components: number[];   // owned components counted towards it (building)
  cost: number;           // gold still to pay: the shop price minus the components you own
}

const SITUATIONAL = [['slot4', '4th'], ['slot5', '5th'], ['slot6', '6th']] as const;

/**
 * Ordered plan from a recommendation: the top core build with the top boots where the games usually finished them
 * (`bootsAfter` completed items; after the core when unknown), then for each later slot its best option that is not
 * planned already and not blocked by a planned item's unique group.
 */
export function buildPlan(rec: BuildRecommendation, items: ItemCatalog): PlanEntry[] {
  const core = rec.coreBuild[0]?.value ?? [];
  const plan: PlanEntry[] = core.map((id, k) => ({
    id, slot: 'Core', alternatives: unique(rec.coreBuild.slice(1).map((o) => o.value[k]).filter((x) => x !== undefined && !core.includes(x))),
  }));
  if (rec.boots[0]) {
    const at = Math.max(0, Math.min(core.length, rec.bootsAfter ?? core.length));
    plan.splice(at, 0, { id: rec.boots[0].value, slot: 'Boots', alternatives: rec.boots.slice(1).map((o) => o.value) });
  }

  const groups = (id: number) => items[id]?.groups ?? [];
  for (const [key, slot] of SITUATIONAL) {
    const options = rec.situational[key].map((o) => o.value);
    const planned = plan.map((p) => p.id);
    const pick = options.find((id) => !planned.includes(id) && !planned.some((p) => groups(p).some((g) => groups(id).includes(g))));
    if (pick !== undefined) plan.push({ id: pick, slot, alternatives: options.filter((id) => id !== pick && !planned.includes(id)) });
  }
  return plan;
}

/**
 * Matches your inventory (Live Client item ids, any order, duplicates allowed) against the plan. Each owned
 * item fills at most one entry: exact matches first, then upgrades, then swaps (an item of the same unique
 * group before a listed alternative), then components. `next` is the first entry not yet filled.
 */
export function planStatus(plan: PlanEntry[], ownedIds: number[], items: ItemCatalog): { entries: PlanStatus[]; next: PlanStatus | null } {
  const pool = ownedIds.filter((id) => id > 0);
  const take = (id: number) => { const i = pool.indexOf(id); if (i >= 0) pool.splice(i, 1); return id; };
  const entries: PlanStatus[] = plan.map((e) => ({ ...e, state: 'todo', owned: null, components: [], cost: items[e.id]?.gold ?? 0 }));
  const open = () => entries.filter((e) => e.state === 'todo');
  const fill = (e: PlanStatus, state: EntryState, owned: number) => { e.state = state; e.owned = take(owned); e.cost = 0; };

  const recipe = (id: number) => ancestors(id, items);
  const finished = (id: number) => items[id]?.finished ?? false;
  const sharesGroup = (a: number, b: number) => (items[a]?.groups ?? []).some((g) => (items[b]?.groups ?? []).includes(g));

  for (const e of open()) if (pool.includes(e.id)) fill(e, 'done', e.id);
  for (const e of open()) {
    const up = pool.find((id) => recipe(id).has(e.id));
    if (up !== undefined) fill(e, 'upgraded', up);
  }
  // A swap must be a finished item, never one of the planned item's own components (Sheen is not Trinity Force).
  const canSwap = (e: PlanStatus, id: number) => finished(id) && !recipe(e.id).has(id);
  for (const e of open()) {
    const hard = pool.find((id) => canSwap(e, id) && (e.slot === 'Boots' ? items[id]?.boots : sharesGroup(id, e.id)));
    if (hard !== undefined) fill(e, 'swap', hard);
  }
  for (const e of open()) {
    const alt = pool.find((id) => canSwap(e, id) && e.alternatives.includes(id));
    if (alt !== undefined) fill(e, 'swap', alt);
  }
  for (const e of open()) {
    e.components = ownedComponents(e.id, pool, items).map(take);
    e.cost = Math.max(0, e.cost - e.components.reduce((s, id) => s + (items[id]?.gold ?? 0), 0));
    if (e.components.length) e.state = 'building';
  }
  return { entries, next: entries.find((e) => e.state === 'building' || e.state === 'todo') ?? null };
}

/** Every item in `id`'s recipe tree (components, and what it was upgraded or transformed from). */
function ancestors(id: number, items: ItemCatalog): Set<number> {
  const seen = new Set<number>();
  const stack = [...(items[id]?.from ?? [])];
  while (stack.length) {
    const x = stack.pop()!;
    if (seen.has(x)) continue;
    seen.add(x);
    stack.push(...(items[x]?.from ?? []));
  }
  return seen;
}

/**
 * Owned items the shop would count towards `id`, as it prices a purchase: walking the recipe from the top,
 * an owned component covers its whole subtree; otherwise look further down.
 */
function ownedComponents(id: number, pool: number[], items: ItemCatalog): number[] {
  const left = [...pool];
  const used: number[] = [];
  const walk = (x: number) => {
    for (const c of items[x]?.from ?? []) {
      const i = left.indexOf(c);
      if (i >= 0) { used.push(c); left.splice(i, 1); } else walk(c);
    }
  };
  walk(id);
  return used;
}

function unique(ids: number[]) { return [...new Set(ids)]; }
