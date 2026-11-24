import assert from 'node:assert/strict';
import { before, test } from 'node:test';
import type { IreliaGameRow } from '../db.ts';
import type { StaticData } from '../ddragon.ts';
import { loadTestStaticData } from '../test/staticData.ts';
import { buildExclusivityIndex, isPurchasableItem, type ExclusivityIndex } from './exclusivity.ts';
import { recommend } from './recommend.ts';

let data: StaticData;
let excl: ExclusivityIndex;
before(() => { data = loadTestStaticData(); excl = buildExclusivityIndex(data); });

const settings = {
  minMatchupGames: 15, generalPriorWeight: 0.15, minLaneGames: 30, otherLaneWeight: 0.15,
  previousPatchWeight: 0.35, minCurrentPatchGames: 40, startingItemWindowMs: 60_000,
};

function game(i: number, { items = [3153, 3078, 3053, 6333], boots = 3047, ...over }: Partial<IreliaGameRow> & { items?: number[]; boots?: number } = {}): IreliaGameRow {
  const purchases = [[1055, 1000], [2003, 1000], [boots, 400_000], ...items.map((id, k) => [id, 600_000 + k * 300_000])];
  return {
    match_id: `T_${i}`, puuid: 'p', patch: data.patch, win: i % 3 === 0 ? 0 : 1, position: 'TOP', opponent_champion_id: 122,
    summoner1: 4, summoner2: 12, primary_style: 8000, sub_style: 8400,
    perks: JSON.stringify([8010, 9111, 9104, 8299, 8444, 8242]), stat_perks: JSON.stringify([5005, 5008, 5011]),
    skill_order: 'QEWQQRQEQERQEEWW', purchases: JSON.stringify(purchases), final_items: '[]',
    kills: 5, deaths: 3, assists: 4, game_duration: 1800, created_at: 0, ...over,
  };
}

test('Opportunity is recognised as removed', () => {
  assert.equal(isPurchasableItem(data.items.get(6701)), false);
});

test('Sunfire Aegis and Hollow Radiance are mutually exclusive', () => {
  assert.match(excl.blocker([3068], 6664) ?? '', /Immolate/);
  assert.equal(excl.conflicts([3068, 6664, 3053]).length, 1);
  assert.match(excl.blocker([3053], 3156) ?? '', /Lifeline/);   // Sterak's vs Maw
  assert.match(excl.blocker([3078], 6662) ?? '', /Spellblade/); // Trinity vs Iceborn
  assert.match(excl.blocker([3047], 3111) ?? '', /Boots/);
  assert.equal(excl.blocker([3153, 3078], 3071), null);
});

test('engine builds a matchup recommendation and discards stale + removed-item games', () => {
  const rows = [
    ...Array.from({ length: 30 }, (_, i) => game(i)),
    game(100, { patch: '15.1' }),                        // stale patch
    game(101, { items: [6701, 3078, 3053] }),            // Opportunity -> invalid
  ];
  const rec = recommend(rows, data, excl, 122, settings);
  assert.deepEqual(rec.coreBuild[0].value, [3153, 3078, 3053]);
  assert.equal(rec.boots[0].value, 3047);
  assert.equal(rec.validation.staleDiscarded, 1);
  assert.equal(rec.validation.invalidDiscarded, 1);
  assert.deepEqual(rec.skills?.value.priority, ['Q', 'E', 'W']);
  assert.equal(rec.skills?.value.path.length, 18);
  assert.deepEqual(rec.runes?.value.perks, [8010, 9111, 9104, 8299, 8444, 8242]);
});

test('situational options never conflict with the core build', () => {
  // Core contains Sterak's (Lifeline); Maw (Lifeline) shows up as a frequent 4th item and must be filtered.
  const rows = Array.from({ length: 30 }, (_, i) =>
    game(i, { items: i % 3 === 0 ? [3153, 3078, 6333, 3156] : [3153, 3078, 3053, 6333] }));
  const rec = recommend(rows, data, excl, null, settings);
  assert.ok(rec.situational.slot4.every((o) => o.value !== 3156));
  assert.ok(rec.validation.conflictsRejected.some((c) => c.includes('Lifeline')));
});

// ---- Lanes: top and mid builds are collected and recommended separately ----------------------------

const TOP_CORE = [3153, 3078, 3053];
const MID_CORE = [3153, 6333, 3071];
const top = (i: number, over: Parameters<typeof game>[1] = {}) =>
  game(i, { position: 'TOP', items: [...TOP_CORE, 6333], boots: 3047, summoner1: 4, summoner2: 12, ...over });
const mid = (i: number, over: Parameters<typeof game>[1] = {}) =>
  game(1000 + i, { position: 'MIDDLE', items: [...MID_CORE, 3053], boots: 3111, summoner1: 4, summoner2: 14, opponent_champion_id: 157, ...over });
const many = (n: number, make: (i: number) => IreliaGameRow) => Array.from({ length: n }, (_, i) => make(i));

test('lanes are separated: a top build never contains mid games and vice versa', () => {
  const rows = [...many(40, (i) => top(i)), ...many(40, (i) => mid(i))];
  const t = recommend(rows, data, excl, null, settings, 'TOP');
  assert.equal(t.lane, 'TOP');
  assert.deepEqual(t.coreBuild[0].value, TOP_CORE);
  assert.deepEqual(t.coreBuild.map((o) => o.value), [TOP_CORE], 'no mid core option at all');
  assert.equal(t.boots[0].value, 3047);
  assert.deepEqual(t.summoners?.value, [4, 12]);
  assert.deepEqual([t.sample.laneGames, t.sample.otherLaneGames, t.sample.usedLaneFallback], [40, 0, false]);

  const m = recommend(rows, data, excl, null, settings, 'MIDDLE');
  assert.equal(m.lane, 'MIDDLE');
  assert.deepEqual(m.coreBuild.map((o) => o.value), [MID_CORE]);
  assert.equal(m.boots[0].value, 3111);
  assert.deepEqual(m.summoners?.value, [4, 14]);
  assert.equal(m.sample.totalGames, 80, 'the total still counts every valid stored game');
});

test('no lane mixes every position, exactly as before lanes existed', () => {
  const rows = [...many(40, (i) => top(i)), ...many(20, (i) => mid(i))];
  const legacy = recommend(rows, data, excl, null, settings);
  assert.equal(legacy.lane, null);
  assert.deepEqual([legacy.sample.laneGames, legacy.sample.totalGames, legacy.sample.otherLaneGames], [60, 60, 0]);
  assert.equal(legacy.sample.usedLaneFallback, false);
  assert.equal(legacy.coreBuild.find((o) => o.value.join() === MID_CORE.join())?.games, 20, 'mid games at full weight');
});

test('matchups only count games in the same lane', () => {
  // Irelia vs Yasuo: plenty of mid games, a few top ones. The top matchup must not borrow the mid sample.
  const rows = [...many(40, (i) => top(i)), ...many(5, (i) => top(100 + i, { opponent_champion_id: 157 })), ...many(40, (i) => mid(i))];
  const t = recommend(rows, data, excl, 157, settings, 'TOP');
  assert.equal(t.sample.matchupGames, 5);
  assert.equal(t.sample.usedFallback, true, 'only 5 top Yasuo games: blended with other top games');
  const m = recommend(rows, data, excl, 157, settings, 'MIDDLE');
  assert.equal(m.sample.matchupGames, 40);
  assert.equal(m.sample.usedFallback, false);
});

test('a thin lane blends in the other lane at reduced weight, and says so', () => {
  const rows = [...many(60, (i) => top(i)), ...many(8, (i) => mid(i))];
  const m = recommend(rows, data, excl, null, settings, 'MIDDLE');
  assert.equal(m.sample.usedLaneFallback, true);
  assert.deepEqual([m.sample.laneGames, m.sample.otherLaneGames], [8, 60]);
  const option = (core: number[]) => m.coreBuild.find((o) => o.value.join() === core.join());
  assert.equal(option(MID_CORE)?.games, 8, 'mid games keep full weight');
  assert.deepEqual([option(TOP_CORE)?.games, option(TOP_CORE)?.rawGames], [60 * 0.15, 60], 'top games count 0.15 each');
  assert.equal(m.winRate, 6 / 8, 'win rate comes from the lane itself, never from the prior');

  // Enough mid games: the top prior is dropped entirely.
  const enough = recommend([...many(60, (i) => top(i)), ...many(30, (i) => mid(i))], data, excl, null, settings, 'MIDDLE');
  assert.deepEqual([enough.sample.usedLaneFallback, enough.sample.otherLaneGames], [false, 0]);
  assert.deepEqual(enough.coreBuild.map((o) => o.value), [MID_CORE]);
});

test('an empty lane is built from the other lane alone, without a win rate', () => {
  const m = recommend(many(40, (i) => top(i)), data, excl, 122, settings, 'MIDDLE');
  assert.deepEqual(m.coreBuild[0].value, TOP_CORE);
  assert.deepEqual([m.sample.laneGames, m.sample.matchupGames, m.sample.otherLaneGames], [0, 0, 40]);
  assert.equal(m.winRate, null);
  assert.equal(m.sample.usedFallback, true);
});

test('jungle, bot, support and unknown-position games never feed a lane build', () => {
  const odd = [3153, 3071, 3742];
  const rows = [
    ...many(10, (i) => top(i)),
    ...many(30, (i) => game(500 + i, { position: 'JUNGLE', items: [...odd, 6333] })),
    ...many(30, (i) => game(600 + i, { position: 'UNKNOWN', items: [...odd, 6333] })),
  ];
  const t = recommend(rows, data, excl, null, settings, 'TOP');
  assert.equal(t.sample.usedLaneFallback, true);
  assert.equal(t.sample.otherLaneGames, 0, 'there is no mid data, and jungle/unknown are not a lane');
  assert.ok(t.coreBuild.every((o) => o.value.join() !== odd.join()));
});

test('the validation report covers the games a lane build can use, not every stored game', () => {
  const broken = { perks: '[]' };   // a rune that no longer exists
  const rows = [
    ...many(40, (i) => top(i)), top(40, broken), top(41, { patch: '1.1' }),
    ...many(8, (i) => mid(i)), mid(8, broken),
    ...many(5, (i) => game(500 + i, { position: 'JUNGLE' })), game(505, { position: 'JUNGLE', ...broken }),
  ];
  const report = (r: ReturnType<typeof recommend>) =>
    [r.validation.freshGames, r.validation.staleDiscarded, r.validation.invalidDiscarded, r.validation.invalidReasons['rune no longer exists']];

  const t = recommend(rows, data, excl, null, settings, 'TOP');
  assert.deepEqual(report(t), [40, 1, 1, 1], '40 top games: no mid prior, so only top games are reported');
  assert.equal(t.sample.totalGames, 53, 'the total still counts every valid stored game');
  const m = recommend(rows, data, excl, null, settings, 'MIDDLE');
  assert.equal(m.sample.usedLaneFallback, true);
  assert.deepEqual(report(m), [48, 1, 2, 2], 'only 8 mid games: the top games blending in are reported too, jungle never');
  assert.deepEqual(report(recommend(rows, data, excl, null, settings)), [53, 1, 3, 3], 'no lane reports every stored game');
});

test('previous-patch admission counts the requested lane, not every stored game', () => {
  assert.notEqual(data.previousPatch, data.patch);
  const rows = [
    ...many(50, (i) => top(i)),
    ...many(10, (i) => mid(i)),
    ...many(20, (i) => mid(200 + i, { patch: data.previousPatch })),
  ];
  const t = recommend(rows, data, excl, null, settings, 'TOP');
  assert.deepEqual(t.sample.patches, [data.patch], '50 current-patch top games are enough');
  const m = recommend(rows, data, excl, null, settings, 'MIDDLE');
  assert.deepEqual(m.sample.patches, [data.patch, data.previousPatch], 'only 10 current-patch mid games');
  assert.equal(m.sample.laneGames, 30);
  assert.equal(m.validation.previousPatchGames, 20);
});

test('boots go where the games finish them: the weighted median of completed items bought before the tier-2 boots', () => {
  // The default game buys its boots at 6:40, before the first item (10:00).
  assert.equal(recommend(many(40, (i) => top(i)), data, excl, null, settings, 'TOP').bootsAfter, 0);
  // Boots at 20:00, after the first two items (10:00, 15:00), in most games.
  const late = (i: number) => {
    const g = top(i);
    const purchases = (JSON.parse(g.purchases) as [number, number][]).map(([id, t]) => [id, id === 3047 ? 1_200_000 : t]);
    return { ...g, purchases: JSON.stringify(purchases) };
  };
  const r = recommend([...many(30, late), ...many(10, (i) => top(100 + i))], data, excl, null, settings, 'TOP');
  assert.equal(r.bootsAfter, 2);
});
