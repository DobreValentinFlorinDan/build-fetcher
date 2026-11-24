// Late-game swaps (0.17.0, docs/SWAPS.md): at Irelia's shop visits from 20:00 with every slot full, what she did with
// each item she held: kept it, sold it for the next legendary she finished, or sold it for nothing. From a stored game's
// item events (capture.ts TimelineCapture.items); the draft model scores each against the win chance at the visit.
import type { TimelineCapture } from '../capture.ts';
import { ITEM_EVENT } from '../capture.ts';
import type { ItemMeta } from '../ddragon.ts';
import { isCompletedItem, isTier2Boots } from '../engine/validator.ts';

export const SWAP_DEFAULTS = {
  /** Shop visits from this minute on count. */
  fromMinute: 20,
  /** Item events at most this far apart are one visit. */
  visitGapMs: 30_000,
  /** A sold item's replacement: the next legendary finished within this long (boots go for components first, often). */
  replaceWithinMs: 10 * 60_000,
  /** Every slot full: this many legendaries and tier-2 boots held. */
  fullSlots: 6,
};

/** Kept the item; sold it with no legendary finished in time. Any other option is the item it was sold for. */
export const KEEP = 0;
export const SELL = -1;

/**
 * One decision: at the visit starting at `t` (ms), the item `held`, and what became of it (`option`: KEEP, SELL or the
 * item it was swapped for). `w`: the game's decision points share one game's weight (1 / its points).
 */
export interface SwapDecision { t: number; held: number; option: number; w: number }

/** A legendary or tier-2 boots: what a swap sells or keeps. */
export const swappable = (item: ItemMeta | undefined) => isCompletedItem(item) || isTier2Boots(item);

/**
 * `pid`'s swap decisions in `cap`, in time order. The inventory is replayed as capture.ts inventoryAt does (buys add,
 * sells and destroys remove, an undo reverts what it names), visit by visit.
 */
export function swapDecisions(cap: TimelineCapture, pid: number, itemOf: (id: number) => ItemMeta | undefined,
  o: typeof SWAP_DEFAULTS = SWAP_DEFAULTS): SwapDecision[] {
  const events = cap.items.filter((e) => e[1] === pid).sort((a, b) => a[0] - b[0]);
  const inv: number[] = [];
  const remove = (id: number) => { const i = inv.lastIndexOf(id); if (i >= 0) inv.splice(i, 1); };
  const apply = (e: number[]) => {
    const [, , kind, item, after] = e;
    if (kind === ITEM_EVENT.BUY) inv.push(item);
    else if (kind === ITEM_EVENT.SELL || kind === ITEM_EVENT.DESTROY) remove(item);
    else if (kind === ITEM_EVENT.UNDO) { if (item) remove(item); if (after) inv.push(after); }
  };
  // Legendaries finished, in time order: the replacements a sold item may have.
  const finished = events.filter((e) => e[2] === ITEM_EVENT.BUY && isCompletedItem(itemOf(e[3])));
  const points: { t: number; held: number[]; options: number[] }[] = [];
  const claimed = new Set<number[]>();
  for (let i = 0; i < events.length;) {
    // One visit: events at most visitGapMs apart.
    let j = i + 1;
    while (j < events.length && events[j][0] - events[j - 1][0] <= o.visitGapMs) j++;
    const visit = events.slice(i, j);
    const start = visit[0][0];
    const held = inv.filter((id) => swappable(itemOf(id)));
    for (const e of visit) apply(e);
    if (start >= o.fromMinute * 60_000 && held.length >= o.fullSlots) {
      // What is no longer there after the visit: sold (a sell taken back is still there).
      const after = [...inv];
      const gone: number[] = [];
      for (const id of held) {
        const k = after.indexOf(id);
        if (k >= 0) after.splice(k, 1); else gone.push(id);
      }
      const options = held.map((id) => {
        const g = gone.indexOf(id);
        if (g < 0) return KEEP;
        gone.splice(g, 1);
        // Sold: the next legendary finished from the visit on, within replaceWithinMs, not held before, not given to
        // another item sold.
        const sellAt = [...visit].reverse().find((e) => e[2] === ITEM_EVENT.SELL && e[3] === id)?.[0] ?? start;
        const next = finished.find((b) => !claimed.has(b) && b[0] >= start && b[0] - sellAt <= o.replaceWithinMs && b[3] !== id && !held.includes(b[3]));
        if (!next) return SELL;
        claimed.add(next);
        return next[3];
      });
      points.push({ t: start, held, options });
    }
    i = j;
  }
  const w = points.length ? 1 / points.length : 0;
  return points.flatMap((p) => p.held.map((held, k) => ({ t: p.t, held, option: p.options[k], w })));
}
