// Scraper worker thread: owns all Riot API traffic so the main event loop (LCU socket, HTTP, /ws) never blocks.
//
// Two kinds of work share the same database, cache and rate limiters:
//  - background lanes: one per Riot host (sourcing.ts). Every platform host (ladders, mastery, ranks) and every
//                      regional host (match ids, matches, timelines) works its own queue at its own pace, so adding
//                      a region adds throughput. Scope from scrape.config.json; toggled by the dashboard Start/Stop.
//  - backfill:         rebuilds the v3 capture of stored matches from the disk cache (backfill.ts), no API calls.
// Nothing else runs here: the model, the evaluation and the audit have threads of their own (server/workers.ts), so
// Start and Stop are always answered at once.
//
// Requests are paced by the per-host rate limiters (riot/rateLimiter.ts). Where a step has many independent
// requests (mastery checks, match parsing), a few run at once through `inOrder`, which still commits the results
// in their original order, so the database ends up exactly as a one-at-a-time loop would leave it.
//
// Protocol (parentPort)
//  in : {type:'start'|'stop'|'status'} | {type:'patch', patch, previousPatch} | {type:'apikey', key}
//       | {type:'backfill', force?} | {type:'me', account} | {type:'me-retry'} | {type:'game-ended'}
//       | {type:'game-pause', on} | {type:'gc'}
//  out: {type:'status', status} | {type:'ratelimit', snapshot} | {type:'gc-done'}
import { parentPort, workerData } from 'node:worker_threads';
import type { BackfillStatus, HostWork, RateSnapshot, ScraperStatus } from '../../shared/types.ts';
import { CAPTURE_VERSION } from '../capture.ts';
import { config, setDataDir } from '../config.ts';
import { openDb } from '../db.ts';
import { RiotAuthError, RiotClient } from '../riot/client.ts';
import { DiskCache } from '../riot/diskCache.ts';
import { sleep } from '../riot/rateLimiter.ts';
import { isPlatform, normalizePlatform, type Platform } from '../riot/routing.ts';
import { loadScope } from '../scrapeScope.ts';
import { readAppConfig } from '../appConfig.ts';
import { collectGarbage } from '../gc.ts';
import { isClientPuuid, resolveAccount, type ClientAccount } from './account.ts';
import { runBackfill } from './backfill.ts';
import { laneHost, Sourcing, type LaneKey } from './sourcing.ts';

// The main thread passes the data folder explicitly: it may have just moved it (routes/storage.ts).
const boot = workerData as { dataDir: string; patch: string; previousPatch?: string; gamePaused?: boolean };
setDataDir(boot.dataDir);
// The main thread opened and migrated the database before starting this thread. Writes here wait up to 30 s for
// another thread's (Delete games of old patches holds the lock for seconds on a large folder), where the main
// thread's wait only 5 s: a wait here holds up fetching, not the dashboard.
const db = openDb(config.dbPath, { migrate: false, busyTimeoutMs: 30_000 });
const cache = new DiskCache(config.riotCacheDir);
const riot = new RiotClient(config.riotApiKey, cache, config.rateLimits, config.pacing);
const concurrency = config.pacing.concurrency;
const IDLE_SLEEP_MS = 60_000;

let ddragon = { patch: boot.patch, previousPatch: boot.previousPatch ?? boot.patch };
let running = false;            // background lanes active
let loopAbort = new AbortController();
let phase = 'idle';
let lastError: string | null = null;
let lastEvent: string | null = null;
let backfill: BackfillStatus | null = null;
let backfillAbort: AbortController | null = null;
/**
 * Pause fetching during games (0.14.0, Settings, on by default): in champ select and in game, no requests and no
 * database writes; fetching resumes, and your own new game is fetched, when it ends. `enabled` is the Start/Stop choice.
 */
let gamePaused = boot.gamePaused === true;
let enabled = false;
/** A backfill the pause stopped (its `force`), to start again after the game. */
let pausedBackfill: boolean | null = null;
let backfillForce = false;

const meOf = () => {
  const puuid = db.getMeta('me_puuid'), platform = db.getMeta('me_platform');
  // Versions before 0.7.1 stored the League client's own PUUID, which the Riot API rejects: look it up again.
  if (puuid && isClientPuuid(puuid)) return null;
  return puuid && platform && isPlatform(platform) ? { puuid, platform } : null;
};
let me = meOf();

const event = (msg: string) => { lastEvent = msg; report(); };

const sourcing = new Sourcing({
  db, riot, scope: loadScope, fallbackPatch: () => ddragon, me: () => me, concurrency, log: event,
});

// ------------------------------------------------------------------------- lanes (one per Riot host)
interface Lane { key: LaneKey; host: string; task: string; busy: boolean; idleUntil: number | null; lastError: string | null; stop: AbortController; wake: () => void }
const lanes = new Map<string, Lane>();
const laneId = (k: LaneKey) => (k.kind === 'platform' ? `p:${k.platform}` : `r:${k.region}`);

/** Sleeps until `ms` pass, the lane is woken (new work elsewhere) or stopped. */
function nap(lane: Lane, ms: number, signal: AbortSignal) {
  return new Promise<void>((resolve) => {
    const done = () => { clearTimeout(t); signal.removeEventListener('abort', done); lane.wake = () => {}; resolve(); };
    const t = setTimeout(done, ms);
    signal.addEventListener('abort', done, { once: true });
    lane.wake = done;
  });
}
const wakeAll = () => { for (const l of lanes.values()) l.wake(); };

/** Starts lanes for hosts the scope needs and stops the ones it no longer does (the scope file is re-read every time). */
function syncLanes() {
  if (!running) return;
  let wanted: LaneKey[];
  try { wanted = sourcing.lanes(); } catch (err) { lastError = String(err instanceof Error ? err.message : err); return; }
  const ids = new Set(wanted.map(laneId));
  for (const [id, l] of lanes) if (!ids.has(id)) { l.stop.abort(); lanes.delete(id); }
  for (const key of wanted) {
    const l = lanes.get(laneId(key));
    if (l) { l.key = key; continue; }
    const lane: Lane = { key, host: laneHost(key), task: 'Starting', busy: false, idleUntil: null, lastError: null, stop: new AbortController(), wake: () => {} };
    lanes.set(laneId(key), lane);
    void runLane(lane);
  }
}

async function runLane(lane: Lane) {
  const signal = AbortSignal.any([lane.stop.signal, loopAbort.signal]);
  while (running && !signal.aborted) {
    try {
      const job = sourcing.nextJob(lane.key);
      if (!job) {
        if (lane.lastError !== null && lastError === lane.lastError) lastError = null;   // recovered, nothing left to do
        Object.assign(lane, { task: 'Up to date', busy: false, idleUntil: Date.now() + IDLE_SLEEP_MS, lastError: null });
        report();
        await nap(lane, IDLE_SLEEP_MS, signal);
        lane.idleUntil = null;
        syncLanes();
        continue;
      }
      const failed = lane.lastError;
      Object.assign(lane, { task: job.label, busy: true, lastError: null });
      report();
      await job.run(signal);
      lane.busy = false;
      // Recovered: the error this lane showed goes (it stayed until the next Start before).
      if (failed !== null && lastError === failed) { lastError = null; report(); }
      wakeAll();   // a new pilot or queued match may be work for another host
      syncLanes();
    } catch (err) {
      lane.busy = false;
      if (signal.aborted) break;
      const msg = err instanceof Error ? err.message : String(err);
      if (err instanceof RiotAuthError) { lastError = msg; stopLanes('Stopped: invalid API key'); db.setMeta('scraper_enabled', '0'); enabled = false; break; }
      Object.assign(lane, { task: 'Recovering from error', lastError: msg });
      lastError = msg;
      report();
      await sleep(5000, signal).catch(() => {});
    }
  }
}

function startLanes() {
  if (running) return;
  running = true; lastError = null; loopAbort = new AbortController();
  syncLanes();
  report();
}

function stopLanes(why = 'idle') {
  running = false;
  loopAbort.abort(new Error('paused'));
  lanes.clear();
  phase = why;
  report();
}

/** The status line: what the busy lanes do, else up to date (with the earliest re-check) or the backfill phase. */
function lanePhase() {
  const all = [...lanes.values()];
  const busy = all.filter((l) => l.busy);
  if (busy.length) return busy.map((l) => l.task).join(' · ');
  if (all.length && all.every((l) => l.idleUntil)) return 'Up to date – no new work';
  return all[0]?.task ?? phase;
}

// ------------------------------------------------------------------------- status
function rateSnapshot(now = Date.now()): RateSnapshot {
  const idle = [...lanes.values()];
  const idleUntil = running && idle.length && idle.every((l) => l.idleUntil) ? Math.min(...idle.map((l) => l.idleUntil!)) : null;
  return { at: now, ...riot.rateState(now), idleUntil };
}

function hostWork(): HostWork[] {
  return [...lanes.values()].map((l) => ({
    host: l.host, kind: l.key.kind, platforms: l.key.kind === 'platform' ? [l.key.platform] : l.key.platforms,
    task: l.task, busy: l.busy, queued: sourcing.queued(l.key), idleUntil: l.idleUntil, lastError: l.lastError,
  }));
}

// Row counts are cheap but not free on a large database: read at most once a second, however often the status goes out.
let countsCache: { at: number; patch: string; c: ReturnType<typeof db.counts>; sources: ReturnType<typeof db.sourceCounts> } | null = null;
function counts() {
  if (!countsCache || Date.now() - countsCache.at > 1000) {
    const patch = sourcing.region(normalizePlatform(loadScopeSafe().platforms[0] ?? 'KR')).patch;
    countsCache = { at: Date.now(), patch, c: db.counts(patch), sources: db.sourceCounts() };
  }
  return countsCache;
}

function report() {
  const { c, sources, patch } = counts();
  const status: ScraperStatus = {
    running, phase: running ? lanePhase() : gamePaused && enabled ? PAUSED : phase,
    ...(gamePaused && enabled ? { paused: 'game' as const } : {}),
    requests: riot.requests, rateWindow: riot.usage(), rate: rateSnapshot(),
    playersSeen: c.players, otpPlayers: c.otp, matchesScanned: c.matches, ireliaGames: c.games,
    currentPatchGames: c.current, currentPatch: patch, lastError, lastEvent,
    mainPlayers: c.mains, sources, hosts: hostWork(), backfill,
    me: me ? { platform: me.platform, games: sources.me ?? 0, riotId: db.getMeta('me_riot_id') ?? undefined } : null,
  };
  parentPort!.postMessage({ type: 'status', status });
}
function loadScopeSafe() { try { return loadScope(); } catch { return { platforms: ['KR'] }; } }

// Pacing waits are normal and show up in the live rate status.
riot.onThrottle(() => report());

// ------------------------------------------------------------------------- backfill (no API calls)
let lastBackfillReport = 0;
async function startBackfill(force = false) {
  if (backfillAbort) return;
  if (gamePaused) { pausedBackfill = pausedBackfill || force; return; }
  backfillAbort = new AbortController();
  backfillForce = force;
  try {
    backfill = await runBackfill({
      db, cache, championId: config.championId, harvest: loadScope().harvestIrelias,
      region: (p: Platform) => sourcing.region(p), mePuuid: () => me?.puuid ?? null,
    }, {
      signal: backfillAbort.signal, force,
      onProgress: (p) => { backfill = p; if (Date.now() - lastBackfillReport > 500) { lastBackfillReport = Date.now(); report(); } },
    });
    sourcing.invalidate();
    event(`Backfill: ${backfill.done} matches captured, ${backfill.games} Irelia timelines${backfill.harvested ? `, ${backfill.harvested} harvested games queued` : ''}${backfill.missing ? `, ${backfill.missing} not on disk` : ''}`);
    wakeAll();
  } catch (err) {
    const msg = err instanceof Error ? err.message : String(err);
    if (!backfillAbort.signal.aborted) { lastError = `Backfill failed: ${msg}`; backfill = backfill && { ...backfill, phase: `Failed: ${msg}` }; }
  } finally {
    backfillAbort = null;
    report();
  }
}

// ------------------------------------------------------------------------- the user's own games
/** With the background lanes stopped, own games are still fetched (a few dozen calls a patch), directly. */
let ownGamesError: string | null = null;
async function ownGamesNow() {
  if (running || gamePaused || !me || !config.riotApiKey || !loadScope().ownGames) return;
  const signal = new AbortController().signal;
  try {
    await sourcing.listOwnGames(signal);
    for (;;) {
      const batch = db.nextQueuedBatch([me.platform], concurrency * 4).filter((q) => q.source === 'me');
      if (!batch.length) break;
      await sourcing.processQueued(batch, signal);
    }
    // It worked this time: the error it showed last time goes.
    if (ownGamesError !== null && lastError === ownGamesError) lastError = null;
    ownGamesError = null;
    report();
  } catch (err) {
    lastError = ownGamesError = err instanceof Error ? err.message : String(err);
    report();
  }
}

/**
 * The signed-in account, from the League client: its Riot ID becomes the Riot API's PUUID, and its server comes from
 * the client, Riot or Settings (scraper/account.ts). Then its Irelia games are fetched as source "me".
 */
let clientAccount: ClientAccount | null = null;
async function setMe(account: ClientAccount | null) {
  if (account) clientAccount = account;
  if (!clientAccount) return;
  if (!config.riotApiKey) { lastError = 'Add a Riot API key in Settings to fetch your own games'; report(); return; }
  let r: Awaited<ReturnType<typeof resolveAccount>>;
  try {
    r = await resolveAccount(riot, clientAccount, readAppConfig().myPlatform ?? null);
  } catch (err) {
    r = { ok: false, error: `Could not look up your account: ${err instanceof Error ? err.message : String(err)}` };
  }
  if (!r.ok) { lastError = r.error; report(); return; }
  if (lastError?.includes('your account') || lastError?.includes('Riot ID') || lastError?.includes('which server')) lastError = null;
  db.setMeta('me_riot_id', r.riotId);
  if (me?.puuid === r.puuid && me.platform === r.platform) { report(); return; }
  db.setMeta('me_puuid', r.puuid);
  db.setMeta('me_platform', r.platform);
  me = { puuid: r.puuid, platform: r.platform };
  db.tagOwnGames(r.puuid);
  sourcing.requestOwnGames([0]);
  syncLanes();
  wakeAll();
  report();
  void ownGamesNow();
}

function gameEnded() {
  // match-v5 has a finished game a few minutes after it ends: look a few times.
  const delays = [2 * 60_000, 6 * 60_000, 15 * 60_000];
  sourcing.requestOwnGames(delays);
  for (const d of delays) setTimeout(() => { wakeAll(); void ownGamesNow(); }, d + 1000).unref();
}

// ------------------------------------------------------------------------- the cache's size index
// 0.14.1 (riot/diskCache.ts): built once, here. It reads every cached entry, so never on the main thread, and not
// while fetching is paused for a game; the Storage card reads the sizes from it.
let sizeIndexed = false;
function sizeIndexLater() {
  setTimeout(() => {
    if (sizeIndexed || gamePaused) return;
    sizeIndexed = true;
    try { if (cache.ensureSizeIndex()) console.log('[scraper] the cache\'s size index is built'); } catch (err) {
      sizeIndexed = false;
      console.warn(`[scraper] the cache's size index: ${err instanceof Error ? err.message : String(err)}`);
    }
  }, 30_000).unref();
}
sizeIndexLater();

// ------------------------------------------------------------------------- champ select and games
const PAUSED = 'Paused during champ select and the game';
function setGamePause(on: boolean) {
  if (on === gamePaused) return;
  gamePaused = on;
  if (on) {
    if (running) stopLanes(PAUSED);
    if (backfillAbort) { pausedBackfill = backfillForce; backfillAbort.abort(new Error('paused')); }
    report();
    return;
  }
  if (phase === PAUSED) phase = 'idle';
  if (!sizeIndexed) sizeIndexLater();
  if (enabled && config.riotApiKey) startLanes();
  if (pausedBackfill !== null) { const force = pausedBackfill; pausedBackfill = null; void startBackfill(force); }
  // The game that just ended (and any own game a timer skipped during it) is fetched now.
  wakeAll();
  void ownGamesNow();
  report();
}

// ------------------------------------------------------------------------- messages
// The Start/Stop choice is persisted so a middleware restart resumes the user's last choice.
parentPort!.on('message', (msg: { type: string; patch?: string; previousPatch?: string; key?: string; force?: boolean; account?: ClientAccount; on?: boolean }) => {
  switch (msg.type) {
    case 'start':
      if (!config.riotApiKey) { lastError = 'RIOT_API_KEY is not set'; report(); return; }
      db.setMeta('scraper_enabled', '1');
      enabled = true;
      if (gamePaused) report(); else startLanes();
      return;
    case 'stop':
      db.setMeta('scraper_enabled', '0');
      enabled = false;
      stopLanes();
      return;
    case 'patch':
      if (msg.patch) { ddragon = { patch: msg.patch, previousPatch: msg.previousPatch ?? ddragon.previousPatch }; sourcing.invalidate(); }
      return;
    case 'apikey':
      config.riotApiKey = msg.key ?? '';
      riot.setApiKey(config.riotApiKey);
      if (lastError?.includes('rejected the key')) { lastError = null; report(); }
      return;
    case 'status': report(); return;
    case 'backfill': void startBackfill(msg.force === true); return;
    case 'me': if (msg.account) void setMe(msg.account); return;
    case 'me-retry': void setMe(null); return;
    case 'game-ended': gameEnded(); return;
    case 'game-pause': setGamePause(msg.on === true); return;
    case 'gc': collectGarbage(true); parentPort!.postMessage({ type: 'gc-done' }); return;
  }
});

// Live rate-limit snapshot for the bars: checked 5×/s, posted only when something visible changed
// (a request started or finished, a window drained, the pace or a Retry-After changed). The full status
// report above keeps its 2 s cadence.
let lastSnapshotKey = '';
function postRateSnapshot() {
  const snapshot = rateSnapshot();
  const key = JSON.stringify([
    snapshot.hosts.map((h) => [h.host, h.windows.map((w) => `${w.used}/${w.limit}/${w.windowSec}`), Math.round(h.paceMs),
      h.nextAt === null ? null : Math.round(h.nextAt / 10), h.queued, h.inFlight, h.retryAfter?.until ?? 0, h.retryAfter?.scope ?? '']),
    snapshot.busiest, snapshot.active, snapshot.idleUntil,
  ]);
  if (key === lastSnapshotKey) return;
  lastSnapshotKey = key;
  parentPort!.postMessage({ type: 'ratelimit', snapshot });
}

enabled = db.getMeta('scraper_enabled') === '1';
report();
if (enabled && !gamePaused) startLanes();
// Matches stored before this version get their participants, flags and timelines from the disk cache.
if (db.countPendingDetail(CAPTURE_VERSION) > 0) void startBackfill();
if (me) { sourcing.requestOwnGames([0]); void ownGamesNow(); }
setInterval(() => { if (running || backfillAbort) report(); }, 2000).unref();
setInterval(postRateSnapshot, config.rateSnapshotMs).unref();
