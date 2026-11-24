import type { RateHostState, RateSnapshot } from '../../shared/types.ts';
import type { MatchDto, TimelineDto } from '../engine/parseMatch.ts';
import type { CacheKind, DiskCache } from './diskCache.ts';
import {
  limitScope, Pacer, RateLimiter, retryAfterMs, sleep,
  type EndpointFamily, type LimitScope, type PacingOptions, type RateWindow,
} from './rateLimiter.ts';
import { APEX_TIERS, platformHost, regionHost, type Division, type Platform, type Tier } from './routing.ts';

export class RiotAuthError extends Error {}

export interface LeagueEntry { puuid: string; leaguePoints: number }

/** champion-mastery-v4 ChampionMasteryDto (the fields the app reads). lastPlayTime is epoch ms. */
export interface MasteryDto { championId: number; championPoints: number; lastPlayTime?: number }

/** league-v4 LeagueEntryDTO from entries/by-puuid. */
export interface RankedEntry { queueType: string; tier: string; rank: string; leaguePoints: number }

/** One page of match-v5 by-puuid ids. startTime is epoch seconds, as Riot's reference says; count is 0..100. */
export interface MatchIdQuery {
  queue: number; startTime?: number; start: number; count: number;
  /** Skip the cached answer (own games right after a game ends: the 10-minute id cache would hide it). */
  fresh?: boolean;
}

/**
 * The Riot calls the scraper makes, so its sourcing logic (scraper/sourcing.ts) can run against a fake in tests.
 * RiotClient implements it with the disk cache and the per-host rate limiters.
 */
export interface RiotApi {
  leagueEntries(platform: Platform, tier: Tier, divisions: Division[], maxPages: number, signal?: AbortSignal): Promise<LeagueEntry[]>;
  /** `since` (0.21.2): a cached answer stored before then isn't used (a new patch's activity check). */
  topMasteries(platform: Platform, puuid: string, count: number, signal?: AbortSignal, since?: number): Promise<MasteryDto[]>;
  championMastery(platform: Platform, puuid: string, championId: number, signal?: AbortSignal, since?: number): Promise<MasteryDto | null>;
  rankedEntries(platform: Platform, puuid: string, signal?: AbortSignal): Promise<RankedEntry[]>;
  matchIds(platform: Platform, puuid: string, queue: number, count: number, signal?: AbortSignal): Promise<string[]>;
  matchIdsPage(platform: Platform, puuid: string, q: MatchIdQuery, signal?: AbortSignal): Promise<string[]>;
  match(platform: Platform, id: string, signal?: AbortSignal): Promise<MatchDto | null>;
  timeline(platform: Platform, id: string, signal?: AbortSignal): Promise<TimelineDto | null>;
  accountRegion(puuid: string, signal?: AbortSignal): Promise<string | null>;
  /** The Riot API's PUUID for a Riot ID (the League client's own PUUID is not the one the API takes). */
  accountByRiotId(gameName: string, tagLine: string, host: string, signal?: AbortSignal): Promise<{ puuid: string } | null>;
}

/** A 429 from Riot, after the limiter has applied it. */
export interface ThrottleInfo { host: string; family: EndpointFamily; scope: LimitScope; waitMs: number }

export type RiotRateState = Omit<RateSnapshot, 'at' | 'idleUntil'>;

const MAX_ATTEMPTS = 5;
/**
 * A request (headers and body) that takes longer is abandoned and tried again. Riot answers in well under a second;
 * a connection that died meanwhile (the computer slept, the network dropped) would otherwise hang for minutes.
 */
export const REQUEST_TIMEOUT_MS = 30_000;

/**
 * Riot API client. Every call is disk-cache first; misses go through a per-host rate limiter
 * (Riot enforces app limits per routing value, so KR and ASIA each get their own budget).
 */
export class RiotClient implements RiotApi {
  /** REQUEST_TIMEOUT_MS; tests shorten it. */
  requestTimeoutMs = REQUEST_TIMEOUT_MS;
  requests = 0;
  readonly defaultPaceMs: number;
  private limiters = new Map<string, RateLimiter>();
  private throttleListener: ((info: ThrottleInfo) => void) | null = null;

  constructor(
    private apiKey: string,
    private cache: DiskCache,
    private limits: RateWindow[],
    private pacing: PacingOptions,
  ) {
    this.defaultPaceMs = new Pacer(limits, pacing).spacing(Date.now());
  }

  get cacheHits() { return this.cache.hits; }

  /** New key from Settings; takes effect on the next request. */
  setApiKey(key: string) { this.apiKey = key; }

  onThrottle(listener: ((info: ThrottleInfo) => void) | null) { this.throttleListener = listener; }

  private limiter(host: string) {
    let l = this.limiters.get(host);
    if (!l) {
      l = new RateLimiter(host, this.limits, this.pacing);
      this.limiters.set(host, l);
    }
    return l;
  }

  /** Window usage of the busiest host (for the status panel). */
  usage() {
    const all = [...this.limiters.values()].map((l) => l.usage());
    if (!all.length) return this.limits.map((l) => ({ used: 0, limit: l.limit, windowSec: l.windowMs / 1000 }));
    return all.reduce((a, b) => (b.reduce((s, w) => s + w.used, 0) > a.reduce((s, w) => s + w.used, 0) ? b : a));
  }

  /** Live pacing state of every host used so far. */
  rateState(now = Date.now()): RiotRateState {
    const hosts = [...this.limiters.values()].map((l) => l.state(now));
    const used = (h: RateHostState) => h.windows.reduce((s, w) => s + w.used, 0);
    const busiest = hosts.reduce<RateHostState | null>((a, b) => (!a || used(b) > used(a) ? b : a), null);
    const recent = hosts
      .filter((h) => h.queued || h.inFlight || now - h.lastStartAt < 3000)
      .sort((a, b) => b.lastStartAt - a.lastStartAt)[0];
    return { hosts, busiest: busiest?.host ?? null, active: recent?.host ?? null, defaultPaceMs: this.defaultPaceMs };
  }

  private async get<T>(host: string, family: EndpointFamily, path: string, signal?: AbortSignal): Promise<T | null> {
    const limiter = this.limiter(host);
    for (let attempt = 0; ; attempt++) {
      const permit = await limiter.acquire(family, signal);
      this.requests++;
      let res!: Response;
      let body: unknown;
      let failed: { error: unknown } | null = null;
      const timeout = AbortSignal.timeout(this.requestTimeoutMs);
      try {
        res = await fetch(`https://${host}${path}`, {
          headers: { 'X-Riot-Token': this.apiKey }, signal: signal ? AbortSignal.any([signal, timeout]) : timeout,
        });
        limiter.updateLimits(family, res.headers.get('x-app-rate-limit'), res.headers.get('x-method-rate-limit'));
        if (res.ok) body = await res.json();   // the slot stays taken until the body has arrived
        else await res.body?.cancel().catch(() => {});   // free the connection for the next request
      } catch (error) {
        failed = { error };
      } finally {
        permit.done();
      }
      if (failed) {
        // Stopped by the caller: let it go. A timeout or a network error (a connection that died while the computer
        // slept, a dropped network) is tried again on a fresh connection, like a 5xx.
        if (signal?.aborted || attempt + 1 >= MAX_ATTEMPTS) throw failed.error;
        await sleep(1000 * 2 ** attempt, signal);
        continue;
      }

      if (res.ok) { limiter.succeeded(); return body as T; }
      if (res.status === 404) { limiter.succeeded(); return null; }
      if (res.status === 401 || res.status === 403) {
        throw new RiotAuthError(`Riot API rejected the key (${res.status}). Development keys expire every 24h - update RIOT_API_KEY in .env.`);
      }
      if (res.status === 429) {
        // Recorded even on the last attempt, so every later request on this host honours Riot's Retry-After.
        const scope = limitScope(res.headers.get('x-rate-limit-type'));
        const waitMs = limiter.rateLimited(family, scope, retryAfterMs(res.headers.get('retry-after')));
        this.throttleListener?.({ host, family, scope, waitMs });
        if (attempt + 1 >= MAX_ATTEMPTS) throw new Error(`Riot API 429 on ${path}`);
        continue;
      }
      if (res.status < 500 || attempt + 1 >= MAX_ATTEMPTS) throw new Error(`Riot API ${res.status} on ${path}`);
      await sleep(1000 * 2 ** attempt, signal);
    }
  }

  /** Cache-first GET. `null` (404) results are not cached. */
  private async cached<T>(kind: CacheKind, scope: string, key: string, host: string, path: string, signal?: AbortSignal, fresh = false, since?: number) {
    const hit = fresh ? undefined : await this.cache.get<T>(kind, scope, key, { since });
    if (hit !== undefined) return hit;
    const value = await this.get<T>(host, kind, path, signal);
    if (value !== null) await this.cache.set(kind, scope, key, value);
    return value;
  }

  /** Every ranked solo entry for a tier: one list for apex tiers, paged per division otherwise. */
  async leagueEntries(platform: Platform, tier: Tier, divisions: Division[], maxPages: number, signal?: AbortSignal) {
    const host = platformHost(platform);
    if (APEX_TIERS.has(tier)) {
      const res = await this.cached<{ entries: LeagueEntry[] }>('league', platform, tier, host,
        `/lol/league/v4/${tier.toLowerCase()}leagues/by-queue/RANKED_SOLO_5x5`, signal);
      return res?.entries ?? [];
    }
    const out: LeagueEntry[] = [];
    for (const division of divisions) {
      for (let page = 1; page <= maxPages; page++) {
        const res = await this.cached<LeagueEntry[]>('league', platform, `${tier}-${division}-p${page}`, host,
          `/lol/league/v4/entries/RANKED_SOLO_5x5/${tier}/${division}?page=${page}`, signal);
        if (!res?.length) break;
        out.push(...res);
      }
    }
    return out;
  }

  /** The player's `count` best masteries, best first (champion-mastery-v4 top). Includes each one's lastPlayTime. */
  async topMasteries(platform: Platform, puuid: string, count: number, signal?: AbortSignal, since?: number) {
    return (await this.cached<MasteryDto[]>('mastery', platform, `${puuid}-top${count}`,
      platformHost(platform), `/lol/champion-mastery/v4/champion-masteries/by-puuid/${puuid}/top?count=${count}`, signal, false, since)) ?? [];
  }

  /** One champion's mastery (champion-mastery-v4 by-champion); null when the player never played it. */
  championMastery(platform: Platform, puuid: string, championId: number, signal?: AbortSignal, since?: number) {
    return this.cached<MasteryDto>('mastery', platform, `${puuid}-c${championId}`,
      platformHost(platform), `/lol/champion-mastery/v4/champion-masteries/by-puuid/${puuid}/by-champion/${championId}`, signal, false, since);
  }

  /** The player's ranked entries (league-v4 entries/by-puuid), for harvested players who are on no seeded ladder. */
  async rankedEntries(platform: Platform, puuid: string, signal?: AbortSignal) {
    return (await this.cached<RankedEntry[]>('league', platform, `by-puuid-${puuid}`,
      platformHost(platform), `/lol/league/v4/entries/by-puuid/${puuid}`, signal)) ?? [];
  }

  /** The `count` newest ranked match ids (the history window before v3: no startTime). */
  async matchIds(platform: Platform, puuid: string, queue: number, count: number, signal?: AbortSignal): Promise<string[]> {
    return (await this.cached<string[]>('matchIds', platform, `${puuid}-${queue}-${count}`, regionHost(platform),
      `/lol/match/v5/matches/by-puuid/${puuid}/ids?queue=${queue}&type=ranked&start=0&count=${count}`, signal)) ?? [];
  }

  /** One page of match ids, newest first. The cache key holds every paging parameter. */
  async matchIdsPage(platform: Platform, puuid: string, q: MatchIdQuery, signal?: AbortSignal): Promise<string[]> {
    const since = q.startTime !== undefined ? `&startTime=${q.startTime}` : '';
    return (await this.cached<string[]>('matchIds', platform, `${puuid}-${q.queue}-${q.startTime ?? 0}-${q.start}-${q.count}`, regionHost(platform),
      `/lol/match/v5/matches/by-puuid/${puuid}/ids?queue=${q.queue}${since}&start=${q.start}&count=${q.count}`, signal, q.fresh)) ?? [];
  }

  /** The platform a PUUID plays League on (account-v1 region by-game), e.g. "euw1"; any regional host answers it. */
  async accountRegion(puuid: string, signal?: AbortSignal) {
    const res = await this.cached<{ region?: string }>('account', 'lol', puuid, 'europe.api.riotgames.com',
      `/riot/account/v1/region/by-game/lol/by-puuid/${puuid}`, signal);
    return res?.region ?? null;
  }

  accountByRiotId(gameName: string, tagLine: string, host: string, signal?: AbortSignal) {
    // Riot IDs are case-insensitive; the cache key is the name in hex (any language, no file-name clashes).
    const key = Buffer.from(`${gameName}#${tagLine}`.toLowerCase()).toString('hex');
    return this.cached<{ puuid: string }>('account', 'riotid', key, host,
      `/riot/account/v1/accounts/by-riot-id/${encodeURIComponent(gameName)}/${encodeURIComponent(tagLine)}`, signal);
  }

  match(platform: Platform, id: string, signal?: AbortSignal) {
    return this.cached<MatchDto>('match', platform, id, regionHost(platform), `/lol/match/v5/matches/${id}`, signal);
  }

  timeline(platform: Platform, id: string, signal?: AbortSignal) {
    return this.cached<TimelineDto>('timeline', platform, id, regionHost(platform), `/lol/match/v5/matches/${id}/timeline`, signal);
  }
}
