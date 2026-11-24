// The matchup the app shows: lane, lane opponent and the two teams, resolved here once for the dashboard and the overlay
// alike from the League client (champ select), the Live Client (the game) and your own picks.
//
// Lane: your pick if you switched (it holds until the draft and its game are over, a reconnect included), else your in-game position, else
// your champ-select lane, else the lane you picked last (saved).
// Opponent: your pick if you made one (same rule), else the enemy in your position in game, else champ select's likely
// opponent for the lane (kept through the game), else, in game, the enemy Irelia faced most in that lane.
import { toLane } from '../shared/lanes.ts';
import type { Lane, LcuState, Matchup, MatchupPhase } from '../shared/types.ts';
import type { GuessOpponents } from './lcu/connector.ts';

/** What the live game says, in champion ids. */
export interface LiveView {
  /** Your in-game position (empty in customs and practice tool). */
  lane: Lane | null;
  enemies: number[];
  allies: number[];
  /** The enemy in your position. */
  positionOpponent: number | null;
}

/** One of your picks, and the phase it was made in. */
type Choice<T> = { value: T; phase: MatchupPhase };

export class MatchupSession {
  private lane: Choice<Lane> | null = null;
  private opponent: Choice<number | null> | null = null;
  private regions: string[] | null = null;
  private phase: MatchupPhase = 'idle';
  private detected: Lane | null = null;
  private saved: Lane | null = null;
  private guessed: { key: string; id: number | null } | null = null;

  constructor(private o: { savedLane: () => Lane | null; saveLane: (lane: Lane) => void; guess: GuessOpponents }) {}

  resolve(lcu: LcuState | null, live: LiveView | null): Matchup {
    const draft = lcu?.champSelect ?? null;
    const phase: MatchupPhase = live ? 'game' : draft ? 'draft' : 'idle';
    if (phase !== this.phase) {
      // Your picks last for a draft and its game, and never outlive them: they end when nothing is on (idle). Moving
      // between champ select and the game keeps them, both ways: the Live Client drops out during a reconnect, and a
      // pick made in game must survive that. An idle pick ends when a draft starts.
      const keep = <T>(p: Choice<T> | null) => (p && p.phase !== 'idle' && phase !== 'idle' ? p : null);
      this.lane = keep(this.lane);
      this.opponent = keep(this.opponent);
      this.phase = phase;
    }

    const draftLane = toLane(draft?.myPosition);
    this.detected = live?.lane ?? draftLane;
    this.saved ??= this.o.savedLane() ?? 'TOP';
    const lane = this.lane?.value ?? this.detected ?? this.saved;
    const laneFrom = this.lane ? 'manual' : live?.lane ? 'game' : draftLane ? 'draft' : 'saved';

    const enemies = live ? live.enemies : (draft?.enemies ?? []).map((e) => e.championId).filter(Boolean);
    const allies = live ? live.allies : (draft?.allies ?? []).map((a) => a.championId).filter((id) => id && id !== draft?.myChampionId);
    const draftPick = draft ? (draft.suggestedOpponents ? draft.suggestedOpponents[lane] : draft.suggestedOpponentId) ?? null : null;

    let opponentId: number | null = null;
    let opponentFrom: Matchup['opponentFrom'] = null;
    // A champ-select pick that is not in the game (the enemy picked someone else in the end) is dropped.
    if (this.opponent && live && this.opponent.phase === 'draft' && this.opponent.value !== null && !live.enemies.includes(this.opponent.value)) {
      this.opponent = null;
    }
    if (this.opponent) {
      opponentId = this.opponent.value;
      opponentFrom = 'manual';
    } else if (live) {
      if (live.positionOpponent !== null) { opponentId = live.positionOpponent; opponentFrom = 'game'; }
      else if (draftPick !== null && live.enemies.includes(draftPick)) { opponentId = draftPick; opponentFrom = 'draft'; }
      else {
        opponentId = this.guess(live.enemies, lane);
        opponentFrom = opponentId === null ? null : 'guess';
      }
    } else if (draftPick !== null) {
      opponentId = draftPick;
      opponentFrom = 'draft';
    }

    const regions = this.regions;
    return {
      phase, lane, laneFrom, detectedLane: this.detected, opponentId, opponentFrom, enemies, allies, regions,
      key: [lane, opponentId ?? '', enemies.join(','), allies.join(','), regions?.join(',') ?? ''].join('|'),
    };
  }

  /** The enemy Irelia faced most in this lane, once per game (it costs a database query). */
  private guess(enemies: number[], lane: Lane): number | null {
    if (!enemies.length) return null;
    const key = `${lane}|${enemies.join(',')}`;
    if (this.guessed?.key !== key) {
      let id: number | null = null;
      try { id = this.o.guess(enemies, [lane])[lane] ?? null; } catch (err) { console.warn('[matchup] opponent guess failed', err); }
      this.guessed = { key, id };
    }
    return this.guessed.id;
  }

  /**
   * You switched lanes: saved for next time, and while a draft or game assigns another lane it overrides that one until
   * they are over. Picking the assigned lane goes back to following it.
   */
  setLane(lane: Lane) {
    this.saved = lane;
    this.o.saveLane(lane);
    this.lane = this.detected && lane !== this.detected ? { value: lane, phase: this.phase } : null;
  }

  /** You picked an opponent (null: all matchups); undefined goes back to following the draft and the game. */
  setOpponent(id: number | null | undefined) {
    this.opponent = id === undefined ? null : { value: id, phase: this.phase };
  }

  /** Regions feeding the draft-adjusted build; null: all. Kept until you change them. */
  setRegions(regions: string[] | null) { this.regions = regions?.length ? regions : null; }
}
