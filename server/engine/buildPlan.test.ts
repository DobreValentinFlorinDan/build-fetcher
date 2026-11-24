// The overlay's build plan and item ownership (shared/buildPlan.ts), checked against the current patch's
// Data Dragon items and fed the way the overlay is: a Live Client payload through analyseLiveGame.
import assert from 'node:assert/strict';
import { before, test } from 'node:test';
import { buildPlan, planStatus, type PlanEntry } from '../../shared/buildPlan.ts';
import type { BuildRecommendation, Option } from '../../shared/types.ts';
import type { IreliaGameRow } from '../db.ts';
import type { StaticData } from '../ddragon.ts';
import { loadTestStaticData } from '../test/staticData.ts';
import { analyseLiveGame, type AllGameData } from './adaptive.ts';
import { buildExclusivityIndex, type ExclusivityIndex } from './exclusivity.ts';
import { itemCatalog, type MetaItem } from './itemCatalog.ts';
import { recommend } from './recommend.ts';

let data: StaticData;
let excl: ExclusivityIndex;
let items: Record<number, MetaItem>;
before(() => { data = loadTestStaticData(); excl = buildExclusivityIndex(data); items = itemCatalog(data, excl); });

const BOTRK = 3153, TRINITY = 3078, STERAKS = 3053, MAW = 3156, DEATHS_DANCE = 6333, BLACK_CLEAVER = 3071, WITS_END = 3091;
const STEELCAPS = 3047, MERCURYS = 3111, BERSERKERS = 3006, ARMORED_ADVANCE = 3174, GUNMETAL = 3172, BOOTS = 1001;
const SHEEN = 3057, LONG_SWORD = 1036, TUNNELER = 2021, TITANIC = 3748, MANAMUNE = 3004, MURAMANA = 3042, STEALTH_WARD = 3340;
const SPIRIT_VISAGE = 3065;

const opt = <T>(value: T): Option<T> => ({ value, games: 10, rawGames: 10, winRate: 0.6, score: 0.5 });
/** Just the parts of a recommendation the plan reads. */
function rec(o: { core: number[][]; boots: number[]; slot4?: number[]; slot5?: number[]; slot6?: number[] }) {
  return {
    coreBuild: o.core.map(opt), boots: o.boots.map(opt),
    situational: { slot4: (o.slot4 ?? []).map(opt), slot5: (o.slot5 ?? []).map(opt), slot6: (o.slot6 ?? []).map(opt) },
  } as unknown as BuildRecommendation;
}
const IRELIA_TOP = rec({
  core: [[BOTRK, TRINITY, STERAKS], [BOTRK, TRINITY, DEATHS_DANCE]], boots: [STEELCAPS, MERCURYS],
  slot4: [DEATHS_DANCE, BLACK_CLEAVER, MAW], slot5: [BLACK_CLEAVER, DEATHS_DANCE], slot6: [WITS_END],
});

/** allgamedata with you (Irelia) holding `inventory`, in that slot order, and a lane opponent. */
function payload(inventory: number[], active: AllGameData['activePlayer'] = { riotId: 'Me#KR1', riotIdGameName: 'Me', currentGold: 900 }): AllGameData {
  const p = (name: string, championName: string, team: 'ORDER' | 'CHAOS', it: number[]) => ({
    championName, riotId: `${name}#KR1`, riotIdGameName: name, summonerName: `${name}#KR1`, team, position: 'TOP', level: 11,
    items: it.map((itemID, slot) => ({ itemID, count: 1, slot })), scores: { kills: 2, deaths: 1, assists: 1, creepScore: 150, wardScore: 10 },
  });
  return { activePlayer: active, allPlayers: [p('Me', 'Irelia', 'ORDER', inventory), p('Opp', 'Darius', 'CHAOS', [BLACK_CLEAVER])], gameData: { gameTime: 1200 } };
}

/** What the overlay does: owned ids from the Live Client, matched against the plan. */
function overlay(r: BuildRecommendation, raw: AllGameData) {
  const me = analyseLiveGame(raw, data, excl).players.find((p) => p.isMe);
  return planStatus(buildPlan(r, items), me?.items ?? [], items);
}
const states = (s: ReturnType<typeof overlay>) => s.entries.map((e) => `${e.slot}:${e.state}`);
const entry = (s: ReturnType<typeof overlay>, slot: string) => s.entries.find((e) => e.slot === slot)!;

// ---- Data Dragon facts the rules rely on --------------------------------------------------------------

test('catalog: tier-3 boots upgrade tier-2 ones, Gunmetal Greaves is boots without the Boots tag', () => {
  assert.deepEqual(items[ARMORED_ADVANCE].from, [STEELCAPS]);
  assert.deepEqual([items[ARMORED_ADVANCE].boots, items[ARMORED_ADVANCE].finished], [true, true]);
  assert.ok(!data.items.get(GUNMETAL)!.tags.includes('Boots'), 'Data Dragon still lacks the tag');
  assert.deepEqual([items[GUNMETAL].from, items[GUNMETAL].boots], [[BERSERKERS], true]);
  assert.deepEqual([items[BOOTS].boots, items[BOOTS].finished], [false, false], 'tier-1 boots are a component');
  assert.deepEqual([items[SHEEN].finished, items[TRINITY].finished], [false, true]);
  assert.deepEqual([items[MURAMANA].from, items[MURAMANA].finished], [[MANAMUNE], true], 'transform via specialRecipe');
  assert.equal(data.items.get(ARMORED_ADVANCE)!.goldTotal, data.items.get(STEELCAPS)!.goldTotal, 'the upgrade itself is free');
});

// ---- The cases from the bug report ------------------------------------------------------------------------

test('(a) recommended boots bought third: ticked, Next is the first entry still missing', () => {
  const s = overlay(IRELIA_TOP, payload([BOTRK, TRINITY, STEELCAPS, STEALTH_WARD]));
  assert.deepEqual(states(s), ['Core:done', 'Core:done', 'Core:todo', 'Boots:done', '4th:todo', '5th:todo', '6th:todo']);
  assert.equal(s.next?.id, STERAKS);
  assert.equal(s.next?.cost, 3200);
  // Inventory order never matters.
  assert.deepEqual(states(overlay(IRELIA_TOP, payload([STEELCAPS, STEALTH_WARD, TRINITY, BOTRK]))), states(s));
});

test('(b) a different pair of boots fills the boots slot as a swap', () => {
  const boots = entry(overlay(IRELIA_TOP, payload([BOTRK, TRINITY, MERCURYS])), 'Boots');
  assert.deepEqual([boots.state, boots.id, boots.owned], ['swap', STEELCAPS, MERCURYS]);
  const notListed = entry(overlay(IRELIA_TOP, payload([BOTRK, TRINITY, BERSERKERS])), 'Boots');
  assert.deepEqual([notListed.state, notListed.owned], ['swap', BERSERKERS], 'any boots, listed or not');
});

test('(c) upgraded tier-3 boots count as the planned pair, including Gunmetal Greaves', () => {
  const s = overlay(IRELIA_TOP, payload([BOTRK, TRINITY, ARMORED_ADVANCE]));
  assert.deepEqual([entry(s, 'Boots').state, entry(s, 'Boots').owned], ['upgraded', ARMORED_ADVANCE]);
  assert.equal(s.next?.id, STERAKS);

  const zerker = rec({ core: [[BOTRK, TRINITY, STERAKS]], boots: [BERSERKERS] });
  assert.equal(entry(overlay(zerker, payload([GUNMETAL])), 'Boots').state, 'upgraded');
  assert.equal(entry(overlay(IRELIA_TOP, payload([GUNMETAL])), 'Boots').state, 'swap', 'boots, though untagged');
});

test('(d) components never tick an item: it is being built, and Next costs what is left to pay', () => {
  const s = overlay(IRELIA_TOP, payload([BOTRK, SHEEN, STEELCAPS]));
  assert.deepEqual(states(s).slice(0, 4), ['Core:done', 'Core:building', 'Core:todo', 'Boots:done']);
  assert.deepEqual(s.entries[1].components, [SHEEN]);
  assert.equal(s.next?.id, TRINITY);
  assert.equal(s.next?.cost, 3333 - 900);

  const t1 = overlay(IRELIA_TOP, payload([BOTRK, TRINITY, STERAKS, BOOTS]));
  assert.deepEqual([entry(t1, 'Boots').state, t1.next?.id, t1.next?.cost], ['building', STEELCAPS, 1200 - 300]);
});

test('(d) an item of the same unique group replaces the planned one: Maw for Sterak\'s', () => {
  const s = overlay(IRELIA_TOP, payload([BOTRK, TRINITY, MAW, STEELCAPS]));
  assert.deepEqual([s.entries[2].state, s.entries[2].id, s.entries[2].owned], ['swap', STERAKS, MAW]);
  assert.equal(s.next?.slot, '4th', 'never "Next: Sterak\'s", which the shop would refuse');
});

test('(d) an alternative the recommendation lists for that slot fills it as a swap', () => {
  const r = rec({
    core: [[BOTRK, TRINITY, STERAKS], [BOTRK, TRINITY, BLACK_CLEAVER]], boots: [STEELCAPS],
    slot4: [DEATHS_DANCE, SPIRIT_VISAGE], slot5: [WITS_END],
  });
  const s = overlay(r, payload([BOTRK, TRINITY, BLACK_CLEAVER, STEELCAPS, SPIRIT_VISAGE]));
  assert.deepEqual([s.entries[2].state, s.entries[2].owned], ['swap', BLACK_CLEAVER], 'core alternative: Black Cleaver 3rd');
  assert.deepEqual([entry(s, '4th').state, entry(s, '4th').owned], ['swap', SPIRIT_VISAGE]);
  assert.equal(s.next?.slot, '5th');

  // An item planned for a later slot is that slot's, even when it is also an earlier slot's alternative.
  const early = overlay(IRELIA_TOP, payload([BOTRK, TRINITY, DEATHS_DANCE, STEELCAPS]));
  assert.deepEqual(states(early).slice(2, 5), ['Core:todo', 'Boots:done', '4th:done']);
  assert.equal(early.next?.id, STERAKS);
});

test('(e) duplicates: the plan never repeats an item, and one owned item fills one entry', () => {
  // The 5th slot's top option is already the 4th: take its next option that fits.
  const plan = buildPlan(rec({ core: [[BOTRK, TRINITY, STERAKS]], boots: [STEELCAPS], slot4: [DEATHS_DANCE], slot5: [DEATHS_DANCE, MAW, BLACK_CLEAVER] }), items);
  assert.deepEqual(plan.map((p) => p.id), [BOTRK, TRINITY, STERAKS, STEELCAPS, DEATHS_DANCE, BLACK_CLEAVER], 'Maw is blocked by Sterak\'s');

  const s = overlay(IRELIA_TOP, payload([BOTRK, TRINITY, STEELCAPS, STEELCAPS, LONG_SWORD, LONG_SWORD]));
  assert.deepEqual(states(s).slice(0, 4), ['Core:done', 'Core:done', 'Core:building', 'Boots:done']);
  assert.deepEqual(s.entries[2].components, [LONG_SWORD], 'Sterak\'s needs one Long Sword (in Tunneler)');
  assert.equal(s.next?.cost, 3200 - 350);
  assert.equal(planStatus([{ id: STERAKS, slot: 'Core', alternatives: [] }], [TUNNELER, LONG_SWORD], items).next?.cost, 3200 - 1150,
    'an owned Tunneler covers its Long Sword; the spare one is not counted twice');

  // Two Long Swords both count towards Titanic Hydra (via Tiamat), each only once.
  const titanic: PlanEntry[] = [{ id: TITANIC, slot: 'Core', alternatives: [] }];
  assert.equal(planStatus(titanic, [LONG_SWORD, LONG_SWORD], items).next?.cost, 3300 - 2 * 350);
  assert.equal(planStatus(titanic, [LONG_SWORD], items).next?.cost, 3300 - 350);
});

test('(f) you are found even when the active player has no Riot ID, like the live tracker does', () => {
  const raw = payload([BOTRK, TRINITY, STEELCAPS], { riotIdGameName: 'Me', summonerName: 'Me', currentGold: 900 });
  assert.deepEqual(states(overlay(IRELIA_TOP, raw)).slice(0, 4), ['Core:done', 'Core:done', 'Core:todo', 'Boots:done']);
  assert.equal(analyseLiveGame(raw, data, excl).laneOpponent?.championName, 'Darius');
});

test('transformed items count as their base item, and a full build leaves nothing next', () => {
  const s = planStatus([{ id: MANAMUNE, slot: 'Core', alternatives: [] }], [MURAMANA], items);
  assert.equal(s.entries[0].state, 'upgraded');
  assert.equal(s.next, null);
  const full = overlay(IRELIA_TOP, payload([BOTRK, TRINITY, STERAKS, ARMORED_ADVANCE, DEATHS_DANCE, BLACK_CLEAVER, WITS_END]));
  assert.equal(full.next, null);
});

test('end to end: a plan from the engine\'s own recommendation', () => {
  const row = (i: number, its: number[]): IreliaGameRow => ({
    match_id: `T_${i}`, puuid: 'p', patch: data.patch, win: i % 3 ? 1 : 0, position: 'TOP', opponent_champion_id: 122,
    summoner1: 4, summoner2: 12, primary_style: 8000, sub_style: 8400,
    perks: JSON.stringify([8010, 9111, 9104, 8299, 8444, 8242]), stat_perks: JSON.stringify([5005, 5008, 5011]), skill_order: 'QEWQQRQEQ',
    purchases: JSON.stringify([[1055, 1000], [STEELCAPS, 400_000], ...its.map((id, k) => [id, 600_000 + k * 300_000])]),
    final_items: '[]', kills: 1, deaths: 1, assists: 1, game_duration: 1800, created_at: 0,
  });
  const settings = { minMatchupGames: 15, generalPriorWeight: 0.15, minLaneGames: 30, otherLaneWeight: 0.15, previousPatchWeight: 0.35, minCurrentPatchGames: 40, startingItemWindowMs: 60_000 };
  const r = recommend(Array.from({ length: 30 }, (_, i) => row(i, [BOTRK, TRINITY, STERAKS, DEATHS_DANCE])), data, excl, 122, settings, 'TOP');
  const s = overlay(r, payload([BOTRK, TRINITY, ARMORED_ADVANCE]));
  // These games buy their boots at 6:40, before any item: the plan starts with them.
  assert.equal(r.bootsAfter, 0);
  assert.deepEqual(states(s).slice(0, 4), ['Boots:upgraded', 'Core:done', 'Core:done', 'Core:todo']);
  assert.equal(s.next?.id, STERAKS);
});

test('the boots take their place in the build order: after the first item when the games buy them there', () => {
  const early = { ...IRELIA_TOP, bootsAfter: 1 } as BuildRecommendation;
  const plan = overlay(early, payload([BOTRK, STEALTH_WARD]));
  assert.deepEqual(states(plan), ['Core:done', 'Boots:todo', 'Core:todo', 'Core:todo', '4th:todo', '5th:todo', '6th:todo']);
  assert.equal(plan.next?.id, STEELCAPS, 'boots are next after the first item');
  assert.deepEqual(states(overlay({ ...IRELIA_TOP, bootsAfter: null } as BuildRecommendation, payload([])))[3], 'Boots:todo', 'unknown: after the core');
});
