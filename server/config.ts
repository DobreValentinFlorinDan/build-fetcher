import path from 'node:path';
import { readAppConfig } from './appConfig.ts';
import { parseLimits, type PacingOptions } from './riot/rateLimiter.ts';

try { process.loadEnvFile(path.resolve('.env')); } catch { /* .env is optional */ }

const clampInt = (raw: string | undefined, lo: number, hi: number, fallback: number) => {
  const n = Math.round(Number(raw));
  return raw && Number.isFinite(n) ? Math.min(hi, Math.max(lo, n)) : fallback;
};

const DEV_KEY_LIMITS = '20:1,100:120';

/** data/ unless the user moved it: ./data in dev, %APPDATA%/irelia-companion/data in the installed app. */
export const defaultDataDir = path.resolve(process.env.IRELIA_DEFAULT_DATA_DIR ?? 'data');

/** IRELIA_DATA_DIR pins the folder for this process (tests, portable setups) and disables moving it. */
export const dataDirLockedBy = process.env.IRELIA_DATA_DIR ? 'IRELIA_DATA_DIR environment variable' : null;

/** Everything the app stores lives under one folder; see server/storage.ts for its layout. */
export function dataPaths(dir: string) {
  return {
    dataDir: dir,
    dbPath: path.join(dir, 'irelia.db'),
    ddragonCacheDir: path.join(dir, 'ddragon'),
    riotCacheDir: path.join(dir, 'cache'),   // raw Riot responses (see riot/diskCache.ts)
  };
}

const appConfig = readAppConfig();

export const config = {
  riotApiKey: appConfig.riotApiKey || process.env.RIOT_API_KEY || '',
  port: Number(process.env.PORT ?? 5174),
  ...dataPaths(path.resolve(process.env.IRELIA_DATA_DIR ?? appConfig.dataDir ?? defaultDataDir)),

  championId: 39,
  championKey: 'Irelia',
  rankedSoloQueue: 420,
  rankedFlexQueue: 440,   // own games only (source "me")

  // Platforms, tiers, OTP threshold and history depth live in scrape.config.json (server/scrapeScope.ts).
  reseedAfterMs: 6 * 3600_000,
  recheckOtpAfterMs: 3 * 24 * 3600_000,      // pilots (OTPs, mains): their Irelia activity is refreshed this often
  recheckOthersAfterMs: 14 * 24 * 3600_000,  // everyone else: rarely becomes an Irelia main
  rescanPlayerAfterMs: 45 * 60_000,

  // Riot development key defaults. Production keys: set RIOT_RATE_LIMITS. Headers refine these at runtime.
  rateLimits: [parseLimits(process.env.RIOT_RATE_LIMITS), parseLimits(DEV_KEY_LIMITS)].find((l) => l.length)!,
  // Request pacing (server/riot/rateLimiter.ts): starts are spaced windowMs / floor(limit × safety) apart,
  // so a dev key sends one request every ~1.33 s instead of 90 at once followed by a 2-minute stall.
  pacing: {
    safety: 0.9,                                                   // plan for 90 % of every window
    burst: 3,                                                      // back-to-back starts allowed after an idle spell (max 3)
    concurrency: clampInt(process.env.RIOT_CONCURRENCY, 1, 3, 3),  // requests in flight per host; never raises the rate
    slowdown: 0.8,                                                 // pace × 0.8 after an application/method 429 …
    recoveryMs: 60_000,                                            // … back to full pace over the next minute
  } satisfies PacingOptions,
  rateSnapshotMs: 200,                                             // live rate-limit bars: checked 5×/s, sent only on change

  // Recommendation engine
  minMatchupGames: 15,          // below this, blend in general Irelia data
  generalPriorWeight: 0.15,     // weight of non-matchup games when blending
  minLaneGames: 30,             // below this many games in the lane, blend in the other lane (top <-> mid)
  otherLaneWeight: 0.15,        // weight of the other lane's games when blending
  previousPatchWeight: 0.35,    // weight of previous-patch games when current patch is thin
  minCurrentPatchGames: 40,     // below this, previous patch is admitted (at reduced weight)
  startingItemWindowMs: 60_000,

  // Live Client Data API polling: fast while a game runs, slower while waiting for one.
  livePollMs: 1000,
  liveIdlePollMs: 3000,

  leaguePaths: [
    process.env.LEAGUE_PATH,
    'C:/Riot Games/League of Legends',
    'D:/Riot Games/League of Legends',
    'C:/Program Files/Riot Games/League of Legends',
  ].filter(Boolean) as string[],
};

/** Points every data path at a new folder (after a data-folder move). Callers reopen what they hold. */
export function setDataDir(dir: string) {
  Object.assign(config, dataPaths(path.resolve(dir)));
}

export type Config = typeof config;
