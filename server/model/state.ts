// B2 game-state model: the chance Irelia's team wins from a moment of the game, using only what the Live Client
// Data API also shows in game (scoreboard items, levels, CS, kills, and the objective events). One feature function
// serves both sides: a stored timeline replayed offline (training, and the state at each purchase for the
// win-probability-added of builds) and the live game. docs/BUILD-SOURCING.md §9.2.
import { ITEM_EVENT, type TimelineCapture } from '../capture.ts';
import type { AllGameData, LiveApiPlayer } from '../engine/adaptive.ts';
import { fitLogistic, predictLogistic } from './logistic.ts';

export interface TeamState {
  inventory: number;   // value of every member's items (Data Dragon total cost)
  kills: number;
  cs: number;          // total minions and monsters
  level: number;       // average level
  towers: number; inhibitors: number;
  dragons: number; elders: number; barons: number;
  grubsHeralds: number;
}
export interface LaneState { inventory: number; level: number; cs: number }

/** A moment of a game from Irelia's team's side ("us"). */
export interface GameStateInput {
  t: number;                    // seconds
  us: TeamState; them: TeamState;
  me: LaneState | null;         // Irelia
  opponent: LaneState | null;   // her lane opponent (null when unknown)
}

export const STATE_DIFFS = [
  'team items (k gold)', 'kills', 'team CS (×10)', 'average level', 'towers', 'inhibitors', 'dragons', 'elder dragons',
  'barons', 'grubs and heralds', 'lane items (k gold)', 'lane level', 'lane CS (×10)',
] as const;
/** Coefficient names: intercept, time, each difference, and each difference × time. */
export const STATE_FEATURES = ['intercept', 'time', ...STATE_DIFFS, ...STATE_DIFFS.map((d) => `${d} × time`)];

/** Time as a model input: 1 at 30:00, held from 40:00 on. */
const tau = (t: number) => Math.min(t, 2400) / 1800;

export function stateFeatures(s: GameStateInput): number[] {
  const lane = s.me && s.opponent;
  const d = [
    (s.us.inventory - s.them.inventory) / 1000, s.us.kills - s.them.kills, (s.us.cs - s.them.cs) / 10, s.us.level - s.them.level,
    s.us.towers - s.them.towers, s.us.inhibitors - s.them.inhibitors, s.us.dragons - s.them.dragons, s.us.elders - s.them.elders,
    s.us.barons - s.them.barons, s.us.grubsHeralds - s.them.grubsHeralds,
    lane ? (s.me!.inventory - s.opponent!.inventory) / 1000 : 0, lane ? s.me!.level - s.opponent!.level : 0,
    lane ? (s.me!.cs - s.opponent!.cs) / 10 : 0,
  ];
  const time = tau(s.t);
  return [1, time, ...d, ...d.map((v) => v * time)];
}

const emptyTeam = (): TeamState => ({ inventory: 0, kills: 0, cs: 0, level: 0, towers: 0, inhibitors: 0, dragons: 0, elders: 0, barons: 0, grubsHeralds: 0 });

// ------------------------------------------------------------------------------------------- timelines

/** A stored game seen from Irelia: whose participant ids are whose (from `participants`, else 1-5 is team 100). */
export interface TimelineGame {
  cap: TimelineCapture;
  irelia: number;                 // participant id
  opponent: number | null;        // lane opponent's participant id
  teamOf: (pid: number) => number;
  priceOf: (itemId: number) => number;
}

const OBJECTIVE_FIELD: Partial<Record<string, keyof TeamState>> = {
  TOWER: 'towers', INHIBITOR: 'inhibitors', DRAGON: 'dragons', ELDER_DRAGON: 'elders', BARON_NASHOR: 'barons',
  RIFTHERALD: 'grubsHeralds', HORDE: 'grubsHeralds',
};

/**
 * The game state at each of `times` (ms, any order), replaying the timeline once: inventories from the item events
 * (events at exactly t included), levels and CS from the last frame at or before t, kills and objectives up to t.
 * `seen` (0.15.0): called at each of `times` with every participant's inventory then (read it there, it changes
 * after), so the live context needs no replay of its own.
 *
 * 0.15.0, the same numbers in less time (a model build replays every stored game twice): each participant's team and
 * frames looked up once, an inventory's value summed again only when it changed, and the frame index carried from one
 * time to the next instead of searched from the start.
 */
export function statesAt(g: TimelineGame, times: number[], seen?: (i: number, inv: ReadonlyMap<number, number[]>) => void): GameStateInput[] {
  const { cap } = g;
  const order = times.map((t, i) => ({ t, i })).sort((a, b) => a.t - b.t);
  const out: GameStateInput[] = new Array(times.length);
  const inv = new Map<number, number[]>();
  const counts = new Map<number, TeamState>([[100, emptyTeam()], [200, emptyTeam()]]);
  let e = 0, k = 0, o = 0, f = 0;
  const myTeam = g.teamOf(g.irelia);
  const people = Object.keys(cap.frames).map((key) => {
    const pid = Number(key);
    return { pid, frames: cap.frames[key], ours: g.teamOf(pid) === myTeam };
  });
  // Each inventory's value, summed again (in the same order, so to the same number) only after it changed.
  const values = new Map<number, number>();
  const value = (pid: number) => {
    let v = values.get(pid);
    if (v === undefined) { v = (inv.get(pid) ?? []).reduce((s, id) => s + g.priceOf(id), 0); values.set(pid, v); }
    return v;
  };
  for (const { t, i } of order) {
    for (; e < cap.items.length && cap.items[e][0] <= t; e++) {
      const [, pid, kind, item, after] = cap.items[e];
      let list = inv.get(pid);
      if (!list) { list = []; inv.set(pid, list); }
      const remove = (id: number) => { const at = list!.lastIndexOf(id); if (at >= 0) list!.splice(at, 1); };
      if (kind === ITEM_EVENT.BUY) list.push(item);
      else if (kind === ITEM_EVENT.SELL || kind === ITEM_EVENT.DESTROY) remove(item);
      else if (kind === ITEM_EVENT.UNDO) { if (item) remove(item); if (after) list.push(after); }
      values.delete(pid);
    }
    for (; k < cap.kills.length && cap.kills[k][0] <= t; k++) {
      const killer = cap.kills[k][1];
      if (killer) counts.get(g.teamOf(killer))!.kills++;
    }
    for (; o < cap.objectives.length && cap.objectives[o][0] <= t; o++) {
      const [, team, kind] = cap.objectives[o];
      const field = OBJECTIVE_FIELD[kind];
      const c = counts.get(team);
      if (field && c) c[field]++;
    }
    while (f + 1 < cap.t.length && cap.t[f + 1] <= t) f++;
    const us = { ...counts.get(myTeam)! }, them = { ...counts.get(myTeam === 100 ? 200 : 100)! };
    const lane = (pid: number | null): LaneState | null => {
      if (pid === null) return null;
      const fr = cap.frames[String(pid)]?.[f];
      return fr ? { inventory: value(pid), level: fr[3], cs: fr[4] } : null;
    };
    let nUs = 0, nThem = 0;
    for (const p of people) {
      const fr = p.frames[f];
      if (!fr) continue;
      const side = p.ours ? us : them;
      side.inventory += value(p.pid);
      side.cs += fr[4];
      side.level += fr[3];
      if (side === us) nUs++; else nThem++;
    }
    if (nUs) us.level /= nUs;
    if (nThem) them.level /= nThem;
    out[i] = { t: t / 1000, us, them, me: lane(g.irelia), opponent: lane(g.opponent) };
    seen?.(i, inv);
  }
  return out;
}

/** Training rows of one game: its state at every frame from 1:00 on. */
export function frameStates(g: TimelineGame): GameStateInput[] {
  return statesAt(g, g.cap.t.filter((t) => t >= 60_000));
}

// ------------------------------------------------------------------------------------------- live game

const LIVE_TEAM: Record<string, number> = { ORDER: 100, CHAOS: 200 };

/** "Turret_T2_C_05_A" / "Barracks_T1_R1": the team that owned the building (T1 = ORDER). */
function buildingOwner(name: string | undefined): number | null {
  const m = /_T([12])_/.exec(name ?? '');
  return m ? (m[1] === '1' ? 100 : 200) : null;
}

/**
 * The live game's state from the Live Client Data API: scoreboard items, levels, CS and kills, and the objectives in
 * its event feed. `teamOfName` resolves a KillerName to a team (lcu/liveClient.ts nameIndex).
 */
export function stateFromLive(raw: AllGameData, me: LiveApiPlayer, opponent: LiveApiPlayer | undefined,
  priceOf: (itemId: number) => number, teamOfName: (name: string | undefined) => number | null): GameStateInput {
  const myTeam = LIVE_TEAM[me.team];
  const teams = new Map<number, TeamState>([[100, emptyTeam()], [200, emptyTeam()]]);
  const value = (p: LiveApiPlayer) => p.items.reduce((s, it) => s + priceOf(it.itemID) * Math.max(1, it.count ?? 1), 0);
  const sizes = new Map<number, number>();
  for (const p of raw.allPlayers) {
    const team = teams.get(LIVE_TEAM[p.team]);
    if (!team) continue;
    team.inventory += value(p);
    team.kills += p.scores.kills;
    team.cs += p.scores.creepScore;
    team.level += p.level;
    sizes.set(LIVE_TEAM[p.team], (sizes.get(LIVE_TEAM[p.team]) ?? 0) + 1);
  }
  for (const [id, t] of teams) t.level /= sizes.get(id) || 1;
  // Objectives from the event feed (DragonKill, BaronKill, HeraldKill, HordeKill, TurretKilled, InhibKilled); other events are ignored.
  for (const ev of raw.events?.Events ?? []) {
    let team: number | null = null, field: keyof TeamState | null = null;
    switch (ev.EventName) {
      case 'DragonKill': team = teamOfName(ev.KillerName); field = ev.DragonType === 'Elder' ? 'elders' : 'dragons'; break;
      case 'BaronKill': team = teamOfName(ev.KillerName); field = 'barons'; break;
      case 'HeraldKill': case 'HordeKill': team = teamOfName(ev.KillerName); field = 'grubsHeralds'; break;
      case 'TurretKilled': { const owner = buildingOwner(ev.TurretKilled); team = owner ? 300 - owner : teamOfName(ev.KillerName); field = 'towers'; break; }
      case 'InhibKilled': { const owner = buildingOwner(ev.InhibKilled); team = owner ? 300 - owner : teamOfName(ev.KillerName); field = 'inhibitors'; break; }
    }
    if (team && field) teams.get(team)![field]++;
  }
  const lane = (p: LiveApiPlayer | undefined): LaneState | null => (p ? { inventory: value(p), level: p.level, cs: p.scores.creepScore } : null);
  return {
    t: raw.gameData.gameTime, us: teams.get(myTeam)!, them: teams.get(300 - myTeam)!,
    me: lane(me), opponent: lane(opponent),
  };
}

// ------------------------------------------------------------------------------------------- the model

export interface StateModel {
  version: 1;
  features: string[];
  beta: number[];
  lambda: number;
  rows: number;
  games: number;
  /** Chance Irelia's team wins at 0:00, before anything happened. */
  base: number;
}

export const STATE_LAMBDA = 1;

/** Fits the state model on `rows` (one per game frame). Every row of a game carries that game's result. */
export function trainStateModel(rows: { x: ArrayLike<number>; win: number }[], games: number, lambda = STATE_LAMBDA): StateModel {
  const fit = fitLogistic(rows.map((r) => r.x), rows.map((r) => r.win), { lambda });
  const model: StateModel = { version: 1, features: STATE_FEATURES, beta: fit.beta, lambda, rows: rows.length, games, base: 0 };
  model.base = winProbability(model, { t: 0, us: emptyTeam(), them: emptyTeam(), me: null, opponent: null });
  return model;
}

export const winProbability = (m: Pick<StateModel, 'beta'>, s: GameStateInput) => predictLogistic(m.beta, stateFeatures(s));
/** The same from features already computed (stateFeatures). */
export const winProbabilityOf = (m: Pick<StateModel, 'beta'>, x: ArrayLike<number>) => predictLogistic(m.beta, x);
