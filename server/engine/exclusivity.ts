import type { ItemMeta, StaticData } from '../ddragon.ts';

/**
 * Unique-passive groups the shop enforces: you can never own two items of the same group.
 * Most are derived from Data Dragon's <passive> tags (so new items join automatically);
 * a few groups don't share a passive name and are listed explicitly.
 */
const EXCLUSIVE_PASSIVES = new Set([
  'Immolate',          // Sunfire Aegis, Hollow Radiance, Bami's Cinder
  'Lifeline',          // Sterak's Gage, Maw of Malmortius, Immortal Shieldbow, Hexdrinker, Protoplasm Harness
  'Spellblade',        // Sheen, Trinity Force, Lich Bane, Iceborn Gauntlet, Essence Reaver, Bloodsong, Dusk and Dawn
  'Cleave',            // Tiamat, Ravenous / Titanic / Profane Hydra, Stridebreaker
  'Manaflow',          // Tear line
  'Annul',             // Banshee's Veil, Edge of Night, Verdant Barrier
  'Glory',             // Dark Seal, Mejai's
  'Eternity',          // Catalyst of Aeons, Rod of Ages
  'Jungle Companions',
]);

const STATIC_GROUPS: Record<string, number[]> = {
  'Last Whisper': [3035, 3036, 3033, 6694],      // Last Whisper, Lord Dominik's, Mortal Reminder, Serylda's Grudge
  Quicksilver: [3140, 3139],                     // Quicksilver Sash, Mercurial Scimitar
  'Support Quest': [3865, 3866, 3867, 3869, 3870, 3871, 3876, 3877],
};

/**
 * Items removed from the game. Data Dragon sometimes still ships removed items (Opportunity is
 * present but flagged purchasable=false), so we never rely on mere presence in item.json.
 */
export const REMOVED_ITEMS = new Set<number>([
  6701,  // Opportunity
]);

export function isPurchasableItem(item: ItemMeta | undefined): item is ItemMeta {
  return !!item && item.onSummonersRift && item.purchasable && item.inStore && !REMOVED_ITEMS.has(item.id) && item.id < 100_000;
}

export function isBoots(item: ItemMeta): boolean {
  return item.tags.includes('Boots');
}

export interface ExclusivityIndex {
  groupsOf(itemId: number): string[];
  conflicts(itemIds: number[]): { group: string; items: number[] }[];
  /** Group name that blocks `candidate` given `owned`, or null if it can be added. */
  blocker(owned: number[], candidate: number): string | null;
  describe(conflict: { group: string; items: number[] }): string;
}

export function buildExclusivityIndex(data: Pick<StaticData, 'items'>): ExclusivityIndex {
  const groups = new Map<number, Set<string>>();
  const add = (id: number, g: string) => {
    if (!groups.has(id)) groups.set(id, new Set());
    groups.get(id)!.add(g);
  };

  for (const item of data.items.values()) {
    if (!item.onSummonersRift || item.id >= 100_000) continue;
    for (const p of item.passives) if (EXCLUSIVE_PASSIVES.has(p)) add(item.id, p);
    if (isBoots(item)) add(item.id, 'Boots');
  }
  for (const [g, ids] of Object.entries(STATIC_GROUPS)) for (const id of ids) add(id, g);

  const groupsOf = (id: number) => [...(groups.get(id) ?? [])];
  const name = (id: number) => data.items.get(id)?.name ?? `#${id}`;

  return {
    groupsOf,
    conflicts(itemIds) {
      const byGroup = new Map<string, number[]>();
      for (const id of itemIds) for (const g of groupsOf(id)) byGroup.set(g, [...(byGroup.get(g) ?? []), id]);
      return [...byGroup].filter(([, ids]) => ids.length > 1).map(([group, items]) => ({ group, items }));
    },
    blocker(owned, candidate) {
      const candGroups = groupsOf(candidate);
      for (const id of owned) {
        if (id === candidate) return 'Duplicate';
        for (const g of groupsOf(id)) if (candGroups.includes(g)) return `${g} (${name(id)})`;
      }
      return null;
    },
    describe: ({ group, items }) => `${items.map(name).join(' + ')} (${group})`,
  };
}
