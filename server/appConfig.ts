import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import type { DesktopSettings } from '../shared/desktop.ts';
import type { BuildMode, Lane } from '../shared/types.ts';

/**
 * Per-user settings shared by the desktop shell and the middleware:
 *   %APPDATA%/irelia-companion/config.json   (IRELIA_CONFIG_DIR overrides the folder, e.g. for tests)
 *
 * Both processes write it. Each write re-reads the file and merges only the caller's keys,
 * then replaces the file atomically, so neither process clobbers the other's settings.
 */
export interface AppConfig {
  dataDir?: string;               // absolute; unset = default location
  riotApiKey?: string;            // set from Settings; wins over RIOT_API_KEY in .env
  riotApiKeySavedAt?: number;     // epoch ms when that key was saved (development keys expire 24 h after they are generated)
  desktop?: Partial<DesktopSettings>;  // owned by the Electron main process
  buildMode?: BuildMode;          // the dashboard's Draft-adjusted / Lane baseline choice; unset = follow eval:builds
  /** Patch memory (model/weights.ts) chosen on the Data tab; unset = follow the last evaluation. */
  patchMemory?: 'on' | 'off';
  /**
   * Runes and summoners against the draft (model/expected.ts, 0.9.0) chosen on the Data tab. Read only while Follow the
   * evaluation is off (patchMemory set): following, the last evaluation decides. Unset = off.
   */
  pregameScoring?: 'on' | 'off';
  /**
   * Late-game swaps (docs/SWAPS.md, 0.17.0) chosen on the Data tab: the Items view's swap advice in place of the Sixth
   * Item. Read only while Follow the evaluation is off (patchMemory set), as pregameScoring. Unset = off.
   */
  lateSwaps?: 'on' | 'off';
  /** Data → Patches: keep patches older than patch memory's window in the archive (server/oldPatches.ts). */
  archiveOldPatches?: boolean;
  /** Settings → Storage: delete cached Riot matches and timelines of patches older than the window. */
  trimCacheToWindow?: boolean;
  lane?: Lane;                    // the lane picked last, used when no draft or game assigns one (server/matchup.ts)
  /** Your server, chosen in Settings when the League client and Riot can't tell (server/scraper/account.ts). */
  myPlatform?: string;
  /** Settings → GPU (0.13.0): Evaluate builds' draft similarities on an NVIDIA graphics card (server/gpu/). Unset = off. */
  useGpu?: boolean;
  /** Settings → While you play (0.14.0): fetching pauses during champ select and games. Unset = on. */
  pauseFetchingInGame?: boolean;
  /** What the Stage C import created in the League client: the only rune page and item set it ever replaces. */
  lcuImport?: { runePageId?: number; itemSetUid?: string };
  /** Auto backup (0.18.0, server/autoExport.ts): the folder the pool's ZIP backups go to by themselves. Unset = off. */
  autoExport?: AutoExportSettings;
}

/** What auto backup needs to remember. */
export interface AutoExportSettings {
  folder: string;
  /** When the last backup finished (epoch ms). */
  last?: number;
  /** Exports found in the folder already imported or dismissed here (file name → its export time). */
  seen?: Record<string, number>;
}

export const appConfigDir = path.resolve(process.env.IRELIA_CONFIG_DIR
  ?? path.join(process.env.APPDATA ?? path.join(os.homedir(), '.config'), 'irelia-companion'));
export const appConfigFile = path.join(appConfigDir, 'config.json');

export function readAppConfig(): AppConfig {
  try { return JSON.parse(fs.readFileSync(appConfigFile, 'utf8')) as AppConfig; } catch { return {}; }
}

/** Merges `patch` into config.json. A key set to `undefined` is removed. */
export function updateAppConfig(patch: Partial<AppConfig>): AppConfig {
  const next = { ...readAppConfig(), ...patch };
  fs.mkdirSync(appConfigDir, { recursive: true });
  const tmp = `${appConfigFile}.${process.pid}.tmp`;
  fs.writeFileSync(tmp, `${JSON.stringify(next, null, 2)}\n`);  // JSON.stringify drops undefined keys
  fs.renameSync(tmp, appConfigFile);
  return next;
}

/**
 * scrape.config.json next to config.json wins over the repo / bundled copy
 * (the installed app's copy is replaced on every update).
 */
export function scopeFile() {
  const user = path.join(appConfigDir, 'scrape.config.json');
  return fs.existsSync(user) ? user : path.resolve('scrape.config.json');
}
