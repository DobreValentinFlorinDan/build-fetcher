// Stage C1: one-click import of the build shown into the League client: a rune page and an item set (it shows in the
// in-game shop). docs/BUILD-SOURCING.md §10.
//
// Rules (docs/DESKTOP.md §2): it runs only when you click, only during champ select, never in game (index.ts checks
// the phase before calling this). It replaces only what it created itself: the rune page whose id it saved (or, if
// that was lost, the page named exactly PAGE_NAME) and the item set with its saved uid (or titled with SET_PREFIX). It
// never deletes or edits your own pages and sets: with no free rune page it says so and leaves them alone.
//
// The League client's endpoints are unofficial: /lol-perks/v1/{pages,inventory} and
// /lol-item-sets/v1/item-sets/{summonerId}/sets (a PUT replaces the whole list, so every other set is sent back as is).
import { randomUUID } from 'node:crypto';
import type { BuildRecommendation, Lane, MatchupBest } from '../../shared/types.ts';

export const PAGE_NAME = 'Irelia Companion';
export const SET_PREFIX = 'Irelia Companion';
const IRELIA = 39;
const SUMMONERS_RIFT = 11;

/** The League client, as far as the import needs it (LcuConnector.send, or a fake in tests). */
export interface LcuApi {
  send<T>(method: 'GET' | 'POST' | 'PUT' | 'DELETE', path: string, body?: unknown): Promise<T | null>;
}

interface PerkPage { id: number; name: string; isEditable?: boolean; isDeletable?: boolean; isActive?: boolean }
interface ItemSet { uid: string; title: string; [key: string]: unknown }
interface ItemSets { accountId?: number; itemSets: ItemSet[]; timestamp?: number }

export interface ImportResult {
  runePage: { ok: true; id: number; replaced: boolean } | { ok: false; error: string };
  itemSet: { ok: true; uid: string; title: string; replaced: boolean } | { ok: false; error: string };
  /** What to remember for next time (saved in config.json). */
  owned: { runePageId?: number; itemSetUid?: string };
}

const message = (err: unknown) => (err instanceof Error ? err.message : String(err));

/** The rune page in the League client's terms: 4 primary runes, 2 secondary, then the offense, flex and defense shards. */
export function runePageBody(build: BuildRecommendation) {
  const r = build.runes?.value;
  if (!r || r.perks.length !== 6 || r.statPerks.length !== 3) return null;
  return { name: PAGE_NAME, primaryStyleId: r.primaryStyle, subStyleId: r.subStyle, selectedPerkIds: [...r.perks, ...r.statPerks], current: true };
}

/** What did best against the lane opponent (model/facts.ts matchupBest), for the "vs" blocks. */
export interface VersusBlocks { name: string; best: MatchupBest }

/**
 * The item set: start, core and boots in order, then the 4th / 5th / 6th item options (each block in the shop). With
 * `vs` (0.9.0), the start and first items that did best against the lane opponent, each after its own part of the
 * build, titled with the opponent and how many games they rest on.
 */
export function itemSetBody(build: BuildRecommendation, title: string, uid: string, vs: VersusBlocks | null = null): ItemSet {
  const block = (type: string, ids: number[]) => {
    const counts = new Map<number, number>();
    for (const id of ids) counts.set(id, (counts.get(id) ?? 0) + 1);
    return { type, items: [...counts].map(([id, count]) => ({ id: String(id), count })) };
  };
  const top = (opts: { value: number }[], n: number) => opts.slice(0, n).map((o) => o.value);
  const start = build.startingItems[0]?.value ?? [];
  // Boots where the games usually finished them (after `bootsAfter` core items).
  const coreItems = build.coreBuild[0]?.value ?? [];
  const at = Math.max(0, Math.min(coreItems.length, build.bootsAfter ?? coreItems.length));
  const core = [...coreItems.slice(0, at), ...top(build.boots, 1), ...coreItems.slice(at)];
  const vsStart = vs?.best.start ?? null;
  const vsFirst = vs?.best.first ?? [];
  const blocks = [
    start.length ? block('Start', start) : null,
    vsStart ? block(`vs ${vs!.name}: best start (${vsStart.games} games)`, vsStart.value) : null,
    core.length ? block('Core and boots, in order', core) : null,
    vsFirst.length ? block(`vs ${vs!.name}: best first item (${vsFirst.map((o) => o.games).join(' · ')} games)`, vsFirst.map((o) => o.value)) : null,
    build.situational.slot4.length ? block('4th item (best first)', top(build.situational.slot4, 3)) : null,
    build.situational.slot5.length ? block('5th item (best first)', top(build.situational.slot5, 3)) : null,
    build.situational.slot6.length ? block('6th item (best first)', top(build.situational.slot6, 3)) : null,
    block('Vision and potions', [2055, 2003]),
  ].filter((b): b is NonNullable<typeof b> => b !== null);
  return {
    uid, title, type: 'custom', map: 'any', mode: 'any', startedFrom: 'blank', sortrank: 0,
    associatedChampions: [IRELIA], associatedMaps: [SUMMONERS_RIFT], preferredItemSlots: [], blocks,
  };
}

async function importRunes(api: LcuApi, build: BuildRecommendation, savedId: number | undefined): Promise<ImportResult['runePage']> {
  const body = runePageBody(build);
  if (!body) return { ok: false, error: 'This build has no complete rune page yet.' };
  const pages = (await api.send<PerkPage[]>('GET', '/lol-perks/v1/pages')) ?? [];
  const editable = pages.filter((p) => p.isEditable !== false && p.isDeletable !== false);
  const ours = editable.find((p) => p.id === savedId && p.name === PAGE_NAME) ?? editable.find((p) => p.name === PAGE_NAME);
  if (ours) await api.send('DELETE', `/lol-perks/v1/pages/${ours.id}`);
  else {
    // A new page needs a free slot: your own pages are never touched.
    const inventory = await api.send<{ ownedPageCount?: number }>('GET', '/lol-perks/v1/inventory');
    const owned = inventory?.ownedPageCount;
    if (owned !== undefined && editable.length >= owned) {
      return { ok: false, error: `No free rune page (${editable.length} of ${owned} in use). Delete one in the client, or rename one to "${PAGE_NAME}" to let the app use it.` };
    }
  }
  const created = await api.send<PerkPage>('POST', '/lol-perks/v1/pages', body);
  if (!created?.id) return { ok: false, error: 'The League client did not create the rune page.' };
  return { ok: true, id: created.id, replaced: !!ours };
}

async function importItems(api: LcuApi, build: BuildRecommendation, title: string, savedUid: string | undefined, vs: VersusBlocks | null): Promise<ImportResult['itemSet']> {
  const me = await api.send<{ summonerId?: number }>('GET', '/lol-summoner/v1/current-summoner');
  if (!me?.summonerId) return { ok: false, error: 'The League client did not say who is signed in.' };
  const path = `/lol-item-sets/v1/item-sets/${me.summonerId}/sets`;
  const current = (await api.send<ItemSets>('GET', path)) ?? { itemSets: [] };
  const sets = current.itemSets ?? [];
  const isOurs = (s: ItemSet) => (savedUid !== undefined && s.uid === savedUid) || (typeof s.title === 'string' && s.title.startsWith(SET_PREFIX));
  const replaced = sets.some(isOurs);
  const set = itemSetBody(build, title, randomUUID(), vs);
  await api.send('PUT', path, { ...current, itemSets: [...sets.filter((s) => !isOurs(s)), set], timestamp: Date.now() });
  return { ok: true, uid: set.uid, title, replaced };
}

/**
 * Saves `build` in the League client as the "Irelia Companion" rune page (made current) and item set. Each part
 * succeeds or fails on its own, with the reason.
 */
export async function importBuild(api: LcuApi, build: BuildRecommendation, o: {
  lane: Lane; opponentName: string | null; owned: { runePageId?: number; itemSetUid?: string };
  /** What did best against the lane opponent (0.9.0): the item set's "vs" blocks. */
  best?: MatchupBest | null;
}): Promise<ImportResult> {
  const title = `${SET_PREFIX} · ${o.lane === 'MIDDLE' ? 'Mid' : 'Top'}${o.opponentName ? ` vs ${o.opponentName}` : ''}`;
  const runePage = await importRunes(api, build, o.owned.runePageId).catch((err): ImportResult['runePage'] => ({ ok: false, error: message(err) }));
  const vs = o.best && o.opponentName ? { name: o.opponentName, best: o.best } : null;
  const itemSet = await importItems(api, build, title, o.owned.itemSetUid, vs).catch((err): ImportResult['itemSet'] => ({ ok: false, error: message(err) }));
  return {
    runePage, itemSet,
    owned: {
      runePageId: runePage.ok ? runePage.id : o.owned.runePageId,
      itemSetUid: itemSet.ok ? itemSet.uid : o.owned.itemSetUid,
    },
  };
}
