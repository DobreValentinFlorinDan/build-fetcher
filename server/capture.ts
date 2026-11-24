// What the app keeps from a match beyond the Irelia row: every participant, and for Irelia games a compact
// timeline. Everything here is a pure function of the match-v5 JSON already on disk, so the backfill
// (server/scraper/backfill.ts) can rebuild it for every cached match without an API call.
// Field names follow Riot's match-v5 reference; see docs/BUILD-SOURCING.md §Schema.
import type { MatchDto, ParticipantDto, TimelineDto } from './engine/parseMatch.ts';

/** Bump when the capture changes: the backfill re-processes every match stored with an older version. */
export const CAPTURE_VERSION = 1;

/** A game shorter than this was remade (the remake vote ends a game around 3:00; no surrender is allowed before 15:00). */
export const REMAKE_MAX_SECONDS = 300;

// ------------------------------------------------------------------------------------------- participants

/** One row per player per match (table `participants`). Damage, healing and CC are whole-game totals. */
export interface ParticipantRow {
  match_id: string;
  participant_id: number;
  puuid: string;
  champion_id: number;
  team_id: number;
  position: string;          // teamPosition, '' when Riot gives none
  win: number;
  champ_level: number;
  kills: number; deaths: number; assists: number;
  gold_earned: number;
  cs: number;                // lane minions + neutral monsters
  items: string;             // JSON [item0..item6]
  dmg_physical: number;      // ...DamageDealtToChampions
  dmg_magic: number;
  dmg_true: number;
  dmg_taken: number;         // totalDamageTaken
  dmg_mitigated: number;     // damageSelfMitigated
  heal: number;              // totalHeal (includes healing others)
  heal_allies: number;       // totalHealsOnTeammates
  shield_allies: number;     // totalDamageShieldedOnTeammates
  cc_time: number;           // timeCCingOthers
  cc_dealt: number;          // totalTimeCCDealt
  time_played: number;       // seconds
  challenges: string | null; // JSON of CHALLENGE_KEYS that were present
}

/** The `challenges` fields worth keeping for champion profiles and lane states (others are cosmetic). */
export const CHALLENGE_KEYS = [
  'damagePerMinute', 'damageTakenOnTeamPercentage', 'teamDamagePercentage', 'effectiveHealAndShielding',
  'enemyChampionImmobilizations', 'goldPerMinute', 'kda', 'soloKills', 'takedowns',
  'laningPhaseGoldExpAdvantage', 'earlyLaningPhaseGoldExpAdvantage', 'maxCsAdvantageOnLaneOpponent',
  'maxLevelLeadLaneOpponent', 'laneMinionsFirst10Minutes', 'turretPlatesTaken', 'visionScorePerMinute',
  'skillshotsHit', 'skillshotsDodged',
] as const;

const n = (v: number | undefined) => (Number.isFinite(v) ? Math.round(v!) : 0);

export function participantRows(match: MatchDto): ParticipantRow[] {
  return match.info.participants.map((p) => {
    const ch = p.challenges ? Object.fromEntries(CHALLENGE_KEYS.filter((k) => typeof p.challenges![k] === 'number').map((k) => [k, p.challenges![k]])) : null;
    return {
      match_id: match.metadata.matchId,
      participant_id: p.participantId,
      puuid: p.puuid,
      champion_id: p.championId,
      team_id: p.teamId,
      position: p.teamPosition ?? '',
      win: p.win ? 1 : 0,
      champ_level: n(p.champLevel),
      kills: n(p.kills), deaths: n(p.deaths), assists: n(p.assists),
      gold_earned: n(p.goldEarned),
      cs: n(p.totalMinionsKilled) + n(p.neutralMinionsKilled),
      items: JSON.stringify([p.item0, p.item1, p.item2, p.item3, p.item4, p.item5, p.item6].map(n)),
      dmg_physical: n(p.physicalDamageDealtToChampions),
      dmg_magic: n(p.magicDamageDealtToChampions),
      dmg_true: n(p.trueDamageDealtToChampions),
      dmg_taken: n(p.totalDamageTaken),
      dmg_mitigated: n(p.damageSelfMitigated),
      heal: n(p.totalHeal),
      heal_allies: n(p.totalHealsOnTeammates),
      shield_allies: n(p.totalDamageShieldedOnTeammates),
      cc_time: n(p.timeCCingOthers),
      cc_dealt: n(p.totalTimeCCDealt),
      time_played: n(p.timePlayed),
      challenges: ch && Object.keys(ch).length ? JSON.stringify(ch) : null,
    };
  });
}

// ------------------------------------------------------------------------------------------- match quality

export interface MatchQuality {
  remake: boolean;            // remake vote (gameEndedInEarlySurrender) or shorter than REMAKE_MAX_SECONDS
  endResult: string | null;   // info.endOfGameResult when Riot sends it
}

export function matchQuality(match: MatchDto): MatchQuality {
  const remake = match.info.participants.some((p) => p.gameEndedInEarlySurrender === true)
    || match.info.gameDuration < REMAKE_MAX_SECONDS;
  return { remake, endResult: match.info.endOfGameResult ?? null };
}

// ------------------------------------------------------------------------------------------- timeline

/** Item event kinds in TimelineCapture.items. */
export const ITEM_EVENT = { BUY: 0, SELL: 1, DESTROY: 2, UNDO: 3 } as const;
const ITEM_TYPES: Record<string, number> = {
  ITEM_PURCHASED: ITEM_EVENT.BUY, ITEM_SOLD: ITEM_EVENT.SELL, ITEM_DESTROYED: ITEM_EVENT.DESTROY, ITEM_UNDO: ITEM_EVENT.UNDO,
};

/** Objective kinds in TimelineCapture.objectives. */
export type ObjectiveKind = 'DRAGON' | 'BARON_NASHOR' | 'RIFTHERALD' | 'HORDE' | 'ATAKHAN' | 'ELDER_DRAGON' | 'TOWER' | 'INHIBITOR' | 'PLATE';

/** Positions are kept for the first POSITION_FRAMES frames (0:00 … 15:00 at Riot's one-minute frames). */
export const POSITION_FRAMES = 16;

/**
 * The compact per-game timeline (table `timelines`, one JSON blob per Irelia game). Everything the Live Client can
 * also show in game, minute by minute, plus who laned against whom:
 *  - frames[pid][i] = [totalGold, currentGold, xp, level, cs] at t[i]
 *  - pos[pid][i]    = [x, y] for the first POSITION_FRAMES frames
 *  - items          = [t, pid, kind, itemId, afterId] (kind: ITEM_EVENT; UNDO keeps Riot's beforeId → itemId, afterId)
 *  - kills          = [t, killerId, victimId, assistIds, bounty, shutdownBounty] (killerId 0: executed by a turret/minion)
 *  - objectives     = [t, teamId that took it, kind, subtype]
 */
export interface TimelineCapture {
  v: 1;
  interval: number;
  t: number[];
  frames: Record<string, number[][]>;
  pos: Record<string, number[][]>;
  items: number[][];
  kills: [number, number, number, number[], number, number][];
  objectives: [number, number, ObjectiveKind, string][];
}

const otherTeam = (teamId: number) => (teamId === 100 ? 200 : 100);

export function captureTimeline(timeline: TimelineDto): TimelineCapture {
  const info = timeline.info;
  const cap: TimelineCapture = {
    v: 1, interval: info.frameInterval ?? 60_000, t: [], frames: {}, pos: {}, items: [], kills: [], objectives: [],
  };
  info.frames.forEach((frame, i) => {
    cap.t.push(frame.timestamp);
    for (const [key, f] of Object.entries(frame.participantFrames ?? {})) {
      const pid = String(f.participantId ?? key);
      (cap.frames[pid] ??= []).push([f.totalGold, f.currentGold, f.xp, f.level, n(f.minionsKilled) + n(f.jungleMinionsKilled)]);
      if (i < POSITION_FRAMES && f.position) (cap.pos[pid] ??= []).push([f.position.x, f.position.y]);
    }
    for (const ev of frame.events) {
      const kind = ITEM_TYPES[ev.type];
      if (kind !== undefined && ev.participantId) {
        cap.items.push(kind === ITEM_EVENT.UNDO
          ? [ev.timestamp, ev.participantId, kind, ev.beforeId ?? 0, ev.afterId ?? 0]
          : [ev.timestamp, ev.participantId, kind, ev.itemId ?? 0]);
      } else if (ev.type === 'CHAMPION_KILL') {
        cap.kills.push([ev.timestamp, ev.killerId ?? 0, ev.victimId ?? 0, ev.assistingParticipantIds ?? [], ev.bounty ?? 0, ev.shutdownBounty ?? 0]);
      } else if (ev.type === 'ELITE_MONSTER_KILL' && ev.monsterType && ev.killerTeamId) {
        const kind = ev.monsterSubType === 'ELDER_DRAGON' ? 'ELDER_DRAGON' : ev.monsterType as ObjectiveKind;
        cap.objectives.push([ev.timestamp, ev.killerTeamId, kind, ev.monsterSubType ?? '']);
      } else if (ev.type === 'BUILDING_KILL' && ev.teamId) {
        // teamId is the team that owned the building, so the other team took it.
        const kind: ObjectiveKind = ev.buildingType === 'INHIBITOR_BUILDING' ? 'INHIBITOR' : 'TOWER';
        cap.objectives.push([ev.timestamp, otherTeam(ev.teamId), kind, ev.towerType ?? ev.laneType ?? '']);
      } else if (ev.type === 'TURRET_PLATE_DESTROYED' && ev.teamId) {
        cap.objectives.push([ev.timestamp, otherTeam(ev.teamId), 'PLATE', ev.laneType ?? '']);
      }
    }
  });
  return cap;
}

// ------------------------------------------------------------------------------------------- leavers

/** Passive gold is 20.4 per 10 s for everyone, AFK or not; an inactive player earns at most this much. */
const IDLE_GOLD_PER_MIN = 140;
/** Inactivity this long (no XP, no CS, passive gold only) after the first two minutes marks a leaver. */
export const LEAVER_MIN_IDLE_MS = 180_000;
/** At level 18 XP no longer moves, so only CS and gold tell: ask for longer than any death plus a standoff. */
export const LEAVER_MIN_IDLE_MAX_LEVEL_MS = 300_000;
const MAX_LEVEL = 18;
const GAME_START_MS = 120_000;

/**
 * Participants who left (or went AFK): for at least LEAVER_MIN_IDLE_MS their XP and CS stop growing and their gold
 * only grows by passive income (LEAVER_MIN_IDLE_MAX_LEVEL_MS once they are level 18, when XP stops anyway). Deaths
 * are far shorter, and the first two minutes (walking to lane, no passive gold yet) are ignored.
 */
export function leavers(cap: TimelineCapture): number[] {
  const out: number[] = [];
  for (const [pid, frames] of Object.entries(cap.frames)) {
    let idle = 0, maxLevel = false;
    for (let i = 1; i < frames.length; i++) {
      const dt = cap.t[i] - cap.t[i - 1];
      const [gold, , xp, level, cs] = frames[i];
      const [gold0, , xp0, , cs0] = frames[i - 1];
      const still = (xp === xp0 || level >= MAX_LEVEL) && cs === cs0 && gold - gold0 <= IDLE_GOLD_PER_MIN * (dt / 60_000);
      if (still && cap.t[i - 1] >= GAME_START_MS) { idle += dt; maxLevel ||= level >= MAX_LEVEL; } else { idle = 0; maxLevel = false; }
      if (idle >= (maxLevel ? LEAVER_MIN_IDLE_MAX_LEVEL_MS : LEAVER_MIN_IDLE_MS)) { out.push(Number(pid)); break; }
    }
  }
  return out.sort((a, b) => a - b);
}

// ------------------------------------------------------------------------------------------- lane opponent

/** Laning is read between these frames' timestamps: after both laners arrive, before lanes break. */
const LANE_FROM_MS = 3 * 60_000;
const LANE_TO_MS = 14 * 60_000;

const median = (xs: number[]) => {
  const s = [...xs].sort((a, b) => a - b);
  return s.length % 2 ? s[(s.length - 1) / 2] : (s[s.length / 2 - 1] + s[s.length / 2]) / 2;
};

/**
 * The enemy who actually laned against `pid`: the one with the smallest median distance to them over the frames
 * between 3:00 and 14:00. Null when the timeline has fewer than three such frames with positions.
 */
export function timelineLaneOpponent(cap: TimelineCapture, pid: number, enemyIds: number[]): number | null {
  const mine = cap.pos[String(pid)];
  if (!mine) return null;
  const frames = cap.t.map((t, i) => i).filter((i) => i < mine.length && cap.t[i] >= LANE_FROM_MS && cap.t[i] <= LANE_TO_MS);
  if (frames.length < 3) return null;
  let best: { id: number; d: number } | null = null;
  for (const id of enemyIds) {
    const theirs = cap.pos[String(id)];
    if (!theirs) continue;
    const ds = frames.filter((i) => i < theirs.length).map((i) => Math.hypot(mine[i][0] - theirs[i][0], mine[i][1] - theirs[i][1]));
    if (ds.length < 3) continue;
    const d = median(ds);
    if (!best || d < best.d) best = { id, d };
  }
  return best?.id ?? null;
}

/** Lane opponents by Riot's position label and by the timeline, for the participant `me`. */
export function laneOpponents(match: MatchDto, cap: TimelineCapture | null, me: ParticipantDto) {
  const enemies = match.info.participants.filter((p) => p.teamId !== me.teamId);
  const byPosition = enemies.find((p) => p.teamPosition && p.teamPosition === me.teamPosition) ?? null;
  const tlId = cap ? timelineLaneOpponent(cap, me.participantId, enemies.map((p) => p.participantId)) : null;
  const byTimeline = tlId === null ? null : enemies.find((p) => p.participantId === tlId) ?? null;
  return {
    position: byPosition?.championId ?? null,
    timeline: byTimeline?.championId ?? null,
    /** The timeline wins when the two disagree: Riot's labels are wrong in lane swaps and odd role assignments. */
    effective: byTimeline?.championId ?? byPosition?.championId ?? null,
  };
}

// ------------------------------------------------------------------------------------------- inventory replay

/**
 * Item ids in `pid`'s inventory just after time `t`, rebuilt from the item events. Purchases add, sells and
 * destroys (components used up by a recipe, consumables, wards) remove, and an undo reverts the event it names.
 * Free transformations that Riot logs without an event (tier-3 boots, Muramana) keep the pre-upgrade id.
 */
export function inventoryAt(cap: TimelineCapture, pid: number, t: number): number[] {
  const inv: number[] = [];
  const remove = (id: number) => { const i = inv.lastIndexOf(id); if (i >= 0) inv.splice(i, 1); };
  for (const [ts, p, kind, item, after] of cap.items) {
    if (p !== pid || ts > t) continue;
    if (kind === ITEM_EVENT.BUY) inv.push(item);
    else if (kind === ITEM_EVENT.SELL || kind === ITEM_EVENT.DESTROY) remove(item);
    else if (kind === ITEM_EVENT.UNDO) { if (item) remove(item); if (after) inv.push(after); }
  }
  return inv;
}
