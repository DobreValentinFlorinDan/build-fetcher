// Sourcing v2: which Riot request each host makes next, and what it does with the answer.
//
// Every platform host (ladders, mastery, ranks) and every regional host (match ids, matches, timelines) has its own
// rate budget, so each one gets its own lane (worker.ts) that asks `nextJob` for the most valuable thing to do:
//  - platform lane: reseed a due ladder, then mastery checks (players seen playing Irelia first, then pilots whose
//    activity needs refreshing, then unchecked ladder players), and rank lookups for harvested pilots;
//  - regional lane: the user's own games, then whichever is worth more in expected Irelia games per request:
//    the best queued match (or harvested timeline), or reading the match history of the most promising due pilot.
// Database writes are synchronous transactions (ingest.ts), so lanes never interleave inside one.
import { config } from '../config.ts';
import type { Db, PilotSource, PlayerRow, QueuedMatch } from '../db.ts';
import { regionPatch, PATCH_CYCLE_MS, type RegionPatch } from '../patches.ts';
import type { MasteryDto, RiotApi } from '../riot/client.ts';
import { normalizePlatform, PLATFORMS, type Platform, type Region, type Tier } from '../riot/routing.ts';
import type { ScrapeScope } from '../scrapeScope.ts';
import { fetchHarvested, fetchMatch, storeGame, storeMatch, type Ingest, type IngestEnv } from './ingest.ts';
import { classifyPilot, historyScore, ireliaShare, matchScore, OWN_GAMES_SCORE } from './pilots.ts';
import { inOrder } from './pool.ts';

export interface SourcingEnv {
  db: Db;
  riot: RiotApi;
  scope: () => ScrapeScope;
  /** Data Dragon's patch pair: the fallback for a platform with no games yet. */
  fallbackPatch: () => { patch: string; previousPatch: string };
  me: () => { puuid: string; platform: Platform } | null;
  concurrency: number;
  log: (text: string) => void;
  now?: () => number;
  /** Re-check intervals (ms): pilots' activity, everyone else's mastery. */
  recheckPilotsMs?: number;
  recheckOthersMs?: number;
  reseedMs?: number;
  rescanPilotMs?: number;
}

/** A unit of work for one host. `score` is expected Irelia games per request (regional) or a priority (platform). */
export interface Job { label: string; score: number; run: (signal: AbortSignal) => Promise<void> }

/** A lane of work: one Riot host and the platforms it serves. */
export type LaneKey = { kind: 'platform'; platform: Platform } | { kind: 'regional'; region: Region; platforms: Platform[] };

export const laneHost = (k: LaneKey) => (k.kind === 'platform' ? `${k.platform.toLowerCase()}.api.riotgames.com` : `${k.region.toLowerCase()}.api.riotgames.com`);

const titleCase = (t: string) => t[0] + t.slice(1).toLowerCase();
const matchNumber = (id: string) => Number(id.slice(id.indexOf('_') + 1)) || 0;

export class Sourcing {
  private regionCache = new Map<string, { at: number; value: RegionPatch }>();
  /** Own games are due when the account is first known, after a game ends (with retries) and every 30 minutes. */
  private ownDueAt: number[] = [];
  private ownLastAt = 0;

  constructor(private env: SourcingEnv) {}

  private now() { return this.env.now?.() ?? Date.now(); }
  private get db() { return this.env.db; }
  private get scope() { return this.env.scope(); }

  // ------------------------------------------------------------------------------- patches

  /** The platform's current patch and its window (server/patches.ts), re-read at most every 2 s. */
  region(platform: Platform): RegionPatch {
    const hit = this.regionCache.get(platform);
    if (hit && this.now() - hit.at < 2000) return hit.value;
    const value = regionPatch(platform, this.db.gameVersions(), this.env.fallbackPatch(), this.now());
    this.regionCache.set(platform, { at: this.now(), value });
    return value;
  }
  /** Forget cached regions (a new patch was seen, or tests moved the clock). */
  invalidate() { this.regionCache.clear(); }

  ingestEnv(): IngestEnv {
    return {
      db: this.db, riot: this.env.riot, championId: config.championId, harvest: this.scope.harvestIrelias,
      region: (p) => this.region(p), mePuuid: () => this.env.me()?.puuid ?? null,
    };
  }

  // ------------------------------------------------------------------------------- lanes

  /** The lanes the current scope (and the user's own account) need: one per platform host, one per regional host. */
  lanes(): LaneKey[] {
    const scope = this.scope;
    const me = this.env.me();
    const platforms = [...new Set([...scope.platforms, ...(me && scope.ownGames ? [me.platform] : [])])];
    const regions = new Map<Region, Platform[]>();
    for (const p of platforms) regions.set(PLATFORMS[p].region, [...(regions.get(PLATFORMS[p].region) ?? []), p]);
    return [
      ...scope.platforms.map((platform) => ({ kind: 'platform' as const, platform })),
      ...[...regions].map(([region, ps]) => ({ kind: 'regional' as const, region, platforms: ps })),
    ];
  }

  nextJob(lane: LaneKey): Job | null {
    return lane.kind === 'platform' ? this.platformJob(lane.platform) : this.regionalJob(lane.platforms);
  }

  // ------------------------------------------------------------------------------- platform host

  private platformJob(platform: Platform): Job | null {
    const scope = this.scope, now = this.now();
    const tier = scope.tiers.find((t) => now - Number(this.db.getMeta(`seeded_at:${platform}:${t}`) ?? 0) > (this.env.reseedMs ?? config.reseedAfterMs));
    if (tier) {
      return { label: `Seeding ${platform} ${titleCase(tier)} ladder`, score: 3, run: async (signal) => {
        this.env.log(`Loaded ${await this.seedTier(platform, tier, signal)} ${platform} ${titleCase(tier)} players`);
      } };
    }
    // Pilots are rechecked every few days, and at once when this platform moves to a new patch (0.21.1): a check from
    // before the patch began can't show Irelia played on it, so until rechecked they'd be left out of the history
    // scans (duePilots reads only pilots active since the patch began), and a new patch's first days filled slowly.
    const region = this.region(platform);
    const patchBegan = region.startKnown ? region.start : 0;
    // Once per platform and patch (0.21.2): pilots checked since the patch began but still showing no Irelia game on it
    // are checked again, so a check that read a mastery answer cached before the patch (0.21.1) doesn't count.
    const marker = `activity_patch:${platform}`;
    if (region.startKnown && this.db.getMeta(marker) !== region.patch) {
      const again = this.db.forgetActivityChecks(platform, region.start);
      this.db.setMeta(marker, region.patch);
      if (again) this.env.log(`${platform} ${region.patch}: checking ${again} pilots' Irelia activity again`);
    }
    const work = (lo: number, hi: number) => this.db.masteryWork(platform, {
      pilotBefore: Math.max(now - (this.env.recheckPilotsMs ?? config.recheckOtpAfterMs), patchBegan),
      otherBefore: now - (this.env.recheckOthersMs ?? config.recheckOthersAfterMs),
      lo, hi, limit: this.env.concurrency * 4,
    });
    const urgent = work(0, 1);
    if (urgent.length) return this.masteryJob(platform, urgent, urgent[0].prio === 0 ? 'Checking harvested Irelia players' : 'Refreshing pilot activity', 2);
    if (scope.lookupHarvestedRanks) {
      const unranked = this.db.unrankedCandidates(platform, this.env.concurrency * 4);
      if (unranked.length) return { label: `Looking up ${platform} harvested pilots' ranks`, score: 1.5, run: (signal) => this.lookupRanks(platform, unranked, signal) };
    }
    const rest = work(2, 3);
    if (rest.length) return this.masteryJob(platform, rest, `Identifying ${platform} Irelia pilots`, 1);
    return null;
  }

  private masteryJob(platform: Platform, players: PlayerRow[], label: string, score: number): Job {
    return { label, score, run: (signal) => this.checkPlayers(players, signal, (p, u) => {
      if (u.pilotClass !== 'none' && p.pilot_class !== u.pilotClass) {
        this.env.log(`New ${platform} Irelia ${u.pilotClass === 'otp' ? 'OTP' : 'main'} found (${Math.round(u.ireliaPoints / 1000)}k mastery)`);
      }
    }) };
  }

  async seedTier(platform: Platform, tier: Tier, signal: AbortSignal) {
    const scope = this.scope;
    const entries = await this.env.riot.leagueEntries(platform, tier, scope.divisions, scope.maxLeaguePagesPerDivision, signal);
    this.db.tx(() => { for (const e of entries) if (e.puuid) this.db.upsertPlayer(e.puuid, platform, tier, e.leaguePoints); });
    this.db.setMeta(`seeded_at:${platform}:${tier}`, String(this.now()));
    return entries.length;
  }

  /** Top masteries, plus Irelia's own entry for players seen playing her when she is not among them. */
  async masteryOf(player: PlayerRow, signal: AbortSignal) {
    const platform = normalizePlatform(player.platform);
    // An answer cached before this platform's patch began can't show Irelia played on it (0.21.2): mastery answers are
    // cached for 3 days, so without this the recheck a new patch calls for (platformJob) read the old answer again.
    const region = this.region(platform);
    const since = region.startKnown ? region.start : undefined;
    const top = await this.env.riot.topMasteries(platform, player.puuid, this.scope.masteryTopCount, signal, since);
    let irelia: MasteryDto | null = null;
    if (!top.some((m) => m.championId === config.championId) && player.found_by && player.found_by !== 'ladder') {
      irelia = await this.env.riot.championMastery(platform, player.puuid, config.championId, signal, since);
    }
    return classifyPilot(top, irelia, config.championId, this.scope);
  }

  /** Mastery checks, a few in flight, stored in order. `onChecked` returning false stops the batch. */
  async checkPlayers(players: PlayerRow[], signal: AbortSignal, onChecked?: (p: PlayerRow, u: ReturnType<typeof classifyPilot>, i: number) => boolean | void) {
    await inOrder(players, { concurrency: this.env.concurrency, signal },
      (p, _i, s) => this.masteryOf(p, s),
      (u, p, i) => { this.db.setPilot(p.puuid, u); return onChecked?.(p, u, i); });
  }

  private async lookupRanks(platform: Platform, players: PlayerRow[], signal: AbortSignal) {
    await inOrder(players, { concurrency: this.env.concurrency, signal },
      (p, _i, s) => this.env.riot.rankedEntries(platform, p.puuid, s),
      (entries, p) => {
        const solo = entries.find((e) => e.queueType === 'RANKED_SOLO_5x5');
        this.db.setRank(p.puuid, solo?.tier ?? 'UNRANKED', solo?.leaguePoints ?? 0);
      });
  }

  // ------------------------------------------------------------------------------- regional host

  private regionalJob(platforms: Platform[]): Job | null {
    const scope = this.scope, now = this.now();
    const me = this.env.me();
    if (me && scope.ownGames && platforms.includes(me.platform) && this.ownGamesDue(now)) {
      return { label: 'Fetching your Irelia games', score: OWN_GAMES_SCORE + 1, run: (signal) => this.listOwnGames(signal) };
    }
    const queued = this.db.nextQueuedBatch(platforms, this.env.concurrency * 4);
    let best: { pilot: PlayerRow; score: number } | null = null;
    for (const platform of platforms.filter((p) => scope.platforms.includes(p))) {
      const region = this.region(platform);
      for (const pilot of this.db.duePilots(platform, scope.pilotClasses, now - (this.env.rescanPilotMs ?? config.rescanPlayerAfterMs), region.start, 25)) {
        const score = historyScore(pilot, now, scope.maxMatchesPerPilot);
        if (!best || score > best.score) best = { pilot, score };
      }
    }
    if (queued.length && (!best || queued[0].score >= best.score)) {
      const harvest = queued[0].kind === 'timeline';
      return {
        label: harvest ? `Fetching harvested Irelia timelines (${queued[0].platform})` : `Fetching ${queued[0].platform} matches`,
        score: queued[0].score,
        run: (signal) => this.processQueued(queued.filter((q) => q.kind === queued[0].kind), signal),
      };
    }
    if (best) {
      const { pilot } = best;
      return { label: `Reading ${pilot.platform} ${pilot.pilot_class === 'main' ? 'main' : 'OTP'} match history`, score: best.score,
        run: (signal) => this.queueHistory(pilot, signal) };
    }
    return null;
  }

  /**
   * Match ids of a pilot's history worth fetching, newest first. 'patch' window: every id since the current patch
   * began on their platform (startTime), 100 per page, up to maxMatchesPerPilot; a page made only of matches already
   * stored ends it. 'newest': the matchIdsPerPlayer newest ids (the behaviour before v3).
   */
  async listHistory(pilot: Pick<PlayerRow, 'puuid' | 'platform'>, signal: AbortSignal, o: { queue?: number; since?: number; cap?: number; fresh?: boolean } = {}) {
    const scope = this.scope;
    const platform = normalizePlatform(pilot.platform);
    const queue = o.queue ?? config.rankedSoloQueue;
    if (scope.historyWindow === 'newest' && o.since === undefined) {
      return this.env.riot.matchIds(platform, pilot.puuid, queue, scope.matchIdsPerPlayer, signal);
    }
    const startTime = Math.floor((o.since ?? this.region(platform).start) / 1000);
    const cap = o.cap ?? scope.maxMatchesPerPilot;
    const ids: string[] = [];
    for (let start = 0; start < cap; start += 100) {
      const count = Math.min(100, cap - start);
      const page = await this.env.riot.matchIdsPage(platform, pilot.puuid, { queue, startTime, start, count, fresh: o.fresh && start === 0 }, signal);
      ids.push(...page);
      if (page.length < count || page.every((id) => this.db.hasMatch(id))) break;
    }
    return ids;
  }

  /** Reads a pilot's history and queues the matches not stored yet, scored by the pilot's Irelia share. */
  async queueHistory(pilot: PlayerRow, signal: AbortSignal) {
    const platform = normalizePlatform(pilot.platform);
    const ids = await this.listHistory(pilot, signal);
    const source: PilotSource = pilot.pilot_class === 'main' ? 'main' : 'otp';
    const score = matchScore(ireliaShare(pilot, source));
    let queued = 0;
    this.db.tx(() => {
      for (const id of ids) if (!this.db.hasMatch(id)) { this.db.enqueue({ match_id: id, platform, kind: 'match', puuid: pilot.puuid, source, score }); queued++; }
      this.db.setScanned(pilot.puuid);
    });
    if (queued) this.env.log(`${platform} ${source === 'main' ? 'main' : 'OTP'} history: ${queued} new matches queued`);
  }

  /** Fetches queued matches (or harvested timelines) a few at a time, stored in queue order. */
  async processQueued(items: QueuedMatch[], signal: AbortSignal) {
    const env = this.ingestEnv();
    await inOrder(items, { concurrency: this.env.concurrency, signal },
      async (q, _i, s) => {
        const platform = normalizePlatform(q.platform);
        if (q.kind === 'timeline') return { q, harvested: await fetchHarvested(env, platform, q.match_id, q.puuid, s) };
        return { q, fetched: await fetchMatch(env, platform, q.match_id, {
          pilotPuuid: q.puuid, source: q.source, keepPreviousPatch: q.source === 'me' }, s) };
      },
      (r) => {
        const { q } = r;
        let result: Ingest | 'harvested' | 'skipped';
        if ('harvested' in r) {
          const g = r.harvested;
          this.db.tx(() => { if (g) storeGame(this.db, g); this.db.dequeue(q.match_id); });
          result = g ? 'harvested' : 'skipped';
        } else {
          result = storeMatch(this.db, q.match_id, r.fetched);
          this.db.dequeue(q.match_id);
          // Ids are newest first and grow over time: once a pilot's history reaches an older patch, older ids are too.
          if (result === 'stale' && q.source !== 'me') this.db.dropOlderQueued(q.puuid, q.platform, matchNumber(q.match_id));
        }
        if (result === 'irelia' || result === 'harvested') {
          const src = result === 'harvested' ? 'harvested' : r.fetched?.kind === 'fetched' ? r.fetched.game?.row.pilot_source : q.source;
          this.env.log(`Stored ${src === 'me' ? 'your' : src ?? ''} Irelia game ${q.match_id}`.replace('  ', ' '));
        }
        if (r.fetched?.kind === 'fetched' && r.fetched.row.patch !== this.region(normalizePlatform(q.platform)).patch) this.invalidate();
      });
  }

  // ------------------------------------------------------------------------------- the user's own games

  /** Own games are due now (account just known, game just ended, or 30 minutes since the last look). */
  ownGamesDue(now = this.now()) {
    if (this.ownDueAt.length && this.ownDueAt[0] <= now) return true;
    return now - this.ownLastAt > 30 * 60_000;
  }

  /** After a game ends match-v5 takes a few minutes to have it: look again at these delays. */
  requestOwnGames(delaysMs = [0]) {
    const now = this.now();
    this.ownDueAt = [...this.ownDueAt, ...delaysMs.map((d) => now + d)].sort((a, b) => a - b);
  }

  /**
   * Queues the user's ranked solo and flex games of the current and previous patch (match ids since one patch cycle
   * before the current patch began on their platform). Irelia games among them are stored with source 'me'.
   */
  async listOwnGames(signal: AbortSignal) {
    const me = this.env.me();
    const now = this.now();
    this.ownLastAt = now;
    this.ownDueAt = this.ownDueAt.filter((t) => t > now);
    if (!me) return;
    const since = this.region(me.platform).start - PATCH_CYCLE_MS;
    let queued = 0;
    for (const queue of [config.rankedSoloQueue, config.rankedFlexQueue]) {
      const ids = await this.listHistory({ puuid: me.puuid, platform: me.platform }, signal, { queue, since, cap: 300, fresh: true });
      this.db.tx(() => {
        for (const id of ids) {
          if (this.db.hasMatch(id)) continue;
          this.db.enqueue({ match_id: id, platform: me.platform, kind: 'match', puuid: me.puuid, source: 'me', score: OWN_GAMES_SCORE });
          queued++;
        }
      });
    }
    this.db.tagOwnGames(me.puuid);
    if (queued) this.env.log(`Your games: ${queued} new ranked matches queued`);
  }

  // ------------------------------------------------------------------------------- status

  /** Work waiting per lane, for the Match Database panel. */
  queued(lane: LaneKey) {
    if (lane.kind === 'regional') {
      return this.db.queueSize().filter((r) => lane.platforms.includes(r.platform as Platform)).reduce((n, r) => n + r.n, 0);
    }
    return 0;
  }

}
