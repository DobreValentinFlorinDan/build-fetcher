// The stored Irelia games as the draft model sees them: every decision a pilot made (runes, summoners, start, each
// completed item, boots, skills), the draft around it, and for items the state of the game when it was bought. The
// game-state model (state.ts) turns each into a win-probability-added: the result minus the chance to win from the
// state the decision was made in, so "ahead players buy greedier items" doesn't make greedy items look good.
import { toLane } from '../../shared/lanes.ts';
import type { Lane, Skill } from '../../shared/types.ts';
import { inventoryAt, type TimelineCapture } from '../capture.ts';
import type { IreliaGameRow, PilotSource } from '../db.ts';
import type { StaticData } from '../ddragon.ts';
import { derivePriority } from '../engine/recommend.ts';
import { isTier2Boots, validateGame } from '../engine/validator.ts';
import { draftContext, liveContext } from './context.ts';
import type { ChampionProfiles } from './profiles.ts';
import { swapDecisions } from './swaps.ts';
import { frameStates, stateFeatures, STATE_FEATURES, statesAt, trainStateModel, winProbabilityOf, type StateModel, type TimelineGame } from './state.ts';

const IRELIA = 39;

/** An Irelia game as db.modelGames reads it: the game, its match's platform and quality flags, and its timeline. */
export interface ModelGameRow extends IreliaGameRow { platform: string; timeline: string | null }
export interface ParticipantLite { match_id: string; participant_id: number; champion_id: number; team_id: number; position: string }

/** One item decision: what, when, the game state just before (state-model features) and the live context. */
export interface ItemChoice { item: number; t: number; x: number[] | null; live: number[] }

/**
 * A late-game swap decision (0.17.0, model/swaps.ts): at the shop visit at `t`, the item `held`, and what became of it
 * (`item`: swaps.ts KEEP, SELL or the item it was sold for), with the state and live context just before the visit.
 * `w`: the game's decision points share one game's weight. `p`: the win chance at the visit under each fold's state
 * model, once an evaluation has worked it out (it drops `x` then).
 */
export interface SwapChoice extends ItemChoice { held: number; w: number; p?: number[] }

export interface PreparedGame {
  matchId: string;
  platform: string;
  patch: string;
  created: number;
  pilot: string;
  source: PilotSource;
  tier: string | null;
  lp: number | null;
  mastery: number | null;
  lane: Lane;
  /** Grouped drafts (0.20.0, model/groups.ts): this game's draft group, in an evaluation trial with grouping on. */
  dg?: number;
  win: 0 | 1;
  opponent: number | null;
  enemies: number[];
  allies: number[];
  /** Draft context (context.ts DRAFT_DIMS), raw values. */
  draft: number[];
  primary: number; sub: number;
  perks: number[];        // keystone, primary rows 1-3, two secondary runes
  shards: number[];
  summoners: string;      // "4,12"
  start: string;          // "1055,2003"; "" when unknown
  maxFirst: Skill | null;
  early: string | null;   // first three skill-ups, "QEW"
  /** Completed items in the order they were finished (up to six). */
  items: ItemChoice[];
  boots: ItemChoice | null;
  /** Late-game swap decisions (0.17.0), in time order; none without a timeline. */
  swaps: SwapChoice[];
  /** State-model rows, one per timeline frame from 1:00 (STATE_FEATURES.length values each). */
  frames: Float32Array | null;
  /**
   * The state model's win probability at each of those frames, once the model is built: the best buy for your gold
   * (buy.ts) reads it, and the frames themselves go (they are 13 times its size).
   */
  pAt?: Float64Array | null;
  /** Completed items and runes the validator saw, for the personal layer. */
  purchases: [number, number][];
  kda: [number, number, number];
  duration: number;
  /** Irelia's total gold minus her lane opponent's at 10:00 and 15:00 (timeline), null when unknown. */
  lane10: number | null;
  lane15: number | null;
}

/** A prepared game scored with a state model: the win-probability-added of each decision. */
export interface ScoredGame {
  g: PreparedGame;
  /**
   * Decisions made before the game starts (runes, summoners, start, skills): result − chance at 0:00, the overall win
   * rate or (0.9.0, scoreGames `expected`) the game's own expected win rate before it started.
   */
  wpa0: number;
  itemWpa: number[];
  bootsWpa: number | null;
  /** Each swap decision's result − win chance just before its visit, as g.swaps (0.17.0; missing: none scored). */
  swapWpa?: number[];
}

export interface PrepareOptions {
  startingItemWindowMs: number;
  /** Where each game's state frames go (default: an array of its own). The model build uses frameSlabs. */
  frames?: (length: number) => Float32Array;
}

/**
 * Frame arrays handed out from large shared blocks (16 MB). The model build drops every game's frames once the state
 * model is fitted: tens of thousands of small arrays, each freed on its own, stay with the system allocator as freed
 * but unreturned memory (about 200 MB at 54,000 games); a few large blocks go back to the system when freed (0.9.1).
 */
export function frameSlabs(slab = 1 << 22): (length: number) => Float32Array {
  let block = new Float32Array(0), at = 0;
  return (length) => {
    if (length > slab) return new Float32Array(length);
    if (at + length > block.length) { block = new Float32Array(slab); at = 0; }
    at += length;
    return block.subarray(at - length, at);
  };
}

/** Skill maxed first (needs nine skill-ups), and the first three. */
export function skillChoices(order: string): { maxFirst: Skill | null; early: string | null } {
  const path = order.split('') as Skill[];
  return {
    maxFirst: path.length >= 9 ? derivePriority(path)[0] : null,
    early: path.length >= 3 ? order.slice(0, 3) : null,
  };
}

/**
 * Validated games (remakes, leavers, aborted games, removed runes and items are left out, as in the lane builds)
 * with their decisions and contexts. Games outside top and mid are skipped.
 */
/**
 * `rows` is read once, in order: the model build streams them from the database (Db.modelGamesIter), so each game's
 * raw timeline can be freed as soon as it is prepared instead of all of them being held at once.
 */
export function prepareGames(rows: Iterable<ModelGameRow>, participants: ParticipantLite[], data: StaticData,
  profiles: ChampionProfiles, o: PrepareOptions): PreparedGame[] {
  const byMatch = new Map<string, ParticipantLite[]>();
  for (const p of participants) {
    let list = byMatch.get(p.match_id);
    if (!list) { list = []; byMatch.set(p.match_id, list); }
    list.push(p);
  }
  const priceOf = (id: number) => data.items.get(id)?.goldTotal ?? 0;
  const out: PreparedGame[] = [];

  for (const row of rows) {
    const lane = toLane(row.position);
    if (!lane) continue;
    const v = validateGame(row, data, () => 1, o.startingItemWindowMs);
    if (!v.ok) continue;
    const game = v.game;
    const ps = byMatch.get(row.match_id) ?? [];
    const me = ps.find((p) => p.participant_id === row.participant_id) ?? ps.find((p) => p.champion_id === IRELIA);
    const pid = me?.participant_id ?? row.participant_id ?? null;
    const myTeam = me?.team_id ?? (pid !== null && pid > 5 ? 200 : 100);
    const enemyPs = ps.filter((p) => p.team_id !== myTeam);
    const allyPs = ps.filter((p) => p.team_id === myTeam && p !== me);
    const opponent = row.opponent_champion_id ?? null;
    const draft = draftContext(profiles,
      enemyPs.map((p) => ({ id: p.champion_id })),
      allyPs.map((p) => ({ id: p.champion_id })),
      opponent ? { id: opponent, position: row.position } : null);

    // Items: the completed ones in finishing order, each with the time it was first bought; boots likewise.
    const firstBuy = new Map<number, number>();
    for (const [id, ts] of game.purchases) if (!firstBuy.has(id)) firstBuy.set(id, ts);
    const itemTimes = game.completed.slice(0, 6).map((id) => ({ item: id, t: firstBuy.get(id)! }));
    const bootsTime = game.boots ? { item: game.boots, t: firstBuy.get(game.boots)! } : null;

    let frames: Float32Array | null = null;
    let lane10: number | null = null, lane15: number | null = null;
    let choices: ItemChoice[] = itemTimes.map((c) => ({ ...c, x: null, live: liveUnknown(c.t) }));
    let boots: ItemChoice | null = bootsTime ? { ...bootsTime, x: null, live: liveUnknown(bootsTime.t) } : null;
    let swaps: SwapChoice[] = [];
    const timeline = gameTimeline(row, ps, priceOf);
    if (timeline) {
      const fs = frameStates(timeline.tg);
      frames = o.frames ? o.frames(fs.length * STATE_FEATURES.length) : new Float32Array(fs.length * STATE_FEATURES.length);
      fs.forEach((st, i) => frames!.set(stateFeatures(st), i * STATE_FEATURES.length));
      // The state just before each purchase: the item itself is not owned yet.
      const decisions = [...itemTimes, ...(bootsTime ? [bootsTime] : [])];
      const moments = momentsAt(timeline, decisions.map((d) => Math.max(0, d.t - 1)), profiles, data);
      const made = decisions.map((d, i): ItemChoice => ({ ...d, x: moments[i].x, live: moments[i].live }));
      choices = made.slice(0, itemTimes.length);
      boots = bootsTime ? made[made.length - 1] : null;
      // Late-game swaps (0.17.0): the state just before each visit, once per visit.
      const decided = swapDecisions(timeline.tg.cap, timeline.tg.irelia, (id) => data.items.get(id));
      if (decided.length) {
        const times = [...new Set(decided.map((d) => d.t))];
        const at = momentsAt(timeline, times.map((t) => Math.max(0, t - 1)), profiles, data);
        const byTime = new Map(times.map((t, k) => [t, at[k]]));
        swaps = decided.map((d) => ({ item: d.option, t: d.t, held: d.held, w: d.w, x: byTime.get(d.t)!.x, live: byTime.get(d.t)!.live }));
      }
      lane10 = laneDiff(timeline.tg.cap, timeline.tg.irelia, timeline.tg.opponent, 10)?.gold ?? null;
      lane15 = laneDiff(timeline.tg.cap, timeline.tg.irelia, timeline.tg.opponent, 15)?.gold ?? null;
    }

    const { maxFirst, early } = skillChoices(row.skill_order);
    out.push({
      matchId: row.match_id, platform: row.platform, patch: row.patch, created: row.created_at, pilot: row.puuid,
      source: (row.pilot_source ?? 'otp') as PilotSource, tier: row.pilot_tier ?? null, lp: row.pilot_lp ?? null,
      mastery: row.pilot_mastery ?? null, lane, win: row.win ? 1 : 0, opponent,
      enemies: enemyPs.map((p) => p.champion_id), allies: allyPs.map((p) => p.champion_id), draft,
      primary: row.primary_style, sub: row.sub_style, perks: game.perks, shards: game.statPerks,
      summoners: [row.summoner1, row.summoner2].sort((a, b) => a - b).join(','), start: game.start.join(','),
      maxFirst, early, items: choices, boots: boots && isTier2Boots(data.items.get(boots.item)) ? boots : null, swaps, frames,
      purchases: game.purchases, kda: [row.kills, row.deaths, row.assists], duration: row.game_duration, lane10, lane15,
    });
  }
  return out;
}

/** `me`'s total gold and CS minus `opp`'s at `minute` (the first frame at or after it); null when either is unknown. */
export function laneDiff(cap: TimelineCapture, me: number, opp: number | null, minute: number) {
  if (opp === null) return null;
  const i = cap.t.findIndex((t) => t >= minute * 60_000);
  if (i < 0) return null;
  const a = cap.frames[String(me)]?.[i], b = cap.frames[String(opp)]?.[i];
  return a && b ? { gold: a[0] - b[0], cs: a[4] - b[4] } : null;
}

/** A stored game's timeline seen from Irelia, and whose participant ids are whose. Null without a usable timeline. */
export interface GameTimeline { tg: TimelineGame; enemies: { pid: number; championId: number }[] }

export function gameTimeline(row: ModelGameRow, ps: ParticipantLite[], priceOf: (id: number) => number): GameTimeline | null {
  if (!row.timeline) return null;
  const me = ps.find((p) => p.participant_id === row.participant_id) ?? ps.find((p) => p.champion_id === IRELIA);
  const pid = me?.participant_id ?? row.participant_id ?? null;
  if (pid === null) return null;
  let cap: TimelineCapture;
  try { cap = JSON.parse(row.timeline) as TimelineCapture; } catch { return null; }
  if (!cap.frames[String(pid)]) return null;
  const myTeam = me?.team_id ?? (pid > 5 ? 200 : 100);
  // Each participant's team, looked up once (the replay asks for it at every frame).
  const teams = new Map<number, number>();
  for (const p of ps) if (!teams.has(p.participant_id)) teams.set(p.participant_id, p.team_id);
  const teamOf = (id: number) => teams.get(id) ?? (id > 5 ? 200 : 100);
  const enemyPs = ps.filter((p) => p.team_id !== myTeam);
  const oppP = row.opponent_champion_id ? enemyPs.find((p) => p.champion_id === row.opponent_champion_id) : undefined;
  return {
    tg: { cap, irelia: pid, opponent: oppP?.participant_id ?? null, teamOf, priceOf },
    enemies: enemyPs.map((p) => ({ pid: p.participant_id, championId: p.champion_id })),
  };
}

/** The state-model features, the live context and Irelia's inventory at each of `times` (ms) of a stored game. */
export function momentsAt(t: GameTimeline, times: number[], profiles: ChampionProfiles, data: StaticData) {
  // The inventories at each time, from the state's own replay (0.15.0: before, one more replay per enemy and moment).
  const enemyItems: number[][][] = new Array(times.length), owned: number[][] = new Array(times.length);
  const states = statesAt(t.tg, times, (i, inv) => {
    enemyItems[i] = t.enemies.map((e) => [...(inv.get(e.pid) ?? [])]);
    owned[i] = [...(inv.get(t.tg.irelia) ?? [])];
  });
  return times.map((at, i) => {
    const s = states[i];
    const enemies = t.enemies.map((e, j) => ({ profile: profiles.get(e.championId), items: enemyItems[i][j] }));
    return {
      x: stateFeatures(s),
      live: liveContext(enemies, (id) => data.items.get(id), {
        laneGold: s.me && s.opponent ? s.me.inventory - s.opponent.inventory : NaN,
        teamGold: s.us.inventory - s.them.inventory, kills: s.us.kills - s.them.kills, minute: at / 60_000,
      }),
      owned: owned[i],
    };
  });
}

/** The live context of a purchase when the game has no timeline: only the minute is known. */
const liveUnknown = (t: number) => [NaN, NaN, NaN, NaN, NaN, NaN, NaN, t / 60_000];

export const STATE_MAX_ROWS = 60_000;

/** The frames a state model is fitted on: views into the games' own frames (nothing copied), and how many games. */
export interface StateRows { rows: { x: ArrayLike<number>; win: number }[]; games: number }

/**
 * The frames of `games` to fit the state model on (the user's own games never: they would tilt the high-elo model);
 * null when there are too few. Above `maxRows` frames, an even subsample keeps fitting fast; every frame of a game
 * carries its result. The same rows serve every penalty the evaluation tries.
 */
export function stateRows(games: PreparedGame[], maxRows = STATE_MAX_ROWS): StateRows | null {
  const width = STATE_FEATURES.length;
  const withFrames = games.filter((g) => g.frames && g.source !== 'me');
  const total = withFrames.reduce((s, g) => s + g.frames!.length / width, 0);
  if (withFrames.length < 20 || total < 200) return null;
  const stride = Math.max(1, Math.ceil(total / maxRows));
  const rows: StateRows['rows'] = [];
  let i = 0;
  for (const g of withFrames) {
    for (let f = 0; f < g.frames!.length / width; f++, i++) {
      if (i % stride) continue;
      rows.push({ x: g.frames!.subarray(f * width, (f + 1) * width), win: g.win });
    }
  }
  return { rows, games: withFrames.length };
}

/** Fits the state model on the timelines of `games` (stateRows); null when there are too few. */
export function trainState(games: PreparedGame[], lambda?: number, maxRows = STATE_MAX_ROWS): StateModel | null {
  const r = stateRows(games, maxRows);
  return r && trainStateModel(r.rows, r.games, lambda);
}

/**
 * Win-probability-added of every decision. Without a state model (too few timelines) it is the result minus 50 %.
 * `expected` (aligned with `games`; 0.9.0, model/expected.ts): each game's chance to win before it started, which
 * pre-game decisions are then scored against instead of the overall win rate; NaN for a game keeps the overall rate.
 */
export function scoreGames(games: PreparedGame[], model: StateModel | null, expected?: ArrayLike<number>): ScoredGame[] {
  const base = model?.base ?? 0.5;
  const p = (x: number[] | null) => (model && x ? winProbabilityOf(model, x) : base);
  return games.map((g, i) => ({
    g,
    wpa0: g.win - (expected && Number.isFinite(expected[i]) ? expected[i] : base),
    itemWpa: g.items.map((c) => g.win - p(c.x)),
    bootsWpa: g.boots ? g.win - p(g.boots.x) : null,
    swapWpa: (g.swaps ?? []).map((c) => g.win - p(c.x)),
  }));
}
