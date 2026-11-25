import { test } from 'node:test';
import assert from 'node:assert/strict';
import { ago, bytes, dataAge, looksLikeKey, meter, optionParts, parseRunePage, pct, pts } from './format.ts';

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
