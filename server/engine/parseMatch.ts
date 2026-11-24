import type { IreliaGameRow } from '../db.ts';
import { toPatch } from '../ddragon.ts';

// Minimal match-v5 shapes: only the fields the app reads. Names follow Riot's match-v5 reference (MatchDto,
// ParticipantDto, TimelineDto, ...); fields added after the first schema are optional because older cached
// responses or other queues may lack them.
export interface MatchDto {
  metadata: { matchId: string; participants: string[] };
  info: {
    gameVersion: string; queueId: number; gameCreation: number; gameDuration: number;   // gameDuration: seconds (patch 11.20+)
    platformId?: string;
    endOfGameResult?: string;   // e.g. "GameComplete"; anything else means the game was terminated
    participants: ParticipantDto[];
  };
}
export interface ParticipantDto {
  puuid: string; participantId: number; championId: number; teamId: number; teamPosition: string; win: boolean;
  summoner1Id: number; summoner2Id: number; kills: number; deaths: number; assists: number;
  item0: number; item1: number; item2: number; item3: number; item4: number; item5: number; item6: number;
  perks: {
    statPerks: { offense: number; flex: number; defense: number };
    styles: { description: string; style: number; selections: { perk: number }[] }[];
  };
  champLevel?: number; goldEarned?: number; totalMinionsKilled?: number; neutralMinionsKilled?: number;
  physicalDamageDealtToChampions?: number; magicDamageDealtToChampions?: number; trueDamageDealtToChampions?: number;
  totalDamageTaken?: number; damageSelfMitigated?: number;
  totalHeal?: number; totalHealsOnTeammates?: number; totalDamageShieldedOnTeammates?: number;
  timeCCingOthers?: number; totalTimeCCDealt?: number; timePlayed?: number;
  gameEndedInEarlySurrender?: boolean;   // the remake vote ("early surrender"); Riot's field description is a copy-paste error
  gameEndedInSurrender?: boolean;
  teamEarlySurrendered?: boolean;
  gameEndedInIGNBSurrender?: boolean;    // newer surrender flag, undocumented: recorded, not relied on
  challenges?: Record<string, number | number[] | undefined>;
}
export interface TimelineFrameDto {
  timestamp: number;
  participantFrames?: Record<string, {
    participantId?: number; totalGold: number; currentGold: number; xp: number; level: number;
    minionsKilled: number; jungleMinionsKilled: number; position?: { x: number; y: number };
  }>;
  events: TimelineEvent[];
}
export interface TimelineDto {
  info: { frameInterval?: number; participants?: { participantId: number; puuid: string }[]; frames: TimelineFrameDto[] };
}
export type TimelineEvent = {
  type: string; timestamp: number; participantId?: number;
  skillSlot?: number; levelUpType?: string;                                    // SKILL_LEVEL_UP
  itemId?: number; beforeId?: number; afterId?: number;                        // ITEM_PURCHASED / _SOLD / _DESTROYED / _UNDO
  killerId?: number; victimId?: number; assistingParticipantIds?: number[];    // CHAMPION_KILL
  bounty?: number; shutdownBounty?: number;
  killerTeamId?: number; monsterType?: string; monsterSubType?: string;         // ELITE_MONSTER_KILL
  teamId?: number; buildingType?: string; towerType?: string; laneType?: string; // BUILDING_KILL, TURRET_PLATE_DESTROYED
};

const SKILL_KEYS = ['', 'Q', 'W', 'E', 'R'];

export function findIrelia(match: MatchDto, puuid: string, championId: number) {
  return match.info.participants.find((p) => p.puuid === puuid && p.championId === championId) ?? null;
}

/** Turns a match + timeline into the raw row stored in irelia_games. */
export function extractIreliaGame(match: MatchDto, timeline: TimelineDto, me: ParticipantDto): IreliaGameRow {
  const pid = timeline.info.participants?.find((p) => p.puuid === me.puuid)?.participantId ?? me.participantId;
  const opponent = match.info.participants.find(
    (p) => p.teamId !== me.teamId && p.teamPosition && p.teamPosition === me.teamPosition,
  );

  let skills = '';
  const purchases: [number, number][] = [];
  for (const frame of timeline.info.frames) {
    for (const ev of frame.events) {
      if (ev.participantId !== pid) continue;
      if (ev.type === 'SKILL_LEVEL_UP' && ev.skillSlot !== undefined && ev.levelUpType === 'NORMAL') {
        skills += SKILL_KEYS[ev.skillSlot] ?? '';
      } else if (ev.type === 'ITEM_PURCHASED' && ev.itemId) {
        purchases.push([ev.itemId, ev.timestamp]);
      } else if (ev.type === 'ITEM_UNDO' && ev.beforeId) {
        // An undo of a purchase: drop the most recent purchase of that item
        for (let i = purchases.length - 1; i >= 0; i--) {
          if (purchases[i][0] === ev.beforeId) { purchases.splice(i, 1); break; }
        }
      }
    }
  }

  const [primary, sub] = me.perks.styles;
  return {
    match_id: match.metadata.matchId,
    puuid: me.puuid,
    patch: toPatch(match.info.gameVersion),
    win: me.win ? 1 : 0,
    position: me.teamPosition || 'UNKNOWN',
    opponent_champion_id: opponent?.championId ?? null,
    summoner1: me.summoner1Id,
    summoner2: me.summoner2Id,
    primary_style: primary.style,
    sub_style: sub.style,
    perks: JSON.stringify([...primary.selections, ...sub.selections].map((s) => s.perk)),
    stat_perks: JSON.stringify([me.perks.statPerks.offense, me.perks.statPerks.flex, me.perks.statPerks.defense]),
    skill_order: skills,
    purchases: JSON.stringify(purchases),
    final_items: JSON.stringify([me.item0, me.item1, me.item2, me.item3, me.item4, me.item5, me.item6]),
    kills: me.kills,
    deaths: me.deaths,
    assists: me.assists,
    game_duration: match.info.gameDuration,
    created_at: match.info.gameCreation,
  };
}
