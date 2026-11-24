// Types shared between the Node middleware and the React dashboard.

export type Skill = 'Q' | 'W' | 'E' | 'R';

/** Lanes Irelia data is collected for (helpers in shared/lanes.ts). */
export type Lane = 'TOP' | 'MIDDLE';

export interface Option<T> {
  value: T;
  games: number;      // weighted sample size
  rawGames: number;   // unweighted count
  winRate: number;    // 0..1, weighted
  score: number;      // Wilson lower bound used for ranking
}

export interface RunePage {
  primaryStyle: number;
  subStyle: number;
  perks: number[];      // 4 primary + 2 secondary
  statPerks: number[];  // offense, flex, defense
}

export interface SkillPlan {
  priority: Skill[];  // e.g. ['Q','E','W']
  path: Skill[];      // levels 1..18
}

export interface SampleInfo {
  matchupGames: number;
  totalGames: number;
  patches: string[];
  usedFallback: boolean;  // matchup sample too small -> blended with general Irelia data
  laneGames: number;          // valid games in the requested lane (every game when no lane was asked for)
  otherLaneGames: number;     // other-lane games blended in as a prior (0 unless usedLaneFallback)
  usedLaneFallback: boolean;  // lane sample too small -> blended with the other lane at reduced weight
}

export interface ValidationReport {
  currentPatch: string;
  freshGames: number;
  previousPatchGames: number;
  staleDiscarded: number;
  invalidDiscarded: number;
  invalidReasons: Record<string, number>;
  conflictsRejected: string[];  // human readable "Sunfire Aegis + Hollow Radiance (Immolate)"
}

export interface BuildRecommendation {
  patch: string;
  ddragonVersion: string;
  enemyChampionId: number | null;
  lane: Lane | null;      // null: every position mixed (no lane requested)
  sample: SampleInfo;
  winRate: number | null;
  runes: Option<RunePage> | null;
  summoners: Option<number[]> | null;
  startingItems: Option<number[]>[];
  coreBuild: Option<number[]>[];
  boots: Option<number>[];
  /**
   * How many completed items the games usually finished before their tier-2 boots (weighted median, 0-3): the boots'
   * place in the build order. Missing or null: after the core.
   */
  bootsAfter?: number | null;
  situational: { slot4: Option<number>[]; slot5: Option<number>[]; slot6: Option<number>[] };
  skills: Option<SkillPlan> | null;
  validation: ValidationReport;
}

// ---- Draft-aware builds (server/model/draft.ts, docs/BUILD-SOURCING.md §9.3) -----------------------

/** Evidence behind one pick of the draft-adjusted build. The lift is an estimate. */
export interface PickStats {
  /** Win-probability-added: how much more often than the game state predicted games with this pick were won. */
  lift: number;
  low: number;            // 95 % interval
  high: number;
  nEff: number;           // effective games after context weighting
  games: number;          // games in the lane with this pick
  pilots: number;
  winRate: number;        // context-weighted
  matchupGames: number;
  /** W̄: mean win chance just before the purchase over the same weighted games; null when unknown. */
  startWin?: number | null;
}

export interface BuildPick {
  key: string;            // keystone, row1, row2, row3, secondaryTree, secondary, shard1-3, summoners, start, core1-3, boots, item4-6, maxFirst, early
  label: string;          // "Keystone", "First item"
  value: string;          // the option: "8010", "3153", "4,12", "QEW"
  stats: PickStats | null;
  /** Why the draft moved it away from the lane-level pick ("enemy magic damage 62%"); null when it didn't. */
  reason: string | null;
  /** Too few similar games: the lane baseline's pick is kept. */
  fallback: boolean;
  /** Differs from the lane baseline's pick. */
  changed: boolean;
  /** The lane baseline's pick for this decision and its lift in this draft (null: no estimate for it here). */
  baseline: { value: string; lift: number | null } | null;
  alternatives: { value: string; stats: PickStats }[];
}

/** One dimension of the draft, with how unusual it is (standard deviations from the stored games' mean). */
export interface ContextChip { key: string; label: string; value: string; z: number }

export type BuildMode = 'baseline' | 'draft';

export interface DraftModelInfo {
  /** Off until npm run eval:builds shows it beats the lane baseline. */
  gate: 'passed' | 'failed' | 'not evaluated';
  evaluatedAt: number | null;
  bandwidth: number;
  k: number;
  stateModel: { games: number; rows: number } | null;
  dataVersion: string;
  games: number;          // games the model learns from (all lanes, all regions)
  /**
   * Patch memory: whether games of patches before the previous one count (`active`), what decides it (`choice`: the
   * last evaluation, or the Data tab's On / Off), the evaluation's verdict and fade per patch, and how many of the
   * model's games are from those older patches. `inModel`: whether the model in use was built with it (0.9.2: it
   * differs from `active` until the rebuild a switch asked for is done).
   */
  memory: {
    active: boolean; choice: 'auto' | 'on' | 'off'; gate: 'passed' | 'failed' | 'not evaluated';
    decay: number | null; patches: number; olderGames: number; inModel: boolean;
  };
  /**
   * Runes and summoners scored against each game's expected win rate before it started (0.9.0): whether it is on,
   * whose choice that is ('auto': Follow the evaluation), the last evaluation's verdict, and whether the model in use
   * was built with it.
   */
  scoring: { active: boolean; choice: 'auto' | 'on' | 'off'; gate: 'passed' | 'failed' | 'not evaluated'; inModel: boolean };
  /**
   * Fine-tuning (0.15.0): the last evaluation's verdict and, when it passed (only then is it used), pilot standing's
   * strength, the item decisions' prior and the fade within a patch (days to half).
   */
  tuning?: { gate: 'passed' | 'failed' | 'not evaluated'; pilotStrength: number; itemK: number | null; recencyDays: number | null };
  /**
   * Late-game swaps (0.17.0, docs/SWAPS.md): whether the advice is shown (`active`), whose choice that is ('auto':
   * Follow the evaluation, on once an evaluation found following it better than keeping), and the last verdict.
   */
  swaps?: { active: boolean; choice: 'auto' | 'on' | 'off'; gate: 'passed' | 'failed' | 'not evaluated' };
}

/**
 * Late-game swap advice for one item of the build (0.17.0, docs/SWAPS.md): at shop visits from 20:00 with every slot
 * full, what players in drafts like this one did with it, kept or sold for another, scored by the result against the
 * game's win chance just before the visit (state-adjusted), compared with keeping it.
 */
export interface SwapAdvice {
  /** The item of the build it is about. */
  held: number;
  /** What to do with it: keep it, sell it for `item` (the next item finished after selling), or sell it for nothing. */
  pick: { kind: 'keep' } | { kind: 'swap'; item: number } | { kind: 'sell' };
  /** The pick's lift over keeping (state-adjusted, as a share: 0.031 = 3.1 points), its 95 % range; null: keep. */
  lift: number | null;
  low: number | null;
  high: number | null;
  /** Effective similar situations behind the pick, and raw ones (shop visits, each game's together counting once). */
  nEff: number;
  games: number;
  /** Too few similar situations to tell: it keeps the item, and says so. */
  thin: boolean;
  /** Other ways players went with it, best first (at most 3): null item = sold for nothing. */
  alternatives: { item: number | null; lift: number; games: number }[];
  /** The typical minute of the swaps behind the pick (median), null for keep. */
  minute: number | null;
}

export interface DraftRecommendation {
  /** The draft-adjusted build in the lane baseline's shape. */
  build: BuildRecommendation;
  picks: BuildPick[];
  context: ContextChip[];
  sample: { games: number; nEff: number; pilots: number; regions: Record<string, number> };
  /** Late-game swaps for the build's items (0.17.0): computed always, shown when DraftModelInfo.swaps is active. */
  swaps?: SwapAdvice[];
}

export interface DraftResponse {
  baseline: BuildRecommendation;
  draft: DraftRecommendation | null;   // null until enough games are stored
  /** What the dashboard shows first: the user's choice, else the draft model once it passed the evaluation. */
  mode: BuildMode;
  model: DraftModelInfo | null;
  /** Stage C2: how this matchup tends to go, from stored high-elo games. Shown before the game only. */
  facts?: MatchupFacts | null;
}

/** A gold difference at one minute across games: the median, the share of games Irelia was ahead, and how many. */
export interface GoldAt { median: number; ahead: number; games: number }

/** Stage C2 (server/model/facts.ts): this lane against this opponent in the stored high-elo games. */
export interface MatchupFacts {
  lane: Lane;
  opponentId: number;
  games: number;
  pilots: number;
  winRate: number;
  /** Irelia's total gold minus her lane opponent's (timeline), at 10:00 and 15:00. */
  gold10: GoldAt | null;
  gold15: GoldAt | null;
  /** Win rate by game length, in minutes ([from, to), to null = and longer). */
  byLength: { from: number; to: number | null; games: number; winRate: number }[];
  /**
   * The start and the first items that did best in this matchup (0.9.0, the imported item set's "vs" blocks): their
   * win-probability-added in these games, shrunk towards 0 (estimates). Null with too few games.
   */
  best?: MatchupBest | null;
}

/** A start or a first item in one matchup: its games there and its win-probability-added, shrunk towards 0. */
export interface MatchupOption<T> { value: T; games: number; lift: number }
export interface MatchupBest { start: MatchupOption<number[]> | null; first: MatchupOption<number>[] }

// ---- Personal layer (server/model/personal.ts) -----------------------------------------------

/** When you finished an item (median minute) against the high-elo median for the same item and lane. */
export interface PersonalTiming { itemId: number; you: number; games: number; highElo: number | null; highEloGames: number }

/** An option you used: your lift, the high-elo lift, and the two blended (your games weigh in gradually). Estimates. */
export interface PersonalOption {
  key: string; label: string; value: string; lane: Lane;
  games: number; yourLift: number; highEloLift: number; blended: number;
}

export interface PersonalReport { games: number; wins: number; timings: PersonalTiming[]; options: PersonalOption[] }

/** A build the app showed in one phase of a game (rec_log). */
export interface ShownBuild { keystone: number | null; core: number[]; boots: number | null; items: number[]; next?: number[] }

export interface GameReview {
  matchId: string;
  created: number;
  lane: Lane | null;
  opponentId: number | null;
  win: boolean;
  kda: [number, number, number];
  minutes: number;
  keystone: number;
  items: { itemId: number; minute: number; highEloMinute: number | null }[];
  boots: number | null;
  shown: { phase: 'draft' | 'live'; mode: BuildMode; build: ShownBuild }[];
  /** How much of the draft build you followed: same keystone, how many of the first three items in place. */
  followed: { keystone: boolean; core: number } | null;
  /** Gold and CS against your lane opponent (post-game, from the match timeline). */
  lane10: { gold: number; cs: number } | null;
  lane15: { gold: number; cs: number } | null;
  itemName: Record<number, string>;
}

export interface DraftPick {
  cellId: number;
  championId: number;
  position: string;
  locked: boolean;
}

export interface LcuState {
  status: 'searching' | 'connecting' | 'connected' | 'disconnected';
  detail?: string;
  summonerName?: string;
  phase: string;  // gameflow phase: None, Lobby, ChampSelect, InProgress, ...
  champSelect: {
    myChampionId: number;
    myPosition: string;                 // assigned position, e.g. "MIDDLE"; "" in blind pick, ARAM and customs
    isIrelia: boolean;
    allies: DraftPick[];
    enemies: DraftPick[];
    suggestedOpponentId: number | null;               // for the assigned lane (top when there is none)
    suggestedOpponents: Record<Lane, number | null>;  // likely lane opponent if you play that lane
  } | null;
}

export interface LivePlayer {
  championName: string;
  championId: number | null;
  team: 'ORDER' | 'CHAOS';
  position: string;
  level: number;
  items: number[];
  kills: number;
  deaths: number;
  assists: number;
  creepScore: number;
  isMe: boolean;
}

export interface AdaptiveSuggestion {
  itemId: number;
  reason: string;
  blockedBy?: string;   // exclusivity group that blocks the purchase given current inventory
  replacement?: number;
}

export interface LiveGameState {
  active: boolean;
  gameTime: number;
  myGold: number;
  players: LivePlayer[];
  laneOpponent: LivePlayer | null;
  /** position: same position on the enemy team; draft: champ select's pick; manual: yours; guess: from the enemies. */
  laneOpponentFrom?: 'position' | 'draft' | 'manual' | 'guess' | null;
  threat: { apShare: number; tanks: number; healers: number };
  suggestions: AdaptiveSuggestion[];
  performance: LivePerformance | null;
  /** Draft-adjusted mode only: the next completed item, re-ranked from the live game (server/model/live.ts). */
  next?: LiveNext | null;
  /** Stage C3: the best purchase for your current gold toward the next item. */
  buy?: BestBuy | null;
  /** Who is ahead, from the scoreboard (engine/adaptive.ts). */
  summary?: LiveSummary | null;
}

/**
 * Stage C3 (server/model/buy.ts): what to buy with the gold you have, toward the next completed item. Shop advice only.
 * `share` and `lift` describe the first item: how often high-elo Irelias buy it first here, and how those games went
 * against the game state (an estimate); null when the item itself is affordable or no stored game shows an order.
 */
export interface BestBuy {
  target: number;
  items: number[];
  cost: number;
  gold: number;
  finishes: boolean;
  games: number;
  share: number | null;
  lift: number | null;
  reason: string;
}

/** Who is ahead, from the scoreboard only (items everyone can see on Tab, kills): estimates, never "do X" calls. */
export interface LiveSummary {
  /** Your team's item value minus the enemy team's, and the kill difference. */
  teamItems: number;
  kills: number;
  /** The enemy and the ally (you included) with the most item value, when one is a quarter above their team's average. */
  fedEnemy: { championId: number; items: number; ap: boolean } | null;
  fedAlly: { championId: number; items: number; isMe: boolean } | null;
}

/** The next item to finish, re-scored in game from the Live Client data. Its lift is an estimate. */
export interface LiveNext {
  itemId: number;
  slot: number;               // 1-6: which completed item this is
  /** What moved it, e.g. "fed Syndra (AP)", "enemy armour from items 85"; null when it is simply the best. */
  reason: string | null;
  stats: PickStats;
  /** You already own components of it. */
  inProgress: boolean;
  alternatives: { itemId: number; stats: PickStats }[];
}

// ---- Live performance tracker (server/lcu/liveClient.ts) ---------------------------------

export interface PlayerPerformance {
  championName: string;
  cs: number;               // minions + monsters (the API reports them combined)
  csPerMin: number;
  visionScore: number;
  visionPerMin: number;
  controlWards: number;     // Control Wards in inventory
  trinket: number | null;   // trinket item id (Stealth Ward / Oracle Lens / Farsight)
  itemGold: number;         // value of the visible inventory (Data Dragon total cost)
  modelGold: number;        // income model: start + passive + CS + kills/assists + objectives
  unspentGold: number;      // you: exact. Opponent: estimated (fog hides it)
  netWorth: number;         // itemGold + unspentGold
}

export interface PerformanceSample { t: number; gold: number; cs: number; vision: number }  // diffs vs lane opponent

export interface LivePerformance {
  gameTime: number;
  me: PlayerPerformance;
  opponent: PlayerPerformance | null;
  diff: { cs: number; vision: number; gold: number; itemGold: number } | null;
  gold: {
    confidence: 'high' | 'medium' | 'low';
    uncertainty: number;    // ± gold on the opponent's net worth
    calibration: number;    // your actual net worth / your modelled income (applied to the opponent)
  };
  csGranularity: 1 | 10;    // some client builds report CS in steps of 10
  history: PerformanceSample[];
}

// ---- Storage manager (/api/storage) ------------------------------------------------------

export interface StorageCategory {
  key: 'database' | 'ddragon' | 'riotCache' | 'other' | 'backup';
  label: string;
  bytes: number;
  files: number;
  parts: { name: string; bytes: number; files: number }[];
}

export interface StorageStats {
  dataDir: string;
  defaultDataDir: string;
  configFile: string;
  lockedBy: string | null;   // set when IRELIA_DATA_DIR pins the folder (moving is disabled)
  totalBytes: number;
  categories: StorageCategory[];
  disk: { freeBytes: number; totalBytes: number } | null;
  ddragonVersion: string;
  busy: boolean;
  /** Rows (and bytes, when SQLite's dbstat is available) per database table. */
  tables?: { name: string; rows: number; bytes: number | null }[];
  /** How long each Storage action would take now (estimates; see ActionEstimates). */
  estimates?: ActionEstimates;
  /** The background conversion (0.14.0), while it has work or just finished. */
  conversion?: ConversionStatus | null;
  /**
   * Compact database is worth running once (0.14.0): the database doesn't give space back by itself yet, and this much
   * is free inside it.
   */
  suggestCompact?: { freeBytes: number } | null;
}

/** What Slim stored data (0.14.0, server/slim.ts) would touch, and the space its backup needs. */
export interface SlimPlan {
  /** (a) cached raw timelines older than the newest two patches, their games captured. */
  timelines: number; timelineBytes: number;
  /** (b) cached matches and timelines to slim. */
  entries: number; entryBytes: number;
  /** (c) archived matches of patches older than patch memory's window. */
  archivedMatches: number; archiveBytes: number;
  dbCopyBytes: number;
  /** The backup and the usual reserve. */
  neededBytes: number;
}

export type SlimStep = 'backup' | 'baseline' | 'apply' | 'verify' | 'verified' | 'rolled-back' | 'kept' | 'cancelled';

/** A Slim stored data run, as recorded step by step (backup-0.14.0/manifest.json and meta slim_manifest). */
export interface SlimManifest {
  version: 1;
  startedAt: number;
  updatedAt: number;
  step: SlimStep;
  plan: SlimPlan | null;
  /** The database's copy (VACUUM INTO) and its SHA-256. */
  db: { file: string; bytes: number; sha256: string } | null;
  /** The cache entries moved into the backup. */
  cache: { file: string; entries: number; bytes: number } | null;
  archived?: number;
  /** SHA-256 of the baseline results. */
  baseline?: string;
  applied?: { slimmed: number; kept: number; archived: number; savedBytes: number };
  /** What differed when the check failed (then it rolled back). */
  differences?: string[];
  note?: string;
  /** Space it gave back, and what the backup holds. */
  freedBytes?: number;
  backupBytes?: number;
}

/** Slim stored data's status for the Storage card. */
export interface SlimStatus {
  running: boolean;
  /** The step running, and how far into it (0-1). */
  step: SlimStep | 'rollback' | null;
  part: number | null;
  label: string | null;
  manifest: SlimManifest | null;
  error: string | null;
  /**
   * 0.14.2: the running phase of its step (copying the database, the quick evaluation, the cached games…): how far it
   * is, with its time left (an estimate, from its own pace; the check's, from the same phase of the baseline), and
   * what it has counted so far.
   */
  progress?: ProgressInfo | null;
  count?: SlimCount | null;
  /** 0.15.0: Stop was pressed; the run is ending and being undone. */
  stopping?: boolean;
}

/** What a phase of Slim stored data has counted: `done` of `total` `unit` ("games", "MB"). */
export interface SlimCount { done: number; total: number; unit: string }

/** The background conversion of data stored by earlier versions (0.14.0, server/convert.ts). */
export interface ConversionStatus {
  running: boolean;
  /** API cache files moved into its database: done of total, and files it couldn't read (left as they are). */
  cache: { done: number; total: number; skipped: number } | null;
  /** Timelines compressed in the database: how far through them (0-1) and how many so far. */
  timelines: { fraction: number; converted: number } | null;
  error: string | null;
  /** Everything converted. */
  done: boolean;
  /** Paused: champ select or a game (it resumes after). */
  paused?: boolean;
}

export interface StorageInspection {
  path: string;
  exists: boolean;
  writable: boolean;
  hasData: boolean;          // already contains irelia.db / cache / ddragon
  freeBytes: number | null;
  requiredBytes: number;
  problems: string[];        // blocking: the move cannot start
}

export type MigrationMode = 'move' | 'copy' | 'adopt';

export interface StorageProgress {
  phase: 'preparing' | 'database' | 'copying' | 'verifying' | 'switching' | 'cleaning' | 'done' | 'error';
  doneBytes: number;
  totalBytes: number;
  text: string;
  progress?: ProgressInfo;
}

/** A Storage action running (Settings → Storage: compact, clear the cache, delete old patches' games). */
export interface StorageAction { action: 'compact' | 'clear-cache' | 'clear-expired' | 'ddragon-old' | 'purge'; text: string; progress: ProgressInfo }

/** Settings → GPU (0.13.0, /api/gpu): Use GPU for Evaluate builds, and its log (server/gpu/log.ts). */
export interface GpuState {
  useGpu: boolean;
  /** Test GPU is running. */
  testing: boolean;
  /** The log's last lines, oldest first ('' when empty). */
  log: string;
  logFile: string;
  /** After Test GPU: whether the GPU passed. */
  ok?: boolean;
}

export interface SettingsState {
  riotApiKey: string | null;  // masked
  keySource: 'settings' | 'env' | 'none';
  keySavedAt: number | null;  // when the key was saved in Settings (not known for a .env key)
  /** Your server chosen by hand (null: detected), and the servers to choose from. */
  myPlatform: string | null;
  platforms: { id: string; label: string }[];
  /** Pause fetching during champ select and games (0.14.0; on unless switched off). */
  pauseFetchingInGame: boolean;
}

export interface ScraperStatus {
  running: boolean;
  /** 'game': fetching is on but paused during champ select and the game (0.14.0, Settings → Pause fetching during games). */
  paused?: 'game';
  phase: string;
  requests: number;
  rateWindow: RateWindowUsage[];   // busiest host
  rate: RateSnapshot;
  playersSeen: number;
  otpPlayers: number;
  matchesScanned: number;
  ireliaGames: number;
  currentPatchGames: number;
  /** The patch currentPatchGames counts: the first fetched platform's own (Data Dragon can be a patch ahead). */
  currentPatch?: string;
  lastError: string | null;
  lastEvent: string | null;
  // v3 (build sourcing v2)
  mainPlayers?: number;
  /** Stored Irelia games per source: otp, main, harvested, me. */
  sources?: Partial<Record<'otp' | 'main' | 'harvested' | 'me', number>>;
  /** One lane per Riot host, each working its own queue. */
  hosts?: HostWork[];
  backfill?: BackfillStatus | null;
  /** The signed-in account whose games are fetched as source "me", once the League client has told us. */
  me?: { platform: string; games: number; riotId?: string } | null;
}

/** What one Riot host's lane is doing (server/scraper/sourcing.ts). */
export interface HostWork {
  host: string;                    // kr.api.riotgames.com, asia.api.riotgames.com, …
  kind: 'platform' | 'regional';
  platforms: string[];
  task: string;                    // "Fetching KR matches", "Up to date"
  busy: boolean;
  queued: number;                  // regional: matches and harvested timelines waiting
  idleUntil: number | null;
  lastError: string | null;
}

/** The capture backfill (server/scraper/backfill.ts): rebuilt from the disk cache, no API calls. */
export interface BackfillStatus {
  phase: string;
  done: number;
  total: number;
  games: number;
  missing: number;
  harvested: number;
  startedAt: number;
  finishedAt: number | null;
  /** While it runs: its time left (worked out on the main thread). */
  progress?: ProgressInfo;
}

// ---- Riot API pacing (server/riot/rateLimiter.ts) -----------------------------------------

/** X-Rate-Limit-Type of a 429: our app budget, one endpoint's method budget, or Riot's backend being busy. */
export type RateLimitScope = 'application' | 'method' | 'service';

export interface RateWindowUsage { used: number; limit: number; windowSec: number }

/** One Riot host (kr.api.riotgames.com, asia.api.riotgames.com, …). Times are epoch ms on the worker's clock. */
export interface RateHostState {
  host: string;
  windows: RateWindowUsage[];      // app limit windows
  paceMs: number;                  // spacing between request starts right now
  nextAt: number | null;           // when the next queued request may start; null = nothing queued or waiting on a response
  queued: number;
  inFlight: number;
  retryAfter: { until: number; scope: RateLimitScope } | null;   // Riot answered 429 and asked us to wait
  lastStartAt: number;
}

export interface RateSnapshot {
  at: number;                      // when it was taken; clients count down from here
  hosts: RateHostState[];          // every host used since the worker started
  busiest: string | null;          // most requests in its windows (the bars show this one)
  active: string | null;           // has requests queued or in flight, or started one in the last 3 s
  defaultPaceMs: number;           // pace of a host that hasn't been used yet
  idleUntil: number | null;        // background loop is up to date and checks again then
}

// ---- The matchup every window shows (server/matchup.ts) ------------------------------------

/** Where the app is: nothing going on, champ select (kept through the loading screen), or a live game. */
export type MatchupPhase = 'idle' | 'draft' | 'game';

/**
 * The lane, opponent and draft the build is for, resolved on the server from the League client, the live game and
 * your own picks, so the dashboard and the overlay always show the same matchup.
 */
export interface Matchup {
  phase: MatchupPhase;
  lane: Lane;
  /** game: your in-game position; draft: your champ-select lane; manual: you switched; saved: the lane you picked last. */
  laneFrom: 'game' | 'draft' | 'manual' | 'saved';
  /** The lane the game or the draft assigns, when there is one (also while you override it). */
  detectedLane: Lane | null;
  opponentId: number | null;
  /**
   * game: same position on the enemy team; draft: champ select's likely opponent; guess: the enemy Irelia faced most
   * in this lane (no positions known); manual: you picked it. null: no opponent (all matchups).
   */
  opponentFrom: 'game' | 'draft' | 'guess' | 'manual' | null;
  enemies: number[];
  allies: number[];
  /** Regions feeding the draft-adjusted build; null: all of them. */
  regions: string[] | null;
  /** Identifies the inputs of a build; a pushed build carries the key it was made for. */
  key: string;
}

/** The build for the current matchup, pushed over /ws whenever the matchup, the mode or the data changes. */
export interface BuildState {
  key: string;
  response: DraftResponse;
  at: number;
}

/** The draft model's state (server/model/worker.ts). */
export interface ModelStatus {
  state: 'loading' | 'building' | 'ready' | 'failed';
  info: DraftModelInfo | null;
  mode: BuildMode;
  error: string | null;
  /** While building: how long it has left (from the last build's time, scaled to the games). */
  progress?: ProgressInfo;
}

/**
 * How far a long action is and how long it has left (server/progress.ts, 0.10.0). `fraction` (0-1) when its work can
 * be counted, else null. `etaMs` is always an estimate: from this computer's last run of the action until its own pace
 * says more, null with nothing to go on yet. `at` is when they were worked out: the dashboard counts down from it.
 */
export interface ProgressInfo { fraction: number | null; etaMs: number | null; startedAt: number; at: number }

/**
 * How long each action would take if started now, on this computer (GET /api/estimates; ms, null: never timed here
 * and nothing to go on). Estimates, from the last run scaled to the size it has now.
 */
export type ActionEstimates = Partial<Record<ActionKind, number | null>>;
/** The app's long actions (server/progress.ts). 'evaluate-update': Evaluate builds' quick update (0.16.0), timed apart. */
export type ActionKind = 'evaluate' | 'evaluate-update' | 'audit' | 'backfill' | 'move' | 'compact' | 'clear-cache' | 'clear-expired'
  | 'ddragon-old' | 'purge' | 'archive' | 'restore' | 'trim' | 'model';

/**
 * This computer, for Evaluate builds (0.11.0, GET /api/system): its processor, how many threads it runs at once, its
 * memory, and the threads an evaluation started now would use. Read here, never sent anywhere.
 */
export interface SystemInfo {
  /** The processor's name as the system reports it (null: not reported). */
  processor: string | null;
  /** Logical cores: the threads the processor runs at once. */
  cores: number;
  /** Bytes: installed, available now (without swapping), and kept free whatever an evaluation does. */
  memory: { total: number; available: number; reserve: number };
  evaluation: {
    /** Threads an evaluation started now would use: every core the available memory holds. */
    threads: number;
    /** Memory each thread beyond the first would take (bytes; an estimate) and whether it was measured here. */
    perThread: number;
    measured: boolean;
    /** What holds the threads back: the cores, the free memory, or the heap the app's threads share (0.12.1). */
    limit: 'cores' | 'memory' | 'heap';
  };
}

/** The one-shot jobs in their own thread (server/jobs.ts): the build evaluation and the data audit. */
export type JobName = 'evaluate' | 'audit';

/**
 * What Evaluate builds would run now (0.16.0, server/model/evalBase.ts planEvaluation, docs/EVAL-UPDATE.md): a quick
 * update while the last full evaluation still describes the stored games, else a full evaluation; nothing with no new
 * games since the last one (a full one can still be asked for).
 */
export interface EvaluationPlan {
  path: 'full' | 'update' | 'none';
  /** Why: "new patch 25.21", "1,240 new games since the last evaluation (full evaluation of 21 Oct)", "up to date: …". */
  reason: string;
  /** Stored games since the last evaluation (full or quick), and since the last full one. */
  newGames: number;
  sinceFull: number;
  /** The last full evaluation (null: none). */
  fullAt: number | null;
}

/** What runs now, how far it is, and how each job's last run ended (kept until the app closes). */
export interface JobsStatus {
  running: {
    job: JobName; startedAt: number; step: number | null; steps: number | null; label: string | null;
    /** An evaluation the app started by itself (server/autoEval.ts), and why: "312 new games", "new patch 16.20". */
    auto?: string;
    /**
     * The evaluation: a quick update or a full evaluation, as planned when it started (0.16.0). A quick update that
     * would change a verdict goes on to a full evaluation: its result says so.
     */
    path?: 'full' | 'update';
    /** How far it is, weighed by how long each step took last time, and its time left. */
    progress?: ProgressInfo;
    /** The evaluation's threads (0.11.0): as many as the cores and the free memory allow; 1: the job thread alone. */
    threads?: number;
  }[];
  last: Partial<Record<JobName, {
    startedAt: number; finishedAt: number; ok: boolean; cancelled?: boolean; error?: string;
    /** The report, where it was saved, and (the evaluation) its gate result. */
    text?: string; file?: string; gate?: 'passed' | 'failed';
    /** The evaluation's threads (0.11.0). */
    threads?: number;
    /** What the evaluation ran, a quick update or a full evaluation, and why (0.16.0). */
    path?: 'full' | 'update';
    reason?: string;
    /** Started by the app itself, and why. */
    auto?: string;
    /** An automatic evaluation stopped because champ select or a game began: it runs again later. */
    paused?: boolean;
  }>>;
  /** Evaluate builds running by itself (server/autoEval.ts). */
  auto?: AutoEvalStatus;
  /**
   * What Evaluate builds would run now (0.16.0), Follow the evaluation on or off: worked out every minute and after
   * each evaluation (server/autoEval.ts), not while one runs.
   */
  plan?: EvaluationPlan;
}

/**
 * Evaluate builds running by itself (server/autoEval.ts): with Follow the evaluation on, a quick update after enough
 * new games, at most every hour, and a full evaluation when the plan says one is needed (a new patch, say), at most
 * every few hours; each once no champ select or game has been on for a while.
 */
export interface AutoEvalStatus {
  /** Follow the evaluation is on. */
  on: boolean;
  /** What the next one would run, and why (the plan, EvaluationPlan; 0.16.0). Null: off, or not worked out yet. */
  path: EvaluationPlan['path'] | null;
  reason: string | null;
  /** Games stored since the last evaluation (full or quick), and since the last full one (0.16.0). */
  newGames: number;
  sinceFull: number;
  /**
   * How many start the next one: for a quick update, new games since the last evaluation; for a full one, games since
   * the last full one (or 3 % of the games it saw, when that is more).
   */
  needed: number;
  /** New games since the last evaluation that start a quick update (0.16.0). */
  updateGames: number;
  /** A patch newer than the last evaluation saw. */
  newPatch: string | null;
  /**
   * Hours between two full evaluations at least, between a quick update and the last evaluation (0.16.0), and minutes
   * without champ select or a game before one starts.
   */
  spacingHours: number;
  updateSpacingHours: number;
  quietMinutes: number;
  /** Due, but waiting: until `notBefore` (spacing), for a quiet spell without champ select or a game, or for the data folder. */
  waiting: 'spacing' | 'quiet' | 'busy' | null;
  notBefore: number | null;
}

/**
 * Old patches (server/oldPatches.ts): the Data tab's archive switch and Storage's cache switch, what runs now, the
 * archive per patch, and how the last operation went.
 */
export interface OldPatchesStatus {
  archive: boolean;
  trimCache: boolean;
  working: 'archiving' | 'restoring' | 'trimming' | null;
  /** While working: how far it is and its time left. */
  progress?: ProgressInfo;
  archived: { patch: string; games: number; bytes: number }[];
  last: string | null;
  error: string | null;
}

export type ServerEvent =
  | { type: 'lcu'; state: LcuState }
  | { type: 'live'; state: LiveGameState }
  | { type: 'scraper'; status: ScraperStatus }
  | { type: 'ratelimit'; snapshot: RateSnapshot }   // lightweight, up to 5×/s while requests run, only when something changed
  | { type: 'patch'; patch: string; version: string }
  | { type: 'storage'; progress: StorageProgress }
  | { type: 'storage-action'; action: StorageAction | null }
  | { type: 'old-patches'; status: OldPatchesStatus }
  | { type: 'matchup'; matchup: Matchup }
  | { type: 'conversion'; status: ConversionStatus }
  | { type: 'slim'; status: SlimStatus }
  | { type: 'build'; build: BuildState }
  | { type: 'model'; status: ModelStatus }
  | { type: 'jobs'; status: JobsStatus }
  | { type: 'backup'; status: BackupStatus };

// ---- Evaluation history (server/model/evalHistory.ts, 0.9.0) ----------------------------------------

/**
 * One comparison of one evaluation: the difference in lift (points of win-probability-added, an estimate), its 95 %
 * interval, and the verdict: better (the gate passed), worse (the whole interval below zero), else unclear.
 */
export interface EvalHistoryPoint { delta: number; low: number; high: number; verdict: 'better' | 'unclear' | 'worse'; reason: string }

/** A stored evaluation, oldest first: draft-adjusted against the lane baseline, and the options the evaluation decides. */
export interface EvalHistoryEntry {
  at: number;
  auto: boolean;
  /**
   * A quick update (0.16.0): the last full evaluation's measurements with the new games' own added, every verdict
   * decided again on all of them. `fullAt`: that full evaluation; `newGames`: the games it added.
   */
  update: boolean;
  fullAt?: number;
  newGames?: number;
  file: string;
  games: number;
  draft: EvalHistoryPoint;
  /** Patch memory against two patches, all games (null: not evaluated). */
  memory: EvalHistoryPoint | null;
  /** Runes against the draft against today's scoring (null: not evaluated, or before 0.9.0). */
  scoring: EvalHistoryPoint | null;
  /** Late-game swaps against keeping every item (null: not evaluated, or before 0.17.0). */
  swaps: EvalHistoryPoint | null;
}

// ---- Backup of the pool (server/autoExport.ts, 0.18.0; player data sets were dropped in 0.19.0) ----------

/** A backup or an import running in the transfer thread (server/transfer.ts). */
export interface TransferProgress {
  kind: 'export' | 'import';
  label: string;
  fraction: number | null;
  startedAt: number;
  finishedAt: number | null;
  error: string | null;
  /** When finished: what it made or did, in a sentence. */
  result: string | null;
  file?: string | null;
}

/** A ZIP in the backup folder, made on another computer and not imported or dismissed here yet. */
export interface ImportOffer { file: string; kind: 'pool'; name: string; exportedAt: number; games: number; bytes: number }

export interface BackupStatus {
  transfer: TransferProgress | null;
  /** The backup folder and when the last backup finished; null while auto backup is off. */
  autoExport: { folder: string; last: number | null; error: string | null } | null;
  offers: ImportOffer[];
}
