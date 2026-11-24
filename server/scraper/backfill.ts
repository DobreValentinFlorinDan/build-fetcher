// Rebuilds the v3 capture (server/capture.ts) for every match already stored, from the Riot responses on disk:
// zero API calls. It runs in the scraper worker, next to the background lanes. Each batch is read from disk first
// and then written in one synchronous transaction, so the lanes never see half a batch, and a match is marked
// done (matches.detail_version) in the same transaction that writes its capture. Interrupted, it resumes where it
// stopped; re-run (force), it rewrites the same rows.
import {
  CAPTURE_VERSION, captureTimeline, laneOpponents, leavers, matchQuality, participantRows,
} from '../capture.ts';
import type { Db } from '../db.ts';
import { toPatch } from '../ddragon.ts';
import type { MatchDto, TimelineDto } from '../engine/parseMatch.ts';
import { isStale, type RegionPatch } from '../patches.ts';
import type { MasteryDto } from '../riot/client.ts';
import type { DiskCache } from '../riot/diskCache.ts';
import { normalizePlatform, type Platform } from '../riot/routing.ts';
import { HARVEST_SCORE } from './pilots.ts';

export interface BackfillEnv {
  db: Db;
  cache: Pick<DiskCache, 'get'>;
  championId: number;
  harvest: boolean;
  region: (platform: Platform) => RegionPatch;
  mePuuid: () => string | null;
}

export interface BackfillProgress {
  phase: string;
  done: number;
  total: number;
  games: number;        // Irelia games whose timeline was captured
  missing: number;      // matches (or timelines) no longer on disk
  harvested: number;    // other players' TOP / MIDDLE Irelia games queued for a timeline call
  startedAt: number;
  finishedAt: number | null;
}

const HARVEST_POSITIONS = new Set(['TOP', 'MIDDLE']);

export async function runBackfill(env: BackfillEnv, o: {
  signal: AbortSignal; onProgress: (p: BackfillProgress) => void; force?: boolean; batch?: number;
}): Promise<BackfillProgress> {
  const { db, cache } = env;
  const p: BackfillProgress = { phase: 'Preparing', done: 0, total: 0, games: 0, missing: 0, harvested: 0, startedAt: Date.now(), finishedAt: null };
  const step = (phase: string) => { p.phase = phase; o.onProgress({ ...p }); };

  if (o.force) db.resetDetail();
  db.rebuildGameVersions();
  const me = env.mePuuid();
  if (me) db.tagOwnGames(me);

  // OTPs checked before v3: the cached mastery answer (top?count=1, keyed by PUUID) already holds Irelia's lastPlayTime.
  step('Reading cached mastery checks');
  for (const player of db.playersMissingActivity()) {
    o.signal.throwIfAborted();
    const top = await cache.get<MasteryDto[]>('mastery', player.platform, player.puuid);
    const irelia = top?.[0]?.championId === env.championId ? top[0] : null;
    if (irelia) db.setIreliaActivity(player.puuid, irelia.championPoints, irelia.lastPlayTime ?? null, 1);
  }

  p.total = db.countPendingDetail(CAPTURE_VERSION);
  step('Capturing matches from the disk cache');
  const size = o.batch ?? 50;
  for (;;) {
    o.signal.throwIfAborted();
    const rows = db.pendingDetail(CAPTURE_VERSION, size);
    if (!rows.length) break;

    // Read the batch from disk (async), then write it in one transaction (sync).
    const loaded = await Promise.all(rows.map(async (r) => {
      let platform: Platform;
      try { platform = normalizePlatform(r.platform); } catch { return { r, platform: null, match: undefined, timeline: undefined }; }
      const match = await cache.get<MatchDto>('match', platform, r.match_id);
      const timeline = r.game_puuid && match ? await cache.get<TimelineDto>('timeline', platform, r.match_id) : undefined;
      return { r, platform, match, timeline };
    }));

    db.tx(() => {
      for (const { r, platform, match, timeline } of loaded) {
        try {
          if (!match || !platform) { p.missing++; db.markDetail(r.match_id, CAPTURE_VERSION); continue; }
          const quality = matchQuality(match);
          db.insertParticipants(r.match_id, participantRows(match));
          db.setQuality(r.match_id, quality.remake, quality.endResult, CAPTURE_VERSION);

          const irelia = match.info.participants.find((x) => x.championId === env.championId) ?? null;
          if (r.game_puuid && irelia && irelia.puuid === r.game_puuid) {
            if (!timeline) { p.missing++; continue; }
            const cap = captureTimeline(timeline);
            const opp = laneOpponents(match, cap, irelia);
            db.insertTimeline(r.match_id, cap.interval, JSON.stringify(cap));
            db.setLeaver(r.match_id, leavers(cap).length > 0);
            db.setOpponents(r.match_id, { participantId: irelia.participantId, position: opp.position, timeline: opp.timeline, effective: opp.effective });
            const pilot = db.player(irelia.puuid);
            if (pilot) db.fillPilotMeta(r.match_id, pilot);
            p.games++;
          } else if (irelia && !r.game_puuid && env.harvest && HARVEST_POSITIONS.has(irelia.teamPosition) && irelia.puuid !== me) {
            // Another player's Irelia in a cached match: a candidate pilot, and on the current patch a game worth a timeline call.
            db.addCandidate(irelia.puuid, platform, 'harvest');
            if (!quality.remake && !isStale(toPatch(match.info.gameVersion), env.region(platform))) {
              db.enqueue({ match_id: r.match_id, platform, kind: 'timeline', puuid: irelia.puuid, source: 'harvested', score: HARVEST_SCORE });
              p.harvested++;
            }
          }
        } catch {
          p.missing++;   // a corrupt cache file: skip it rather than stall the backfill on it forever
          db.markDetail(r.match_id, CAPTURE_VERSION);
        }
      }
    });
    p.done += rows.length;
    o.onProgress({ ...p });
  }
  p.finishedAt = Date.now();
  step('Done');
  return p;
}
