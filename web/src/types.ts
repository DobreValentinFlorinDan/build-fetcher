// The server's JSON: crates/core/src/build.rs (the contract, camelCase) and the views in crates/server.

export type Role = 'TOP' | 'JUNGLE' | 'MIDDLE' | 'BOTTOM' | 'UTILITY';
export const ROLES: Role[] = ['TOP', 'JUNGLE', 'MIDDLE', 'BOTTOM', 'UTILITY'];
export type Platform = 'EUN1' | 'EUW1' | 'KR';
export const PLATFORMS: Platform[] = ['EUN1', 'EUW1', 'KR'];
export type RankBand = 'masterPlus' | 'diamond' | 'mine';
export type Activity = 'idle' | 'busy' | 'champSelect' | 'inGame';
export type Patch = { major: number; minor: number };

export type OptionKind =
  | 'runePage' | 'keystone' | 'summoners' | 'skillMax' | 'skillOrder'
  | 'startingItems' | 'firstItem' | 'coreItems' | 'boots' | 'lateItem';

export type BuildOption = { kind: OptionKind; key: string };
export type Basis = 'matchup' | 'profile' | 'lane' | 'champion';

export type RankedOption = {
  option: BuildOption;
  label: string;
  games: number;
  wins: number;
  weightedGames: number;
  winRate: number;
  wpa: number;
  predictedWin: number | null;
  basis: Basis;
  thin: boolean;
};

export type Section = { kind: OptionKind; options: RankedOption[] };

export type BuildRecommendation = {
  champion: number;
  role: Role;
  patch: Patch | null;
  tier: 'a' | 'b';
  laneOpponent: number | null;
  enemyProfile: 'physical' | 'magic' | 'mixed' | null;
  games: number;
  sections: Section[];
  model: string | null;
  notes: string[];
};

export type DraftContext = {
  champion: number;
  role: Role | null;
  allies: [number, Role | null][];
  enemies: [number, Role | null][];
  patch: Patch | null;
};

export type LiveContext = {
  draft: DraftContext;
  gameTimeS: number;
  myItems: number[];
  enemyItems: [number, number[]][];
  myLevel: number;
  myCs: number;
  myGold: number;
  opponentLevel: number | null;
  opponentCs: number | null;
  teamKills: number;
  enemyKills: number;
  towers: [number, number];
  dragons: [number, number];
  barons: [number, number];
};

export type LiveAdvice = {
  nextBuys: RankedOption[];
  counters: [RankedOption, string][];
  predictedWin: number | null;
  notes: string[];
};

export type BudgetStatus = { scope: string; limits: string; used: number; ours: number; limit: number; windowS: number; shared: boolean };

export type FetchStatus = {
  running: boolean;
  paused: string | null;
  budgets: BudgetStatus[];
  requestsLastHour: number;
  matchesLastHour: number;
  matchesTotal: number;
  dropped: [string, number][];
  current: string | null;
  keyError: string | null;
};

export type StorageStatus = {
  budgetBytes: number;
  usedBytes: number;
  parts: [string, number][];
  patches: Patch[];
  lastCleanupFreedBytes: number | null;
};

export type Metrics = { games: number; logLoss: number; brier: number; calibration: number[] };
export type CheckpointInfo = { version: string; kind: string; createdMs: number; metrics: Metrics; recordsSeen: number; active: boolean };
export type ModelStatus = { learning: boolean; backend: string; checkpoints: CheckpointInfo[]; log: string[] };

export type PairVolume = { champion: number; role: Role; patch: Patch; games: number; wins: number; newestMs: number };

export type LcuView = { status: string; phase: string };

export type Status = {
  version: string;
  activity: Activity;
  lcu: LcuView;
  fetch: FetchStatus;
  model: ModelStatus;
  firstRunComplete: boolean;
  keySet: boolean;
  staticReady: boolean;
};

export type Pick = { champion: number; role: Role | null; locked: boolean; ally: boolean; me: boolean };
export type ChampSelect = { draft: DraftContext; picks: Pick[]; myPickLocked: boolean };
export type DraftState = { champSelect: ChampSelect; recommendation: BuildRecommendation | null; pending: boolean; error: string | null };

export type Performance = {
  me?: { cs: number; csPerMin: number; visionScore: number };
  opponent?: { cs: number; championName: string } | null;
  diff?: { cs: number; gold: number; vision: number } | null;
} | null;

export type LiveState = {
  active: boolean;
  simulated?: boolean;
  context?: LiveContext | null;
  advice?: LiveAdvice | null;
  error?: string | null;
  performance?: Performance;
};

export type AppSettings = {
  regions: Platform[];
  rankFloor: RankBand;
  diskBudgetBytes: number;
  tierA: number[];
  tierADetected: number[];
  gpu: boolean;
  learning: boolean;
  firstRunComplete: boolean;
};

export type KeyOffer = { state: 'available' | 'missing' | 'empty' } | { state: 'unreadable'; reason: string };

export type KeyView = { masked: string; set: boolean; error: string | null; ireliaOffer: KeyOffer; portal: string };
export type SettingsView = { settings: AppSettings; key: KeyView };

export type PartResult = { ok: boolean; error?: string; replaced: boolean };
export type ImportResult = { runePage: PartResult; itemSet: PartResult; title: string };

export type NamedIcon = { name: string; icon: string };
export type Champion = { id: number; key: string; name: string; tags: string[] };
export type StaticView = {
  ready: boolean;
  version: string | null;
  patch: string | null;
  champions: Champion[];
  items: Record<string, NamedIcon>;
  runes: Record<string, NamedIcon>;
  summoners: Record<string, NamedIcon>;
};

/** A /ws frame. */
export type ServerEvent =
  | { type: 'status'; status: Status | null }
  | { type: 'lcu'; state: LcuView }
  | { type: 'activity'; activity: Activity }
  | { type: 'draft'; draft: DraftState | null }
  | { type: 'live'; state: LiveState }
  | { type: 'static'; ready: boolean };
