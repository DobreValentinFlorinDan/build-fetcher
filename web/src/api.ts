import { useEffect, useState } from 'react';
import type { PlanItem } from '../../shared/buildPlan.ts';
import type {
  ActionEstimates, BackupStatus, BuildMode, BuildState, ConversionStatus, EvalHistoryEntry, SlimStatus, GameReview, GpuState, JobName, JobsStatus, Lane, LcuState, LiveGameState, Matchup, MigrationMode, ModelStatus,
  OldPatchesStatus, PersonalReport, ScraperStatus, ServerEvent, SettingsState, StorageAction, StorageInspection, StorageProgress, StorageStats, SystemInfo,
} from '../../shared/types.ts';
import { rateStore } from './rate.ts';

const CDN = 'https://ddragon.leagueoflegends.com/cdn';

export interface RuneTree {
  id: number; key: string; name: string; icon: string;
  slots: { runes: { id: number; key: string; name: string; icon: string; shortDesc: string }[] }[];
}

export interface Meta {
  version: string;
  patch: string;
  previousPatch: string;
  champions: { id: number; key: string; name: string; tags: string[] }[];
  items: Record<number, { name: string } & PlanItem>;   // every Summoner's Rift item (server/engine/itemCatalog.ts)
  runes: RuneTree[];
  summoners: Record<number, { name: string; image: string }>;
  spells: { key: string; name: string; image: string }[];
}

export interface DbStats {
  players: number; otp: number; mains?: number; matches: number; games: number; current: number;
  currentPatch: string; previousPatch: string;
  patches: { patch: string; games: number; lanes?: Record<string, number> }[];   // lanes: games per position (build sources)
  sources?: Partial<Record<'otp' | 'main' | 'harvested' | 'me', number>>;      // stored Irelia games per source
  buildSources?: string[];
  /** Patches within patch memory's window (what Remove and Archive old patches keep), and the archive per patch. */
  windowPatches?: string[];
  archive?: { patch: string; games: number; bytes: number }[];
  /** Current patch per platform, from its own games (server/patches.ts). */
  regions?: {
    platform: string; patch: string; previousPatch: string; start: number; startKnown: boolean; fromDataDragon: boolean;
    builds: { gameVersion: string; firstSeen: number; games: number }[];
    hotfixes: { at: number; source: 'build' | 'manual'; label: string }[];
  }[];
}

export const img = {
  item: (v: string, id: number) => `${CDN}/${v}/img/item/${id}.png`,
  champ: (v: string, key: string) => `${CDN}/${v}/img/champion/${key}.png`,
  spell: (v: string, file: string) => `${CDN}/${v}/img/spell/${file}`,
  rune: (icon: string) => `${CDN}/img/${icon}`,
};

export async function getJson<T>(url: string, init?: RequestInit): Promise<T> {
  const res = await fetch(url, init);
  if (!res.ok) {
    // Prefer the server's own message ({ error }) over a bare status code.
    const body = await res.json().catch(() => null) as { error?: string } | null;
    throw new Error(body?.error ?? `${res.status} ${url}`);
  }
  return res.json() as Promise<T>;
}

const send = <T>(url: string, method: string, body: unknown) =>
  getJson<T>(url, { method, headers: { 'Content-Type': 'application/json' }, body: JSON.stringify(body) });

export const api = {
  meta: () => getJson<Meta>('/api/meta'),
  /**
   * Changes the matchup every window shows (the server pushes it and its build back over /ws): a lane, an opponent
   * (null: all matchups), back to following the draft and the game, or the regions feeding the draft-adjusted build.
   */
  setMatchup: (change: { lane?: Lane } | { opponentId: number | null } | { follow: true } | { regions: string[] | null }) =>
    send<{ matchup: Matchup }>('/api/matchup', 'PUT', change),
  /**
   * npm run eval:builds in the app, in its own thread. Its progress and result also arrive over /ws (Realtime.jobs).
   * 'auto' (Evaluate builds) runs what the plan says (JobsStatus.plan: a quick update or a full evaluation); 'full'
   * (Full evaluation) every game again, and replaces a quick update that is running (0.16.0).
   */
  evaluate: (path: 'auto' | 'full' = 'auto') => send<{
    text: string; file: string; gate: 'passed' | 'failed'; mode: BuildMode; path?: 'full' | 'update'; reason?: string;
  }>('/api/model/evaluate', 'POST', { path }),
  cancelJob: (job: JobName) => send<JobsStatus>('/api/jobs/cancel', 'POST', { job }),
  /** null: follow npm run eval:builds (draft-adjusted once it passed). */
  setMode: (mode: BuildMode | null) => send<{ mode: BuildMode }>('/api/model/mode', 'PUT', { mode }),
  /** Patch memory on, off, or null: follow npm run eval:builds (on once it found it better). The model rebuilds. */
  setMemory: (memory: 'on' | 'off' | null) => send<{ memory: 'on' | 'off' | null }>('/api/model/memory', 'PUT', { memory }),
  /** Every saved evaluation reduced to its comparisons, oldest first (the Data tab's history). */
  evaluations: () => getJson<EvalHistoryEntry[]>('/api/model/evaluations'),
  setScoring: (scoring: 'on' | 'off') => send<{ scoring: 'on' | 'off'; memory: 'on' | 'off' }>('/api/model/scoring', 'PUT', { scoring }),
  /** Late-game swaps in the Items view (0.17.0): your choice, so Follow the evaluation goes off. Nothing rebuilds. */
  setSwaps: (swaps: 'on' | 'off') => send<{ swaps: 'on' | 'off'; memory: 'on' | 'off' }>('/api/model/swaps', 'PUT', { swaps }),
  /** Stage C1: the build shown as a rune page and an item set in the League client (champ select only). */
  importBuild: (key: string, mode: BuildMode) => send<{
    runePage: { ok: true; id: number; replaced: boolean } | { ok: false; error: string };
    itemSet: { ok: true; title: string; replaced: boolean } | { ok: false; error: string };
  }>('/api/lcu/import', 'POST', { key, mode }),
  personal: (lane: Lane | null) => getJson<{ report: PersonalReport | null; games: { matchId: string; created: number; win: boolean; lane: Lane | null; opponentId: number | null }[] }>(
    `/api/personal${lane ? `?lane=${lane}` : ''}`),
  review: (matchId: string) => getJson<GameReview>(`/api/personal/review/${encodeURIComponent(matchId)}`),
  stats: () => getJson<DbStats>('/api/db/stats'),
  startScraper: () => getJson('/api/scraper/start', { method: 'POST' }),
  stopScraper: () => getJson('/api/scraper/stop', { method: 'POST' }),
  purgeStale: () => getJson<{ removed: number }>('/api/db/purge-stale', { method: 'POST' }),
  slim: () => getJson<SlimStatus>('/api/storage/slim'),
  slimAction: (action: 'start' | 'stop' | 'rollback' | 'keep') => send<SlimStatus>('/api/storage/slim', 'POST', { action }),
  /** Data → Patches' archive switch and Storage's cache switch: answered once the change is applied. */
  oldPatches: () => getJson<OldPatchesStatus>('/api/old-patches'),
  setOldPatches: (change: { archive?: boolean; trimCache?: boolean }) => send<OldPatchesStatus>('/api/old-patches', 'PUT', change),
  backfill: (force = false) => send<{ ok: true }>('/api/db/backfill', 'POST', { force }),
  audit: () => send<{ text: string; file: string }>('/api/db/audit', 'POST', {}),
  estimates: () => getJson<ActionEstimates>('/api/estimates'),
  /** This computer's processor and memory, and the threads Evaluate builds would use now (0.11.0). */
  system: () => getJson<SystemInfo>('/api/system'),

  storageStats: () => getJson<StorageStats>('/api/storage/stats'),
  inspectFolder: (dir: string) => getJson<StorageInspection>(`/api/storage/inspect?dir=${encodeURIComponent(dir)}`),
  clearStorage: (target: 'riot-cache' | 'riot-expired' | 'ddragon-old' | 'compact-db') =>
    send<{ freedBytes: number; files: number; stats: StorageStats }>('/api/storage/clear', 'POST', { target }),
  migrateStorage: (targetDir: string, mode: MigrationMode) =>
    send<{ ok: true; dataDir: string; from: string; copiedBytes: number; leftovers: string[]; stats: StorageStats }>(
      '/api/storage/migrate', 'POST', { targetDir, mode }),
  settings: () => getJson<SettingsState>('/api/settings'),
  saveApiKey: (riotApiKey: string) => send<SettingsState>('/api/settings', 'PUT', { riotApiKey }),
  /** Your server by hand (null: detect it from the League client and Riot again). */
  setMyPlatform: (myPlatform: string | null) => send<SettingsState>('/api/settings', 'PUT', { myPlatform }),
  setPauseFetchingInGame: (pauseFetchingInGame: boolean) => send<SettingsState>('/api/settings', 'PUT', { pauseFetchingInGame }),
  /** Settings → GPU (0.13.0): Use GPU for Evaluate builds, Test GPU and its log. */
  gpu: () => getJson<GpuState>('/api/gpu'),
  setUseGpu: (useGpu: boolean) => send<GpuState>('/api/gpu', 'PUT', { useGpu }),
  testGpu: () => send<GpuState>('/api/gpu/test', 'POST', {}),
  clearGpuLog: () => send<GpuState>('/api/gpu/log', 'DELETE', {}),

  // ---- Backup of the pool (0.18.0) ----
  backup: () => getJson<BackupStatus>('/api/backup'),
  /** The backup folder; null turns auto backup off. */
  setBackupFolder: (folder: string | null) => send<BackupStatus>('/api/backup/folder', 'PUT', { folder }),
  backUpNow: () => send<BackupStatus>('/api/backup/now', 'POST', {}),
  inspectImport: (file: string) => getJson<ImportPreview>(`/api/import/inspect?file=${encodeURIComponent(file)}`),
  importZip: (file: string) => send<BackupStatus>('/api/import', 'POST', { file, confirm: true }),
  dismissImport: (file: string, exportedAt: number) => send<BackupStatus>('/api/import/dismiss', 'POST', { file, exportedAt }),
};

/** What a ZIP holds, read before importing it. */
export interface ImportPreview {
  file: string; name: string; exportedAt: number; games: number; bytes: number; machine: string;
  patches: { patch: string; games: number }[];
}

export const fmtBytes = (n: number) => {
  const u = ['B', 'KB', 'MB', 'GB', 'TB'];
  let i = 0;
  while (n >= 1024 && i < u.length - 1) { n /= 1024; i++; }
  return `${n.toFixed(i && n < 10 ? 1 : 0)} ${u[i]}`;
};

export interface Realtime {
  connected: boolean;
  lcu: LcuState | null;
  live: LiveGameState | null;
  scraper: ScraperStatus | null;
  patchVersion: string | null;
  storage: StorageProgress | null;   // data-folder move in progress
  /** A Storage action running (compact, clear the cache, delete old patches' games), with its time left. */
  storageAction: StorageAction | null;
  /** The matchup every window shows, and its build (server/matchup.ts), pushed whenever either changes. */
  matchup: Matchup | null;
  build: BuildState | null;
  model: ModelStatus | null;
  /** The build evaluation and the data audit: what runs, how far, and how the last run ended. */
  jobs: JobsStatus | null;
  /** Backup of the pool (0.18.0): a backup or import running, the backup folder, and the exports offered from another computer. */
  backup: BackupStatus | null;
}

/** How long each long action would take if started now (GET /api/estimates; estimates), fetched again when `key` changes. */
export function useEstimates(key: unknown): ActionEstimates {
  const [estimates, setEstimates] = useState<ActionEstimates>({});
  useEffect(() => { api.estimates().then(setEstimates).catch(() => {}); }, [key]);
  return estimates;
}

/**
 * This computer (GET /api/system): fetched again when `key` changes and every minute while shown, since the memory
 * available, and so the threads Evaluate builds would use, change as other programs run.
 */
export function useSystem(key: unknown): SystemInfo | null {
  const [info, setInfo] = useState<SystemInfo | null>(null);
  useEffect(() => {
    const load = () => { api.system().then(setInfo).catch(() => {}); };
    load();
    const t = setInterval(load, 60_000);
    return () => clearInterval(t);
  }, [key]);
  return info;
}

/** Every open useOldPatches: the server pushes the status while an operation runs (its progress). */
const oldPatchesListeners = new Set<(s: OldPatchesStatus) => void>();

/**
 * The old-patch switches (Data → Patches' archive, Settings → Storage's cache): their status, and a setter whose change
 * shows as `pending` until the server has applied it (seconds on a large folder).
 */
export function useOldPatches(onChanged?: () => void) {
  const [status, setStatus] = useState<OldPatchesStatus | null>(null);
  const [pending, setPending] = useState<{ archive?: boolean; trimCache?: boolean } | null>(null);
  const [error, setError] = useState<string | null>(null);
  useEffect(() => { api.oldPatches().then(setStatus).catch(() => {}); }, []);
  useEffect(() => {
    oldPatchesListeners.add(setStatus);
    return () => { oldPatchesListeners.delete(setStatus); };
  }, []);
  const set = (change: { archive?: boolean; trimCache?: boolean }) => {
    setPending(change); setError(null);
    api.setOldPatches(change)
      .then((s) => { setStatus(s); onChanged?.(); }, (e) => setError(e instanceof Error ? e.message : String(e)))
      .finally(() => setPending(null));
  };
  return { status, pending, error: error ?? status?.error ?? null, set };
}

/** Every open useSlim: Slim stored data's status as the server pushes it (0.14.0). */
const slimListeners = new Set<(s: SlimStatus) => void>();
export function useSlim() {
  const [status, setStatus] = useState<SlimStatus | null>(null);
  useEffect(() => { api.slim().then(setStatus).catch(() => {}); }, []);
  useEffect(() => {
    slimListeners.add(setStatus);
    return () => { slimListeners.delete(setStatus); };
  }, []);
  return [status, setStatus] as const;
}

/** Every open useConversion: the server pushes the background conversion's progress (0.14.0). */
const conversionListeners = new Set<(s: ConversionStatus) => void>();
/** The background conversion's status: `initial` from the Storage card's stats, then as the server pushes it. */
export function useConversion(initial: ConversionStatus | null | undefined) {
  const [status, setStatus] = useState<ConversionStatus | null>(initial ?? null);
  useEffect(() => { if (initial) setStatus(initial); }, [initial]);
  useEffect(() => {
    conversionListeners.add(setStatus);
    return () => { conversionListeners.delete(setStatus); };
  }, []);
  return status;
}

/** Single websocket to the middleware; reconnects automatically. */
export function useRealtime(): Realtime {
  const [state, setState] = useState<Realtime>({
    connected: false, lcu: null, live: null, scraper: null, patchVersion: null, storage: null, storageAction: null, matchup: null, build: null,
    model: null, jobs: null, backup: null,
  });

  useEffect(() => {
    let ws: WebSocket | null = null;
    let retry: ReturnType<typeof setTimeout>;
    let closed = false;
    const open = () => {
      ws = new WebSocket(`${location.protocol === 'https:' ? 'wss' : 'ws'}://${location.host}/ws`);
      ws.onopen = () => setState((s) => ({ ...s, connected: true }));
      ws.onclose = () => {
        setState((s) => ({ ...s, connected: false }));
        if (!closed) retry = setTimeout(open, 2000);
      };
      ws.onmessage = (e) => {
        const ev = JSON.parse(e.data as string) as ServerEvent;
        // Pacing state goes to its own store, so only the panels that show it re-render (up to 5×/s).
        if (ev.type === 'ratelimit') { rateStore.offer(ev.snapshot); return; }
        if (ev.type === 'old-patches') { for (const l of oldPatchesListeners) l(ev.status); return; }
        if (ev.type === 'conversion') { for (const l of conversionListeners) l(ev.status); return; }
        if (ev.type === 'slim') { for (const l of slimListeners) l(ev.status); return; }
        if (ev.type === 'scraper') rateStore.offer(ev.status.rate);
        setState((s) => {
          switch (ev.type) {
            case 'lcu': return { ...s, lcu: ev.state };
            case 'live': return { ...s, live: ev.state.active ? ev.state : null };
            case 'scraper': return { ...s, scraper: ev.status };
            case 'patch': return { ...s, patchVersion: ev.version };
            case 'storage': return { ...s, storage: ev.progress };
            case 'storage-action': return { ...s, storageAction: ev.action };
            case 'matchup': return { ...s, matchup: ev.matchup };
            case 'build': return { ...s, build: ev.build };
            case 'model': return { ...s, model: ev.status };
            case 'jobs': return { ...s, jobs: ev.status };
            case 'backup': return { ...s, backup: ev.status };
            default: return s;
          }
        });
      };
    };
    open();
    return () => { closed = true; clearTimeout(retry); ws?.close(); };
  }, []);

  return state;
}

export const pct = (x: number | null | undefined, digits = 1) => (x == null ? '—' : `${(x * 100).toFixed(digits)}%`);
/** A win-probability-added in percentage points, signed: "+3.2 pts". */
export const pts = (x: number, digits = 1) => `${x >= 0 ? '+' : '−'}${Math.abs(x * 100).toFixed(digits)} pts`;
export const wrClass = (x: number | null | undefined) =>
  x == null ? '' : x >= 0.55 ? 'wr-great' : x >= 0.52 ? 'wr-good' : x >= 0.48 ? 'wr-ok' : 'wr-bad';
