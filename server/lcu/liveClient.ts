import { EventEmitter } from 'node:events';
import type { LivePerformance, PerformanceSample, PlayerPerformance } from '../../shared/types.ts';
import { identify, type AllGameData, type LiveApiEvent, type LiveApiPlayer } from '../engine/adaptive.ts';
import { loopbackJson } from './https.ts';

const LIVE_PORT = 2999;

// ================================================================== gold model
//
// The Live Client Data API gives your own unspent gold but nobody else's (fog of war), so the
// opponent's net worth is estimated from what the scoreboard shows anyway:
//   visible inventory value (Data Dragon total cost)                      exact
//   + income model: start + passive + CS + kill/assist bounties + objectives   estimate
// The income model is calibrated on *you* every tick (your real net worth / your modelled income),
// which absorbs consumables, sold items and bounty rules the model does not know about.

/** Summoner's Rift income constants. Tunable; calibration corrects systematic error. */
export const GOLD = {
  start: 500,
  passiveFrom: 65,        // s: passive income starts at 1:05
  passivePerSec: 2.04,    // 20.4 gold every 10 s
  perCs: 20,              // wave average: 3 melee (21g) + 3 caster (14g) + a cannon (60g+) every other wave
  kill: 300,
  firstBlood: 100,
  assistShare: 0.5,       // assisters split half of the victim's bounty
  streakBonus: 100,       // per kill beyond the first in the victim's current streak (shutdown)
  deathStreakFactor: 0.85,  // each consecutive death without a kill lowers the victim's bounty
  minBounty: 100,
  maxBounty: 1000,
  turret: 250,            // local gold, split between participating champions
  inhibitor: 50,
  baron: 300,             // every member of the killing team
};

const CONTROL_WARD = 2055;
const TRINKETS = new Set([3340, 3363, 3364]);  // Stealth Ward, Farsight Alteration, Oracle Lens
const HISTORY_EVERY_S = 15;
const HISTORY_MAX = 240;    // one hour of samples

const clamp = (x: number, lo: number, hi: number) => Math.min(hi, Math.max(lo, x));
const norm = (s: string | undefined) => (s ?? '').trim().toLowerCase();
const playerKey = (p: LiveApiPlayer) => p.riotId ?? p.summonerName ?? p.championName;

/**
 * Event names (KillerName, Assisters, ...) may be a Riot ID game name, a full "name#tag",
 * a legacy summoner name or, for bots, the champion name. Map every variant to the player.
 */
export function nameIndex(players: LiveApiPlayer[]) {
  const index = new Map<string, LiveApiPlayer>();
  for (const p of players) {
    for (const n of [p.riotIdGameName, p.riotId, p.riotId?.split('#')[0], p.summonerName, p.summonerName?.split('#')[0], p.championName]) {
      if (n && !index.has(norm(n))) index.set(norm(n), p);
    }
  }
  return (name: string | undefined) => index.get(norm(name));
}

/**
 * Gold from kills, assists and objectives per player key, replayed from the event feed (which
 * announces every kill, fogged or not). Returns null when the names cannot be resolved; callers
 * then fall back to scoreboard KDA.
 */
export function eventIncome(players: LiveApiPlayer[], events: LiveApiEvent[]): Map<string, number> | null {
  const find = nameIndex(players);
  const gold = new Map<string, number>(players.map((p) => [playerKey(p), 0]));
  const add = (p: LiveApiPlayer | undefined, g: number) => { if (p) gold.set(playerKey(p), (gold.get(playerKey(p)) ?? 0) + g); };
  const killStreak = new Map<string, number>();
  const deathStreak = new Map<string, number>();
  let kills = 0, resolved = 0;

  for (const e of [...events].sort((a, b) => a.EventTime - b.EventTime)) {
    const killer = find(e.KillerName);
    const assisters = (e.Assisters ?? []).map(find).filter((p): p is LiveApiPlayer => !!p);
    switch (e.EventName) {
      case 'ChampionKill': {
        kills++;
        const victim = find(e.VictimName);
        if (!victim) break;
        resolved++;
        const vk = playerKey(victim);
        const streak = killStreak.get(vk) ?? 0;
        const deaths = deathStreak.get(vk) ?? 0;
        const bounty = clamp((GOLD.kill + GOLD.streakBonus * Math.max(0, streak - 1)) * GOLD.deathStreakFactor ** deaths,
          GOLD.minBounty, GOLD.maxBounty);
        if (killer && killer.team !== victim.team) {
          add(killer, bounty);
          const kk = playerKey(killer);
          killStreak.set(kk, (killStreak.get(kk) ?? 0) + 1);
          deathStreak.set(kk, 0);
        }
        for (const a of assisters) add(a, (bounty * GOLD.assistShare) / assisters.length);
        killStreak.set(vk, 0);
        deathStreak.set(vk, deaths + 1);
        break;
      }
      case 'FirstBlood': add(find(e.Recipient), GOLD.firstBlood); break;
      case 'TurretKilled': {
        const team = [killer, ...assisters].filter((p): p is LiveApiPlayer => !!p);
        for (const p of team) add(p, GOLD.turret / team.length);
        break;
      }
      case 'InhibKilled': for (const p of [killer, ...assisters]) add(p, GOLD.inhibitor); break;
      case 'BaronKill': {
        const team = killer?.team ?? assisters[0]?.team;
        if (team) for (const p of players) if (p.team === team) add(p, GOLD.baron);
        break;
      }
    }
  }
  return kills > 0 && resolved / kills < 0.8 ? null : gold;
}

/** Scoreboard fallback when kill events cannot be attributed: flat bounties, ~2 assisters per kill. */
const kdaIncome = (p: LiveApiPlayer) => p.scores.kills * GOLD.kill + (p.scores.assists * GOLD.kill * GOLD.assistShare) / 2;

export function modelIncome(p: LiveApiPlayer, gameTime: number, fromEvents: Map<string, number> | null) {
  const passive = Math.max(0, gameTime - GOLD.passiveFrom) * GOLD.passivePerSec;
  const combat = fromEvents ? fromEvents.get(playerKey(p)) ?? 0 : kdaIncome(p);
  return GOLD.start + passive + p.scores.creepScore * GOLD.perCs + combat;
}

export const inventoryValue = (p: LiveApiPlayer, priceOf: (itemId: number) => number) =>
  p.items.reduce((s, it) => s + priceOf(it.itemID) * Math.max(1, it.count ?? 1), 0);

// ================================================================== tracker

/** Turns successive allgamedata snapshots into CS / vision / gold-difference metrics plus a short history. */
export class PerformanceTracker {
  private history: PerformanceSample[] = [];
  private lastSampleT = -Infinity;
  private lastGameTime = 0;
  private csSeen = new Set<number>();
  private opponentName: string | null | undefined = undefined;

  reset() {
    this.history = [];
    this.lastSampleT = -Infinity;
    this.lastGameTime = 0;
    this.csSeen.clear();
    this.opponentName = undefined;
  }

  /** `opponentName`: who you face (identify in engine/adaptive.ts); without it, the enemy in your position. */
  update(raw: AllGameData, priceOf: (itemId: number) => number, opponentName?: string | null): LivePerformance | null {
    const t = raw.gameData.gameTime;
    if (t + 5 < this.lastGameTime) this.reset();   // a new game started without an 'ended' in between
    this.lastGameTime = t;

    const { me, opponent } = identify(raw, opponentName);
    if (!me) return null;
    // Another lane opponent (picked in the dashboard, or found once positions show): the history compared you with
    // someone else, so it starts again.
    const against = opponent?.championName ?? null;
    if (this.opponentName !== undefined && against !== this.opponentName) { this.history = []; this.lastSampleT = -Infinity; }
    this.opponentName = against;
    const minutes = Math.max(t / 60, 1 / 60);
    const income = eventIncome(raw.allPlayers, raw.events?.Events ?? []);

    for (const p of raw.allPlayers) if (p.scores.creepScore > 0) this.csSeen.add(p.scores.creepScore);
    const csGranularity = this.csSeen.size >= 5 && [...this.csSeen].every((cs) => cs % 10 === 0) ? 10 : 1;

    const base = (p: LiveApiPlayer) => {
      const vision = p.scores.wardScore ?? 0;
      return {
        championName: p.championName,
        cs: p.scores.creepScore,
        csPerMin: round1(p.scores.creepScore / minutes),
        visionScore: round1(vision),
        visionPerMin: Math.round((vision / minutes) * 100) / 100,
        controlWards: p.items.filter((i) => i.itemID === CONTROL_WARD).reduce((s, i) => s + Math.max(1, i.count ?? 1), 0),
        trinket: p.items.find((i) => TRINKETS.has(i.itemID))?.itemID ?? null,
        itemGold: Math.round(inventoryValue(p, priceOf)),
        modelGold: Math.round(modelIncome(p, t, income)),
      };
    };

    // You: exact net worth. Calibration = how far the model is off for you, faded in after the first minute.
    const mb = base(me);
    const myNetWorth = mb.itemGold + Math.floor(raw.activePlayer.currentGold);
    const ratio = mb.modelGold > 0 ? myNetWorth / mb.modelGold : 1;
    const calibration = Math.round(clamp(1 + (ratio - 1) * clamp((t - 60) / 300, 0, 1), 0.75, 1.15) * 1000) / 1000;
    const mePerf: PlayerPerformance = { ...mb, unspentGold: Math.floor(raw.activePlayer.currentGold), netWorth: myNetWorth };

    let opp: PlayerPerformance | null = null;
    let goldInfo: LivePerformance['gold'] = { confidence: 'high', uncertainty: 0, calibration };
    if (opponent) {
      const ob = base(opponent);
      const netWorth = Math.round(Math.max(ob.itemGold, ob.modelGold * calibration));
      const unspent = netWorth - ob.itemGold;
      opp = { ...ob, unspentGold: unspent, netWorth };
      goldInfo = {
        confidence: unspent <= 350 ? 'high' : unspent <= 900 ? 'medium' : 'low',
        uncertainty: Math.round((100 + 0.04 * ob.modelGold + 0.5 * unspent) / 10) * 10,
        calibration,
      };
    }

    const diff = opp ? {
      cs: mePerf.cs - opp.cs,
      vision: round1(mePerf.visionScore - opp.visionScore),
      gold: mePerf.netWorth - opp.netWorth,
      itemGold: mePerf.itemGold - opp.itemGold,
    } : null;

    if (diff && t - this.lastSampleT >= HISTORY_EVERY_S) {
      this.lastSampleT = t;
      this.history.push({ t: Math.round(t), gold: diff.gold, cs: diff.cs, vision: diff.vision });
      if (this.history.length > HISTORY_MAX) this.history.shift();
    }

    return { gameTime: t, me: mePerf, opponent: opp, diff, gold: goldInfo, csGranularity, history: [...this.history] };
  }
}

const round1 = (x: number) => Math.round(x * 10) / 10;

// ================================================================== poller

/**
 * Polls the in-game Live Client Data API (https://127.0.0.1:2999/liveclientdata/allgamedata).
 * Emits 'data' (raw snapshot, LivePerformance | null) every `activeMs` while a game runs and
 * 'ended' when it stops answering (END_AFTER_FAILURES polls in a row). Requests never overlap: the next poll is scheduled after the
 * previous one settles.
 */
/** A running game ends after this many failed polls in a row: one slow or dropped answer is not the end of a game. */
export const END_AFTER_FAILURES = 3;

export class LiveClientPoller extends EventEmitter {
  private timer: NodeJS.Timeout | null = null;
  private running = false;
  private active = false;
  private failures = 0;
  private tracker = new PerformanceTracker();

  constructor(private priceOf: (itemId: number) => number, private intervals = { activeMs: 1000, idleMs: 3000 },
    private opponentOf: (raw: AllGameData) => string | null | undefined = () => undefined) {
    super();
  }

  start() {
    if (this.running) return;
    this.running = true;
    const poll = async () => {
      try {
        const raw = await loopbackJson<AllGameData>(LIVE_PORT, '/liveclientdata/allgamedata');
        if (!this.running) return;
        if (!raw.allPlayers?.length) throw new Error('loading');
        this.active = true;
        this.failures = 0;
        this.emit('data', raw, this.tracker.update(raw, this.priceOf, this.opponentOf(raw)));
      } catch {
        if (++this.failures >= END_AFTER_FAILURES) this.end();
      }
      if (this.running) this.timer = setTimeout(poll, this.active ? this.intervals.activeMs : this.intervals.idleMs);
    };
    void poll();
  }

  stop() {
    this.running = false;
    if (this.timer) clearTimeout(this.timer);
    this.timer = null;
    this.end();
  }

  private end() {
    this.failures = 0;
    if (!this.active) return;
    this.active = false;
    this.tracker.reset();
    this.emit('ended');
  }
}
