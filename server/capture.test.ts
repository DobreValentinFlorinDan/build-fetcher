// What v3 keeps from a match (server/capture.ts): participants, remakes, leavers, the lane opponent from the
// timeline, objectives and inventory replay. Synthetic match-v5 fixtures, offline.
import assert from 'node:assert/strict';
import { test } from 'node:test';
import {
  captureTimeline, inventoryAt, ITEM_EVENT, laneOpponents, leavers, matchQuality, participantRows, REMAKE_MAX_SECONDS,
} from './capture.ts';
import { IRELIA, makeMatch, makeTimeline } from './test/fixtures.ts';

test('every participant becomes a row with damage by type, healing, CC and the useful challenges', () => {
  const rows = participantRows(makeMatch({ id: 'KR_1' }));
  assert.equal(rows.length, 10);
  const syndra = rows.find((r) => r.champion_id === 134)!;
  assert.deepEqual([syndra.participant_id, syndra.team_id, syndra.position], [8, 200, 'MIDDLE']);
  assert.deepEqual([syndra.dmg_physical, syndra.dmg_magic, syndra.dmg_true], [1000, 25_000, 1500]);
  assert.equal(syndra.cs, 210);
  assert.deepEqual(JSON.parse(syndra.items), [3153, 3078, 3047, 0, 0, 0, 3340]);
  assert.equal(rows.find((r) => r.champion_id === 89)!.heal_allies, 4000);
  const ch = JSON.parse(rows[0].challenges!);
  assert.equal(ch.damagePerMinute, 600);
  assert.equal(ch.unrelatedCosmetic, undefined, 'only the listed challenge fields are kept');
});

test('remakes: the early-surrender vote or a game under five minutes', () => {
  assert.equal(matchQuality(makeMatch({ id: 'KR_1' })).remake, false);
  assert.equal(matchQuality(makeMatch({ id: 'KR_2', remakeVote: true, minutes: 4 })).remake, true);
  const short = makeMatch({ id: 'KR_3' });
  short.info.gameDuration = REMAKE_MAX_SECONDS - 1;
  assert.equal(matchQuality(short).remake, true);
  assert.equal(matchQuality(makeMatch({ id: 'KR_4' })).endResult, 'GameComplete');
});

test('leavers: XP stops and gold only grows by passive income for three minutes or more', () => {
  const match = makeMatch({ id: 'KR_1', minutes: 30 });
  assert.deepEqual(leavers(captureTimeline(makeTimeline(match))), []);
  assert.deepEqual(leavers(captureTimeline(makeTimeline(match, { leaver: { pid: 4, fromMinute: 18 } }))), [4]);
  // Two idle minutes (a long death) are not a leaver; the first two minutes never count.
  assert.deepEqual(leavers(captureTimeline(makeTimeline(match, { leaver: { pid: 4, fromMinute: 28 } }))), []);
});

test('leavers at level 18: XP no longer moves, so CS and gold must stop for five minutes', () => {
  // A hand-made capture: participant 1 at level 18, XP frozen at the cap, passive gold only, no CS.
  const frames = (idleFrom: number, minutes: number) => Array.from({ length: minutes + 1 }, (_, m) => {
    const active = Math.min(m, idleFrom);
    return [500 + active * 400 + Math.max(0, m - idleFrom) * 122, 0, 18_360, 18, 300 + active * 6];
  });
  const cap = (idleFrom: number, minutes = 45) => ({
    v: 1 as const, interval: 60_000, t: Array.from({ length: minutes + 1 }, (_, m) => m * 60_000),
    frames: { '1': frames(idleFrom, minutes) }, pos: {}, items: [], kills: [], objectives: [],
  });
  assert.deepEqual(leavers(cap(41)), [], 'four quiet minutes at level 18: a long death and a standoff');
  assert.deepEqual(leavers(cap(38)), [1], 'seven: gone');
  // Below 18, XP flat but CS still growing is not idle.
  const farming = cap(41);
  farming.frames['1'] = farming.frames['1'].map(([g, c, , , cs], m) => [g, c, 9000, 11, cs + m]);
  assert.deepEqual(leavers(farming), []);
});

test('the lane opponent comes from positions, and wins over teamPosition in a lane swap', () => {
  const match = makeMatch({ id: 'KR_1' });
  const irelia = match.info.participants[0];
  const normal = laneOpponents(match, captureTimeline(makeTimeline(match)), irelia);
  assert.deepEqual(normal, { position: 122, timeline: 122, effective: 122 });

  // Riot labels Darius TOP, but Syndra (participant 8) stood in top lane with Irelia and Darius went mid.
  const swapped = makeTimeline(match, { spots: { 6: [7400, 7400], 8: [1500, 12500] } });
  assert.deepEqual(laneOpponents(match, captureTimeline(swapped), irelia), { position: 122, timeline: 134, effective: 134 });
  assert.deepEqual(laneOpponents(match, null, irelia), { position: 122, timeline: null, effective: 122 }, 'no timeline: the label');
});

test('the compact timeline keeps frames for all ten, 16 position frames, kills and objectives by the team that took them', () => {
  const match = makeMatch({ id: 'KR_1', minutes: 25 });
  const cap = captureTimeline(makeTimeline(match));
  assert.equal(cap.t.length, 26);
  assert.equal(Object.keys(cap.frames).length, 10);
  assert.deepEqual(cap.frames['1'][10], [500 + 10 * 380, 200, 6000, 6, 70]);
  assert.equal(cap.pos['1'].length, 16);
  assert.deepEqual(cap.kills[0], [6 * 60_000 + 200, 1, 6, [2], 300, 0]);
  assert.deepEqual(cap.objectives.map(([, team, kind]) => [team, kind]), [[200, 'DRAGON'], [100, 'PLATE'], [100, 'TOWER']],
    'the tower and plate belonged to team 200, so team 100 took them');
  const json = JSON.stringify(cap);
  assert.ok(json.length < 40_000, `compact: ${json.length} bytes`);
});

test('inventory replay: buys, sells, components used up and undos', () => {
  const match = makeMatch({ id: 'KR_1' });
  const cap = captureTimeline(makeTimeline(match, {
    items: [[3, 'ITEM_PURCHASED', 1036], [4, 'ITEM_PURCHASED', 3057], [5, 'ITEM_UNDO', 3057, 0],
      [8, 'ITEM_DESTROYED', 1036], [8, 'ITEM_PURCHASED', 3078], [10, 'ITEM_SOLD', 1055], [11, 'ITEM_UNDO', 0, 1055]],
  }));
  assert.ok(cap.items.some(([, , kind]) => kind === ITEM_EVENT.UNDO));
  assert.deepEqual(inventoryAt(cap, 1, 4 * 60_000 + 600).sort(), [1036, 1055, 2003, 3057].sort());
  assert.deepEqual(inventoryAt(cap, 1, 5 * 60_000 + 600).sort(), [1036, 1055, 2003].sort(), 'undo removes the purchase');
  assert.deepEqual(inventoryAt(cap, 1, 9 * 60_000).sort(), [1055, 2003, 3078].sort(), 'the recipe used up the Long Sword');
  assert.deepEqual(inventoryAt(cap, 1, 10 * 60_000 + 600).sort(), [2003, 3078].sort());
  assert.deepEqual(inventoryAt(cap, 1, 11 * 60_000 + 600).sort(), [1055, 2003, 3078].sort(), 'an undone sale comes back');
  assert.equal(match.info.participants[0].championId, IRELIA);
});
