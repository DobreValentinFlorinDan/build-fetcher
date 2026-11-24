// Synthetic match-v5 matches and timelines in Riot's shapes, and an in-memory Riot API, so the sourcing,
// capture, backfill and audit tests run offline. Ten players: participants 1-5 are team 100 and 6-10 team 200,
// each in TOP, JUNGLE, MIDDLE, BOTTOM, UTILITY order.
import type { MatchDto, ParticipantDto, TimelineDto, TimelineEvent, TimelineFrameDto } from '../engine/parseMatch.ts';
import type { LeagueEntry, MasteryDto, MatchIdQuery, RankedEntry, RiotApi } from '../riot/client.ts';
import type { Division, Platform, Tier } from '../riot/routing.ts';

export const IRELIA = 39;
export const POSITIONS = ['TOP', 'JUNGLE', 'MIDDLE', 'BOTTOM', 'UTILITY'] as const;
/** Irelia top vs Darius by default; Syndra is the enemy mid. */
export const DEFAULT_CHAMPIONS = [IRELIA, 64, 103, 222, 412, 122, 121, 134, 21, 89];

/** Where each role stands during laning (Summoner's Rift is 0..15000 on both axes). */
const LANE_SPOT: Record<string, [number, number]> = {
  TOP: [1500, 12500], JUNGLE: [4000, 8000], MIDDLE: [7400, 7400], BOTTOM: [12500, 1500], UTILITY: [12000, 2000],
};

export interface MatchSpec {
  id: string;
  version?: string;           // gameVersion, e.g. "16.19.712.4471"
  creation?: number;          // epoch ms
  minutes?: number;           // game length
  champions?: number[];       // 10 champion ids, participant 1..10
  puuids?: string[];          // 10 PUUIDs
  positions?: string[];       // 10 teamPositions (default: POSITIONS twice)
  winner?: 100 | 200;
  remakeVote?: boolean;       // gameEndedInEarlySurrender
  queue?: number;
}

export function makeMatch(s: MatchSpec): MatchDto {
  const champions = s.champions ?? DEFAULT_CHAMPIONS;
  const puuids = s.puuids ?? champions.map((_, i) => `${s.id}-p${i + 1}`);
  const positions = s.positions ?? [...POSITIONS, ...POSITIONS];
  const minutes = s.minutes ?? 30;
  const winner = s.winner ?? 100;
  const participants: ParticipantDto[] = champions.map((championId, i) => {
    const teamId = i < 5 ? 100 : 200;
    return {
      puuid: puuids[i], participantId: i + 1, championId, teamId, teamPosition: positions[i], win: teamId === winner,
      summoner1Id: 4, summoner2Id: 12, kills: 3, deaths: 2, assists: 5,
      item0: 3153, item1: 3078, item2: 3047, item3: 0, item4: 0, item5: 0, item6: 3340,
      perks: {
        statPerks: { offense: 5005, flex: 5008, defense: 5011 },
        styles: [
          { description: 'primaryStyle', style: 8000, selections: [{ perk: 8010 }, { perk: 9111 }, { perk: 9104 }, { perk: 8299 }] },
          { description: 'subStyle', style: 8400, selections: [{ perk: 8444 }, { perk: 8242 }] },
        ],
      },
      champLevel: 16, goldEarned: 12_000, totalMinionsKilled: 200, neutralMinionsKilled: 10,
      physicalDamageDealtToChampions: championId === 134 ? 1000 : 15_000, magicDamageDealtToChampions: championId === 134 ? 25_000 : 2000,
      trueDamageDealtToChampions: 1500, totalDamageTaken: 20_000, damageSelfMitigated: 12_000,
      totalHeal: 3000, totalHealsOnTeammates: championId === 89 ? 4000 : 0, totalDamageShieldedOnTeammates: 0,
      timeCCingOthers: 20, totalTimeCCDealt: 90, timePlayed: minutes * 60,
      gameEndedInEarlySurrender: s.remakeVote ?? false, gameEndedInSurrender: false,
      challenges: { damagePerMinute: 600, kda: 4, soloKills: 1, laningPhaseGoldExpAdvantage: 1, unrelatedCosmetic: 3 },
    };
  });
  return {
    metadata: { matchId: s.id, participants: puuids },
    info: {
      gameVersion: s.version ?? '16.19.712.4471', queueId: s.queue ?? 420, gameCreation: s.creation ?? Date.UTC(2026, 8, 25),
      gameDuration: minutes * 60, endOfGameResult: 'GameComplete', platformId: s.id.split('_')[0], participants,
    },
  };
}

export interface TimelineSpec {
  /** Participant who stops playing from this minute on (XP flat, passive gold only). */
  leaver?: { pid: number; fromMinute: number };
  /** Lane spots by participant, overriding the role's (a lane swap). */
  spots?: Record<number, [number, number]>;
  /** Irelia's skill-ups, as a string ("QEWQQRQEQERQEEWWRW"). */
  skills?: string;
  /** Item events for participant 1 (Irelia by default): [minute, type, itemId, afterId?]. */
  items?: [number, string, number, number?][];
}

export function makeTimeline(match: MatchDto, spec: TimelineSpec = {}): TimelineDto {
  const minutes = Math.round(match.info.gameDuration / 60);
  const ps = match.info.participants;
  const frames: TimelineFrameDto[] = [];
  const irelia = ps.find((p) => p.championId === IRELIA)?.participantId ?? 1;
  for (let m = 0; m <= minutes; m++) {
    const participantFrames: TimelineFrameDto['participantFrames'] = {};
    for (const p of ps) {
      const left = spec.leaver && p.participantId === spec.leaver.pid && m > spec.leaver.fromMinute;
      const activeMin = left ? spec.leaver!.fromMinute : m;
      const idleMin = left ? m - spec.leaver!.fromMinute : 0;
      const spot = spec.spots?.[p.participantId] ?? LANE_SPOT[p.teamPosition] ?? [7000, 7000];
      const side = p.teamId === 100 ? -150 : 150;
      participantFrames[String(p.participantId)] = {
        participantId: p.participantId,
        totalGold: 500 + activeMin * 380 + idleMin * 122, currentGold: 200, xp: activeMin * 600, level: Math.min(18, 1 + Math.floor(activeMin / 2)),
        minionsKilled: activeMin * 7, jungleMinionsKilled: p.teamPosition === 'JUNGLE' ? activeMin * 5 : 0,
        position: { x: spot[0] + side + (m % 3) * 40, y: spot[1] + side },
      };
    }
    const events: TimelineEvent[] = [];
    if (m === 0) events.push({ type: 'ITEM_PURCHASED', timestamp: 10_000, participantId: irelia, itemId: 1055 }, { type: 'ITEM_PURCHASED', timestamp: 11_000, participantId: irelia, itemId: 2003 });
    for (const [min, type, itemId, afterId] of spec.items ?? []) {
      if (min === m) events.push(type === 'ITEM_UNDO'
        ? { type, timestamp: m * 60_000 + 500, participantId: irelia, beforeId: itemId, afterId: afterId ?? 0 }
        : { type, timestamp: m * 60_000 + 500, participantId: irelia, itemId });
    }
    const skills = spec.skills ?? 'QEWQQRQEQERQEEWWRW';
    if (m >= 1 && m <= skills.length) {
      events.push({ type: 'SKILL_LEVEL_UP', timestamp: m * 60_000 + 100, participantId: irelia, skillSlot: 'QWER'.indexOf(skills[m - 1]) + 1, levelUpType: 'NORMAL' });
    }
    if (m === 6) events.push({ type: 'CHAMPION_KILL', timestamp: 6 * 60_000 + 200, killerId: irelia, victimId: 6, assistingParticipantIds: [2], bounty: 300, shutdownBounty: 0 });
    if (m === 9) events.push({ type: 'ELITE_MONSTER_KILL', timestamp: 9 * 60_000, killerId: 7, killerTeamId: 200, monsterType: 'DRAGON', monsterSubType: 'FIRE_DRAGON' });
    if (m === 12) {
      events.push({ type: 'TURRET_PLATE_DESTROYED', timestamp: 12 * 60_000, teamId: 200, laneType: 'TOP_LANE' });
      events.push({ type: 'BUILDING_KILL', timestamp: 12 * 60_000 + 300, killerId: irelia, teamId: 200, buildingType: 'TOWER_BUILDING', towerType: 'OUTER_TURRET', laneType: 'TOP_LANE' });
    }
    frames.push({ timestamp: m * 60_000, participantFrames, events });
  }
  return {
    info: {
      frameInterval: 60_000,
      participants: ps.map((p) => ({ participantId: p.participantId, puuid: p.puuid })),
      frames,
    },
  };
}

// ------------------------------------------------------------------------------------------- fake Riot API

export interface FakeHistory { id: string; creation: number }

/**
 * An in-memory RiotApi. Histories are the match ids a PUUID played, with their creation time; matchIdsPage
 * filters by startTime (epoch seconds) and pages newest first, like match-v5. Every call is logged.
 */
export class FakeRiot implements RiotApi {
  calls: { method: string; args: unknown[] }[] = [];
  ladders = new Map<string, LeagueEntry[]>();              // `${platform}:${tier}`
  masteries = new Map<string, MasteryDto[]>();             // puuid → best first
  ranks = new Map<string, RankedEntry[]>();
  histories = new Map<string, FakeHistory[]>();             // `${puuid}:${queue}` → any order
  matches = new Map<string, MatchDto>();
  timelines = new Map<string, TimelineDto>();
  regions = new Map<string, string>();

  private log(method: string, ...args: unknown[]) { this.calls.push({ method, args }); }
  count(method: string) { return this.calls.filter((c) => c.method === method).length; }

  /** Adds a match (and its timeline) to the store and to every player's history. */
  add(match: MatchDto, timeline: TimelineDto | null = makeTimeline(match)) {
    this.matches.set(match.metadata.matchId, match);
    if (timeline) this.timelines.set(match.metadata.matchId, timeline);
    for (const p of match.info.participants) {
      const key = `${p.puuid}:${match.info.queueId}`;
      this.histories.set(key, [...(this.histories.get(key) ?? []), { id: match.metadata.matchId, creation: match.info.gameCreation }]);
    }
    return match;
  }

  async leagueEntries(platform: Platform, tier: Tier, _d: Division[], _m: number) { this.log('leagueEntries', platform, tier); return this.ladders.get(`${platform}:${tier}`) ?? []; }
  async topMasteries(platform: Platform, puuid: string, count: number) { this.log('topMasteries', platform, puuid, count); return (this.masteries.get(puuid) ?? []).slice(0, count); }
  async championMastery(platform: Platform, puuid: string, championId: number) {
    this.log('championMastery', platform, puuid, championId);
    return (this.masteries.get(puuid) ?? []).find((m) => m.championId === championId) ?? null;
  }
  async rankedEntries(platform: Platform, puuid: string) { this.log('rankedEntries', platform, puuid); return this.ranks.get(puuid) ?? []; }
  async matchIds(platform: Platform, puuid: string, queue: number, count: number) {
    this.log('matchIds', platform, puuid, queue, count);
    return this.sorted(puuid, queue).slice(0, count).map((h) => h.id);
  }
  async matchIdsPage(platform: Platform, puuid: string, q: MatchIdQuery) {
    this.log('matchIdsPage', platform, puuid, q);
    const since = q.startTime === undefined ? -Infinity : q.startTime * 1000;
    return this.sorted(puuid, q.queue).filter((h) => h.creation >= since).slice(q.start, q.start + q.count).map((h) => h.id);
  }
  async match(platform: Platform, id: string) { this.log('match', platform, id); return this.matches.get(id) ?? null; }
  async timeline(platform: Platform, id: string) { this.log('timeline', platform, id); return this.timelines.get(id) ?? null; }
  async accountRegion(puuid: string) { this.log('accountRegion', puuid); return this.regions.get(puuid) ?? null; }
  /** Riot ID ("Name#TAG", any case) → the API's PUUID. */
  accounts = new Map<string, string>();
  async accountByRiotId(gameName: string, tagLine: string, host: string) {
    this.log('accountByRiotId', gameName, tagLine, host);
    const puuid = this.accounts.get(`${gameName}#${tagLine}`.toLowerCase());
    return puuid ? { puuid } : null;
  }

  private sorted(puuid: string, queue: number) {
    return [...(this.histories.get(`${puuid}:${queue}`) ?? [])].sort((a, b) => b.creation - a.creation);
  }
}
