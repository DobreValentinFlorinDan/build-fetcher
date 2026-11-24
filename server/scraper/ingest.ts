// Fetching and storing one match with everything v3 keeps (server/capture.ts): the participants, the quality
// flags, and for an Irelia game its timeline. Fetching writes nothing; storing is one synchronous transaction, so
// results committed in order (pool.ts inOrder) leave the database exactly as a one-at-a-time loop would.
import {
  CAPTURE_VERSION, captureTimeline, laneOpponents, leavers, matchQuality, participantRows,
  type MatchQuality, type ParticipantRow, type TimelineCapture,
} from '../capture.ts';
import type { Db, IreliaGameRow, MatchRow, PilotSource, PlayerRow } from '../db.ts';
import { comparePatch, toPatch } from '../ddragon.ts';
import { extractIreliaGame, type MatchDto, type ParticipantDto, type TimelineDto } from '../engine/parseMatch.ts';
import { isStale, type RegionPatch } from '../patches.ts';
import type { RiotApi } from '../riot/client.ts';
import type { Platform } from '../riot/routing.ts';

/** Lanes whose Irelia games are harvested from other players' matches. */
const HARVEST_POSITIONS = new Set(['TOP', 'MIDDLE']);

export interface IngestEnv {
  db: Db;
  riot: RiotApi;
  championId: number;
  /** The platform's current patch (server/patches.ts). */
  region: (platform: Platform) => RegionPatch;
  harvest: boolean;
  /** The signed-in player's PUUID: their Irelia games are always tagged 'me'. */
  mePuuid: () => string | null;
}

/** Whose history listed the match. `null` puuid: no pilot (a re-fetch). */
export interface IngestContext {
  pilotPuuid: string | null;
  source: PilotSource;
  /** Own games keep the previous patch too (the personal layer compares patches); everyone else's must be current. */
  keepPreviousPatch?: boolean;
}

export interface GameCapture {
  row: IreliaGameRow;
  timeline: TimelineCapture;
  leaverIds: number[];
}

export type Fetched =
  | { kind: 'known' | 'missing' }
  | {
    kind: 'fetched';
    row: MatchRow;
    quality: MatchQuality;
    participants: ParticipantRow[];
    stale: boolean;                      // older than the platform's current patch
    game: GameCapture | null;
    pilotPlayedIrelia: boolean;          // the listing pilot was this match's Irelia (their history's Irelia share)
    candidate: { puuid: string; platform: string } | null;   // another TOP / MIDDLE Irelia: checked as a pilot later
    ctx: IngestContext;
  };

export type Ingest = 'irelia' | 'other' | 'stale' | 'remake' | 'missing' | 'known';

/** The Irelia row of a stored game, with its v3 fields: lane opponents from the timeline, source and pilot metadata. */
export function captureGame(
  match: MatchDto, timeline: TimelineDto, me: ParticipantDto, source: PilotSource, pilot: PlayerRow | undefined,
): GameCapture {
  const cap = captureTimeline(timeline);
  const base = extractIreliaGame(match, timeline, me);
  const opp = laneOpponents(match, cap, me);
  const row: IreliaGameRow = {
    ...base,
    opponent_champion_id: opp.effective,
    participant_id: me.participantId,
    position_opponent_id: opp.position,
    timeline_opponent_id: opp.timeline,
    pilot_source: source,
    pilot_tier: pilot?.tier ?? null,
    pilot_lp: pilot?.league_points ?? null,
    pilot_mastery: pilot?.irelia_points ?? pilot?.irelia_mastery ?? null,
    pilot_last_play: pilot?.irelia_last_play ?? null,
    pilot_top1: pilot ? (pilot.irelia_rank === 1 || pilot.is_otp === 1 ? 1 : pilot.irelia_rank ? 0 : null) : null,
  };
  return { row, timeline: cap, leaverIds: leavers(cap) };
}

/** 'otp' or 'main' when the player is a classified pilot, else null. */
function knownPilotClass(env: IngestEnv, puuid: string): 'otp' | 'main' | null {
  const c = env.db.player(puuid)?.pilot_class;
  return c === 'otp' || c === 'main' ? c : null;
}

/** Fetches one match (cache first), plus its timeline when an Irelia game in it will be stored. Writes nothing. */
export async function fetchMatch(env: IngestEnv, platform: Platform, id: string, ctx: IngestContext, signal: AbortSignal): Promise<Fetched> {
  if (env.db.hasMatch(id)) return { kind: 'known' };
  const match = await env.riot.match(platform, id, signal);
  if (!match) return { kind: 'missing' };
  const patch = toPatch(match.info.gameVersion);
  const region = env.region(platform);
  const stale = isStale(patch, region);
  const keep = !stale || (ctx.keepPreviousPatch === true && comparePatch(patch, region.previousPatch) >= 0);
  const quality = matchQuality(match);
  const row: MatchRow = {
    id, platform, patch, version: match.info.gameVersion, queue: match.info.queueId,
    creation: match.info.gameCreation, duration: match.info.gameDuration, hasIrelia: false,
  };

  const irelia = match.info.participants.find((p) => p.championId === env.championId) ?? null;
  const mine = !!irelia && irelia.puuid === ctx.pilotPuuid;
  // The Irelia player's own class decides the source, whichever pilot's history listed the match: a stored match is
  // never looked at again, so an OTP's game first seen in a main's history must still be stored as the OTP's.
  const pilotClass = irelia ? knownPilotClass(env, irelia.puuid) : null;
  const harvested = !!irelia && !mine && !pilotClass && env.harvest && HARVEST_POSITIONS.has(irelia.teamPosition);
  const me = env.mePuuid();
  const source: PilotSource | null = !irelia ? null
    : irelia.puuid === me ? 'me' : pilotClass ?? (mine ? ctx.source : harvested ? 'harvested' : null);

  let game: GameCapture | null = null;
  // Only fresh, real games of a kept source spend a timeline call; everything else is recorded so it is never refetched.
  if (irelia && source && keep && !quality.remake) {
    const timeline = await env.riot.timeline(platform, id, signal);
    if (!timeline) return { kind: 'missing' };
    game = captureGame(match, timeline, irelia, source, env.db.player(irelia.puuid));
    row.hasIrelia = true;
  }
  return {
    kind: 'fetched', row, quality, participants: participantRows(match), stale: !keep, game,
    pilotPlayedIrelia: mine, candidate: harvested && irelia.puuid !== me ? { puuid: irelia.puuid, platform } : null, ctx,
  };
}

/** Writes a fetched match in one transaction: match, participants, quality flags, Irelia game and timeline, candidate pilot. */
export function storeMatch(db: Db, id: string, fetched: Fetched): Ingest {
  if (fetched.kind !== 'fetched') return fetched.kind;
  return db.tx(() => {
    if (!db.insertMatch(fetched.row)) return 'known';   // an earlier entry of this run stored it already
    db.insertParticipants(id, fetched.participants);
    db.setQuality(id, fetched.quality.remake, fetched.quality.endResult, CAPTURE_VERSION);
    if (fetched.game) storeGame(db, fetched.game);
    if (fetched.candidate) db.addCandidate(fetched.candidate.puuid, fetched.candidate.platform, 'harvest');
    const { pilotPuuid, source } = fetched.ctx;
    if (pilotPuuid && source !== 'harvested' && !fetched.stale) db.countHistory(pilotPuuid, 1, fetched.pilotPlayedIrelia && fetched.game ? 1 : 0);
    return fetched.game ? 'irelia' : fetched.quality.remake ? 'remake' : fetched.stale ? 'stale' : 'other';
  });
}

/** The Irelia game, its timeline and the leaver flag (inside the caller's transaction). */
export function storeGame(db: Db, g: GameCapture) {
  db.insertGame(g.row);
  db.insertTimeline(g.row.match_id, g.timeline.interval, JSON.stringify(g.timeline));
  db.setLeaver(g.row.match_id, g.leaverIds.length > 0);
  db.setHasIrelia(g.row.match_id);
}

/**
 * A harvested game whose match is already stored (found in a cached match by the backfill): the match comes from
 * the disk cache, so this costs one timeline call. Returns the capture, or null when there is nothing to store.
 */
export async function fetchHarvested(env: IngestEnv, platform: Platform, id: string, puuid: string, signal: AbortSignal) {
  if (env.db.gameSource(id) !== undefined) return null;
  const match = await env.riot.match(platform, id, signal);
  const irelia = match?.info.participants.find((p) => p.championId === env.championId && p.puuid === puuid);
  if (!match || !irelia || matchQuality(match).remake) return null;
  if (isStale(toPatch(match.info.gameVersion), env.region(platform))) return null;
  const timeline = await env.riot.timeline(platform, id, signal);
  if (!timeline) return null;
  return captureGame(match, timeline, irelia, irelia.puuid === env.mePuuid() ? 'me' : knownPilotClass(env, puuid) ?? 'harvested', env.db.player(puuid));
}
