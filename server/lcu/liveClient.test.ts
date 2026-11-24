import assert from 'node:assert/strict';
import { test } from 'node:test';
import type { AllGameData, LiveApiEvent, LiveApiPlayer } from '../engine/adaptive.ts';
import { eventIncome, GOLD, PerformanceTracker } from './liveClient.ts';

const PRICES: Record<number, number> = { 3153: 3200, 1055: 450, 1036: 350, 3044: 1100, 2055: 75, 3340: 0, 3364: 0 };
const priceOf = (id: number) => PRICES[id] ?? 0;

function player(name: string, championName: string, team: 'ORDER' | 'CHAOS', position: string,
  o: { cs?: number; ward?: number; k?: number; d?: number; a?: number; items?: number[] } = {}): LiveApiPlayer {
  return {
    championName, riotId: `${name}#KR1`, riotIdGameName: name, summonerName: `${name}#KR1`, team, position, level: 9,
    items: (o.items ?? []).map((itemID, slot) => ({ itemID, count: 1, slot })),
    scores: { kills: o.k ?? 0, deaths: o.d ?? 0, assists: o.a ?? 0, creepScore: o.cs ?? 0, wardScore: o.ward ?? 0 },
  };
}

const kill = (t: number, killer: string, victim: string, assisters: string[] = []): LiveApiEvent =>
  ({ EventID: t, EventName: 'ChampionKill', EventTime: t, KillerName: killer, VictimName: victim, Assisters: assisters });

function game(t: number, o: { meCs?: number; oppCs?: number; meGold?: number; events?: LiveApiEvent[]; meItems?: number[]; oppItems?: number[] } = {}): AllGameData {
  return {
    activePlayer: { riotId: 'Me#KR1', riotIdGameName: 'Me', currentGold: o.meGold ?? 800 },
    allPlayers: [
      player('Me', 'Irelia', 'ORDER', 'TOP', { cs: o.meCs ?? 80, ward: 12.6, k: 1, items: o.meItems ?? [3153, 2055, 3340] }),
      player('Jg', 'LeeSin', 'ORDER', 'JUNGLE', { cs: 50, a: 1 }),
      player('Opp', 'Darius', 'CHAOS', 'TOP', { cs: o.oppCs ?? 70, ward: 8.2, d: 1, items: o.oppItems ?? [1055, 1036, 3044, 3364] }),
      player('Mid', 'Ahri', 'CHAOS', 'MIDDLE', { cs: 90 }),
    ],
    events: { Events: o.events ?? [
      { EventID: 0, EventName: 'GameStart', EventTime: 0 },
      kill(300, 'Me', 'Opp', ['Jg']),
      { EventID: 1, EventName: 'FirstBlood', EventTime: 300, Recipient: 'Me' },
    ] },
    gameData: { gameTime: t },
  };
}

test('kill events pay the killer, split the assist share and add first blood', () => {
  const g = game(600);
  const income = eventIncome(g.allPlayers, g.events!.Events)!;
  assert.equal(income.get('Me#KR1'), GOLD.kill + GOLD.firstBlood);
  assert.equal(income.get('Jg#KR1'), GOLD.kill * GOLD.assistShare);
  assert.equal(income.get('Opp#KR1'), 0);
});

test('repeated deaths lower the bounty and shutdowns raise it', () => {
  const g = game(900);
  const income = eventIncome(g.allPlayers, [kill(100, 'Me', 'Opp'), kill(200, 'Me', 'Opp'), kill(300, 'Opp', 'Me')])!;
  // Opp's second consecutive death is worth less; Me was on a 2-kill streak when Opp shut it down.
  assert.equal(income.get('Me#KR1'), GOLD.kill + GOLD.kill * GOLD.deathStreakFactor);
  assert.equal(income.get('Opp#KR1'), GOLD.kill + GOLD.streakBonus);
});

test('unresolvable event names fall back to scoreboard KDA', () => {
  const g = game(600);
  assert.equal(eventIncome(g.allPlayers, [kill(100, 'Someone', 'Nobody')]), null);
});

test('tracker reports CS / vision / gold differences against the lane opponent', () => {
  const tracker = new PerformanceTracker();
  const p = tracker.update(game(600), priceOf)!;
  assert.equal(p.opponent?.championName, 'Darius');
  assert.equal(p.diff?.cs, 10);
  assert.equal(p.diff?.vision, 4.4);
  assert.equal(p.me.controlWards, 1);
  assert.equal(p.me.trinket, 3340);
  assert.equal(p.opponent?.trinket, 3364);

  // Your net worth is exact: inventory + unspent gold.
  assert.equal(p.me.itemGold, 3275);
  assert.equal(p.me.netWorth, 3275 + 800);
  // The model is calibrated on you and applied to the fogged opponent.
  const passive = (600 - GOLD.passiveFrom) * GOLD.passivePerSec;
  const myModel = Math.round(GOLD.start + passive + 80 * GOLD.perCs + GOLD.kill + GOLD.firstBlood);
  assert.equal(p.me.modelGold, myModel);
  assert.equal(p.gold.calibration, Math.round(Math.min(1.15, (3275 + 800) / myModel) * 1000) / 1000);
  const oppModel = Math.round(GOLD.start + passive + 70 * GOLD.perCs);
  assert.equal(p.opponent?.netWorth, Math.round(Math.max(1900, oppModel * p.gold.calibration)));
  assert.equal(p.opponent!.unspentGold, p.opponent!.netWorth - 1900);
  assert.equal(p.diff?.gold, p.me.netWorth - p.opponent!.netWorth);
  assert.equal(p.diff?.itemGold, 3275 - 1900);
  assert.ok(p.gold.uncertainty > 0);
});

test('visible items set a floor under the opponent estimate', () => {
  const p = new PerformanceTracker().update(game(600, { oppItems: [3153, 3153, 3044] }), priceOf)!;
  assert.equal(p.opponent?.netWorth, 7500);
  assert.equal(p.opponent?.unspentGold, 0);
  assert.equal(p.gold.confidence, 'high');
});

test('history samples every 15 s of game time and resets on a new game', () => {
  const tracker = new PerformanceTracker();
  for (let t = 600; t <= 660; t += 1) tracker.update(game(t), priceOf);
  const h = tracker.update(game(661), priceOf)!.history;
  assert.deepEqual(h.map((s) => s.t), [600, 615, 630, 645, 660]);
  assert.equal(tracker.update(game(30), priceOf)!.history.length, 1);   // clock went backwards: new game
});

test('CS reported in steps of 10 is flagged as approximate', () => {
  const tracker = new PerformanceTracker();
  let p = tracker.update(game(300, { meCs: 30, oppCs: 20 }), priceOf)!;
  assert.equal(p.csGranularity, 1);   // the fixture's 50 / 90 CS teammates alone are not enough evidence
  p = tracker.update(game(400, { meCs: 40, oppCs: 30 }), priceOf)!;
  assert.equal(p.csGranularity, 10);
  p = tracker.update(game(450, { meCs: 43, oppCs: 30 }), priceOf)!;
  assert.equal(p.csGranularity, 1);
});

test('another lane opponent (picked by hand, or found later) starts the history again', () => {
  const tracker = new PerformanceTracker();
  for (let t = 600; t <= 660; t += 15) tracker.update(game(t), priceOf);
  assert.equal(tracker.update(game(675), priceOf)!.history.length, 6);
  const vsAhri = tracker.update(game(690), priceOf, 'Ahri')!;
  assert.equal(vsAhri.opponent?.championName, 'Ahri');
  assert.deepEqual(vsAhri.history.map((s) => s.t), [690], 'only samples against Ahri');
  assert.equal(tracker.update(game(705), priceOf, 'Ahri')!.history.length, 2);
});
