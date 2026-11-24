// Data audit (npm run audit:data): what the stored data covers and what sourcing v2 would add, measured on the
// user's own data folder. Strictly read-only: the database is opened read-only (any schema version, so raw SQL
// with existence checks), cache files are only read, and no Riot API call is made. The numbers feed the yield
// model in docs/BUILD-SOURCING.md.
import fs from 'node:fs/promises';
import path from 'node:path';
import { promisify } from 'node:util';
import zlib from 'node:zlib';
import { DatabaseSync } from 'node:sqlite';
import { captureTimeline, laneOpponents, leavers, matchQuality, participantRows } from './capture.ts';
import { buildStaticData, comparePatch, toPatch, type RawStaticData } from './ddragon.ts';
import type { IreliaGameRow } from './db.ts';
import { buildExclusivityIndex } from './engine/exclusivity.ts';
import type { MatchDto, TimelineDto } from './engine/parseMatch.ts';
import { recommend, type EngineSettings } from './engine/recommend.ts';
import type { BuildRecommendation, Lane } from '../shared/types.ts';
import { regionPatch, type VersionRow } from './patches.ts';
import { CacheStore, type CacheKind } from './riot/diskCache.ts';
import { PLATFORMS } from './riot/routing.ts';

const gunzip = promisify(zlib.gunzip);
const DAY = 24 * 3600_000;
/** A development key: 100 requests per 2 minutes per host, planned at 90 % (riot/rateLimiter.ts). */
export const DEV_KEY_CALLS_PER_DAY = Math.floor(100 * 0.9) * 30 * 24;
const HARVEST_POSITIONS = new Set(['TOP', 'MIDDLE']);

export interface AuditReport {
  generatedAt: string;
  dataDir: string;
  schemaVersion: number;
  dataDragon: { patch: string | null; previousPatch: string | null };
  players: {
    total: number; checked: number; unchecked: number;
    byPlatformTier: { platform: string; tier: string; players: number; checked: number; otps: number; mains: number }[];
  };
  matches: {
    total: number;
    byPatch: { patch: string; matches: number; ireliaGames: number; top: number; mid: number; other: number }[];
    byPlatform: { platform: string; matches: number; ireliaGames: number; currentPatch: string; currentPatchMatches: number; currentPatchIrelia: number }[];
  };
  /** Share of fetched matches that are a stored Irelia game. */
  ireliaShare: { matches: number; ireliaGames: number; share: number };
  pilots: {
    distinct: number;
    gamesPerPilot: { min: number; median: number; p90: number; max: number };
    top10Share: number;
    byLane: Record<string, { games: number; pilots: number; top10Share: number }>;
    bySource: Record<string, number>;
  };
  matchups: Record<string, { games: number; opponents: number; gamesInMatchupsWith15: number; share15: number }>;
  quality: {
    matchesRead: number; remakes: number; earlySurrenderFlag: number; shorterThan5Min: number;
    endResults: Record<string, number>; ireliaRemakes: number; ireliaLeavers: number; timelinesRead: number;
  };
  laneOpponent: { gamesWithTimeline: number; compared: number; disagree: number; share: number; noPositionOpponent: number };
  builds: { platform: string; patch: string; firstGame: number; builds: { gameVersion: string; firstSeen: number; games: number }[] }[];
  cache: {
    matchFiles: number; timelineFiles: number; masteryFiles: number;
    matchesWithoutFile: number; gamesWithoutTimeline: number; matchFilesNotInDb: number; timelineFilesNotInDb: number;
  };
  harvest: { cachedMatchesWithOtherIrelia: number; currentPatch: number; currentPatchNotRemake: number; players: number; byLane: Record<string, number> };
  disk: { avgParticipantRowBytes: number; avgTimelineBytes: number; matchesPerIreliaGame: number; per1000Games: { participantsMB: number; timelinesMB: number; totalMB: number } };
  /** Parameters of the yield model (docs/BUILD-SOURCING.md §Yield model), measured here. */
  yield: YieldParams;
  projection: { option: string; gamesPerPatch: number; callsPerPatch: number; daysAtDevKey: number; note: string }[];
  /** How each data-quality fix changes the lane builds (null when no Data Dragon files are cached in the folder). */
  buildImpact: BuildImpact[] | null;
}

/** One lane build under one variant: v0.3.0's behaviour, then each data-quality fix on top of the previous. */
export interface BuildImpact {
  lane: Lane;
  variant: 'v0.3.0' | '+ remakes' | '+ leavers' | '+ timeline opponent' | '+ patch per region';
  laneGames: number;
  used: number;
  discarded: Record<string, number>;
  keystone: number | null;
  core: number[];
  boots: number | null;
  skills: string;
  winRate: number | null;
  topMatchups: { id: number; games: number }[];
}

export interface YieldParams {
  pilots: number;                 // OTPs known (pilot_class otp/main when classified)
  activeShare: number | null;     // share of OTPs whose cached mastery shows Irelia played since the patch began
  activeKnown: number;            // OTPs with a cached mastery answer
  matchesPerPilotDay: number;     // ranked matches a pilot plays per day (from the matches they appear in)
  ireliaShareOfPilotMatches: number;  // of those, the share that are their Irelia game
  patchDaysElapsed: number;
  patchDays: number;
  historyIdsNow: number;          // match ids read per pilot before v3
  harvestPerMatch: number;        // other TOP / MID Irelias (current patch, not remade) per fetched match
  callsPerIreliaGameNow: number;  // (id lists + matches + timelines) / Irelia games
  regionalHosts: number;
}

const median = (xs: number[]) => (xs.length ? [...xs].sort((a, b) => a - b)[Math.floor(xs.length / 2)] : 0);
const quantile = (xs: number[], q: number) => (xs.length ? [...xs].sort((a, b) => a - b)[Math.min(xs.length - 1, Math.floor(xs.length * q))] : 0);
const share = (a: number, b: number) => (b ? a / b : 0);
const round = (x: number, d = 3) => Math.round(x * 10 ** d) / 10 ** d;

/** A cached response: a file from before 0.14.0, or an entry of the cache's database (riot/diskCache.ts). */
type Cached = string | { store: CacheStore; kind: CacheKind; scope: string; key: string };

async function listCache(root: string, kind: CacheKind, store: CacheStore | null) {
  const out = new Map<string, Cached>();   // scope/key -> where
  const dir = path.join(root, kind);
  for (const scope of await fs.readdir(dir).catch(() => [] as string[])) {
    for (const f of await fs.readdir(path.join(dir, scope)).catch(() => [] as string[])) {
      if (f.endsWith('.json.gz')) out.set(`${scope}/${f.slice(0, -'.json.gz'.length)}`, path.join(dir, scope, f));
    }
  }
  if (store) {
    for (const r of store.db.prepare('SELECT scope, key FROM entries WHERE kind = ?').all(kind) as { scope: string; key: string }[]) {
      out.set(`${r.scope}/${r.key}`, { store, kind, scope: r.scope, key: r.key });
    }
  }
  return out;
}

async function readGz<T>(where: Cached): Promise<T | null> {
  try {
    if (typeof where !== 'string') { const hit = where.store.read(where.kind, where.scope, where.key); return hit ? JSON.parse(hit.raw.toString('utf8')) as T : null; }
    return JSON.parse((await gunzip(await fs.readFile(where))).toString('utf8')) as T;
  } catch { return null; }
}

/** Runs `fn` over `items`, `width` at a time. */
async function pool<T>(items: T[], width: number, fn: (item: T) => Promise<void>) {
  let next = 0;
  await Promise.all(Array.from({ length: Math.min(width, items.length) }, async () => {
    while (next < items.length) await fn(items[next++]);
  }));
}

/** SQLite bytes of a row, roughly: values plus the record header and cell overhead. */
const rowBytes = (row: Record<string, unknown>) => Object.values(row).reduce<number>((n, v) =>
  n + (typeof v === 'string' ? v.length + 2 : v === null ? 1 : 5), 12);

/** The audit's steps, in order (its progress: step, and how far into the two that read the cache). */
export const AUDIT_STEPS = ['Reading players', 'Reading matches and games', 'Listing the API cache', 'Reading cached matches',
  'Reading cached timelines', 'Measuring the build impact'] as const;
export interface AuditProgress { step: number; steps: number; label: string; part?: number }

export async function auditData(dataDir: string, o: { now?: number; championId?: number; concurrency?: number; onProgress?: (p: AuditProgress) => void } = {}): Promise<AuditReport> {
  const now = o.now ?? Date.now();
  const championId = o.championId ?? 39;
  const progress = (step: number, part?: number) =>
    o.onProgress?.({ step, steps: AUDIT_STEPS.length, label: AUDIT_STEPS[step - 1], ...(part !== undefined ? { part } : {}) });
  const dbFile = path.join(dataDir, 'irelia.db');
  await fs.access(dbFile).catch(() => { throw new Error(`No irelia.db in ${dataDir}`); });
  const db = new DatabaseSync(dbFile, { readOnly: true });
  let store: CacheStore | null = null;
  try {
    const columns = (table: string) => new Set((db.prepare(`PRAGMA table_info(${table})`).all() as { name: string }[]).map((c) => c.name));
    const all = <T>(sql: string, ...args: (string | number)[]) => db.prepare(sql).all(...args) as T[];
    const playerCols = columns('players');
    const gameCols = columns('irelia_games');
    const schemaVersion = Number((all<{ value: string }>("SELECT value FROM meta WHERE key = 'schema_version'")[0]?.value) ?? 2);

    // ---- Data Dragon (the newest version cached; the audit never goes online)
    const versions = (await fs.readdir(path.join(dataDir, 'ddragon')).catch(() => [] as string[])).sort((a, b) => comparePatch(b, a));
    const ddPatch = versions[0] ? toPatch(versions[0]) : null;
    const ddPrev = versions.map(toPatch).find((p) => p !== ddPatch) ?? null;

    // ---- players
    progress(1);
    const hasClass = playerCols.has('pilot_class');
    const byPT = all<{ platform: string; tier: string; players: number; checked: number; otps: number; mains: number }>(`
      SELECT platform, tier, COUNT(*) AS players, SUM(is_otp IS NOT NULL) AS checked, COALESCE(SUM(is_otp = 1), 0) AS otps,
        ${hasClass ? "COALESCE(SUM(pilot_class = 'main'), 0)" : '0'} AS mains FROM players GROUP BY platform, tier ORDER BY platform, players DESC`);
    const pilotRows = all<{ puuid: string; platform: string }>(`SELECT puuid, platform FROM players WHERE is_otp = 1 ${hasClass ? "OR pilot_class IN ('otp', 'main')" : ''}`);
    const pilotSet = new Map(pilotRows.map((p) => [p.puuid, p.platform]));

    // ---- matches and games
    progress(2);
    const matchRows = all<{ match_id: string; platform: string; patch: string; game_version: string; game_creation: number; game_duration: number; has_irelia: number }>(
      'SELECT match_id, platform, patch, game_version, game_creation, game_duration, has_irelia FROM matches');
    const games = all<{ match_id: string; puuid: string; patch: string; position: string; opponent_champion_id: number | null; source: string }>(
      `SELECT match_id, puuid, patch, position, opponent_champion_id, ${gameCols.has('pilot_source') ? "COALESCE(pilot_source, 'otp')" : "'otp'"} AS source FROM irelia_games`);
    const gameById = new Map(games.map((g) => [g.match_id, g]));
    const versionRows: VersionRow[] = [];
    {
      const m = new Map<string, VersionRow>();
      for (const r of matchRows) {
        const k = `${r.platform}|${r.game_version}`;
        const v = m.get(k) ?? { platform: r.platform, game_version: r.game_version, patch: r.patch, first_seen: r.game_creation, last_seen: r.game_creation, games: 0 };
        v.first_seen = Math.min(v.first_seen, r.game_creation); v.last_seen = Math.max(v.last_seen, r.game_creation); v.games++;
        m.set(k, v);
      }
      versionRows.push(...m.values());
    }
    const fallback = { patch: ddPatch ?? '0.0', previousPatch: ddPrev ?? '0.0' };
    const platforms = [...new Set(matchRows.map((r) => r.platform))].sort();
    const regions = new Map(platforms.map((p) => [p, regionPatch(p, versionRows, fallback, now)]));

    const byPatch = new Map<string, { patch: string; matches: number; ireliaGames: number; top: number; mid: number; other: number }>();
    for (const r of matchRows) {
      const e = byPatch.get(r.patch) ?? { patch: r.patch, matches: 0, ireliaGames: 0, top: 0, mid: 0, other: 0 };
      e.matches++;
      const g = gameById.get(r.match_id);
      if (g) { e.ireliaGames++; if (g.position === 'TOP') e.top++; else if (g.position === 'MIDDLE') e.mid++; else e.other++; }
      byPatch.set(r.patch, e);
    }
    const byPlatform = platforms.map((p) => {
      const cur = regions.get(p)!.patch;
      const ms = matchRows.filter((r) => r.platform === p);
      return {
        platform: p, matches: ms.length, ireliaGames: ms.filter((r) => gameById.has(r.match_id)).length, currentPatch: cur,
        currentPatchMatches: ms.filter((r) => r.patch === cur).length,
        currentPatchIrelia: ms.filter((r) => r.patch === cur && gameById.has(r.match_id)).length,
      };
    });

    // ---- pilots and matchups (build games: OTP sources; every game when the database predates sources)
    const buildGames = games.filter((g) => g.source !== 'me');
    const perPilot = new Map<string, number>();
    for (const g of buildGames) perPilot.set(g.puuid, (perPilot.get(g.puuid) ?? 0) + 1);
    const counts = [...perPilot.values()];
    const topShare = (gs: typeof buildGames) => {
      const m = new Map<string, number>();
      for (const g of gs) m.set(g.puuid, (m.get(g.puuid) ?? 0) + 1);
      return { pilots: m.size, top10Share: share([...m.values()].sort((a, b) => b - a).slice(0, 10).reduce((a, b) => a + b, 0), gs.length) };
    };
    const lanes = ['TOP', 'MIDDLE'];
    const byLane = Object.fromEntries(lanes.map((l) => { const gs = buildGames.filter((g) => g.position === l); return [l, { games: gs.length, ...topShare(gs) }]; }));
    const bySource: Record<string, number> = {};
    for (const g of games) bySource[g.source] = (bySource[g.source] ?? 0) + 1;
    const matchups = Object.fromEntries(lanes.map((l) => {
      const gs = buildGames.filter((g) => g.position === l && regions.get(g.match_id.split('_')[0])?.patch === g.patch);
      const m = new Map<number, number>();
      for (const g of gs) if (g.opponent_champion_id) m.set(g.opponent_champion_id, (m.get(g.opponent_champion_id) ?? 0) + 1);
      const in15 = [...m.values()].filter((n) => n >= 15).reduce((a, b) => a + b, 0);
      return [l, { games: gs.length, opponents: m.size, gamesInMatchupsWith15: in15, share15: round(share(in15, gs.length)) }];
    }));

    // ---- cache
    progress(3);
    const cacheRoot = path.join(dataDir, 'cache');
    try { store = new CacheStore(cacheRoot); } catch { store = null; }
    const matchFiles = await listCache(cacheRoot, 'match', store);
    const timelineFiles = await listCache(cacheRoot, 'timeline', store);
    const masteryFiles = await listCache(cacheRoot, 'mastery', store);
    const inDb = new Set(matchRows.map((r) => `${r.platform}/${r.match_id}`));
    const cache = {
      matchFiles: matchFiles.size, timelineFiles: timelineFiles.size, masteryFiles: masteryFiles.size,
      matchesWithoutFile: matchRows.filter((r) => !matchFiles.has(`${r.platform}/${r.match_id}`)).length,
      gamesWithoutTimeline: games.filter((g) => !timelineFiles.has(`${g.match_id.split('_')[0]}/${g.match_id}`)).length,
      matchFilesNotInDb: [...matchFiles.keys()].filter((k) => !inDb.has(k)).length,
      timelineFilesNotInDb: [...timelineFiles.keys()].filter((k) => !gameById.has(k.split('/')[1])).length,
    };

    // ---- every cached match of the database: quality, harvest potential, pilots' play rate, participant row size
    const remakeOf = new Map<string, boolean>();
    const leaverOf = new Map<string, boolean>();
    const timelineOppOf = new Map<string, number | null>();
    const quality = { matchesRead: 0, remakes: 0, earlySurrenderFlag: 0, shorterThan5Min: 0, endResults: {} as Record<string, number>, ireliaRemakes: 0, ireliaLeavers: 0, timelinesRead: 0 };
    const harvest = { cachedMatchesWithOtherIrelia: 0, currentPatch: 0, currentPatchNotRemake: 0, players: 0, byLane: {} as Record<string, number> };
    const harvestPlayers = new Set<string>();
    const pilotMatches = new Map<string, number[]>();     // pilot → creation times of their current-patch matches
    const pilotIrelia = new Map<string, number>();
    let rowBytesSum = 0, rowCount = 0;
    const rows = matchRows.filter((r) => matchFiles.has(`${r.platform}/${r.match_id}`));
    let read = 0;
    progress(4, 0);
    await pool(rows, o.concurrency ?? 8, async (r) => {
      const match = await readGz<MatchDto>(matchFiles.get(`${r.platform}/${r.match_id}`)!);
      if (++read % 500 === 0) progress(4, read / rows.length);
      if (!match?.info?.participants) return;
      quality.matchesRead++;
      const q = matchQuality(match);
      if (q.remake) quality.remakes++;
      if (match.info.participants.some((p) => p.gameEndedInEarlySurrender)) quality.earlySurrenderFlag++;
      if (match.info.gameDuration < 300) quality.shorterThan5Min++;
      const end = q.endResult ?? '(not sent)';
      quality.endResults[end] = (quality.endResults[end] ?? 0) + 1;
      const current = regions.get(r.platform)?.patch === r.patch;
      for (const row of participantRows(match)) { rowBytesSum += rowBytes(row as unknown as Record<string, unknown>); rowCount++; }

      const irelia = match.info.participants.find((p) => p.championId === championId);
      const stored = gameById.get(r.match_id);
      if (stored) remakeOf.set(r.match_id, q.remake);
      if (stored && q.remake) quality.ireliaRemakes++;
      // Another player's Irelia: known pilots' unstored games are older-patch ones their own history skipped.
      if (irelia && !stored && !pilotSet.has(irelia.puuid) && HARVEST_POSITIONS.has(irelia.teamPosition)) {
        harvest.cachedMatchesWithOtherIrelia++;
        harvest.byLane[irelia.teamPosition] = (harvest.byLane[irelia.teamPosition] ?? 0) + 1;
        harvestPlayers.add(irelia.puuid);
        if (current) { harvest.currentPatch++; if (!q.remake) harvest.currentPatchNotRemake++; }
      }
      if (current) {
        for (const p of match.info.participants) {
          if (!pilotSet.has(p.puuid)) continue;
          const ts = pilotMatches.get(p.puuid) ?? [];
          ts.push(match.info.gameCreation);
          pilotMatches.set(p.puuid, ts);
          if (stored?.puuid === p.puuid) pilotIrelia.set(p.puuid, (pilotIrelia.get(p.puuid) ?? 0) + 1);
        }
      }
    });
    harvest.players = harvestPlayers.size;

    // ---- timelines of stored games: leavers, the lane opponent from positions, blob size
    progress(5);
    const laneOpponent = { gamesWithTimeline: 0, compared: 0, disagree: 0, share: 0, noPositionOpponent: 0 };
    let blobSum = 0, blobs = 0;
    const withTl = games.filter((g) => timelineFiles.has(`${g.match_id.split('_')[0]}/${g.match_id}`) && matchFiles.has(`${g.match_id.split('_')[0]}/${g.match_id}`));
    read = 0;
    await pool(withTl, o.concurrency ?? 8, async (g) => {
      const platform = g.match_id.split('_')[0];
      const [match, timeline] = await Promise.all([
        readGz<MatchDto>(matchFiles.get(`${platform}/${g.match_id}`)!), readGz<TimelineDto>(timelineFiles.get(`${platform}/${g.match_id}`)!),
      ]);
      if (++read % 200 === 0) progress(5, read / withTl.length);
      if (!match || !timeline?.info?.frames) return;
      quality.timelinesRead++;
      const cap = captureTimeline(timeline);
      blobSum += JSON.stringify(cap).length; blobs++;
      const left = leavers(cap).length > 0;
      leaverOf.set(g.match_id, left);
      if (left) quality.ireliaLeavers++;
      const me = match.info.participants.find((p) => p.puuid === g.puuid && p.championId === championId);
      if (!me) return;
      laneOpponent.gamesWithTimeline++;
      const opp = laneOpponents(match, cap, me);
      timelineOppOf.set(g.match_id, opp.timeline);
      if (opp.position === null) laneOpponent.noPositionOpponent++;
      if (opp.position !== null && opp.timeline !== null) {
        laneOpponent.compared++;
        if (opp.position !== opp.timeline) laneOpponent.disagree++;
      }
    });
    laneOpponent.share = round(share(laneOpponent.disagree, laneOpponent.compared));

    // ---- activity of OTPs from cached mastery answers (top?count=1 before v3, keyed by PUUID; -topN after)
    let activeKnown = 0, active = 0;
    const topFiles = new Map([...masteryFiles].filter(([k]) => /-top\d+$/.test(k)).map(([k, f]) => [k.replace(/-top\d+$/, ''), f]));
    for (const [puuid, platform] of pilotSet) {
      const file = topFiles.get(`${platform}/${puuid}`) ?? masteryFiles.get(`${platform}/${puuid}`);
      if (!file) continue;
      const top = await readGz<{ championId: number; lastPlayTime?: number }[]>(file);
      const e = top?.find((m) => m.championId === championId);
      if (!e?.lastPlayTime) continue;
      activeKnown++;
      if (e.lastPlayTime >= (regions.get(platform)?.start ?? now - 14 * DAY)) active++;
    }

    // ---- builds within each patch, and when each region's current patch was first seen
    const builds = platforms.flatMap((p) => {
      const patches = [...new Set(versionRows.filter((v) => v.platform === p).map((v) => v.patch))].sort((a, b) => comparePatch(b, a)).slice(0, 2);
      return patches.map((patch) => {
        const vs = versionRows.filter((v) => v.platform === p && v.patch === patch).sort((a, b) => a.first_seen - b.first_seen);
        return { platform: p, patch, firstGame: vs[0]?.first_seen ?? 0, builds: vs.map((v) => ({ gameVersion: v.game_version, firstSeen: v.first_seen, games: v.games })) };
      });
    });

    // ---- yield parameters
    const rates = [...pilotMatches.entries()].filter(([, ts]) => ts.length >= 5).map(([, ts]) => ts.length / Math.max(1, (Math.max(...ts) - Math.min(...ts)) / DAY));
    const pilotShare = share([...pilotIrelia.values()].reduce((a, b) => a + b, 0), [...pilotMatches.values()].reduce((a, ts) => a + ts.length, 0));
    const elapsed = Math.max(...platforms.map((p) => (now - (regions.get(p)?.start ?? now)) / DAY), 0);
    const currentMatches = byPlatform.reduce((a, p) => a + p.currentPatchMatches, 0);
    const y: YieldParams = {
      pilots: pilotSet.size,
      activeShare: activeKnown ? round(active / activeKnown) : null,
      activeKnown,
      matchesPerPilotDay: round(median(rates), 2),
      ireliaShareOfPilotMatches: round(pilotShare),
      patchDaysElapsed: round(Math.min(14, elapsed), 1),
      patchDays: 14,
      historyIdsNow: 20,
      harvestPerMatch: round(share(harvest.currentPatchNotRemake, currentMatches)),
      callsPerIreliaGameNow: round(share(pilotSet.size + matchRows.length + games.length, games.length), 2),
      regionalHosts: new Set(platforms.map((p) => PLATFORMS[p as keyof typeof PLATFORMS]?.region ?? p)).size || 1,
    };

    const avgRow = rowCount ? rowBytesSum / rowCount : 0;
    const avgBlob = blobs ? blobSum / blobs : 0;
    const mpg = share(matchRows.length, games.length);
    const disk = {
      avgParticipantRowBytes: Math.round(avgRow), avgTimelineBytes: Math.round(avgBlob), matchesPerIreliaGame: round(mpg, 2),
      per1000Games: {
        participantsMB: round((1000 * mpg * 10 * avgRow) / 1e6, 1),
        timelinesMB: round((1000 * avgBlob) / 1e6, 1),
        totalMB: round((1000 * (mpg * 10 * avgRow + avgBlob)) / 1e6, 1),
      },
    };

    // ---- build impact of the data-quality fixes, with the Data Dragon files cached in the folder
    progress(6);
    let buildImpact: BuildImpact[] | null = null;
    const staticData = versions[0] ? await loadCachedStatic(path.join(dataDir, 'ddragon', versions[0]), versions[0], ddPrev ?? ddPatch!) : null;
    if (staticData) {
      const rows = all<IreliaGameRow>(`SELECT g.*, m.game_version FROM irelia_games g JOIN matches m USING(match_id)
        ${gameCols.has('pilot_source') ? "WHERE COALESCE(g.pilot_source, 'otp') = 'otp'" : ''}`)
        .map((g) => ({ ...g, opponent_champion_id: gameCols.has('position_opponent_id') ? g.position_opponent_id ?? g.opponent_champion_id : g.opponent_champion_id }));
      buildImpact = buildImpactOf(rows, staticData, { remakeOf, leaverOf, timelineOppOf, regions });
    }

    return {
      generatedAt: new Date(now).toISOString(), dataDir, schemaVersion, dataDragon: { patch: ddPatch, previousPatch: ddPrev },
      players: { total: byPT.reduce((a, r) => a + r.players, 0), checked: byPT.reduce((a, r) => a + r.checked, 0), unchecked: byPT.reduce((a, r) => a + r.players - r.checked, 0), byPlatformTier: byPT },
      matches: { total: matchRows.length, byPatch: [...byPatch.values()].sort((a, b) => comparePatch(b.patch, a.patch)), byPlatform },
      ireliaShare: { matches: matchRows.length, ireliaGames: games.length, share: round(share(games.length, matchRows.length)) },
      pilots: {
        distinct: perPilot.size,
        gamesPerPilot: { min: counts.length ? Math.min(...counts) : 0, median: median(counts), p90: quantile(counts, 0.9), max: counts.length ? Math.max(...counts) : 0 },
        top10Share: round(topShare(buildGames).top10Share), byLane, bySource,
      },
      matchups, quality, laneOpponent, builds, cache, harvest, disk, yield: y, buildImpact,
      projection: projectYield(y, {
        currentGames: games.filter((g) => regions.get(g.match_id.split('_')[0])?.patch === g.patch).length,
        ladderPlayers: byPT.reduce((n, r) => n + r.players, 0),
      }),
    };
  } finally {
    store?.close();
    db.close();
  }
}

/**
 * The yield model (docs/BUILD-SOURCING.md §Yield model): current-patch Irelia games per full patch, the API calls
 * they cost on the regional host (match ids L, matches M, timelines = games G) and on the platform host (mastery
 * checks C), and how many days a development key needs to collect them. Each option adds to the previous one.
 *   P pilots, a active share, m = matches a pilot plays per patch, s Irelia share of them, h harvestable per match,
 *   N ladder players, B = DEV_KEY_CALLS_PER_DAY per host.
 * Inactive pilots have no current-patch games: before the lastPlayTime filter each still costs an id list and the
 * one older-patch match that stops the scan.
 */
export function projectYield(y: YieldParams, o: { currentGames: number; ladderPlayers?: number }) {
  const P = y.pilots, a = y.activeShare ?? 0.6, s = y.ireliaShareOfPilotMatches || 0.5, h = y.harvestPerMatch;
  const m = (y.matchesPerPilotDay || 6) * y.patchDays;
  const N = o.ladderPlayers ?? P * 200;
  const B = DEV_KEY_CALLS_PER_DAY;
  const pages = Math.max(1, Math.ceil(m / 100));
  const rows: { option: string; gamesPerPatch: number; callsPerPatch: number; daysAtDevKey: number; note: string }[] = [];
  const push = (option: string, x: { G: number; M: number; L: number; C: number; hosts: number; parallel: boolean; budget?: number }, note: string) => {
    const regional = x.M + x.G + x.L;
    const budget = (x.budget ?? B) * x.hosts;
    // One loop shares its time between hosts; per-host lanes run them side by side.
    const days = x.parallel ? Math.max(regional, x.C) / budget : (regional + x.C) / budget;
    rows.push({ option, gamesPerPatch: Math.round(x.G), callsPerPatch: Math.round(regional + x.C), daysAtDevKey: round(days, 2),
      note: `${note}; ${round(x.G ? regional / x.G : 0, 2)} regional calls per game` });
  };
  // Mastery checks per patch: before v3 every ladder player every 3 days; after, pilots every 3 days, others every 14.
  const C0 = (N * y.patchDays) / 3;
  const C1 = (P * y.patchDays) / 3 + (N - P);
  const now = { G: P * a * Math.min(y.historyIdsNow, m) * s, M: P * a * Math.min(y.historyIdsNow, m) + P * (1 - a), L: P, C: C0, hosts: 1, parallel: false };
  push('as now (20 newest ids)', now, `measured: ${o.currentGames} games on the current patch so far`);
  const pag = { ...now, G: P * a * m * s, M: P * a * m + P * (1 - a), L: P * a * pages + P * (1 - a) };
  push('+ whole-patch pagination', pag, `${Math.round(m)} matches per active pilot per patch`);
  const act = { ...pag, M: P * a * m, L: P * a * pages, C: C1 };
  push('+ lastPlayTime filter', act, `skips ${Math.round(P * (1 - a))} inactive pilots; others rechecked every 14 days`);
  const har = { ...act, G: act.G + act.M * h };
  push('+ harvesting other Irelias', har, `${h} harvestable per match, one timeline call each`);
  const lanes = { ...har, parallel: true };
  push('+ per-host queues', lanes, 'match-v5 never waits for mastery checks');
  const k = 3 / Math.max(1, y.regionalHosts);
  const regions = { ...lanes, G: lanes.G * k, M: lanes.M * k, L: lanes.L * k, C: lanes.C * k, hosts: 3 };
  push('+ EUW1 and NA1', regions, 'as many pilots per region (assumed); three regional and platform hosts');
  const tiers = { ...regions, G: regions.G * 2, M: regions.M * 2, L: regions.L * 2, C: regions.C * 2 };
  push('+ Master / Diamond I tiers', tiers, 'twice the pilots and ladder (assumed; the audit counts pilots per tier)');
  push('+ production key', { ...tiers, budget: 30_000 * 6 * 24 * 0.9 }, 'budget ×66; a personal key keeps dev limits but never expires');
  return rows;
}

const ENGINE: EngineSettings = {
  minMatchupGames: 15, generalPriorWeight: 0.15, minLaneGames: 30, otherLaneWeight: 0.15,
  previousPatchWeight: 0.35, minCurrentPatchGames: 40, startingItemWindowMs: 60_000,
};

/** Data Dragon from the folder's own cache (data/ddragon/<version>/), never the network. */
async function loadCachedStatic(dir: string, version: string, previousPatch: string) {
  const read = async (f: string) => JSON.parse(await fs.readFile(path.join(dir, f), 'utf8'));
  try {
    const raw: RawStaticData = {
      item: await read('item.json'), champion: await read('champion.json'), runesReforged: await read('runesReforged.json'),
      summoner: await read('summoner.json'), champ: await read('Irelia.json'),
    };
    return buildStaticData(version, previousPatch, raw);
  } catch { return null; }
}

/**
 * The lane builds of the stored OTP games under v0.3.0's rules, then with each data-quality fix added in turn:
 * remakes out, leavers out, the timeline's lane opponent, and each region's own current patch.
 */
export function buildImpactOf(
  rows: IreliaGameRow[], data: ReturnType<typeof buildStaticData>,
  f: { remakeOf: Map<string, boolean>; leaverOf: Map<string, boolean>; timelineOppOf: Map<string, number | null>; regions: Map<string, { patch: string; previousPatch: string }> },
): BuildImpact[] {
  const excl = buildExclusivityIndex(data);
  // v0.3.0 had no remake check: lift short games over the threshold so only the later variants drop them.
  const v030 = rows.map((r) => ({ ...r, remake: 0, leaver: 0, game_duration: Math.max(r.game_duration, 300) }));
  const remakes = rows.map((r) => ({ ...r, remake: f.remakeOf.get(r.match_id) ? 1 : 0, leaver: 0 }));
  const leavers = remakes.map((r) => ({ ...r, leaver: f.leaverOf.get(r.match_id) ? 1 : 0 }));
  const opponents = leavers.map((r) => ({ ...r, opponent_champion_id: f.timelineOppOf.get(r.match_id) ?? r.opponent_champion_id }));
  const variants: [BuildImpact['variant'], IreliaGameRow[], boolean][] = [
    ['v0.3.0', v030, false], ['+ remakes', remakes, false], ['+ leavers', leavers, false], ['+ timeline opponent', opponents, false], ['+ patch per region', opponents, true],
  ];
  const out: BuildImpact[] = [];
  for (const lane of ['TOP', 'MIDDLE'] as Lane[]) {
    for (const [variant, rs, perRegion] of variants) {
      const b: BuildRecommendation = recommend(rs, data, excl, null, ENGINE, lane, perRegion ? (p) => f.regions.get(p) : undefined);
      const m = new Map<number, number>();
      for (const r of rs) if (r.position === lane && r.opponent_champion_id) m.set(r.opponent_champion_id, (m.get(r.opponent_champion_id) ?? 0) + 1);
      out.push({
        lane, variant, laneGames: b.sample.laneGames, used: b.validation.freshGames + b.validation.previousPatchGames,
        discarded: { stale: b.validation.staleDiscarded, ...b.validation.invalidReasons },
        keystone: b.runes?.value.perks[0] ?? null, core: b.coreBuild[0]?.value ?? [], boots: b.boots[0]?.value ?? null,
        skills: b.skills?.value.priority.join('>') ?? '', winRate: b.winRate,
        topMatchups: [...m].sort((a, c) => c[1] - a[1]).slice(0, 3).map(([id, games]) => ({ id, games })),
      });
    }
  }
  return out;
}

/** A compact plain-text summary for the console. */
export function formatAudit(r: AuditReport) {
  const pct = (x: number | null) => (x === null ? '—' : `${(x * 100).toFixed(1)}%`);
  const lines: string[] = [];
  const t = (title: string) => lines.push('', title, '-'.repeat(title.length));
  lines.push(`Irelia Companion data audit · ${r.dataDir} · schema v${r.schemaVersion} · Data Dragon ${r.dataDragon.patch ?? '?'}`);
  t('Players');
  lines.push(`${r.players.total.toLocaleString()} players: ${r.players.checked.toLocaleString()} checked, ${r.players.unchecked.toLocaleString()} unchecked`);
  for (const p of r.players.byPlatformTier) lines.push(`  ${p.platform.padEnd(5)} ${p.tier.padEnd(12)} ${String(p.players).padStart(7)} players  ${String(p.checked).padStart(7)} checked  ${String(p.otps).padStart(4)} OTPs  ${String(p.mains).padStart(4)} mains`);
  t('Matches and Irelia games');
  lines.push(`${r.matches.total.toLocaleString()} matches, ${r.ireliaShare.ireliaGames.toLocaleString()} Irelia games (${pct(r.ireliaShare.share)} of fetched matches)`);
  for (const p of r.matches.byPatch) lines.push(`  ${p.patch.padEnd(6)} ${String(p.matches).padStart(6)} matches ${String(p.ireliaGames).padStart(5)} Irelia: ${p.top} top / ${p.mid} mid / ${p.other} other`);
  for (const p of r.matches.byPlatform) lines.push(`  ${p.platform.padEnd(5)} current ${p.currentPatch}: ${p.currentPatchMatches} matches, ${p.currentPatchIrelia} Irelia games`);
  t('Pilots and matchups');
  lines.push(`${r.pilots.distinct} pilots · games per pilot: median ${r.pilots.gamesPerPilot.median}, p90 ${r.pilots.gamesPerPilot.p90}, max ${r.pilots.gamesPerPilot.max} · top-10 pilots ${pct(r.pilots.top10Share)}`);
  lines.push(`  by source: ${Object.entries(r.pilots.bySource).map(([k, v]) => `${k} ${v}`).join(', ') || '—'}`);
  for (const [lane, m] of Object.entries(r.matchups)) lines.push(`  ${lane.padEnd(6)} ${m.games} games, ${m.opponents} opponents, ${pct(m.share15)} in matchups with 15+ games, top-10 pilots ${pct(r.pilots.byLane[lane]?.top10Share ?? 0)}`);
  t('Data quality');
  lines.push(`${r.quality.matchesRead} matches read: ${r.quality.remakes} remakes (${r.quality.earlySurrenderFlag} by the early-surrender flag, ${r.quality.shorterThan5Min} under 5 min)`);
  lines.push(`  end results: ${Object.entries(r.quality.endResults).map(([k, v]) => `${k} ${v}`).join(', ')}`);
  lines.push(`  stored Irelia games: ${r.quality.ireliaRemakes} remakes, ${r.quality.ireliaLeavers} with a leaver (of ${r.quality.timelinesRead} timelines)`);
  lines.push(`  lane opponent: teamPosition and timeline differ in ${r.laneOpponent.disagree} of ${r.laneOpponent.compared} games (${pct(r.laneOpponent.share)}); ${r.laneOpponent.noPositionOpponent} without a position opponent`);
  t('Patches and builds');
  for (const b of r.builds) lines.push(`  ${b.platform.padEnd(5)} ${b.patch.padEnd(6)} first game ${new Date(b.firstGame).toISOString().slice(0, 16)} · ${b.builds.map((v) => `${v.gameVersion} (${v.games})`).join(', ')}`);
  t('Cache coverage');
  const c = r.cache;
  lines.push(`${c.matchFiles} match files, ${c.timelineFiles} timeline files, ${c.masteryFiles} mastery files`);
  lines.push(`  in the database but not on disk: ${c.matchesWithoutFile} matches, ${c.gamesWithoutTimeline} Irelia timelines`);
  lines.push(`  on disk but not in the database: ${c.matchFilesNotInDb} matches, ${c.timelineFilesNotInDb} timelines`);
  t('Harvesting');
  lines.push(`${r.harvest.cachedMatchesWithOtherIrelia} cached matches have another player's top/mid Irelia (${r.harvest.players} players); ${r.harvest.currentPatchNotRemake} on the current patch, not remade → that many games for one timeline call each`);
  t('Disk');
  lines.push(`participant row ≈ ${r.disk.avgParticipantRowBytes} B, timeline ≈ ${(r.disk.avgTimelineBytes / 1024).toFixed(1)} KB, ${r.disk.matchesPerIreliaGame} matches per Irelia game → per 1,000 games ≈ ${r.disk.per1000Games.totalMB} MB (${r.disk.per1000Games.participantsMB} participants + ${r.disk.per1000Games.timelinesMB} timelines)`);
  t('Yield model');
  const y = r.yield;
  lines.push(`pilots ${y.pilots}, active ${pct(y.activeShare)} (of ${y.activeKnown} with cached mastery), ${y.matchesPerPilotDay} matches/pilot/day, Irelia share ${pct(y.ireliaShareOfPilotMatches)}, patch day ${y.patchDaysElapsed}/${y.patchDays}, harvest ${y.harvestPerMatch}/match, ${y.callsPerIreliaGameNow} calls/game now`);
  for (const p of r.projection) lines.push(`  ${p.option.padEnd(30)} ${String(p.gamesPerPatch).padStart(7)} games/patch ${String(p.callsPerPatch).padStart(8)} calls  ${String(p.daysAtDevKey).padStart(6)} days at a dev key · ${p.note}`);
  t('Lane builds: v0.3.0, then each data-quality fix');
  if (!r.buildImpact) lines.push('(no Data Dragon files cached in this folder: start the app once to cache them)');
  for (const b of r.buildImpact ?? []) {
    const gone = Object.entries(b.discarded).filter(([, n]) => n).map(([k, n]) => `${k} ${n}`).join(', ');
    lines.push(`  ${b.lane.padEnd(6)} ${b.variant.padEnd(20)} ${String(b.used).padStart(5)} used${gone ? ` (dropped: ${gone})` : ''} · keystone ${b.keystone ?? '—'} · core ${b.core.join(' ') || '—'} · boots ${b.boots ?? '—'} · ${b.skills || '—'} · WR ${pct(b.winRate)} · vs ${b.topMatchups.map((m) => `${m.id}×${m.games}`).join(' ')}`);
  }
  return lines.join('\n');
}
