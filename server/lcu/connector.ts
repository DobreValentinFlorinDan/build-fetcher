import { execFile } from 'node:child_process';
import { EventEmitter } from 'node:events';
import fs from 'node:fs/promises';
import path from 'node:path';
import WebSocket from 'ws';
import { LANES, toLane } from '../../shared/lanes.ts';
import type { DraftPick, Lane, LcuState } from '../../shared/types.ts';
import { loopbackJson, loopbackRequest } from './https.ts';

interface Credentials { port: number; password: string }

export interface ChampSelectSession {
  localPlayerCellId: number;
  myTeam: { cellId: number; championId: number; championPickIntent: number; assignedPosition: string }[];
  theirTeam: { cellId: number; championId: number; championPickIntent: number; assignedPosition: string }[];
  actions: { actorCellId: number; championId: number; completed: boolean; type: string }[][];
}

/** Likely opponent for each of `lanes` among `enemyIds`, a different pick for each lane (engine/laneOpponent.ts). */
export type GuessOpponents = (enemyIds: number[], lanes: Lane[]) => Partial<Record<Lane, number | null>>;

/** Riot positions outside the lanes Irelia data covers: an enemy shown in one of them is nobody's lane opponent. */
const OTHER_POSITIONS = ['JUNGLE', 'BOTTOM', 'UTILITY'];

/**
 * Draft state from an LCU champ-select session. `myPosition` is the assigned position ("" in blind pick, ARAM
 * and customs: the dashboard then uses the lane the player picked). The lane opponent is suggested for each
 * lane: an enemy whose role is visible wins (ranked hides them), else the database guesses from the picks whose
 * role is hidden, never the same pick for two lanes.
 */
export function parseChampSelect(s: ChampSelectSession, ireliaId: number, guessOpponents: GuessOpponents): NonNullable<LcuState['champSelect']> {
  const lockedCells = new Set(s.actions.flat().filter((a) => a.type === 'pick' && a.completed).map((a) => a.actorCellId));
  const pick = (p: ChampSelectSession['myTeam'][number]): DraftPick => ({
    cellId: p.cellId,
    championId: p.championId || p.championPickIntent,
    position: (p.assignedPosition || '').toUpperCase(),
    locked: lockedCells.has(p.cellId),
  });
  const me = s.myTeam.find((p) => p.cellId === s.localPlayerCellId);
  const myChampionId = me ? me.championId || me.championPickIntent : 0;
  const myPosition = (me?.assignedPosition || '').toUpperCase();
  const enemies = s.theirTeam.map(pick);

  const shown = (lane: Lane) => enemies.find((e) => e.championId && toLane(e.position) === lane)?.championId ?? null;
  const roleShown = (e: DraftPick) => toLane(e.position) !== null || OTHER_POSITIONS.includes(e.position);
  const hiddenIds = enemies.filter((e) => e.championId && !roleShown(e)).map((e) => e.championId);
  const open = LANES.filter((lane) => shown(lane) === null);
  const guessed = open.length && hiddenIds.length ? guessOpponents(hiddenIds, open) : {};
  const suggestedOpponents = Object.fromEntries(LANES.map((lane) => [lane, shown(lane) ?? guessed[lane] ?? null])) as Record<Lane, number | null>;
  return {
    myChampionId,
    myPosition,
    isIrelia: myChampionId === ireliaId,
    allies: s.myTeam.map(pick),
    enemies,
    suggestedOpponentId: suggestedOpponents[toLane(myPosition) ?? 'TOP'],
    suggestedOpponents,
  };
}

const SUBSCRIPTIONS = [
  'OnJsonApiEvent_lol-champ-select_v1_session',
  'OnJsonApiEvent_lol-gameflow_v1_gameflow-phase',
];

/**
 * The signed-in account, from the League client: whose games are fetched as source "me". The client's `puuid` is its
 * own (not the Riot API's, which is looked up from the Riot ID: scraper/account.ts).
 */
export interface LcuAccount { puuid: string; gameName: string | null; tagLine: string | null; platform: string | null }

/** Phases that keep the last champ select: the draft itself, the loading screen and its game. */
const KEEPS_DRAFT = new Set(['ChampSelect', 'GameStart', 'InProgress', 'Reconnect']);

/** Phases after a game: the match is over and match-v5 will have it within minutes. */
const AFTER_GAME = new Set(['PreEndOfGame', 'WaitingForStats', 'EndOfGame']);

/**
 * Finds the running League client (lockfile, falling back to the LeagueClientUx process command
 * line via PowerShell), opens the LCU WAMP websocket and emits 'state' whenever draft/phase changes,
 * 'account' once the signed-in account is known, and 'gameEnded' when a game finishes.
 */
export class LcuConnector extends EventEmitter {
  state: LcuState = { status: 'searching', phase: 'None', champSelect: null };
  account: LcuAccount | null = null;
  private creds: Credentials | null = null;
  private ws: WebSocket | null = null;
  private timer: NodeJS.Timeout | null = null;

  constructor(private leaguePaths: string[], private ireliaId: number, private guessOpponents: GuessOpponents) {
    super();
  }

  start() { void this.tick(); }

  private schedule(ms: number) {
    if (this.timer) clearTimeout(this.timer);
    this.timer = setTimeout(() => void this.tick(), ms);
  }

  private set(patch: Partial<LcuState>) {
    this.state = { ...this.state, ...patch };
    this.emit('state', this.state);
  }

  private async tick() {
    if (this.ws) return;
    const creds = await this.discover();
    if (!creds) {
      this.set({ status: 'searching', detail: 'League client not running', phase: 'None', champSelect: null });
      return this.schedule(3000);
    }
    this.creds = creds;
    this.set({ status: 'connecting', detail: `127.0.0.1:${creds.port}` });
    this.connect(creds);
  }

  private async discover(): Promise<Credentials | null> {
    for (const dir of this.leaguePaths) {
      try {
        const [, , port, password] = (await fs.readFile(path.join(dir, 'lockfile'), 'utf8')).split(':');
        if (port && password) return { port: Number(port), password };
      } catch { /* try next */ }
    }
    if (process.platform !== 'win32') return null;
    const cmdline = await new Promise<string>((resolve) =>
      execFile('powershell.exe', ['-NoProfile', '-Command',
        "(Get-CimInstance Win32_Process -Filter \"name='LeagueClientUx.exe'\").CommandLine"],
      { windowsHide: true, timeout: 5000 }, (_e, out) => resolve(out ?? '')));
    const port = /--app-port=(\d+)/.exec(cmdline)?.[1];
    const password = /--remoting-auth-token=([\w-]+)/.exec(cmdline)?.[1];
    return port && password ? { port: Number(port), password } : null;
  }

  private get auth() {
    return 'Basic ' + Buffer.from(`riot:${this.creds!.password}`).toString('base64');
  }

  request<T>(endpoint: string) {
    if (!this.creds) return Promise.reject(new Error('LCU not connected'));
    return loopbackJson<T>(this.creds.port, endpoint, this.auth);
  }

  /** Any method: the Stage C import (lcu/importer.ts) is the only writer, on your click in champ select. */
  send<T>(method: 'GET' | 'POST' | 'PUT' | 'DELETE', endpoint: string, body?: unknown) {
    if (!this.creds) return Promise.reject(new Error('The League client is not connected'));
    return loopbackRequest<T>(this.creds.port, method, endpoint, this.auth, body);
  }

  private connect(creds: Credentials) {
    const ws = new WebSocket(`wss://127.0.0.1:${creds.port}/`, 'wamp', {
      headers: { Authorization: this.auth },
      rejectUnauthorized: false,  // loopback self-signed Riot cert
    });
    this.ws = ws;

    ws.on('open', async () => {
      for (const topic of SUBSCRIPTIONS) ws.send(JSON.stringify([5, topic]));   // WAMP SUBSCRIBE
      this.set({ status: 'connected', detail: `127.0.0.1:${creds.port}` });
      await this.bootstrap();
    });

    ws.on('message', (raw) => {
      try {
        const [opcode, , payload] = JSON.parse(raw.toString()) as [number, string, { uri: string; eventType: string; data: unknown }];
        if (opcode !== 8) return;  // WAMP EVENT
        this.handleEvent(payload.uri, payload.eventType, payload.data);
      } catch { /* ignore non-JSON frames */ }
    });

    const reset = () => {
      if (this.ws !== ws) return;
      this.ws = null;
      this.creds = null;
      this.set({ status: 'disconnected', detail: 'Client closed', phase: 'None', champSelect: null });
      this.schedule(3000);
    };
    ws.on('close', reset);
    ws.on('error', reset);
  }

  /**
   * Your server, from what the League client says. None of these endpoints is in Riot's public docs and they have
   * changed over the years, so each is tried in turn: the chat identity ("EUW1"), the Riot client's region ("EUW"),
   * then the older login data packet. The worker falls back to Riot's account-v1, then to Settings.
   */
  private async platform(): Promise<string | null> {
    const asText = (v: unknown) => (typeof v === 'string' && v.trim() ? v.trim() : null);
    const chat = await this.request<{ platformId?: string }>('/lol-chat/v1/me').catch(() => null);
    if (asText(chat?.platformId)) return asText(chat!.platformId);
    const locale = await this.request<{ region?: string }>('/riotclient/region-locale').catch(() => null);
    if (asText(locale?.region)) return asText(locale!.region);
    return asText(await this.request<unknown>('/lol-platform-config/v1/namespaces/LoginDataPacket/platformId').catch(() => null));
  }

  private async bootstrap() {
    try {
      // current-summoner carries the Riot ID (the worker turns it into the Riot API's PUUID) and the client's PUUID.
      const me = await this.request<{ gameName?: string; tagLine?: string; displayName?: string; puuid?: string }>('/lol-summoner/v1/current-summoner');
      this.set({ summonerName: me.gameName ? `${me.gameName}#${me.tagLine}` : me.displayName });
      if (me.puuid) {
        this.account = { puuid: me.puuid, gameName: me.gameName || null, tagLine: me.tagLine || null, platform: await this.platform() };
        this.emit('account', this.account);
      }
    } catch { /* client still starting */ }
    try {
      this.handleEvent('/lol-gameflow/v1/gameflow-phase', 'Update', await this.request<string>('/lol-gameflow/v1/gameflow-phase'));
    } catch { /* ignore */ }
    try {
      this.handleEvent('/lol-champ-select/v1/session', 'Update', await this.request<ChampSelectSession>('/lol-champ-select/v1/session'));
    } catch { /* not in champ select */ }
  }

  private handleEvent(uri: string, eventType: string, data: unknown) {
    if (uri === '/lol-gameflow/v1/gameflow-phase') {
      const phase = String(data ?? 'None');
      if (AFTER_GAME.has(phase) && !AFTER_GAME.has(this.state.phase)) this.emit('gameEnded');
      this.set({ phase, ...(KEEPS_DRAFT.has(phase) ? {} : { champSelect: null }) });
    } else if (uri === '/lol-champ-select/v1/session') {
      // The session is deleted as champ select ends, before the phase moves on: the draft is kept for its game (the
      // lane opponent when the game has no positions) and dropped by the phase change above after a dodge.
      if (eventType === 'Delete' || !data) return this.set({ champSelect: KEEPS_DRAFT.has(this.state.phase) ? this.state.champSelect : null });
      this.set({ champSelect: parseChampSelect(data as ChampSelectSession, this.ireliaId, this.guessOpponents) });
    }
  }
}
