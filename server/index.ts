import fs from 'node:fs';
import http from 'node:http';
import os from 'node:os';
import path from 'node:path';
import v8 from 'node:v8';
import express from 'express';
import { WebSocketServer, WebSocket } from 'ws';
import { buildPlan, planStatus } from '../shared/buildPlan.ts';
import { toLane } from '../shared/lanes.ts';
import type {
  ActionEstimates, BestBuy, BuildMode, BuildRecommendation, BuildState, DraftResponse, EvaluationPlan, GameReview, Lane, LcuState, LiveGameState,
  LiveNext, LivePerformance, JobName, JobsStatus, Matchup, ModelStatus, PersonalReport, ScraperStatus, ServerEvent, ShownBuild, SystemInfo,
} from '../shared/types.ts';
import { readAppConfig, updateAppConfig } from './appConfig.ts';
import { config } from './config.ts';
import { openDb, swappableDb } from './db.ts';
import { historyVersions, loadStaticData, type StaticData } from './ddragon.ts';
import { analyseLiveGame, identify, type AllGameData } from './engine/adaptive.ts';
import { buildExclusivityIndex, type ExclusivityIndex } from './engine/exclusivity.ts';
import { itemCatalog } from './engine/itemCatalog.ts';
import { guessLaneOpponents } from './engine/laneOpponent.ts';
import { exposeGcToThreads } from './gc.ts';
import { GameMode } from './gameMode.ts';
import { Conversion, TIMELINE_CURSOR } from './convert.ts';
import { closeCacheStores } from './riot/diskCache.ts';
import { recoverSlim, SLIM_META } from './slim.ts';
import { SlimControl } from './slimControl.ts';
import { AutoEvaluation } from './autoEval.ts';
import { CANCELLED, Jobs, type EvaluationResult } from './jobs.ts';
import { LcuConnector, parseChampSelect, type LcuAccount } from './lcu/connector.ts';
import { importBuild } from './lcu/importer.ts';
import { LiveClientPoller, PerformanceTracker } from './lcu/liveClient.ts';
import { simulateLive } from './lcu/simulateLive.ts';
import { MatchupSession, type LiveView } from './matchup.ts';
import { ModelClient } from './model/client.ts';
import { linkRecommendations } from './model/personal.ts';
import { ChampionProfiles, type ChampionTotals } from './model/profiles.ts';
import { planEvaluation } from './model/evalBase.ts';
import { adoptIncomingEvaluation, incomingEvaluation } from './model/evalTravel.ts';
import { evalHistory } from './model/evalHistory.ts';
import { availableMemory, cores, memoryReserve, planThreads } from './model/evalTeam.ts';
import { evalSize, plannedThreads, THREAD_LAYOUT, threadBytes } from './model/runEval.ts';
import { currentMemory, currentMode, currentScoring, currentSwaps, evalDir, modelPatches, readEvaluation } from './model/service.ts';
import { HISTORY_PATCHES, MEMORY_PATCHES } from './model/weights.ts';
import { OldPatches } from './oldPatches.ts';
import { expectedMs, Progress, recordTiming, threadMeasure } from './progress.ts';
import { gpuRoutes } from './routes/gpu.ts';
import { settingsRoutes } from './routes/settings.ts';
import { storageRoutes } from './routes/storage.ts';
import { ScraperController } from './scraper/controller.ts';
import { loadScope } from './scrapeScope.ts';
import { Backup, backupRoutes } from './routes/backup.ts';

// Swappable so the data folder can move at runtime (routes/storage.ts) without re-wiring its users.
// Before any thread starts: the evaluation and the model thread collect their garbage after heavy work (server/gc.ts).
exposeGcToThreads();
// The desktop shell (desktop/serverProcess.ts) runs this file in an Electron utility process. Until the server listens,
// it hears what the start is doing every 2 s (0.14.3): it waits as long as that keeps coming, and shows it.
const desktop = (process as NodeJS.Process & { parentPort?: { postMessage(msg: unknown): void } }).parentPort;
let starting = 'Starting the middleware…';
const startBeat = desktop ? setInterval(() => desktop.postMessage({ type: 'starting', text: starting }), 2000) : null;
// Slim stored data cut short by a crash or a closed app (0.14.0): undone before anything opens the database.
let rollbackStep = '';
const recoveredSlim = await recoverSlim(config.dataDir, config.riotCacheDir, (text) => {
  starting = `Rolling back the Slim stored data run that was cut short: ${text}…`;
  // The log gets each part once, not each count.
  const step = text.replace(/[:(].*$/, '').trim();
  if (step !== rollbackStep) { rollbackStep = step; console.log(`[slim] rolling back the run cut short: ${step}`); }
}).catch((err) => {
  console.warn('[slim] could not recover the last run:', err instanceof Error ? err.message : err);
  return null;
});
if ((recoveredSlim?.step === 'rolled-back' || recoveredSlim?.step === 'cancelled') && recoveredSlim.note) console.log(`[slim] ${recoveredSlim.note}`);
starting = 'Starting the middleware…';
const store = swappableDb(openDb(config.dbPath));
const db = store.handle;
if (recoveredSlim && (recoveredSlim.step === 'rolled-back' || recoveredSlim.step === 'cancelled') && recoveredSlim.note) db.setMeta(SLIM_META, JSON.stringify(recoveredSlim));
console.log(`[data] ${config.dataDir}`);
let data: StaticData = await loadStaticData();
let excl: ExclusivityIndex = buildExclusivityIndex(data);
console.log(`[ddragon] patch ${data.patch} (${data.version}), previous ${data.previousPatch}`);

// ---------------------------------------------------------------- realtime fan-out
const app = express();
app.use(express.json());
const server = http.createServer(app);
const wss = new WebSocketServer({ server, path: '/ws' });

function broadcast(ev: ServerEvent) {
  const msg = JSON.stringify(ev);
  for (const c of wss.clients) if (c.readyState === WebSocket.OPEN) c.send(msg);
}

// ---------------------------------------------------------------- worker threads (server/workers.ts)
// Riot API traffic, the model and the one-shot jobs each run in their own thread: nothing here waits on them.
const scraper = new ScraperController(data.patch, () => config.dataDir, data.previousPatch, () => config.riotApiKey);
scraper.on('status', (status) => broadcast({ type: 'scraper', status: withBackfillProgress(status) }));

/**
 * Rebuild from cache (the backfill, in the scraper thread): its time left, from its last run's pace here until its own
 * says more, and its time kept once it's done (only a run that read matches: one with nothing to do says nothing).
 */
let backfillRun: { startedAt: number; tracker: Progress | null; recorded: boolean } | null = null;
function withBackfillProgress(status: ScraperStatus): ScraperStatus {
  const b = status.backfill;
  if (!b) return status;
  if (!backfillRun || backfillRun.startedAt !== b.startedAt) backfillRun = { startedAt: b.startedAt, tracker: null, recorded: false };
  if (b.finishedAt === null) {
    if (!backfillRun.tracker && b.total > 0) backfillRun.tracker = new Progress(expectedMs('backfill', b.total), b.startedAt);
    const tracker = backfillRun.tracker ?? new Progress(null, b.startedAt);
    return { ...status, backfill: { ...b, progress: tracker.info(b.total > 0 ? b.done / b.total : null) } };
  }
  if (!backfillRun.recorded && !b.phase.startsWith('Failed') && b.total > 0) {
    recordTiming('backfill', { size: b.total, ms: b.finishedAt - b.startedAt, at: b.finishedAt });
    backfillRun.recorded = true;
  }
  return status;
}
scraper.on('ratelimit', (snapshot) => broadcast({ type: 'ratelimit', snapshot }));
const workerBoot = () => ({ dataDir: config.dataDir, versions: data.versions ?? [data.version] });
const models = new ModelClient(workerBoot);
// Backup of the pool (0.18.0): the backup folder, Back up now, importing a ZIP from another computer (routes/backup.ts).
const rebuildAfterImport = () => { if (matchup.phase !== 'game') requestBuild(); };
const backup = new Backup({
  dataDir: () => config.dataDir, versions: () => data.versions ?? [data.version],
  broadcast: (status) => broadcast({ type: 'backup', status }), rebuild: rebuildAfterImport,
});
// What the evaluation's threads share the heap with (0.15.1): this thread, the model and the scraper (model/evalTeam.ts).
const otherThreadsHeap = async () => v8.getHeapStatistics().total_heap_size + (await models.heapSize()) + (await scraper.heapSize());
const jobs = new Jobs(workerBoot, plannedThreads, otherThreadsHeap);
// Champ select and games (0.14.0): below-normal priority, and the threads' garbage collected when one starts.
const gameMode = new GameMode({ collect: () => [scraper.collectGarbage(), models.collectGarbage()], log: (line) => console.log(line) });
scraper.setInGame(false, readAppConfig().pauseFetchingInGame !== false);
// Evaluate builds by itself (server/autoEval.ts): set up below, once the data folder's state can be read. It also
// works out what Evaluate builds would run now (the plan, 0.16.0), for the Data tab.
let autoEval: AutoEvaluation | null = null;
const jobsStatus = (): JobsStatus => {
  const plan = autoEval?.plan() ?? null;
  return { ...jobs.status, ...(autoEval ? { auto: autoEval.status() } : {}), ...(plan ? { plan } : {}) };
};
jobs.on('status', () => {
  // No model build starts while an evaluation runs (0.15.1): the two share one heap (model/worker.ts).
  models.setEnv({ evaluating: jobs.status.running.some((r) => r.job === 'evaluate') });
  broadcast({ type: 'jobs', status: jobsStatus() });
});

// The live threat summary uses the champion profiles of the last model build (from the model worker).
let profiles: ChampionProfiles | null = null;
models.on('status', (status: ModelStatus, rows?: { current: ChampionTotals[]; older: ChampionTotals[] }) => {
  if (rows) profiles = new ChampionProfiles(rows.current, rows.older, data);
  broadcast({ type: 'model', status });
  if (status.state === 'ready' || status.state === 'failed' || !build) requestBuild();
});
// New games refresh the build, except in game: the overlay's plan never changes under you mid-game.
models.on('data', () => { if (matchup.phase !== 'game') requestBuild(); });

// The high-elo builds use the scope's build sources (OTP games by default; never the user's own games).
const buildSources = () => { try { return loadScope().buildSources; } catch { return ['otp' as const]; } };

/** What a build looks like in rec_log. */
function shownBuild(b: BuildRecommendation): ShownBuild {
  return {
    keystone: b.runes?.value.perks[0] ?? null, core: b.coreBuild[0]?.value ?? [], boots: b.boots[0]?.value ?? null,
    items: [b.situational.slot4[0], b.situational.slot5[0], b.situational.slot6[0]].filter(Boolean).map((o) => o!.value),
  };
}

// ---------------------------------------------------------------- LCU + live game
// For each lane, the enemy pick Irelia faced most often there, else one whose class fits the lane, never the same
// pick for two lanes (engine/laneOpponent.ts).
// Champ select asks on every hover: who Irelia faced in each lane is read at most once a minute.
const facedCache = new Map<Lane, { at: number; faced: Map<number, number> }>();
function facedIn(lane: Lane) {
  const hit = facedCache.get(lane);
  if (hit && Date.now() - hit.at < 60_000) return hit.faced;
  const faced = new Map(db.opponentCounts([data.patch, data.previousPatch], lane, buildSources()).map((r) => [r.id, r.games]));
  facedCache.set(lane, { at: Date.now(), faced });
  return faced;
}
function guessOpponents(enemyIds: number[], lanes: Lane[]) {
  return guessLaneOpponents(enemyIds, lanes, Object.fromEntries(lanes.map((lane) => [lane, facedIn(lane)])), data.champions);
}

const lcu = new LcuConnector(config.leaguePaths, config.championId, guessOpponents);
// Who you face in game is the app's matchup (below), so the tracker, the threat summary and "Next" agree with it.
const live = new LiveClientPoller((itemId) => data.items.get(itemId)?.goldTotal ?? 0,
  { activeMs: config.livePollMs, idleMs: config.liveIdlePollMs }, (raw) => liveHint(raw).opponentName);
let liveState: LiveGameState | null = null;

// Simulated draft (POST /api/dev/simulate-draft) is shown only while the real client has no draft of its own.
let simulatedDraft: LcuState['champSelect'] = null;
const effectiveLcu = (): LcuState => lcu.state.champSelect || !simulatedDraft
  ? lcu.state
  : { ...lcu.state, phase: 'ChampSelect (simulated)', champSelect: simulatedDraft };

// ---------------------------------------------------------------- the matchup (server/matchup.ts) and its build
// One matchup for every window: the dashboard and the overlay show the same lane, opponent and build. The build is
// made in the model worker and pushed over /ws whenever the matchup, the mode or the data changes.
const session = new MatchupSession({
  savedLane: () => readAppConfig().lane ?? null,
  saveLane: (lane) => { try { updateAppConfig({ lane }); } catch (err) { console.warn('[config]', err); } },
  guess: guessOpponents,
});
let liveView: LiveView | null = null;
let matchup: Matchup = session.resolve(effectiveLcu(), null);
let build: BuildState | null = null;

/**
 * In queue (0.15.0): champ select can begin any second, and a model build started now (a minute or two on a large
 * database) would still hold the model thread through it. Builds wait; nothing else changes.
 */
const QUEUE_PHASES = new Set(['Matchmaking', 'ReadyCheck']);

function refreshMatchup() {
  const next = session.resolve(effectiveLcu(), liveView);
  const busy = next.phase !== 'idle';
  models.setEnv({ busy, queued: QUEUE_PHASES.has(lcu.state.phase) });
  // Champ select or a game stops a running evaluation's other threads, so their memory goes to the game (it finishes on one).
  jobs.setBusy(busy);
  // ...pauses fetching (Settings → Pause fetching during games) and lowers the middleware's priority.
  scraper.setInGame(busy);
  gameMode.set(busy);
  conversion.setBusy(busy);
  slim?.setBusy(busy);
  if (JSON.stringify(next) === JSON.stringify(matchup)) return;
  const rebuild = next.key !== matchup.key;
  const phaseChanged = next.phase !== matchup.phase;
  matchup = next;
  broadcast({ type: 'matchup', matchup });
  if (rebuild) requestBuild();
  if (phaseChanged) autoEval?.check();   // champ select or a game pauses an automatic evaluation at once
}

// Builds are asked for one at a time; a change meanwhile asks again once the current one is back.
let buildTimer: NodeJS.Timeout | null = null;
let building = false, buildAgain = false;
let buildRetryMs = 5000;   // after a failed build: 5 s, doubling to a minute, back to 5 s on success
function requestBuild(delayMs = 60) {
  if (buildTimer) clearTimeout(buildTimer);
  buildTimer = setTimeout(() => { buildTimer = null; void runBuild(); }, delayMs);
}
async function runBuild() {
  if (building) { buildAgain = true; return; }
  building = true;
  const m = matchup;
  try {
    const response = await models.request<DraftResponse>('draft',
      { lane: m.lane, opponent: m.opponentId, enemies: m.enemies, allies: m.allies, regions: m.regions }, 120_000);
    build = { key: m.key, response, at: Date.now() };
    buildRetryMs = 5000;
    broadcast({ type: 'build', build });
    logDraft(m, response);
  } catch (err) {
    console.warn('[build]', err instanceof Error ? err.message : err);
    // The worker is restarting or busy: try again, so no window keeps a build for an old matchup, mode or patch.
    setTimeout(() => requestBuild(), buildRetryMs).unref();
    buildRetryMs = Math.min(60_000, buildRetryMs * 2);
  } finally {
    building = false;
    if (buildAgain) { buildAgain = false; void runBuild(); }
  }
}

// rec_log sessions: one row per champ select and per game (server/model/personal.ts links them to your games).
let champSelectSince: number | null = null;
let gameSince: number | null = null;
function logDraft(m: Matchup, response: DraftResponse) {
  // During a real champ select on Irelia, remember what was shown (the personal layer's "shown versus bought").
  if (!champSelectSince || lcu.state.phase !== 'ChampSelect' || !lcu.state.champSelect?.isIrelia) return;
  const shown = response.mode === 'draft' && response.draft ? response.draft.build : response.baseline;
  try {
    db.logRecommendation({ session: `draft:${champSelectSince}`, phase: 'draft', lane: m.lane, opponentId: m.opponentId, mode: response.mode, shown: shownBuild(shown) });
  } catch (err) { console.warn('[rec_log]', err); }
}

// The signed-in account's games are fetched as source "me" (never part of the high-elo builds), and again after each game.
lcu.on('account', (account: LcuAccount) => scraper.setMe(account));
// Your server (the region boost of the draft model) as the scraper resolved it.
scraper.on('status', (status: ScraperStatus) => { if (status.me?.platform) models.setEnv({ ownPlatform: status.me.platform }); });
lcu.on('gameEnded', () => scraper.gameEnded());
lcu.on('state', (state: LcuState) => {
  champSelectSince = state.phase === 'ChampSelect' ? champSelectSince ?? Date.now() : null;
  broadcast({ type: 'lcu', state: effectiveLcu() });
  refreshMatchup();
  // Without an LCU connection we keep polling the live API anyway (companion opened mid-game).
  if (state.phase === 'InProgress' || state.phase === 'Reconnect' || state.status !== 'connected') live.start(); else live.stop();
});

/** The live game in the matchup's terms, and who you face: the matchup's opponent, by champion name. */
const championId = (p: { championName: string }) => data.championsByKey.get(p.championName)?.id ?? null;
let hintCache: { raw: AllGameData; hint: { opponentName: string | undefined; lane: Lane } } | null = null;
function liveHint(raw: AllGameData) {
  if (hintCache?.raw === raw) return hintCache.hint;
  const { me, opponent } = identify(raw);
  const ids = (ps: AllGameData['allPlayers']) => ps.map(championId).filter((id): id is number => id !== null);
  liveView = {
    lane: toLane(me?.position), positionOpponent: opponent ? championId(opponent) : null,
    enemies: me ? ids(raw.allPlayers.filter((p) => p.team !== me.team)) : [],
    allies: me ? ids(raw.allPlayers.filter((p) => p.team === me.team && p !== me)) : [],
  };
  const m = session.resolve(effectiveLcu(), liveView);
  const enemy = me && m.opponentId !== null ? raw.allPlayers.find((p) => p.team !== me.team && championId(p) === m.opponentId) : undefined;
  // No opponent in the matchup ("All matchups", or none known): the enemy in your position, if any, still is the lane
  // opponent for the CS / vision / gold comparison (undefined lets identify() use the position).
  const hint = { opponentName: enemy?.championName ?? undefined as string | undefined, lane: m.lane };
  hintCache = { raw, hint };
  return hint;
}
const OPPONENT_FROM = { game: 'position', draft: 'draft', manual: 'manual', guess: 'guess' } as const;

// Every Summoner's Rift item as the build plan sees it (shared/buildPlan.ts), once per Data Dragon version.
let catalog: { version: string; items: ReturnType<typeof itemCatalog> } | null = null;
const planItems = () => (catalog?.version === data.version ? catalog.items : (catalog = { version: data.version, items: itemCatalog(data, excl) }).items);

/**
 * The build plan's next entry for your inventory, in the build shown: the overlay's "Next" in lane-baseline mode, and
 * in either mode when it is the boots (the plan puts them where high-elo games finish them).
 */
function planTarget(raw: AllGameData, opponentName: string | undefined): { id: number; boots: boolean } | null {
  if (!build) return null;
  const r = build.response;
  const shown = r.mode === 'draft' && r.draft ? r.draft.build : r.baseline;
  const owned = identify(raw, opponentName).me?.items.map((i) => i.itemID).filter((id) => id > 0) ?? [];
  const next = planStatus(buildPlan(shown, planItems()), owned, planItems()).next;
  return next ? { id: next.id, boots: next.slot === 'Boots' } : null;
}

// In the model worker on every poll: the next item re-ranked from the live game (draft-adjusted mode, build advice
// only) and the best purchase for your gold toward it (Stage C3, shop advice only). One request at a time; the live
// view never waits for it.
let liveNext: LiveNext | null = null;
let liveBuy: BestBuy | null = null;
let reranking = false;
function onLiveData(raw: AllGameData, performance: LivePerformance | null, simulated = false) {
  gameSince ??= Date.now() - raw.gameData.gameTime * 1000;
  const hint = liveHint(raw);
  refreshMatchup();
  const analysis = analyseLiveGame(raw, data, excl, profiles, hint.opponentName);
  liveState = {
    ...analysis,
    // Without a matchup opponent the lane opponent shown is the enemy in your position.
    laneOpponentFrom: matchup.opponentId !== null && matchup.opponentFrom ? OPPONENT_FROM[matchup.opponentFrom] : analysis.laneOpponent ? 'position' : null,
    performance, next: liveNext, buy: liveBuy,
  };
  broadcast({ type: 'live', state: liveState });
  if (reranking) return;
  reranking = true;
  // The worker needs the players, your gold and the time; the event feed (hundreds of entries late) stays here.
  const lite: AllGameData = { activePlayer: raw.activePlayer, allPlayers: raw.allPlayers, gameData: raw.gameData };
  models.request<{ next: LiveNext | null; buy: BestBuy | null }>('live', { raw: lite, hint, planTarget: planTarget(raw, hint.opponentName) }, 10_000)
    .then(({ next, buy }) => {
      if (!liveState?.active || (JSON.stringify(next) === JSON.stringify(liveNext) && JSON.stringify(buy) === JSON.stringify(liveBuy))) return;
      liveNext = next;
      liveBuy = buy;
      liveState = { ...liveState, next, buy };
      broadcast({ type: 'live', state: liveState });
      if (next && !simulated) logLiveNext(next.itemId);
    }).catch((err) => console.warn('[model] live re-rank failed:', err instanceof Error ? err.message : err))
    .finally(() => { reranking = false; });
}
let liveShown: number[] = [];
function logLiveNext(itemId: number) {
  if (liveShown[liveShown.length - 1] === itemId) return;
  liveShown = [...liveShown, itemId];
  try {
    db.logRecommendation({ session: `live:${gameSince}`, phase: 'live', lane: null, opponentId: null, mode: 'draft',
      shown: { keystone: null, core: [], boots: null, items: [], next: liveShown } satisfies ShownBuild });
  } catch (err) { console.warn('[rec_log]', err); }
}
function resetLive() {
  liveState = null;
  liveView = null;
  hintCache = null;
  liveNext = null;
  liveBuy = null;
  gameSince = null;
  liveShown = [];
  models.liveReset();
}
function onLiveEnded() {
  resetLive();
  broadcast({ type: 'live', state: { active: false, gameTime: 0, myGold: 0, players: [], laneOpponent: null, threat: { apShare: 0, tanks: 0, healers: 0 }, suggestions: [], performance: null } });
  refreshMatchup();
}
// Simulated game (POST /api/dev/simulate-live) runs until stopped or a real game starts.
let stopSimulation: (() => void) | null = null;
live.on('data', (raw: AllGameData, performance: LivePerformance | null) => {
  if (stopSimulation) { stopSimulation(); stopSimulation = null; resetLive(); }
  onLiveData(raw, performance);
});
live.on('ended', () => { if (!stopSimulation) onLiveEnded(); });
lcu.start();
requestBuild(0);

// ---------------------------------------------------------------- patch watch
setInterval(async () => {
  try {
    const next = await loadStaticData();
    if (next.version === data.version) return;
    data = next;
    excl = buildExclusivityIndex(data);
    scraper.setPatch(data.patch, data.previousPatch);
    models.setPatch(data.versions ?? [data.version]);   // the worker answers with 'data' once it has loaded it
    catalog = null;
    broadcast({ type: 'patch', patch: data.patch, version: data.version });
    console.log(`[ddragon] new patch ${data.patch} - older games are now excluded from recommendations`);
  } catch (e) { console.warn('[ddragon] refresh failed', e); }
}, 3600_000).unref();

// ---------------------------------------------------------------- REST API
// Earlier versions' data, re-encoded in the background while the app is idle (0.14.0, server/convert.ts).
const conversion = new Conversion({
  dataDir: () => config.dataDir, cacheDir: () => config.riotCacheDir,
  timelinesLeft: () => db.plainTimelinesAfter(Number(db.getMeta(TIMELINE_CURSOR) ?? 0)),
  onStatus: (status) => broadcast({ type: 'conversion', status }),
  log: (line) => console.log(line),
});
setTimeout(() => conversion.check(), 30_000).unref();
let slim: SlimControl | null = null;
/** Slim stored data compares the results before and after: nothing else may change the database meanwhile. */
const slimRunning = () => slim?.status.running === true;
const SLIM_RUNNING = 'Slim stored data is running: try again once it is done.';
const storage = storageRoutes({
  store, broadcast,
  // Every thread with the database open stops while the data folder moves, and starts again on the new one.
  pause: async (what) => { await Promise.all([scraper.terminate(what), models.terminate(), jobs.cancelAll(what), conversion.pause()]); },
  resume: () => { scraper.respawn(); models.respawn(); requestBuild(); conversion.resume(); },
  conversion: () => conversion.status,
  ddragonVersion: () => data.version,
  ddragonHistory: () => [...historyVersions(data.versions ?? [data.version], HISTORY_PATCHES).values()],
  blocked: () => (oldPatches.busy() ? 'Old patches are being archived, restored or cleared from the cache.'
    : slimRunning() ? 'Slim stored data is running.' : null),
});
// Old patches (server/oldPatches.ts): the archive and cache switches, applied again whenever a patch leaves the window.
const oldPatches = new OldPatches({
  db: () => db, data: () => data, cacheDir: () => config.riotCacheDir, blocked: () => storage.busy() || slimRunning(), idle: () => matchup.phase === 'idle',
  changed: (status) => broadcast({ type: 'old-patches', status }),
});
setTimeout(() => void oldPatches.maintain().catch(() => {}), 60_000).unref();

// Slim stored data (0.14.0, server/slim.ts): Settings → Storage only, never in champ select or a game.
slim = new SlimControl({
  dataDir: () => config.dataDir, cacheDir: () => config.riotCacheDir, versions: () => data.versions ?? [data.version], db: () => db,
  // The model thread keeps its model but lets go of its request caches: the check builds a model of its own (0.15.2).
  pauseWriters: async () => {
    models.release();
    await Promise.all([scraper.terminate('Slim stored data runs'), jobs.cancelAll('Slim stored data runs'), conversion.pause()]);
  },
  resumeWriters: () => { scraper.respawn(); conversion.resume(); },
  withDatabaseClosed: async (between) => {
    await models.terminate();
    store.swap(openDb(':memory:')).close();
    closeCacheStores();
    try { await between(); } finally {
      store.swap(openDb(config.dbPath)).close();
      models.respawn();
      requestBuild();
    }
  },
  busy: () => matchup.phase !== 'idle',
  blocked: () => (storage.busy() ? 'The data folder is being moved or the database compacted.'
    : oldPatches.busy() ? 'Old patches are being archived, restored or cleared from the cache.'
      : conversion.working ? 'Stored data is still being converted to the smaller format: Slim stored data runs once that is done.'
        : jobs.status.running.length ? 'Evaluate builds or the data audit is running: Slim stored data can start once it is done.' : null),
  onStatus: (status) => broadcast({ type: 'slim', status }),
  log: (line) => console.log(line),
});
app.get('/api/storage/slim', (_req, res) => { res.json(slim!.status); });
app.post('/api/storage/slim', async (req, res) => {
  const action = String(req.body?.action ?? 'start');
  try {
    if (action === 'start') res.json(await slim!.start());
    else if (action === 'rollback') res.json(await slim!.rollback());
    else if (action === 'keep') res.json(slim!.keep());
    else if (action === 'stop') res.json(slim!.stop());
    else res.status(400).json({ error: 'action must be start, stop, rollback or keep' });
  } catch (err) { res.status(409).json({ error: err instanceof Error ? err.message : String(err) }); }
});
setInterval(() => void oldPatches.maintain().catch(() => {}), 3600_000).unref();

/**
 * A finished evaluation (a click or an automatic one, a quick update or a full evaluation): it may change the default
 * mode, patch memory, the bandwidth and the prior, so the model takes it at once. Once per evaluation, even when a
 * click joined an automatic one. What the next one would run is worked out again (autoEval.check).
 */
/**
 * The other computer's evaluation an import left (0.21.0, model/evalTravel.ts): taken now when it is valid here, so its
 * verdicts apply and the next evaluation measures only the games it didn't have. While an evaluation runs it waits
 * (that one writes the same files as it ends) and is taken afterwards (afterEvaluation, and the minute's check).
 */
function adoptEvaluation(): string | null {
  if (!incomingEvaluation()) return null;
  if (jobs.runningJob('evaluate')) return 'its evaluation is checked once the evaluation running now ends';
  const r = adoptIncomingEvaluation(db, data);
  if (!r) return null;
  console.log(`[backup] ${r.message}`);
  if (r.adopted) {
    models.evaluated();
    if (matchup.phase === 'idle') models.rebuild();
    requestBuild();
    autoEval?.check();
  }
  return r.message;
}

let appliedEvaluation = '';
function afterEvaluation(result: EvaluationResult) {
  if (result.file === appliedEvaluation) return result;
  appliedEvaluation = result.file;
  if (incomingEvaluation()) adoptEvaluation();
  models.evaluated();
  if (matchup.phase === 'idle') models.rebuild();   // a new state-model penalty applies on the next build
  requestBuild();
  autoEval?.check();
  return result;
}
// With Follow the evaluation on, Evaluate builds runs again by itself: a quick update after enough new games, a full
// evaluation when the plan says one is needed (a new patch, say). Each runs what the plan says as it starts ('auto').
autoEval = new AutoEvaluation({
  following: () => readAppConfig().patchMemory === undefined,
  quiet: () => matchup.phase === 'idle',
  busy: () => storage.busy() || oldPatches.busy() || slimRunning(),
  summary: readEvaluation,
  db: () => db,
  // Reads the settings and the last evaluation's base besides counting games: only here, once a minute and after each
  // evaluation, never per request or progress message (the plan is cached in autoEval).
  plan: (): EvaluationPlan => planEvaluation(db, data),
  running: () => jobs.runningJob('evaluate'),
  start: (why, planned) => jobs.run({ job: 'evaluate', auto: why, path: 'auto', planned, size: evalSize(db, data) }).then(afterEvaluation),
  pause: () => jobs.cancel('evaluate', true),
  changed: () => broadcast({ type: 'jobs', status: jobsStatus() }),
  log: (line) => console.log(`[auto-eval] ${line}`),
});
autoEval.check();
setInterval(() => {
  try { if (incomingEvaluation()) adoptEvaluation(); } catch (err) { console.warn('[backup]', err); }
  try { autoEval?.check(); } catch (err) { console.warn('[auto-eval]', err); }
}, 60_000).unref();
// While the data folder moves (or the database is compacted), the database and caches are in flux: everything but storage waits.
app.use('/api', (req, res, next) => {
  if ((storage.busy() || slim?.status.step === 'rollback') && !req.path.startsWith('/storage') && req.path !== '/health') {
    res.status(503).json({ error: 'The data folder is being moved or the database compacted. Try again in a moment.' });
    return;
  }
  next();
});
app.use('/api/storage', storage.router);
app.use('/api', backupRoutes(backup, { rebuild: rebuildAfterImport, adoptEvaluation }));
app.use('/api/settings', settingsRoutes(scraper));
app.use('/api/gpu', gpuRoutes({ evaluating: () => jobs.status.running.some((r) => r.job === 'evaluate'), busy: () => jobs.busy }));
app.get('/api/health', (_req, res) => { res.json({ ok: true, patch: data.patch, dataDir: config.dataDir }); });

app.get('/api/meta', (_req, res) => {
  const items = planItems();   // every Summoner's Rift item, for names and the overlay's build plan
  res.json({
    version: data.version,
    patch: data.patch,
    previousPatch: data.previousPatch,
    champions: [...data.champions.values()].map(({ id, key, name, tags }) => ({ id, key, name, tags })).sort((a, b) => a.name.localeCompare(b.name)),
    items,
    runes: data.runes,
    summoners: Object.fromEntries(data.summoners),
    spells: data.spells,
  });
});

/** A model-worker request as an HTTP answer: a timeout or error becomes a 503 / 500 with the reason, never a hang. */
async function answer(res: express.Response, work: Promise<unknown>) {
  try { res.json(await work); } catch (err) {
    const msg = err instanceof Error ? err.message : String(err);
    res.status(/busy|restarting/.test(msg) ? 503 : 500).json({ error: msg });
  }
}

// The matchup every window shows, and its build (also pushed over /ws). PUT {lane} | {opponentId: id | null} (null:
// all matchups) | {follow: true} (back to the draft's or game's opponent) | {regions: [...] | null}.
app.get('/api/matchup', (_req, res) => { res.json({ matchup, build }); });
app.put('/api/matchup', (req, res) => {
  const b = req.body ?? {};
  if (b.lane !== undefined) {
    const lane = toLane(String(b.lane));
    if (!lane) { res.status(400).json({ error: `Unknown lane "${String(b.lane)}": use TOP or MIDDLE` }); return; }
    session.setLane(lane);
  }
  if (b.follow === true) session.setOpponent(undefined);
  else if (b.opponentId !== undefined) {
    const id = b.opponentId === null ? null : Number(b.opponentId);
    if (id !== null && !data.champions.has(id)) { res.status(400).json({ error: `Unknown champion ${String(b.opponentId)}` }); return; }
    session.setOpponent(id);
  }
  if (b.regions !== undefined) session.setRegions(Array.isArray(b.regions) ? b.regions.map((r: unknown) => String(r).toUpperCase()) : null);
  refreshMatchup();
  res.json({ matchup });
});

// GET /api/recommendation?enemy=122&lane=MIDDLE. Without a lane every position is mixed (the pre-lane build).
app.get('/api/recommendation', (req, res) => {
  const enemy = req.query.enemy ? Number(req.query.enemy) : null;
  const lane = req.query.lane ? toLane(String(req.query.lane)) : null;
  if (req.query.lane && !lane) {
    res.status(400).json({ error: `Unknown lane "${String(req.query.lane)}": use TOP or MIDDLE` });
    return;
  }
  void answer(res, models.request('recommendation', { enemy, lane }));
});

const ids = (v: unknown) => (v ? String(v).split(',').map(Number).filter((id) => data.champions.has(id)) : []);

// GET /api/recommendation/draft?lane=TOP&enemy=122&enemies=122,64,103&allies=121,86&regions=KR,EUW1
// The lane baseline and the draft-adjusted build side by side (Stage B), for any draft (the app's own matchup is
// pushed over /ws instead).
app.get('/api/recommendation/draft', (req, res) => {
  const lane = toLane(String(req.query.lane ?? 'TOP'));
  if (!lane) { res.status(400).json({ error: `Unknown lane "${String(req.query.lane)}": use TOP or MIDDLE` }); return; }
  const enemy = req.query.enemy && data.champions.has(Number(req.query.enemy)) ? Number(req.query.enemy) : null;
  const regions = req.query.regions ? String(req.query.regions).split(',').map((r) => r.trim().toUpperCase()).filter(Boolean) : null;
  void answer(res, models.request('draft', { lane, opponent: enemy, enemies: ids(req.query.enemies), allies: ids(req.query.allies), regions }));
});

// The draft model's state, and the dashboard's Draft-adjusted / Lane baseline choice ({"mode": null} follows eval:builds).
app.get('/api/model', (_req, res) => { res.json(models.status); });
app.put('/api/model/mode', (req, res) => {
  const mode = req.body?.mode as BuildMode | null;
  if (mode !== null && mode !== 'draft' && mode !== 'baseline') { res.status(400).json({ error: 'mode must be "draft", "baseline" or null' }); return; }
  updateAppConfig({ buildMode: mode ?? undefined });
  models.evaluated();
  requestBuild(0);
  res.json({ mode: currentMode() });
});

// Patch memory ({"memory": "on" | "off" | null}; null follows the last evaluation, i.e. Follow the evaluation on): the
// model rebuilds with its window.
app.put('/api/model/memory', (req, res) => {
  const memory = req.body?.memory as 'on' | 'off' | null;
  if (memory !== null && memory !== 'on' && memory !== 'off') { res.status(400).json({ error: 'memory must be "on", "off" or null' }); return; }
  // Leaving Follow the evaluation keeps runes and summoners against the draft and late-game swaps as they are now (the
  // choice is yours).
  if (memory !== null && readAppConfig().patchMemory === undefined) {
    updateAppConfig({ pregameScoring: currentScoring() ? 'on' : 'off', lateSwaps: currentSwaps() ? 'on' : 'off' });
  }
  updateAppConfig({ patchMemory: memory ?? undefined });
  switchesSaved();
  autoEval?.check();   // Follow the evaluation decides whether it runs by itself
  res.json({ memory });
});

// Runes and summoners against the draft ({"scoring": "on" | "off"}): your choice, so Follow the evaluation goes off and
// patch memory and late-game swaps keep what they are now (as moving patch memory's own switch does). The model rebuilds.
app.put('/api/model/scoring', (req, res) => {
  const scoring = req.body?.scoring as 'on' | 'off';
  if (scoring !== 'on' && scoring !== 'off') { res.status(400).json({ error: 'scoring must be "on" or "off"' }); return; }
  const chosen = readAppConfig().patchMemory, following = chosen === undefined;
  const memory = chosen ?? (currentMemory() !== null ? 'on' : 'off');
  updateAppConfig({ pregameScoring: scoring, patchMemory: memory, ...(following ? { lateSwaps: currentSwaps() ? 'on' : 'off' } : {}) });
  switchesSaved();
  autoEval?.check();
  res.json({ scoring, memory });
});

// Late-game swaps ({"swaps": "on" | "off"}, docs/SWAPS.md): your choice, so Follow the evaluation goes off and patch
// memory and runes against the draft keep what they are now. Only the Items view changes: no rebuild is due for it.
app.put('/api/model/swaps', (req, res) => {
  const swaps = req.body?.swaps as 'on' | 'off';
  if (swaps !== 'on' && swaps !== 'off') { res.status(400).json({ error: 'swaps must be "on" or "off"' }); return; }
  const chosen = readAppConfig().patchMemory, following = chosen === undefined;
  const memory = chosen ?? (currentMemory() !== null ? 'on' : 'off');
  updateAppConfig({ lateSwaps: swaps, patchMemory: memory, ...(following ? { pregameScoring: currentScoring() ? 'on' : 'off' } : {}) });
  switchesSaved();
  autoEval?.check();
  res.json({ swaps, memory });
});

/**
 * A Data tab switch was saved (here only: the model thread just reads them). The model rebuilds with it, and the Data
 * tab shows it at once rather than when the model thread answers, which is after a build it may be in (0.9.2).
 */
function switchesSaved() {
  const status = models.optionsChanged();
  if (status.info) broadcast({ type: 'model', status });
}

// The Data tab's evaluation history: every saved report, reduced to its comparisons (server/model/evalHistory.ts).
app.get('/api/model/evaluations', (_req, res) => { res.json(evalHistory(evalDir())); });

// npm run eval:builds in the app (its own thread; seconds to a few minutes with the number of stored games). The
// gate result decides the default mode. Its progress and result are also pushed over /ws ('jobs').
// {"path": "auto"} (the default: Evaluate builds) runs what the plan says as it starts (model/evalBase.ts, 0.16.0): a
// quick update while the last full evaluation still describes the stored games, else a full one; {"path": "full"}
// (Full evaluation) evaluates every game again. {"quick": true}: one bandwidth and prior, no live replay (full).
// Asked for while one runs, it joins it; a full evaluation asked for while a quick update runs replaces it.
app.post('/api/model/evaluate', async (req, res) => {
  if (slimRunning()) { res.status(409).json({ error: SLIM_RUNNING }); return; }
  const quick = req.body?.quick === true;
  const asked = (req.body?.path ?? 'auto') as unknown;
  if (asked !== 'auto' && asked !== 'full') { res.status(400).json({ error: 'path must be "auto" or "full"' }); return; }
  // What the plan says now (worked out every minute, autoEval): for its estimate, and nothing to add when up to date.
  const plan = asked === 'auto' && !quick ? autoEval?.plan() ?? null : null;
  if (plan?.path === 'none' && !jobs.runningJob('evaluate')) {
    res.status(409).json({ error: 'Up to date: no new games since the last evaluation. Full evaluation evaluates every game again.' });
    return;
  }
  try {
    const result = afterEvaluation(await jobs.run({
      job: 'evaluate', quick, path: quick ? 'full' : asked, planned: plan?.path === 'update' ? 'update' : 'full', size: evalSize(db, data),
    }));
    res.json({ ...result, mode: currentMode() });
  } catch (err) {
    const message = err instanceof Error ? err.message : String(err);
    res.status(message === CANCELLED ? 409 : 500).json({ error: message });
  }
});
// Stops a running evaluation or audit ({"job":"evaluate"|"audit"}); its request ends with "Cancelled".
app.post('/api/jobs/cancel', async (req, res) => {
  const job = req.body?.job as JobName;
  if (job !== 'evaluate' && job !== 'audit') { res.status(400).json({ error: 'job must be "evaluate" or "audit"' }); return; }
  await jobs.cancel(job);
  res.json(jobsStatus());
});

// Personal layer: your own games (source "me") against the high-elo games. GET /api/personal?lane=TOP
app.get('/api/personal', (req, res) => {
  linkRecommendations(db);
  const lane = req.query.lane ? toLane(String(req.query.lane)) : null;
  void answer(res, models.request<PersonalReport | null>('personal', { lane }).then((report) => ({
    report,
    games: db.ownGames(20).map((g) => ({ matchId: g.match_id, created: g.created_at, win: g.win === 1, lane: toLane(g.position), opponentId: g.opponent_champion_id })),
  })));
});
app.get('/api/personal/review/:matchId', (req, res) => {
  linkRecommendations(db);
  void models.request<GameReview | null>('review', { matchId: req.params.matchId }).then((review) => {
    if (!review) res.status(404).json({ error: 'Not one of your stored games' }); else res.json(review);
  }, (err) => res.status(503).json({ error: err instanceof Error ? err.message : String(err) }));
});

// Stage C1: the build shown, saved in the League client as a rune page and an item set. Only on your click (the
// dashboard sends the key of the build it shows), only during a real champ select, never in game, and only from the
// app's own pages: a request from any other site is refused.
const isLocalOrigin = (origin: string | undefined) => {
  if (!origin) return true;
  try { return ['localhost', '127.0.0.1'].includes(new URL(origin).hostname); } catch { return false; }
};
app.post('/api/lcu/import', async (req, res) => {
  if (!isLocalOrigin(req.headers.origin)) { res.status(403).json({ error: 'Only the app itself can write to the League client.' }); return; }
  if (lcu.state.status !== 'connected' || lcu.state.phase !== 'ChampSelect' || !lcu.state.champSelect || liveState?.active) {
    res.status(409).json({ error: 'Builds can only be saved to the League client during champ select.' });
    return;
  }
  if (!lcu.state.champSelect.isIrelia) { res.status(409).json({ error: 'Pick Irelia first: the rune page would become your current page.' }); return; }
  const mode = req.body?.mode === 'draft' ? 'draft' : 'baseline';
  // The build must be the one shown, and made for the matchup now (not one still being replaced after a pick).
  if (!build || build.key !== req.body?.key || build.key !== matchup.key) {
    res.status(409).json({ error: 'The build just changed. Check it and press again.' });
    return;
  }
  const shown = mode === 'draft' && build.response.draft ? build.response.draft.build : build.response.baseline;
  try {
    const result = await importBuild(lcu, shown, {
      lane: matchup.lane, opponentName: matchup.opponentId !== null ? data.champions.get(matchup.opponentId)?.name ?? null : null,
      owned: readAppConfig().lcuImport ?? {}, best: build.response.facts?.best ?? null,
    });
    updateAppConfig({ lcuImport: result.owned });
    res.json(result);
  } catch (err) { res.status(500).json({ error: err instanceof Error ? err.message : String(err) }); }
});

app.get('/api/state', (_req, res) => {
  res.json({ lcu: effectiveLcu(), live: liveState, scraper: scraper.status, matchup, build, model: models.status, jobs: jobsStatus() });
});

app.post('/api/scraper/start', (_req, res) => { scraper.start(); res.json({ ok: true }); });
app.post('/api/scraper/stop', (_req, res) => { scraper.stop(); res.json({ ok: true }); });

// Counts, patches and regions for the Data tab: read in the model worker (a large database takes a moment).
app.get('/api/db/stats', (_req, res) => { void answer(res, models.request('stats')); });

// Read-only data audit (the same report as npm run audit:data): no API calls, nothing written to the data folder.
// The report is also saved under %APPDATA%\irelia-companion\audits. Takes about a minute on a large database.
app.post('/api/db/audit', async (_req, res) => {
  if (slimRunning()) { res.status(409).json({ error: SLIM_RUNNING }); return; }
  try { res.json(await jobs.run({ job: 'audit', size: db.counts(data.patch).matches })); } catch (err) { res.status(500).json({ error: err instanceof Error ? err.message : String(err) }); }
});

// Rebuilds participants, quality flags and timelines of stored matches from the disk cache: no API calls.
// {"force":true} redoes matches already captured. Progress arrives in the scraper status (backfill).
app.post('/api/db/backfill', (req, res) => {
  scraper.backfill(req.body?.force === true);
  res.json({ ok: true });
});

// Keeps every patch within patch memory's window of a region's current patch (a region can lag behind Data Dragon),
// plus Data Dragon's pair: what the model can read, with patch memory on or off.
app.post('/api/db/purge-stale', async (_req, res) => {
  if (slimRunning()) { res.status(409).json({ error: SLIM_RUNNING }); return; }
  if (oldPatches.busy()) { res.status(409).json({ error: 'Old patches are being archived, restored or cleared from the cache.' }); return; }
  const keep = modelPatches(db, data, MEMORY_PATCHES);
  const size = db.staleCount(keep);
  // One transaction on this thread: shown with its estimate before it starts, its time kept for the next.
  const tracker = new Progress(size > 0 ? expectedMs('purge', size) : null);
  broadcast({ type: 'storage-action', action: { action: 'purge', text: 'Deleting the games of old patches…', progress: tracker.info(null) } });
  await new Promise((r) => setTimeout(r, 30));
  try {
    const removed = db.purgeStale(keep);
    if (removed) db.reclaim();   // the space comes back (0.14.0)
    if (size > 0) recordTiming('purge', { size, ms: Date.now() - tracker.startedAt, at: Date.now() });
    res.json({ removed });
  } catch (err) {
    res.status(500).json({ error: err instanceof Error ? err.message : String(err) });
  } finally {
    broadcast({ type: 'storage-action', action: null });
  }
});

// How long each long action would take if started now (estimates: this computer's last run of it, scaled to the data as
// it is now; null when it has never run here and there is nothing to go on). The Storage card's come with its stats.
app.get('/api/estimates', (_req, res) => {
  const counts = db.counts(data.patch);
  const games = evalSize(db, data);
  const threads = plannedThreads(games);
  const estimates: ActionEstimates = {
    // A full evaluation and a quick update (0.16.0), each from its own last run: the Data tab shows the one it would run.
    evaluate: expectedMs('evaluate', games, { threads }),
    'evaluate-update': expectedMs('evaluate-update', games, { threads }),
    audit: expectedMs('audit', counts.matches),
    purge: expectedMs('purge', db.staleCount(modelPatches(db, data, MEMORY_PATCHES))),
    ...oldPatches.estimates(),
  };
  res.json(estimates);
});

// This computer (0.11.0): the processor, its threads, the memory, and the threads Evaluate builds would use now. Read
// here for the Data tab, never sent anywhere.
app.get('/api/system', (_req, res) => {
  const games = evalSize(db, data);
  const threads = plannedThreads(games);
  const info: SystemInfo = {
    processor: os.cpus()[0]?.model.trim() || null,
    cores: cores(),
    memory: { total: os.totalmem(), available: availableMemory(), reserve: memoryReserve() },
    evaluation: {
      threads, perThread: threadBytes(games), measured: threadMeasure(THREAD_LAYOUT) !== null,
      limit: threads >= cores() ? 'cores' : planThreads(threadBytes(games)) > threads ? 'heap' : 'memory',
    },
  };
  res.json(info);
});

// The archive and cache switches ({"archive": true|false} and / or {"trimCache": true|false}): the answer comes once
// the change is applied (archiving, restoring or trimming takes seconds on a large folder). Nothing is deleted but
// cached Riot files of patches older than the window.
app.get('/api/old-patches', (_req, res) => { res.json(oldPatches.status()); });
app.put('/api/old-patches', async (req, res) => {
  const { archive, trimCache } = req.body ?? {};
  if ((archive !== undefined && typeof archive !== 'boolean') || (trimCache !== undefined && typeof trimCache !== 'boolean')) {
    res.status(400).json({ error: 'archive and trimCache must be true or false' }); return;
  }
  let status = oldPatches.status();
  if (archive !== undefined) status = await oldPatches.setArchive(archive);
  if (trimCache !== undefined) status = await oldPatches.setTrimCache(trimCache);
  res.json(status);
});

// Draft simulator: lets you exercise the dashboard without the League client running.
// {"enemies":[...], "lane":"MIDDLE"}; lane defaults to TOP, "" simulates blind pick (no assigned lane).
app.post('/api/dev/simulate-draft', (req, res) => {
  const enemyIds: number[] = (req.body?.enemies ?? []).map(Number).filter((id: number) => data.champions.has(id));
  const lane = req.body?.lane === undefined ? 'TOP' : toLane(String(req.body.lane)) ?? String(req.body.lane).toUpperCase();
  // Ranked-style session: Irelia locked in your assigned lane, enemy roles hidden. Parsed like a real one.
  simulatedDraft = enemyIds.length ? parseChampSelect({
    localPlayerCellId: 0,
    myTeam: [{ cellId: 0, championId: config.championId, championPickIntent: 0, assignedPosition: lane }],
    theirTeam: enemyIds.map((championId, i) => ({ cellId: 5 + i, championId, championPickIntent: 0, assignedPosition: '' })),
    actions: [enemyIds.map((_, i) => ({ actorCellId: 5 + i, championId: enemyIds[i], completed: true, type: 'pick' }))],
  }, config.championId, guessOpponents) : null;
  const state = effectiveLcu();
  broadcast({ type: 'lcu', state });
  refreshMatchup();
  res.json(state);
});

// Live game simulator: drives the live tracker, the dashboard's live panel and the overlay without League.
// {"startAt":420, "lane":"MIDDLE", "positions":false} (lane defaults to TOP; no positions as in customs); {"stop":true} ends it.
app.post('/api/dev/simulate-live', (req, res) => {
  stopSimulation?.();
  stopSimulation = null;
  if (req.body?.stop) { onLiveEnded(); res.json({ running: false }); return; }
  const lane = req.body?.lane === undefined ? 'TOP' : toLane(String(req.body.lane));
  if (!lane) { res.status(400).json({ error: `Unknown lane "${String(req.body.lane)}": use TOP or MIDDLE` }); return; }
  resetLive();
  const tracker = new PerformanceTracker();
  stopSimulation = simulateLive((raw) => onLiveData(raw, tracker.update(raw, (id) => data.items.get(id)?.goldTotal ?? 0, liveHint(raw).opponentName), true),
    { startAt: Number(req.body?.startAt) || 0, lane, positions: req.body?.positions !== false });
  res.json({ running: true });
});

wss.on('connection', (socket) => {
  socket.send(JSON.stringify({ type: 'lcu', state: effectiveLcu() } satisfies ServerEvent));
  if (scraper.status) socket.send(JSON.stringify({ type: 'scraper', status: scraper.status } satisfies ServerEvent));
  if (scraper.rate) socket.send(JSON.stringify({ type: 'ratelimit', snapshot: scraper.rate } satisfies ServerEvent));
  if (liveState) socket.send(JSON.stringify({ type: 'live', state: liveState } satisfies ServerEvent));
  socket.send(JSON.stringify({ type: 'matchup', matchup } satisfies ServerEvent));
  if (conversion.status) socket.send(JSON.stringify({ type: 'conversion', status: conversion.status } satisfies ServerEvent));
  if (slim) socket.send(JSON.stringify({ type: 'slim', status: slim.status } satisfies ServerEvent));
  if (build) socket.send(JSON.stringify({ type: 'build', build } satisfies ServerEvent));
  socket.send(JSON.stringify({ type: 'model', status: models.status } satisfies ServerEvent));
  socket.send(JSON.stringify({ type: 'jobs', status: jobsStatus() } satisfies ServerEvent));
  socket.send(JSON.stringify({ type: 'backup', status: backup.status() } satisfies ServerEvent));
});

// Serve the built dashboard if present (npm run build); in dev, Vite serves it on :5173.
const dist = path.resolve('dist');
if (fs.existsSync(dist)) {
  app.use(express.static(dist));
  app.get(/^(?!\/api).*/, (_req, res) => res.sendFile(path.join(dist, 'index.html')));
}

// The desktop shell asks for a preferred port, accepts any free one if that is taken, and waits for the 'ready' message
// with the real port (`desktop`: its parentPort, above).
// The WebSocket server passes the HTTP server's errors on: handled there (an unhandled one ended the process).
wss.on('error', () => {});
server.on('error', (err: NodeJS.ErrnoException) => {
  if (err.code === 'EADDRINUSE' && desktop) { server.listen(0, '127.0.0.1'); return; }
  throw err;
});
server.on('listening', () => {
  const { port } = server.address() as { port: number };
  console.log(`[api] http://127.0.0.1:${port}${desktop ? '' : '  (dashboard: http://localhost:5173 in dev)'}`);
  if (startBeat) clearInterval(startBeat);
  desktop?.postMessage({ type: 'ready', port });
});
server.listen(config.port, '127.0.0.1');

// The desktop app gone without ending this process (0.14.2: its process ended by hand or crashed): this one ends too,
// by its own PID, so it doesn't keep the port and the database from the next start. A Slim stored data run cut short
// is rolled back at that start.
const parentPid = Number(process.env.IRELIA_PARENT_PID);
if (desktop && parentPid > 0) {
  setInterval(() => {
    try { process.kill(parentPid, 0); } catch (err) {
      if ((err as NodeJS.ErrnoException).code !== 'ESRCH') return;
      console.warn('[desktop] the app is gone: the middleware ends');
      process.kill(process.pid, 'SIGKILL');
    }
  }, 5000).unref();
}
