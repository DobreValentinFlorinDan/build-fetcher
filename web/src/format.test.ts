import { test } from 'node:test';
import assert from 'node:assert/strict';
import { ago, bytes, dataAge, goldLine, keystoneOf, looksLikeKey, meter, optionParts, pageForKeystone, parseRunePage, pct, pickedRunePage, pts } from './format.ts';
import type { BuildRecommendation, RankedOption } from './types.ts';

test('numbers', () => {
  assert.equal(pct(0.5351), '53.5%');
  assert.equal(pct(null), '–');
  assert.equal(pts(0.021), '+2.1 pts');
  assert.equal(pts(-0.004), '−0.4 pts');
  assert.equal(bytes(2_147_483_648), '2.1 GB');
  assert.equal(bytes(512), '512 B');
  assert.equal(ago(0, 30_000), 'just now');
  assert.equal(ago(0, 3 * 3_600_000), '3 h ago');
  assert.equal(ago(0, 5 * 86_400_000), '5 days ago');
});

test('rune pages and options', () => {
  const page = parseRunePage('8000:8010,9111,9104,8299/8400:8444,8473/5008,5008,5011');
  assert.deepEqual(page, { style: 8000, perks: [8010, 9111, 9104, 8299], subStyle: 8400, subPerks: [8444, 8473], shards: [5008, 5008, 5011] });
  assert.equal(parseRunePage('8000:8010/8400:8444,8473/5008,5008,5011'), null);
  assert.equal(parseRunePage('nonsense'), null);
  assert.deepEqual(optionParts({ kind: 'coreItems', key: '3078>3074>3053' }), { kind: 'items', ids: [3078, 3074, 3053], ordered: true });
  assert.deepEqual(optionParts({ kind: 'startingItems', key: '1055,2003' }), { kind: 'items', ids: [1055, 2003], ordered: false });
  assert.deepEqual(optionParts({ kind: 'summoners', key: '4,14' }), { kind: 'summoners', ids: [4, 14] });
  assert.deepEqual(optionParts({ kind: 'skillMax', key: 'Q>E>W' }), { kind: 'skills', letters: ['Q', 'E', 'W'], ordered: true });
});

test('budget meter: ours and another app', () => {
  const m = meter({ scope: 'EUROPE', limits: '20:1,100:120', used: 60, ours: 45, limit: 100, windowS: 120, shared: true });
  assert.deepEqual(m, { used: 0.6, ours: 0.45, other: 0.6 - 0.45 });
  assert.deepEqual(meter({ scope: 'ASIA', limits: '', used: 150, ours: 200, limit: 100, windowS: 120, shared: false }), { used: 1, ours: 1, other: 0 });
});

test('data age per champion', () => {
  const p = { major: 16, minor: 19 };
  const rows = dataAge([
    { champion: 103, role: 'MIDDLE', patch: p, games: 100, wins: 50, newestMs: 10 },
    { champion: 103, role: 'TOP', patch: p, games: 5, wins: 2, newestMs: 30 },
    { champion: 39, role: 'TOP', patch: p, games: 50, wins: 25, newestMs: 20 },
  ]);
  assert.deepEqual(rows.map((r) => [r.champion, r.games, r.newestMs, r.roles]), [[103, 105, 30, ['MIDDLE', 'TOP']], [39, 50, 20, ['TOP']]]);
});

test('key shape', () => {
  assert.ok(looksLikeKey(' RGAPI-00000000-test '));
  assert.ok(!looksLikeKey('RGAPI-'));
  assert.ok(!looksLikeKey('hello'));
});

test('gold line: buy now, or what is missing and the part to buy now', () => {
  const name = (id: number) => (id === 1026 ? 'Blasting Wand' : `Item ${id}`);
  assert.equal(goldLine({ cost: 3000, short: 0, part: null, partCost: null }, name), 'Buy now (3000)');
  assert.equal(goldLine({ cost: 2900, short: 650, part: 1026, partCost: 850 }, name), '650 short · Blasting Wand now (850)');
  assert.equal(goldLine({ cost: 2900, short: 2800, part: null, partCost: null }, name), '2800 short');
});

test('picking the rune page to import', () => {
  const opt = (kind: 'runePage' | 'keystone', key: string): RankedOption =>
    ({ option: { kind, key }, label: '', games: 100, wins: 50, weightedGames: 100, winRate: 0.5, wpa: 0, predictedWin: null, basis: 'lane', thin: false });
  const conq = '8000:8010,9111,9104,8299/8400:8444,8473/5008,5008,5011';
  const conq2 = '8000:8010,9111,9105,8299/8400:8444,8473/5008,5008,5011';
  const grasp = '8400:8437,8446,8444,8451/8000:9111,9105/5008,5008,5011';
  const rec = {
    champion: 266, role: 'TOP', patch: null, tier: 'b', laneOpponent: null, enemyProfile: null, games: 500, model: null, notes: [],
    sections: [
      { kind: 'keystone', options: [opt('keystone', '8010'), opt('keystone', '8437')] },
      { kind: 'runePage', options: [opt('runePage', conq), opt('runePage', grasp), opt('runePage', conq2)] },
    ],
  } satisfies BuildRecommendation;
  assert.equal(pickedRunePage(rec, null), conq, 'nothing picked: the best page');
  assert.equal(pickedRunePage(rec, grasp), grasp);
  assert.equal(pickedRunePage(rec, '8100:8112,8139,8140,8135/8300:8304,8347/5008,5008,5001'), conq, 'a page no longer listed falls back to the best');
  assert.equal(keystoneOf(grasp), 8437);
  assert.equal(keystoneOf(null), null);
  assert.equal(pageForKeystone(rec, 8437), grasp);
  assert.equal(pageForKeystone(rec, 8010), conq, "the keystone's best page");
  assert.equal(pageForKeystone(rec, 8128), null);
  assert.equal(pickedRunePage({ ...rec, sections: [] }, grasp), null);
});
