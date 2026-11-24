// The current patch per region, when it started there, and the builds (hotfixes) seen within it.
//
// Data Dragon names the newest patch as soon as it is published, often hours before a region runs it. Taking the
// "current patch" from Data Dragon alone would turn every game of a region still on the old patch into a stale one.
// So each platform's current patch is the newest one seen in its own matches (table game_versions), with Data
// Dragon as the fallback for a platform with no games yet. Riot has no "patch start" endpoint either; the start is
// bracketed by the newest previous-patch game and the oldest current-patch game seen on that platform.
import { comparePatch, toPatch } from './ddragon.ts';

export interface VersionRow { platform: string; game_version: string; patch: string; first_seen: number; last_seen: number; games: number }

export interface RegionPatch {
  platform: string;
  patch: string;                 // current patch on this platform
  previousPatch: string;
  /**
   * Lower bound for "played on the current patch" (epoch ms): the newest previous-patch game seen there, else the
   * oldest current-patch game minus a full patch cycle. Match ids are paged from here (startTime); the scan still
   * stops at the first older-patch match, so a bound that is too early only costs a few match-id pages.
   */
  start: number;
  startKnown: boolean;           // true when a previous-patch game brackets the start
  builds: { gameVersion: string; firstSeen: number; games: number }[];   // builds of the current patch, oldest first
  fromDataDragon: boolean;       // no game seen on this platform yet
}

/** A patch lasts two weeks; without a previous-patch game, the window reaches this far back from the oldest game seen. */
export const PATCH_CYCLE_MS = 14 * 24 * 3600_000;

export function regionPatch(
  platform: string, versions: VersionRow[], fallback: { patch: string; previousPatch: string }, now = Date.now(),
): RegionPatch {
  const mine = versions.filter((v) => v.platform === platform);
  if (!mine.length) {
    return { platform, patch: fallback.patch, previousPatch: fallback.previousPatch, start: now - PATCH_CYCLE_MS, startKnown: false, builds: [], fromDataDragon: true };
  }
  const patch = mine.map((v) => v.patch).reduce((a, b) => (comparePatch(b, a) > 0 ? b : a));
  const previousPatch = previousOf(patch, fallback);
  const current = mine.filter((v) => v.patch === patch);
  const older = mine.filter((v) => comparePatch(v.patch, patch) < 0);
  const firstSeen = Math.min(...current.map((v) => v.first_seen));
  const newestOlder = older.length ? Math.max(...older.map((v) => v.last_seen)) : null;
  return {
    platform, patch, previousPatch,
    start: newestOlder ?? firstSeen - PATCH_CYCLE_MS,
    startKnown: newestOlder !== null,
    builds: current.sort((a, b) => a.first_seen - b.first_seen).map((v) => ({ gameVersion: v.game_version, firstSeen: v.first_seen, games: v.games })),
    fromDataDragon: false,
  };
}

/** The patch before `patch`: Data Dragon's pair when it names the same patch, else the previous minor number. */
export function previousOf(patch: string, fallback: { patch: string; previousPatch: string }) {
  if (fallback.patch === patch) return fallback.previousPatch;
  const [major, minor] = patch.split('.').map(Number);
  if (minor > 1) return `${major}.${minor - 1}`;
  return comparePatch(fallback.previousPatch, patch) < 0 ? fallback.previousPatch : patch;
}

/** Every platform that has games, plus `platforms` (scope), keyed by platform. */
export function regionPatches(
  versions: VersionRow[], fallback: { patch: string; previousPatch: string }, platforms: readonly string[] = [], now = Date.now(),
) {
  const all = new Set([...versions.map((v) => v.platform), ...platforms]);
  return new Map([...all].map((p) => [p, regionPatch(p, versions, fallback, now)]));
}

/** Is a game on `patch` older than its platform's current patch? */
export const isStale = (patch: string, region: Pick<RegionPatch, 'patch'>) => comparePatch(patch, region.patch) < 0;

/** "16.19.712.4471" → its patch; the full string is the build. */
export const patchOfVersion = toPatch;

// ------------------------------------------------------------------------------------------- hotfixes

/** A hotfix entered by hand (scrape.config.json `hotfixes`): not every hotfix changes the game build number. */
export interface ManualHotfix { patch: string; date: string; note?: string }

export interface Hotfix { patch: string; at: number; source: 'build' | 'manual'; label: string }

/**
 * Hotfix moments within `patch` on one platform: every build after the first one seen, plus the manual dates.
 * Stage B weights games played after an Irelia or item hotfix above the ones before it.
 */
export function hotfixes(region: RegionPatch, manual: ManualHotfix[]): Hotfix[] {
  const fromBuilds = region.builds.slice(1).map((b) => ({ patch: region.patch, at: b.firstSeen, source: 'build' as const, label: b.gameVersion }));
  const fromConfig = manual.filter((h) => h.patch === region.patch && Number.isFinite(Date.parse(h.date)))
    .map((h) => ({ patch: h.patch, at: Date.parse(h.date), source: 'manual' as const, label: h.note ?? 'manual hotfix' }));
  return [...fromBuilds, ...fromConfig].sort((a, b) => a.at - b.at);
}
