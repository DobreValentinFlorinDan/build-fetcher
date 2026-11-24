// Model worker thread: the lane baseline, the draft-aware model (Stage B) and everything read from it run here, so the
// main thread (League client socket, live poll, HTTP, /ws) never waits on a model build or an estimate.
//
// Requests are answered with the model as it is: before the first build, drafts get the lane baseline alone and the
// draft-adjusted part follows once the model is built (a 'status' message tells the main thread). Builds happen when
// the data changed, at most every few minutes, and never during champ select or a game (model/service.ts).
//
// Protocol (parentPort)
//  in : {type:'request', id, kind, args}                kind: draft | recommendation | live | personal | review | status | stats
//       {type:'env', ownPlatform?, busy?, queued?, evaluating?} | {type:'patch', versions} | {type:'evaluated'} | {type:'rebuild'} | {type:'live-reset'}
//       {type:'options'}                                 a Data tab switch moved (saved by the main thread, which alone writes them)
//       {type:'release'}                                 Slim stored data starts: the request caches go (0.15.2)
//  out: {type:'reply', id, value?, error?}
//       {type:'status', status, profiles?, built?}     before and after every build (profiles: what the threat summary
//                                                       needs; built: its time and games, kept for the next estimate)
//       {type:'data'}                                   the stored games changed: the baseline may have too
import { parentPort, workerData } from 'node:worker_threads';
import type { BuildRecommendation, Lane, ModelStatus } from '../../shared/types.ts';
import { config, setDataDir } from '../config.ts';
import { openDb, type Db } from '../db.ts';
import { loadPatchHistory, loadStaticData, type StaticData } from '../ddragon.ts';
import { identify, type AllGameData } from '../engine/adaptive.ts';
import { buildExclusivityIndex, isPurchasableItem } from '../engine/exclusivity.ts';
import { recommend, type RegionPatchOf, type ValidatedRows } from '../engine/recommend.ts';
import { collectGarbage } from '../gc.ts';
import { Progress } from '../progress.ts';
import { hotfixes, regionPatches } from '../patches.ts';
import { DEFAULT_SCOPE, loadScope } from '../scrapeScope.ts';
import { BuyIndex } from './buy.ts';
import { matchupBest, matchupFacts } from './facts.ts';
import { LiveReranker, type LiveHint } from './live.ts';
import { personalReport, reviewGame } from './personal.ts';
import { prepareThreads } from './prepareTeam.ts';
import { modelPatches, ModelService } from './service.ts';
import { HISTORY_PATCHES, MEMORY_PATCHES, patchAger } from './weights.ts';

const IRELIA = 39;
const boot = workerData as { dataDir: string; versions: string[]; lastBuildMs?: number | null };
setDataDir(boot.dataDir);
// The main thread opened (and migrated) the database first; this thread only reads it.
const db = openDb(config.dbPath, { migrate: false });
/** Data Dragon's newest files, with the fingerprints of the patches patch memory can reach. */
async function staticData(versions?: string[]): Promise<StaticData> {
  const d = await loadStaticData(versions);
  return { ...d, history: await loadPatchHistory(d.versions ?? versions ?? [d.version], HISTORY_PATCHES) };
}
let data = await staticData(boot.versions);
let excl = buildExclusivityIndex(data);
// busy: champ select or a game; queued (0.15.0): in queue, when no build may start (champ select can begin any second);
// evaluating (0.15.1): Evaluate builds runs, whose threads share this one's heap, and a build would add a second model
// to it (at 118,000 games about 0.8 GB more, an estimate). The first build doesn't wait.
const env = { ownPlatform: null as string | null, busy: false, queued: false, evaluating: false };
/** No build starts now (one running goes on unless champ select, a game or the queue begins). */
const held = () => env.busy || env.queued || env.evaluating;

const scope = () => { try { return loadScope(); } catch { return DEFAULT_SCOPE; } };
// Each platform's own current patch (server/patches.ts): re-read at most every 30 s, the live re-rank asks every second.
let regionCache: { at: number; of: RegionPatchOf } | null = null;
function regionPatchOf(): RegionPatchOf {
  if (regionCache && Date.now() - regionCache.at < 30_000) return regionCache.of;
  const regions = regionPatches(db.gameVersions(), data);
  regionCache = { at: Date.now(), of: (platform) => regions.get(platform) };
  return regionCache.of;
}

/**
 * The games of `list` from each region's current and previous patch: what the matchup facts, the high-elo timings (Your
 * games and the review) and the best buy describe. A model built with patch memory also holds older patches' games,
 * for the draft-aware picks only; otherwise this is `list` itself.
 */
function recent<T>(list: T[], game: (x: T) => { platform: string; patch: string }): T[] {
  if (!service.holdsOlderPatches()) return list;
  const ageOf = patchAger(data, regionPatchOf());
  return list.filter((x) => ageOf(game(x)) <= 1);
}

const service = new ModelService({
  db, data: () => data, excl: () => excl, scope, regionPatch: regionPatchOf,
  ownPlatform: () => env.ownPlatform, busy: held, startingItemWindowMs: config.startingItemWindowMs,
});
const reranker = new LiveReranker();
// Stage C3: purchase paths per target item, kept while the model holds.
// Kept for one model and one Data Dragon version: a new patch changes recipes and prices.
let buyIndex: { for: object | null; data: object; index: BuyIndex } | null = null;
function buys(): BuyIndex {
  const b = service.current();
  if (!buyIndex || buyIndex.for !== b || buyIndex.data !== data) {
    buyIndex = { for: b, data, index: new BuyIndex(recent(b?.model.scored ?? [], (x) => x.g), b?.model.state ?? null, data) };
  }
  return buyIndex.index;
}

/**
 * The lane builds: the scope's build sources (OTP games by default; never your own games), every stored patch. Kept
 * per lane and opponent until the stored games, the patch or the sources change: every matchup change asks for one,
 * and each means reading every stored game.
 */
let baselines = { version: '', byQuery: new Map<string, BuildRecommendation>() };
const MAX_BASELINES = 32;
/**
 * The games the lane builds are made from, read once per data version (0.15.0): each opponent hovered in champ select
 * read them all again before (about a second at 54,000 games). Let go a few minutes after the last use (trimRows).
 */
let baselineRows: { version: string; rows: ReturnType<Db['gamesForPatches']>; usedAt: number; validated: Map<string, ValidatedRows> } | null = null;
const ROWS_IDLE_MS = 3 * 60_000;
function trimRows() {
  if (!baselineRows || Date.now() - baselineRows.usedAt < ROWS_IDLE_MS) return false;
  baselineRows = null;
  return true;
}
function baseline(enemy: number | null, lane: Lane | null): BuildRecommendation {
  const sources = scope().buildSources;
  const version = `${db.dataVersion()}|${data.version}|${sources.join(',')}`;
  if (baselines.version !== version) baselines = { version, byQuery: new Map() };
  const opponent = enemy && data.champions.has(enemy) ? enemy : null;
  const key = `${lane}|${opponent}`;
  let b = baselines.byQuery.get(key);
  if (!b) {
    if (baselineRows?.version !== version) {
      const patches = db.patchBreakdown(sources).map((p) => p.patch);
      baselineRows = { version, rows: db.gamesForPatches(patches, sources), usedAt: 0, validated: new Map() };
    }
    baselineRows.usedAt = Date.now();
    b = recommend(baselineRows.rows, data, excl, opponent, config, lane, regionPatchOf(), baselineRows.validated);
    if (baselines.byQuery.size >= MAX_BASELINES) baselines.byQuery.delete(baselines.byQuery.keys().next().value!);
    baselines.byQuery.set(key, b);
  }
  return b;
}

let state: ModelStatus['state'] = 'loading';
/** While building: since when, and how long the last build took (this session's, else the last one kept on disk). */
let building: { tracker: Progress } | null = null;
let lastBuildMs = boot.lastBuildMs ?? null;
function status(): ModelStatus {
  return { state, info: service.info(), mode: service.mode(), error: service.error(), ...(building ? { progress: building.tracker.info(null) } : {}) };
}

/**
 * A build (0.15.0: its games prepared on helper threads, model/prepareTeam.ts): this thread answers requests with the
 * model it has meanwhile. Champ select, a game or the queue stops it, unless there is no model yet.
 */
let buildRunning = false;
/** A build asked for while one ran (a new patch, an evaluation): it follows, as it did when builds blocked this thread. */
let buildAgain = false;
async function build() {
  if (buildRunning) { buildAgain = true; return; }
  buildRunning = true;
  state = 'building';
  building = { tracker: new Progress(lastBuildMs) };
  parentPort!.postMessage({ type: 'status', status: status() });
  const before = service.current();
  const built = await service.rebuildAsync({
    dataDir: boot.dataDir, threads: prepareThreads(), stop: () => service.current() !== null && (env.busy || env.queued),
  }).finally(() => { buildRunning = false; });
  regionCache = null;
  building = null;
  // A failed rebuild keeps the previous model: still 'ready', with the error in the status for the Data tab.
  state = built ? 'ready' : service.error() ? 'failed' : 'loading';
  const fresh = built && built !== before ? built : null;
  if (fresh) lastBuildMs = fresh.buildMs;
  parentPort!.postMessage({
    type: 'status', status: status(), profiles: fresh?.profileRows,
    // Kept by the main thread for the next session's first estimate.
    ...(fresh ? { built: { ms: fresh.buildMs, games: fresh.prepared.length } } : {}),
  });
  // The previous model and the build's own scratch are garbage now: the thread goes back to the new model's size.
  releaseMemory();
  if (buildAgain) {
    buildAgain = false;
    if (!held()) void build();
  }
}

/**
 * After work that leaves much behind (a rebuild, a cache let go): a thorough collection, which hands the empty pages
 * back to the system (server/gc.ts), and another a moment later, which finds a little more. Never during champ select
 * or a game (half a second each at 54,000 games): rebuilds don't run then, and the second one waits.
 */
let releaseTimer: NodeJS.Timeout | null = null;
function releaseMemory() {
  collectGarbage(true);
  if (releaseTimer) clearTimeout(releaseTimer);
  releaseTimer = setTimeout(() => { releaseTimer = null; if (!env.busy) collectGarbage(true); }, 2000);
  releaseTimer.unref();
}

// Requests come first: a due build waits until no request arrived for a moment.
let buildTimer: NodeJS.Timeout | null = null;
function buildSoon(ms = 400) {
  if (buildTimer) clearTimeout(buildTimer);
  buildTimer = setTimeout(() => { buildTimer = null; if (!buildRunning && service.needsBuild()) void build(); }, ms);
}

const handlers: Record<string, (args: any) => unknown> = {
  draft: (q: { lane: Lane; opponent: number | null; enemies: number[]; allies: number[]; regions: string[] | null }) => {
    const b = service.current();
    const facts = b ? matchupFacts(recent(b.prepared, (g) => g), q.lane, q.opponent) : null;
    // What did best in this matchup, for the imported item set's "vs" blocks (scored as the model scores them).
    const best = facts && b ? matchupBest(recent(b.model.scored, (s) => s.g), q.lane, q.opponent, (id) => isPurchasableItem(data.items.get(id))) : null;
    // Champ select keeps the draft caches between picks; outside it a draft goes without them (service.ts).
    return { ...service.draft(q, baseline(q.opponent, q.lane), env.busy), facts: facts && { ...facts, best } };
  },
  recommendation: (q: { enemy: number | null; lane: Lane | null }) => baseline(q.enemy, q.lane),
  // In game, every poll: the next item re-ranked from the live game (draft-adjusted mode), and the best purchase for
  // your gold toward it, else toward the build plan's next item (`planTarget`). Build and shop advice only.
  // When the plan's next entry is the boots, they come first in both modes (the re-rank is about completed items).
  live: (q: { raw: AllGameData; hint: LiveHint; planTarget: { id: number; boots: boolean } | null }) => {
    const b = service.current();
    const next = b && service.mode() === 'draft' && !q.planTarget?.boots
      ? reranker.rerank(b.model, b.version, q.raw, data, excl, regionPatchOf(), service.settings(), q.hint)
      : null;
    const { me } = identify(q.raw, q.hint.opponentName);
    const target = next?.itemId ?? q.planTarget?.id ?? null;
    const isIrelia = me && data.championsByKey.get(me.championName)?.id === IRELIA;
    const buy = isIrelia && target ? buys().best({
      lane: q.hint.lane ?? 'TOP', target, gold: q.raw.activePlayer.currentGold,
      opponent: q.hint.opponentName ? data.championsByKey.get(q.hint.opponentName)?.id ?? null : null,
      owned: me.items.map((i) => i.itemID).filter((id) => id > 0),
    }) : null;
    return { next, buy };
  },
  personal: (q: { lane: Lane | null }) => {
    const b = service.current();
    return b ? personalReport(recent(b.model.scored, (x) => x.g), q.lane) : null;
  },
  review: (q: { matchId: string }) => {
    const b = service.current();
    // Your game itself may be from any patch the model holds; the high-elo timings it is compared with are recent.
    return reviewGame(q.matchId, b?.prepared ?? [], recent(b?.model.scored ?? [], (x) => x.g), db, data, db.participantsOf(q.matchId));
  },
  status: () => status(),
  // The Data tab: row counts, games per patch and lane, and each region's patch (read here: a large database takes a moment).
  stats: () => {
    const sc = scope();
    const regions = [...regionPatches(db.gameVersions(), data, sc.platforms).values()].map((r) => ({ ...r, hotfixes: hotfixes(r, sc.hotfixes) }));
    return {
      ...db.counts(data.patch), patches: db.patchBreakdown(sc.buildSources), currentPatch: data.patch, previousPatch: data.previousPatch,
      sources: db.sourceCounts(), buildSources: sc.buildSources, regions,
      windowPatches: modelPatches(db, data, MEMORY_PATCHES), archive: db.archiveSummary(),
    };
  },
};

parentPort!.on('message', async (msg: { type: string; id?: number; kind?: string; args?: unknown; ownPlatform?: string | null; busy?: boolean; queued?: boolean; evaluating?: boolean; versions?: string[] }) => {
  switch (msg.type) {
    case 'request': {
      try {
        const handler = handlers[msg.kind ?? ''];
        if (!handler) throw new Error(`Unknown request ${msg.kind}`);
        parentPort!.postMessage({ type: 'reply', id: msg.id, value: handler(msg.args) });
      } catch (err) {
        parentPort!.postMessage({ type: 'reply', id: msg.id, error: err instanceof Error ? err.message : String(err) });
      }
      if (state !== 'loading' && state !== 'building') buildSoon(2000);
      return;
    }
    case 'env':
      if (msg.ownPlatform !== undefined) env.ownPlatform = msg.ownPlatform;
      if (msg.busy !== undefined) env.busy = msg.busy;
      if (msg.queued !== undefined) env.queued = msg.queued;
      if (msg.evaluating !== undefined) env.evaluating = msg.evaluating;
      return;
    case 'patch':
      try {
        data = await staticData(msg.versions);
        excl = buildExclusivityIndex(data);
        regionCache = null;
        // The main thread asks for a new build now that this one has the patch; the model follows unless busy.
        parentPort!.postMessage({ type: 'data' });
        if (!held()) void build();
      } catch (err) { console.warn('[model] could not load the new patch', err); }
      return;
    case 'evaluated':
      // A new evaluation changes the default mode, the bandwidth and the prior.
      service.evaluated();
      parentPort!.postMessage({ type: 'status', status: status() });
      return;
    case 'rebuild':
      // After an evaluation: its state-model penalty applies on the next build.
      if (!held()) void build();
      return;
    case 'gc':
      // Champ select began (server/index.ts): what the last build left goes before the game needs the memory.
      collectGarbage(true);
      parentPort!.postMessage({ type: 'gc-done' });
      return;
    case 'release':
      // Slim stored data starts (0.15.2): its check builds a model of its own in the heap this thread shares. What is
      // kept for requests goes (the lane builds' games: about 130 MB at 54,000 games); the model stays.
      baselineRows = null;
      baselines = { version: '', byQuery: new Map() };
      buyIndex = null;
      service.trimCaches(0);
      releaseMemory();
      return;
    case 'live-reset':
      // The game is over (or a new one starts): its observations go, and with them their memory.
      reranker.reset();
      if (reranker.release() && !env.busy) releaseMemory();
      return;
    case 'options':
      // A Data tab switch moved (patch memory, runes against the draft, Follow the evaluation): rebuild with it, unless
      // champ select or a game is on (then after it). Switches moved during a build arrive together when it ends and
      // take one rebuild, none if they cancel out (buildSoon asks needsBuild).
      service.optionsChanged();
      parentPort!.postMessage({ type: 'status', status: status() });
      buildSoon(0);
      return;
  }
});

// The first build right away; then the data is looked at every 30 s. New games change the baseline at once (the main
// thread pushes a fresh build) and the model at its next build.
let seen = db.dataVersion();
setInterval(() => {
  try {
    const v = db.dataVersion();
    if (v !== seen) { seen = v; parentPort!.postMessage({ type: 'data' }); }
    if (!buildTimer && !buildRunning && service.needsBuild()) buildSoon(0);
    const trimmed = service.trimCaches();
    if ((trimRows() || trimmed) && !env.busy) releaseMemory();
  } catch (err) { console.warn('[model] data check failed', err); }
}, 30_000).unref();
parentPort!.postMessage({ type: 'status', status: status() });
buildSoon(300);
