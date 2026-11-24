// Stage C on synthetic games: matchup facts (C2) and the best purchase for your gold (C3), against the tests' Data
// Dragon snapshot (Trinity Force = Sheen + Phage + Hearthbound Axe).
import assert from 'node:assert/strict';
import { test } from 'node:test';
import type { Lane } from '../../shared/types.ts';
import { liveSummary } from '../engine/adaptive.ts';
import { loadTestStaticData } from '../test/staticData.ts';
import { BuyIndex, missingTree } from './buy.ts';
import type { PreparedGame, ScoredGame } from './dataset.ts';
import { BEST_PRIOR, matchupBest, matchupFacts } from './facts.ts';

const data = loadTestStaticData();
const TRINITY = 3078, SHEEN = 3057, PHAGE = 3044, AXE = 3051, LONG_SWORD = 1036, RUBY = 1028;
const DARIUS = 122, SETT = 875;

let n = 0;
function game(o: { lane?: Lane; opponent?: number; win?: 0 | 1; source?: string; minutes?: number; lane10?: number | null; lane15?: number | null;
  path?: number[] }): PreparedGame {
  const path = o.path ?? [];
  return {
    matchId: `KR_${++n}`, pilot: `p${n % 40}`, source: o.source ?? 'otp', lane: o.lane ?? 'TOP', opponent: o.opponent ?? DARIUS, win: o.win ?? 1,
    duration: (o.minutes ?? 30) * 60, lane10: o.lane10 ?? null, lane15: o.lane15 ?? null, frames: null,
    // Parts a minute apart, then Trinity Force.
    purchases: [...path.map((id, i) => [id, (i + 3) * 60_000] as [number, number]), [TRINITY, (path.length + 4) * 60_000]],
    items: [{ item: TRINITY, t: (path.length + 4) * 60_000, x: null, live: [] }],
  } as unknown as PreparedGame;
}
const scored = (gs: PreparedGame[]) => gs.map((g) => ({ g, wpa0: 0, itemWpa: [0], bootsWpa: null }) as ScoredGame);

test('C2 matchup facts: the lane gold at 10 and 15 and the win rate by length, from this lane against this opponent only', () => {
  const games = [
    game({ lane10: -300, lane15: -500, minutes: 22, win: 0 }), game({ lane10: -100, lane15: 200, minutes: 27, win: 1 }),
    game({ lane10: 200, lane15: 900, minutes: 33, win: 1 }), game({ lane10: null, minutes: 40, win: 1 }),
    game({ opponent: SETT, lane10: 5000 }), game({ lane: 'MIDDLE', lane10: 5000 }), game({ source: 'me', lane10: 5000 }),
  ];
  const f = matchupFacts(games, 'TOP', DARIUS)!;
  assert.equal(f.games, 4);
  assert.equal(f.winRate, 0.75);
  assert.deepEqual(f.gold10, { median: -100, ahead: 1 / 3, games: 3 });
  assert.deepEqual(f.gold15, { median: 200, ahead: 2 / 3, games: 3 });
  assert.deepEqual(f.byLength.map((b) => [b.from, b.games, b.winRate]), [[0, 1, 0], [25, 1, 1], [30, 1, 1], [35, 1, 1]]);
  assert.equal(matchupFacts(games, 'TOP', null), null, 'no opponent, no facts');
  assert.equal(matchupFacts(games, 'TOP', SETT), null, 'too few games');
});

test('the item set\'s "vs" blocks: the start and first items that did best in this matchup, shrunk towards 0', () => {
  const BOTRK = 3153, STERAK = 3053, REMOVED = 999_999;
  const g = (start: string, first: number, wpa: number, o: { opponent?: number; source?: string } = {}) => ({
    ...scored([game({ opponent: o.opponent, source: o.source })])[0],
    g: { ...game({ opponent: o.opponent, source: o.source }), start, items: [{ item: first, t: 0, x: null, live: [] }] } as unknown as PreparedGame,
    wpa0: wpa, itemWpa: [wpa],
  });
  const games = [
    // Doran's Blade + potion: 10 games, all a little better; long sword: 6 games, much better; a lucky item in 2 games.
    ...Array.from({ length: 10 }, () => g('1055,2003', BOTRK, 0.1)),
    ...Array.from({ length: 6 }, () => g('1036,2003', STERAK, 0.3)),
    ...Array.from({ length: 2 }, () => g('', TRINITY, 0.9)),
    ...Array.from({ length: 8 }, () => g('', REMOVED, 0.9)),
    // Not this matchup, or your own games: never counted.
    ...Array.from({ length: 30 }, () => g('2051', AXE, 0.9, { opponent: SETT })),
    ...Array.from({ length: 30 }, () => g('2051', AXE, 0.9, { source: 'me' })),
  ];
  const buyable = (id: number) => id !== REMOVED;
  const best = matchupBest(games, 'TOP', DARIUS, buyable)!;
  // Shrunk: 6 × 0.3 / (6 + 20) beats 10 × 0.1 / (10 + 20); the 2-game item has too few games; the removed one can't be bought.
  assert.deepEqual([best.start!.value, best.start!.games], [[1036, 2003], 6]);
  assert.ok(Math.abs(best.start!.lift - (6 * 0.3) / (6 + BEST_PRIOR)) < 1e-12);
  assert.deepEqual(best.first.map((o) => [o.value, o.games]), [[STERAK, 6], [BOTRK, 10]]);
  assert.equal(matchupBest(games, 'TOP', null, buyable), null, 'no opponent');
  assert.equal(matchupBest(games.slice(0, 10), 'TOP', DARIUS, buyable), null, 'too few games against it');
});

test('C3 the missing parts of an item, and their price with what you own', () => {
  const none = missingTree(data, TRINITY, [])!;
  assert.equal(none.cost, 3333);
  const withSheen = missingTree(data, TRINITY, [SHEEN, LONG_SWORD])!;
  assert.equal(withSheen.cost, 3333 - 900 - 350, 'a Long Sword counts toward Phage or the Axe');
  assert.equal(missingTree(data, TRINITY, [TRINITY]), null);
});

test('C3 best buy: the part high-elo Irelias buy first, the rest of the gold to the next part, the item itself once affordable', () => {
  const games = [
    ...Array.from({ length: 30 }, () => game({ path: [SHEEN, LONG_SWORD, RUBY, PHAGE, AXE] })),
    ...Array.from({ length: 10 }, () => game({ path: [PHAGE, SHEEN, AXE] })),
  ];
  const buys = new BuyIndex(scored(games), null, data);
  const q = { lane: 'TOP' as Lane, opponent: SETT, target: TRINITY, owned: [] as number[] };
  const a = buys.best({ ...q, gold: 1000 })!;
  assert.deepEqual(a.items, [SHEEN]);
  assert.equal(a.finishes, false);
  assert.equal(a.share, 30 / 40);
  assert.match(a.reason, /75% of 40 high-elo games buy Sheen first toward Trinity Force/);
  assert.deepEqual(buys.best({ ...q, gold: 1300 })!.items, [SHEEN, LONG_SWORD], '400g left: the next part they buy');
  const done = buys.best({ ...q, owned: [SHEEN], gold: 2500 })!;
  assert.deepEqual([done.items, done.finishes, done.cost], [[TRINITY], true, 2433]);
  assert.equal(buys.best({ ...q, gold: 100 }), null, 'nothing affordable');
  // Games against your lane opponent count three times: here they start with Phage.
  const vsOpponent = new BuyIndex(scored([...games, ...Array.from({ length: 12 }, () => game({ opponent: SETT, path: [PHAGE, SHEEN] }))]), null, data);
  const vs = vsOpponent.best({ ...q, gold: 1200 })!;
  assert.deepEqual(vs.items, [PHAGE]);
  assert.match(vs.reason, /42% of 52 high-elo games buy Phage first toward Trinity Force \(12 of 12 against your opponent\)/,
    'the text counts games, the ranking weighs them');
});

test('C3 a part bought first less often wins when its games clearly did better than the game state predicted', () => {
  const games = [
    ...Array.from({ length: 40 }, () => game({ path: [SHEEN], win: 0 })),
    ...Array.from({ length: 35 }, () => game({ path: [PHAGE], win: 1 })),
  ];
  const b = new BuyIndex(scored(games), null, data).best({ lane: 'TOP', opponent: null, target: TRINITY, owned: [], gold: 1150 })!;
  assert.deepEqual(b.items, [PHAGE]);
  assert.match(b.reason, /less often than Sheen, but those games went better/);
  assert.ok(b.lift! > 0);
});

test('who is ahead, from the scoreboard: team items and kills, and a most fed player only when clearly above the team', () => {
  const p = (championId: number, team: 'ORDER' | 'CHAOS', items: number[], kills = 0, isMe = false) =>
    ({ championName: '', championId, team, position: '', level: 10, items, kills, deaths: 0, assists: 0, creepScore: 0, isMe });
  const me = p(39, 'ORDER', [TRINITY], 3, true);
  const players = [me, p(222, 'ORDER', [SHEEN]), p(64, 'ORDER', [LONG_SWORD]), p(134, 'CHAOS', [TRINITY, PHAGE], 4), p(DARIUS, 'CHAOS', [SHEEN], 1), p(SETT, 'CHAOS', [])];
  const s = liveSummary(players, me, data);
  assert.equal(s.teamItems, (3333 + 900 + 350) - (3333 + 1100 + 900));
  assert.equal(s.kills, -2);
  assert.deepEqual(s.fedEnemy, { championId: 134, items: 4433, ap: true });
  assert.deepEqual(s.fedAlly, { championId: 39, items: 3333, isMe: true });
  const even = liveSummary([me, p(134, 'CHAOS', [TRINITY])], me, data);
  assert.equal(even.fedEnemy, null, 'one enemy: nobody stands out');
});
