// The matchup every window shows (server/matchup.ts): where the lane and the opponent come from in each phase, and how
// long your own picks hold.
import assert from 'node:assert/strict';
import { test } from 'node:test';
import type { Lane, LcuState } from '../shared/types.ts';
import { MatchupSession, type LiveView } from './matchup.ts';

const DARIUS = 122, LEESIN = 64, SYNDRA = 134, JINX = 222, THRESH = 412, AHRI = 103, IRELIA = 39;

function session(saved: Lane | null = null, guess: Partial<Record<Lane, number | null>> = {}) {
  const saves: Lane[] = [];
  const s = new MatchupSession({ savedLane: () => saved, saveLane: (l) => saves.push(l), guess: () => guess });
  return { s, saves };
}

const idle: LcuState = { status: 'connected', phase: 'Lobby', champSelect: null };
function draft(position: string, enemies: number[], suggested: Partial<Record<Lane, number | null>>): LcuState {
  return {
    status: 'connected', phase: 'ChampSelect',
    champSelect: {
      myChampionId: IRELIA, myPosition: position, isIrelia: true,
      allies: [{ cellId: 0, championId: IRELIA, position, locked: true }, { cellId: 1, championId: AHRI, position: '', locked: true }],
      enemies: enemies.map((championId, i) => ({ cellId: 5 + i, championId, position: '', locked: true })),
      suggestedOpponentId: suggested.TOP ?? null, suggestedOpponents: { TOP: suggested.TOP ?? null, MIDDLE: suggested.MIDDLE ?? null },
    },
  };
}
const game = (lane: Lane | null, positionOpponent: number | null, enemies = [DARIUS, LEESIN, SYNDRA, JINX, THRESH]): LiveView =>
  ({ lane, positionOpponent, enemies, allies: [AHRI] });

test('idle: the lane you picked last, no opponent; a pick is saved and holds while nothing assigns a lane', () => {
  const { s, saves } = session('MIDDLE');
  let m = s.resolve(idle, null);
  assert.deepEqual([m.phase, m.lane, m.laneFrom, m.opponentId], ['idle', 'MIDDLE', 'saved', null]);
  s.setLane('TOP');
  m = s.resolve(idle, null);
  assert.deepEqual([m.lane, m.laneFrom], ['TOP', 'saved']);
  assert.deepEqual(saves, ['TOP']);
  s.setOpponent(DARIUS);
  m = s.resolve(idle, null);
  assert.deepEqual([m.opponentId, m.opponentFrom], [DARIUS, 'manual']);
  assert.equal(session().s.resolve(idle, null).lane, 'TOP', 'Top before any pick');
});

test('champ select: the assigned lane and its likely opponent; your picks hold until the draft and its game end', () => {
  const { s } = session('TOP');
  s.setOpponent(SYNDRA);   // an idle pick never reaches the draft
  let m = s.resolve(draft('MIDDLE', [DARIUS, SYNDRA], { TOP: DARIUS, MIDDLE: SYNDRA }), null);
  assert.deepEqual([m.phase, m.lane, m.laneFrom, m.opponentId, m.opponentFrom], ['draft', 'MIDDLE', 'draft', SYNDRA, 'draft']);
  assert.deepEqual([m.enemies, m.allies], [[DARIUS, SYNDRA], [AHRI]]);

  // Switching to the other lane follows that lane's likely opponent; picking the assigned lane follows the draft again.
  s.setLane('TOP');
  m = s.resolve(draft('MIDDLE', [DARIUS, SYNDRA], { TOP: DARIUS, MIDDLE: SYNDRA }), null);
  assert.deepEqual([m.lane, m.laneFrom, m.detectedLane, m.opponentId], ['TOP', 'manual', 'MIDDLE', DARIUS]);
  s.setLane('MIDDLE');
  assert.equal(s.resolve(draft('MIDDLE', [DARIUS, SYNDRA], { TOP: DARIUS, MIDDLE: SYNDRA }), null).laneFrom, 'draft');

  s.setOpponent(DARIUS);
  m = s.resolve(draft('MIDDLE', [DARIUS, SYNDRA], { TOP: DARIUS, MIDDLE: SYNDRA }), null);
  assert.deepEqual([m.opponentId, m.opponentFrom], [DARIUS, 'manual']);
  s.setOpponent(undefined);
  assert.equal(s.resolve(draft('MIDDLE', [DARIUS, SYNDRA], { TOP: DARIUS, MIDDLE: SYNDRA }), null).opponentFrom, 'draft', 'follow the draft again');

  // A dodge: back to idle drops every pick from the draft.
  s.setOpponent(DARIUS);
  m = s.resolve(idle, null);
  assert.deepEqual([m.phase, m.lane, m.opponentId], ['idle', 'MIDDLE', null]);
});

test('in game: your position finds the opponent; without positions, champ select\'s pick, else the database guess', () => {
  const lcu = draft('TOP', [DARIUS, LEESIN, SYNDRA, JINX, THRESH], { TOP: DARIUS, MIDDLE: SYNDRA });
  let m = session().s.resolve(lcu, game('TOP', DARIUS));
  assert.deepEqual([m.phase, m.lane, m.laneFrom, m.opponentId, m.opponentFrom], ['game', 'TOP', 'game', DARIUS, 'game']);

  // Customs and the practice tool have no positions: the draft (kept through the game) says who, and which lane.
  m = session().s.resolve(lcu, game(null, null));
  assert.deepEqual([m.lane, m.laneFrom, m.opponentId, m.opponentFrom], ['TOP', 'draft', DARIUS, 'draft']);

  // The draft's pick is not in the game (they swapped at the end), or there was no draft: the database guess.
  const { s } = session('MIDDLE', { MIDDLE: SYNDRA });
  m = s.resolve(idle, game(null, null, [AHRI, JINX, SYNDRA]));
  assert.deepEqual([m.lane, m.laneFrom, m.opponentId, m.opponentFrom], ['MIDDLE', 'saved', SYNDRA, 'guess']);
  assert.equal(session('TOP', {}).s.resolve(idle, game(null, null)).opponentFrom, null, 'nothing fits: all matchups');
});

test('a champ-select pick carries into its game, unless that champion is not in it; a pick in game wins over the position', () => {
  const { s } = session();
  const lcu = draft('TOP', [DARIUS, LEESIN, SYNDRA, JINX, THRESH], { TOP: DARIUS });
  s.resolve(lcu, null);
  s.setOpponent(LEESIN);
  let m = s.resolve(lcu, game('TOP', DARIUS));
  assert.deepEqual([m.opponentId, m.opponentFrom], [LEESIN, 'manual']);

  const other = session();
  other.s.resolve(lcu, null);
  other.s.setOpponent(AHRI);   // hovered, then someone else was locked in
  m = other.s.resolve(lcu, game('TOP', DARIUS));
  assert.deepEqual([m.opponentId, m.opponentFrom], [DARIUS, 'game']);

  s.setOpponent(SYNDRA);
  m = s.resolve(lcu, game('TOP', DARIUS));
  assert.deepEqual([m.opponentId, m.opponentFrom], [SYNDRA, 'manual']);
  m = s.resolve(idle, null);
  assert.deepEqual([m.phase, m.opponentId], ['idle', null], 'over once the game is');
});

test('the key changes with what the build depends on, and regions are part of it', () => {
  const { s } = session();
  const a = s.resolve(draft('TOP', [DARIUS], { TOP: DARIUS }), null);
  const b = s.resolve(draft('TOP', [DARIUS, SYNDRA], { TOP: DARIUS }), null);
  assert.notEqual(a.key, b.key);
  s.setRegions(['KR']);
  const c = s.resolve(draft('TOP', [DARIUS, SYNDRA], { TOP: DARIUS }), null);
  assert.deepEqual(c.regions, ['KR']);
  assert.notEqual(b.key, c.key);
  s.setRegions([]);
  assert.equal(s.resolve(draft('TOP', [DARIUS, SYNDRA], { TOP: DARIUS }), null).key, b.key, 'no regions means all of them');
});

test('a pick made in game survives the Live Client dropping out (a reconnect) and ends with the game', () => {
  const { s } = session();
  const lcu = draft('TOP', [DARIUS, LEESIN, SYNDRA, JINX, THRESH], { TOP: DARIUS });
  s.resolve(lcu, game('TOP', DARIUS));
  s.setOpponent(SYNDRA);
  s.setLane('MIDDLE');
  assert.equal(s.resolve(lcu, null).phase, 'draft', 'the game drops out; champ select is still kept');
  const m = s.resolve(lcu, game('TOP', DARIUS));
  assert.deepEqual([m.lane, m.laneFrom, m.opponentId, m.opponentFrom], ['MIDDLE', 'manual', SYNDRA, 'manual']);
  const after = s.resolve(idle, null);
  assert.deepEqual([after.opponentId, after.laneFrom], [null, 'saved']);
});
