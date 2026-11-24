// Stage C1 (lcu/importer.ts) against a fake League client: the rune page and item set it writes, and that it only
// ever replaces its own.
import assert from 'node:assert/strict';
import { test } from 'node:test';
import type { BuildRecommendation } from '../../shared/types.ts';
import { importBuild, itemSetBody, PAGE_NAME, runePageBody, type LcuApi } from './importer.ts';

const opt = <T>(value: T) => ({ value, games: 50, rawGames: 50, winRate: 0.55, score: 0.5 });
const build = {
  runes: opt({ primaryStyle: 8000, subStyle: 8400, perks: [8010, 9111, 9104, 8299, 8444, 8242], statPerks: [5008, 5008, 5001] }),
  startingItems: [opt([1055, 2003, 2003])],
  coreBuild: [opt([3153, 3078, 6333])],
  boots: [opt(3047), opt(3111)],
  situational: { slot4: [opt(3053), opt(6609), opt(3026), opt(3065)], slot5: [opt(3026)], slot6: [] },
} as unknown as BuildRecommendation;

/** An in-memory League client: rune pages, the page inventory and item sets, with a log of every write. */
function fakeClient(o: { pages?: { id: number; name: string; isEditable?: boolean }[]; ownedPageCount?: number; sets?: { uid: string; title: string }[] } = {}) {
  let nextId = 900;
  const state = {
    pages: [...(o.pages ?? [])] as { id: number; name: string; isEditable?: boolean; selectedPerkIds?: number[] }[],
    sets: { accountId: 7, itemSets: [...(o.sets ?? [])] as { uid: string; title: string; blocks?: unknown }[], timestamp: 1 },
    writes: [] as string[],
  };
  const api: LcuApi = {
    async send<T>(method: string, path: string, body?: unknown) {
      if (method !== 'GET') state.writes.push(`${method} ${path}`);
      if (method === 'GET' && path === '/lol-perks/v1/pages') return state.pages as T;
      if (method === 'GET' && path === '/lol-perks/v1/inventory') return { ownedPageCount: o.ownedPageCount ?? 5 } as T;
      if (method === 'DELETE' && path.startsWith('/lol-perks/v1/pages/')) {
        state.pages = state.pages.filter((p) => p.id !== Number(path.split('/').pop()));
        return null;
      }
      if (method === 'POST' && path === '/lol-perks/v1/pages') {
        const page = { id: nextId++, ...(body as { name: string; selectedPerkIds: number[] }), isEditable: true };
        state.pages.push(page);
        return page as T;
      }
      if (method === 'GET' && path === '/lol-summoner/v1/current-summoner') return { summonerId: 42 } as T;
      if (method === 'GET' && path === '/lol-item-sets/v1/item-sets/42/sets') return structuredClone(state.sets) as T;
      if (method === 'PUT' && path === '/lol-item-sets/v1/item-sets/42/sets') { state.sets = body as typeof state.sets; return null; }
      throw new Error(`HTTP 404 ${method} ${path}`);
    },
  };
  return { api, state };
}

test('the rune page and item set say what the build says, in the League client\'s terms', () => {
  assert.deepEqual(runePageBody(build)!.selectedPerkIds, [8010, 9111, 9104, 8299, 8444, 8242, 5008, 5008, 5001]);
  const set = itemSetBody(build, 'Irelia Companion · Top vs Darius', 'u1') as unknown as { blocks: { type: string; items: { id: string; count: number }[] }[]; associatedChampions: number[] };
  assert.deepEqual(set.associatedChampions, [39]);
  assert.deepEqual(set.blocks.map((b) => b.type), ['Start', 'Core and boots, in order', '4th item (best first)', '5th item (best first)', 'Vision and potions']);
  assert.deepEqual(set.blocks[0].items, [{ id: '1055', count: 1 }, { id: '2003', count: 2 }]);
  assert.deepEqual(set.blocks[1].items.map((i) => i.id), ['3153', '3078', '6333', '3047']);
  assert.equal(set.blocks[2].items.length, 3, 'the three best 4th items');
  const early = itemSetBody({ ...build, bootsAfter: 1 } as BuildRecommendation, 't', 'u2') as unknown as typeof set;
  assert.deepEqual(early.blocks[1].items.map((i) => i.id), ['3153', '3047', '3078', '6333'], 'boots where the games buy them');
});

test('the item set\'s "vs" blocks: what did best against the opponent, each after its own part of the build', () => {
  const best = { start: { value: [1036, 2003], games: 24, lift: 0.04 }, first: [{ value: 3053, games: 31, lift: 0.05 }, { value: 3153, games: 40, lift: 0.02 }] };
  const set = itemSetBody(build, 't', 'u3', { name: 'Darius', best }) as unknown as { blocks: { type: string; items: { id: string; count: number }[] }[] };
  assert.deepEqual(set.blocks.map((b) => b.type), [
    'Start', 'vs Darius: best start (24 games)', 'Core and boots, in order', 'vs Darius: best first item (31 · 40 games)',
    '4th item (best first)', '5th item (best first)', 'Vision and potions',
  ]);
  assert.deepEqual(set.blocks[1].items, [{ id: '1036', count: 1 }, { id: '2003', count: 1 }]);
  assert.deepEqual(set.blocks[3].items.map((i) => i.id), ['3053', '3153']);
  const onlyItems = itemSetBody(build, 't', 'u4', { name: 'Darius', best: { start: null, first: best.first.slice(0, 1) } }) as unknown as typeof set;
  assert.deepEqual(onlyItems.blocks.map((b) => b.type).filter((t) => t.startsWith('vs')), ['vs Darius: best first item (31 games)']);
});

test('first import: a new current page and a new set; your pages and sets are kept as they are', async () => {
  const { api, state } = fakeClient({ pages: [{ id: 1, name: 'My Irelia' }, { id: 2, name: 'Default', isEditable: false }], sets: [{ uid: 'mine', title: 'My set' }] });
  const r = await importBuild(api, build, { lane: 'TOP', opponentName: 'Darius', owned: {} });
  assert.ok(r.runePage.ok && !r.runePage.replaced);
  assert.ok(r.itemSet.ok && r.itemSet.title === 'Irelia Companion · Top vs Darius');
  assert.deepEqual(state.pages.map((p) => p.name), ['My Irelia', 'Default', PAGE_NAME]);
  assert.deepEqual(state.sets.itemSets.map((s) => s.title), ['My set', 'Irelia Companion · Top vs Darius']);
  assert.equal(state.sets.accountId, 7, 'the rest of the list goes back unchanged');
  assert.deepEqual(state.writes, ['POST /lol-perks/v1/pages', 'PUT /lol-item-sets/v1/item-sets/42/sets']);
  assert.equal(r.owned.runePageId, state.pages[2].id);
});

test('importing again replaces only the app\'s own page and set', async () => {
  const { api, state } = fakeClient({
    pages: [{ id: 1, name: 'My Irelia' }, { id: 77, name: PAGE_NAME }],
    sets: [{ uid: 'mine', title: 'My set' }, { uid: 'old', title: 'Irelia Companion · Mid vs Syndra' }],
  });
  const r = await importBuild(api, build, { lane: 'TOP', opponentName: null, owned: { runePageId: 77, itemSetUid: 'old' } });
  assert.ok(r.runePage.ok && r.runePage.replaced);
  assert.ok(r.itemSet.ok && r.itemSet.replaced);
  assert.deepEqual(state.pages.map((p) => p.name), ['My Irelia', PAGE_NAME]);
  assert.notEqual(state.pages[1].id, 77);
  assert.deepEqual(state.sets.itemSets.map((s) => s.title), ['My set', 'Irelia Companion · Top']);
  assert.deepEqual(state.writes, ['DELETE /lol-perks/v1/pages/77', 'POST /lol-perks/v1/pages', 'PUT /lol-item-sets/v1/item-sets/42/sets']);
});

test('no free rune page: nothing of yours is deleted, the item set is still saved, and the reason is given', async () => {
  const { api, state } = fakeClient({ pages: [{ id: 1, name: 'A' }, { id: 2, name: 'B' }], ownedPageCount: 2 });
  const r = await importBuild(api, build, { lane: 'MIDDLE', opponentName: 'Syndra', owned: {} });
  assert.equal(r.runePage.ok, false);
  assert.match(!r.runePage.ok ? r.runePage.error : '', /No free rune page \(2 of 2 in use\)/);
  assert.ok(r.itemSet.ok);
  assert.deepEqual(state.pages.map((p) => p.name), ['A', 'B']);
  assert.ok(!state.writes.some((w) => w.startsWith('DELETE')));
});

test('a client error on one part leaves the other part done', async () => {
  const { api } = fakeClient();
  const failing: LcuApi = { send: (m, p, b) => (p.includes('item-sets') ? Promise.reject(new Error('HTTP 500 PUT')) : api.send(m, p, b)) };
  const r = await importBuild(failing, build, { lane: 'TOP', opponentName: null, owned: { itemSetUid: 'keep' } });
  assert.ok(r.runePage.ok);
  assert.equal(r.itemSet.ok, false);
  assert.equal(r.owned.itemSetUid, 'keep', 'what was saved before is remembered');
});
