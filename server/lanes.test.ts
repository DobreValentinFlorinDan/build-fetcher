// Lane awareness outside the build engine: database queries, champ select, simulators.
import assert from 'node:assert/strict';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { after, test } from 'node:test';
import type { Lane } from '../shared/types.ts';
import { openDb, type IreliaGameRow } from './db.ts';
import { LANES } from '../shared/lanes.ts';
import { guessLaneOpponent, guessLaneOpponents } from './engine/laneOpponent.ts';
import { parseChampSelect, type ChampSelectSession, type GuessOpponents } from './lcu/connector.ts';
import { PerformanceTracker } from './lcu/liveClient.ts';
import { simulateLive } from './lcu/simulateLive.ts';

const dir = fs.mkdtempSync(path.join(os.tmpdir(), 'irelia-lanes-'));
after(() => fs.rmSync(dir, { recursive: true, force: true }));

function seed(db: ReturnType<typeof openDb>, id: string, patch: string, position: string, opponent: number | null) {
  db.insertMatch({ id, platform: 'KR', patch, version: `${patch}.1`, queue: 420, creation: 0, duration: 1800, hasIrelia: true });
  db.insertGame({
    match_id: id, puuid: 'p', patch, win: 1, position, opponent_champion_id: opponent, summoner1: 4, summoner2: 12,
    primary_style: 8000, sub_style: 8400, perks: '[]', stat_perks: '[]', skill_order: '', purchases: '[]', final_items: '[]',
    kills: 0, deaths: 0, assists: 0, game_duration: 1800, created_at: 0,
  } satisfies IreliaGameRow);
}

test('stored games split by lane per patch, and opponents can be counted per lane', () => {
  const db = openDb(path.join(dir, 'counts.db'));
  seed(db, 'KR_1', '16.19', 'TOP', 122);
  seed(db, 'KR_2', '16.19', 'TOP', 157);
  seed(db, 'KR_3', '16.19', 'MIDDLE', 157);
  seed(db, 'KR_4', '16.19', 'MIDDLE', 157);
  seed(db, 'KR_5', '16.19', 'UNKNOWN', null);
  seed(db, 'KR_6', '16.18', 'MIDDLE', 134);

  assert.deepEqual(db.patchBreakdown(), [
    { patch: '16.19', games: 5, lanes: { TOP: 2, MIDDLE: 2, UNKNOWN: 1 } },
    { patch: '16.18', games: 1, lanes: { MIDDLE: 1 } },
  ]);
  const byId = (rows: { id: number; games: number }[]) => Object.fromEntries(rows.map((r) => [r.id, r.games]));
  assert.deepEqual(byId(db.opponentCounts(['16.19'])), { 122: 1, 157: 3 });
  assert.deepEqual(byId(db.opponentCounts(['16.19'], 'TOP')), { 122: 1, 157: 1 });
  assert.deepEqual(byId(db.opponentCounts(['16.19', '16.18'], 'MIDDLE')), { 134: 1, 157: 2 });
  db.close();
});

// ---- Champ select: your lane and the likely lane opponent ----------------------------------------------

const DARIUS = 122, YASUO = 157, SYNDRA = 134, AHRI = 103, JINX = 222, LEESIN = 64, ZED = 238, AATROX = 266, ORIANNA = 61;
const CLASSES = new Map<number, { tags: string[] }>([
  [DARIUS, { tags: ['Fighter', 'Tank'] }], [YASUO, { tags: ['Fighter', 'Assassin'] }], [SYNDRA, { tags: ['Mage'] }],
  [AHRI, { tags: ['Mage', 'Assassin'] }], [JINX, { tags: ['Marksman'] }], [LEESIN, { tags: ['Fighter', 'Assassin'] }],
  [ZED, { tags: ['Assassin'] }],
]);

test('the lane opponent guess uses who Irelia faced in that lane, then the classes that play it', () => {
  const picks = [JINX, LEESIN, SYNDRA, DARIUS, YASUO];
  const none = new Map<number, number>();
  assert.equal(guessLaneOpponent(picks, 'TOP', none, CLASSES), LEESIN, 'first Fighter/Tank, as before lanes');
  assert.equal(guessLaneOpponent(picks, 'MIDDLE', none, CLASSES), SYNDRA, 'first Mage/Assassin');
  assert.equal(guessLaneOpponent([JINX, DARIUS, YASUO], 'MIDDLE', none, CLASSES), YASUO, 'second class when no main class fits');
  assert.equal(guessLaneOpponent([JINX], 'MIDDLE', none, CLASSES), JINX, 'nothing fits: first pick');
  assert.equal(guessLaneOpponent([], 'TOP', none, CLASSES), null);

  // Per-lane database counts beat the class heuristic: Irelia met Yasuo mid 12 times, Syndra 3 times.
  assert.equal(guessLaneOpponent(picks, 'MIDDLE', new Map([[YASUO, 12], [SYNDRA, 3]]), CLASSES), YASUO);
  assert.equal(guessLaneOpponent(picks, 'TOP', new Map([[DARIUS, 7]]), CLASSES), DARIUS);
});

// Yasuo is 30 of Irelia's 300 top games (10%) and 20 of her 100 mid games (20%).
const FACED = {
  TOP: new Map([[YASUO, 30], [DARIUS, 20], [AATROX, 250]]),
  MIDDLE: new Map([[YASUO, 20], [SYNDRA, 10], [ORIANNA, 70]]),
};

test('two lanes never get the same likely opponent: the lane where the pick is likelier keeps it', () => {
  const picks = [JINX, LEESIN, SYNDRA, DARIUS, YASUO];
  assert.equal(guessLaneOpponent(picks, 'TOP', FACED.TOP, CLASSES), YASUO, 'on its own, top would take Yasuo too');
  assert.deepEqual(guessLaneOpponents(picks, LANES, FACED, CLASSES), { TOP: DARIUS, MIDDLE: YASUO }, 'Yasuo goes mid, top takes Darius');

  // A lane with a handful of games can't claim a pick on one match: 1 of 2 mid games does not make 50%.
  const thinMid = { TOP: FACED.TOP, MIDDLE: new Map([[YASUO, 1], [ORIANNA, 1]]) };
  assert.deepEqual(guessLaneOpponents(picks, LANES, thinMid, CLASSES), { TOP: YASUO, MIDDLE: SYNDRA });

  // One pick so far: it goes to the lane its class plays, and the other lane waits for the next pick.
  assert.deepEqual(guessLaneOpponents([SYNDRA], LANES, {}, CLASSES), { TOP: null, MIDDLE: SYNDRA });
  assert.deepEqual(guessLaneOpponents([DARIUS], LANES, {}, CLASSES), { TOP: DARIUS, MIDDLE: null });
  assert.deepEqual(guessLaneOpponents([JINX], LANES, {}, CLASSES), { TOP: JINX, MIDDLE: null }, 'nothing fits either lane: top keeps it');
  assert.deepEqual(guessLaneOpponents([], LANES, {}, CLASSES), { TOP: null, MIDDLE: null });

  // Only the lanes asked for are guessed.
  assert.deepEqual(guessLaneOpponents(picks, ['TOP'], FACED, CLASSES), { TOP: YASUO });
});

function session(myPosition: string, enemies: [number, string][]): ChampSelectSession {
  return {
    localPlayerCellId: 2,
    myTeam: [0, 1, 2, 3, 4].map((cellId) => ({ cellId, championId: cellId === 2 ? 39 : 0, championPickIntent: 0, assignedPosition: cellId === 2 ? myPosition : '' })),
    theirTeam: enemies.map(([championId, assignedPosition], i) => ({ cellId: 5 + i, championId, championPickIntent: 0, assignedPosition })),
    actions: [[{ actorCellId: 2, championId: 39, completed: true, type: 'pick' }]],
  };
}

test('champ select reads your assigned lane and suggests an opponent for each lane', () => {
  const asked: [number[], Lane[]][] = [];
  const guess: GuessOpponents = (ids, lanes) => { asked.push([ids, lanes]); return { TOP: DARIUS, MIDDLE: SYNDRA }; };
  const ranked = parseChampSelect(session('middle', [[DARIUS, ''], [SYNDRA, ''], [JINX, '']]), 39, guess);
  assert.equal(ranked.myPosition, 'MIDDLE');
  assert.equal(ranked.isIrelia, true);
  assert.deepEqual(ranked.suggestedOpponents, { TOP: DARIUS, MIDDLE: SYNDRA });
  assert.equal(ranked.suggestedOpponentId, SYNDRA, 'follows the assigned lane');
  assert.deepEqual(asked, [[[DARIUS, SYNDRA, JINX], ['TOP', 'MIDDLE']]], 'both lanes guessed together, from every pick');

  // With the real guess, a pick both lanes would take is suggested for one lane only.
  const real: GuessOpponents = (ids, lanes) => guessLaneOpponents(ids, lanes, FACED, CLASSES);
  assert.deepEqual(parseChampSelect(session('top', [[YASUO, ''], [DARIUS, ''], [JINX, '']]), 39, real).suggestedOpponents,
    { TOP: DARIUS, MIDDLE: YASUO });
});

test('blind pick has no lane: nothing is assumed, and hidden enemy roles never count as a match', () => {
  const blind = parseChampSelect(session('', [[DARIUS, ''], [SYNDRA, '']]), 39, () => ({ TOP: DARIUS, MIDDLE: SYNDRA }));
  assert.equal(blind.myPosition, '', 'no more silent TOP default');
  assert.deepEqual(blind.suggestedOpponents, { TOP: DARIUS, MIDDLE: SYNDRA });
  assert.equal(blind.suggestedOpponentId, DARIUS, 'legacy field keeps its top default');
});

test('a visible enemy role beats the database guess for that lane', () => {
  const draft = parseChampSelect(session('top', [[ZED, 'MIDDLE'], [DARIUS, 'TOP'], [YASUO, '']]), 39, () => ({ TOP: YASUO, MIDDLE: YASUO }));
  assert.deepEqual(draft.suggestedOpponents, { TOP: DARIUS, MIDDLE: ZED });
  const noPicks = parseChampSelect(session('top', [[0, ''], [0, '']]), 39, () => ({ TOP: DARIUS, MIDDLE: DARIUS }));
  assert.deepEqual(noPicks.suggestedOpponents, { TOP: null, MIDDLE: null });
});

test('an enemy shown in another role is never guessed for a lane', () => {
  const asked: [number[], Lane[]][] = [];
  const guess: GuessOpponents = (ids, lanes) => { asked.push([ids, lanes]); return { TOP: ids[0] }; };
  const draft = parseChampSelect(session('top', [[ZED, 'MIDDLE'], [LEESIN, 'jungle'], [DARIUS, ''], [YASUO, '']]), 39, guess);
  assert.deepEqual(asked, [[[DARIUS, YASUO], ['TOP']]], 'only picks whose role is hidden, only for the lane still open');
  assert.deepEqual(draft.suggestedOpponents, { TOP: DARIUS, MIDDLE: ZED });
});

// ---- Live game: the simulator and the tracker pair you with your lane opponent ---------------------------

test('the live simulator plays either lane, and the tracker pairs you with that lane opponent', () => {
  for (const [lane, opponent] of [['TOP', 'Darius'], ['MIDDLE', 'Syndra']] as const) {
    let first: Parameters<Parameters<typeof simulateLive>[0]>[0] | null = null;
    const stop = simulateLive((raw) => { first ??= raw; }, { startAt: 600, lane, tickMs: 60_000 });
    stop();
    const me = first!.allPlayers.find((p) => p.riotIdGameName === 'You')!;
    assert.deepEqual([me.championName, me.position], ['Irelia', lane]);
    const perf = new PerformanceTracker().update(first!, () => 0)!;
    assert.equal(perf.opponent?.championName, opponent);
    assert.ok(perf.diff, `${lane}: CS / vision / gold differences against ${opponent}`);
  }
});
