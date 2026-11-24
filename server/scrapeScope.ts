import fs from 'node:fs';
import { scopeFile } from './appConfig.ts';
import type { PilotSource } from './db.ts';
import type { ManualHotfix } from './patches.ts';
import { DIVISIONS, isPlatform, isTier, type Division, type Platform, type Tier } from './riot/routing.ts';

/** Pilot classes whose match histories are scanned (scraper/pilots.ts). */
export type PilotClass = 'otp' | 'main';

export interface ScrapeScope {
  platforms: Platform[];
  tiers: Tier[];
  divisions: Division[];
  maxLeaguePagesPerDivision: number;
  otpMinMasteryPoints: number;
  /** v3: Irelia mastery points that make an active player a "main" (Irelia among their top masteries). */
  mainMinMasteryPoints: number;
  /** v3: masteries read per player (champion-mastery-v4 top?count=N); Irelia must be among them to count as a main. */
  masteryTopCount: number;
  /** v3: whose histories are scanned. Only players who played Irelia during the current patch are scanned. */
  pilotClasses: PilotClass[];
  /** v3: store other players' TOP / MIDDLE Irelia games found in fetched matches (one timeline call each). */
  harvestIrelias: boolean;
  /** v3: 'patch' pages every match id of the current patch (startTime); 'newest' reads the matchIdsPerPlayer newest, as before v3. */
  historyWindow: 'patch' | 'newest';
  matchIdsPerPlayer: number;
  /** v3: most match ids read per pilot and patch ('patch' window). */
  maxMatchesPerPilot: number;
  /** v3: which sources feed the high-elo builds (the baseline). 'me' never does. */
  buildSources: Exclude<PilotSource, 'me'>[];
  /** v3: look up harvested pilots' rank (league-v4 by-puuid) when they are not on a seeded ladder. */
  lookupHarvestedRanks: boolean;
  /** v3: fetch the signed-in player's own ranked Irelia games (source "me", never in the high-elo builds). */
  ownGames: boolean;
  /** v3: hotfixes entered by hand, for those that do not change the game build number. */
  hotfixes: ManualHotfix[];
  /** v4: sources the draft-aware model learns from, weighted (OTP 1, main 0.8, harvested 0.6). 'me' never. */
  draftSources: Exclude<PilotSource, 'me'>[];
  /** v4: draft model weight per platform ({"KR": 1, "NA1": 0.5}); missing platforms count 1. */
  regionWeights: Record<string, number>;
  /** v4: extra weight of your own platform's games in the draft model. */
  ownRegionBoost: number;
  /** v4: previous-patch weight by patch, overriding the Data Dragon diff ({"16.18": 0.5}). */
  patchWeights: Record<string, number>;
}

export const DEFAULT_SCOPE: ScrapeScope = {
  platforms: ['KR', 'EUW1', 'EUN1', 'NA1', 'JP1'],
  tiers: ['CHALLENGER', 'GRANDMASTER', 'MASTER'],
  divisions: ['I', 'II', 'III', 'IV'],
  maxLeaguePagesPerDivision: 1,
  otpMinMasteryPoints: 150_000,
  mainMinMasteryPoints: 100_000,
  masteryTopCount: 3,
  pilotClasses: ['otp', 'main'],
  harvestIrelias: true,
  historyWindow: 'patch',
  matchIdsPerPlayer: 20,
  maxMatchesPerPilot: 300,
  buildSources: ['otp'],
  lookupHarvestedRanks: true,
  ownGames: true,
  hotfixes: [],
  draftSources: ['otp', 'main', 'harvested'],
  regionWeights: {},
  ownRegionBoost: 1.5,
  patchWeights: {},
};

const list = (raw: string | undefined) => raw?.split(',').map((s) => s.trim().toUpperCase()).filter(Boolean);

/** Validates a partial scope (from scrape.config.json, env or a request) and fills defaults. */
export function parseScope(input: Partial<Record<keyof ScrapeScope, unknown>>, base: ScrapeScope = DEFAULT_SCOPE): ScrapeScope {
  const strings = (v: unknown) => (Array.isArray(v) ? v.map((x) => String(x).toUpperCase()) : undefined);
  const lower = (v: unknown) => (Array.isArray(v) ? v.map((x) => String(x).toLowerCase()) : undefined);
  const platforms = strings(input.platforms) ?? base.platforms;
  const tiers = strings(input.tiers) ?? base.tiers;
  const divisions = strings(input.divisions) ?? base.divisions;
  const classes = lower(input.pilotClasses) ?? base.pilotClasses;
  const sources = lower(input.buildSources) ?? base.buildSources;
  const draftSources = lower(input.draftSources) ?? base.draftSources;

  const badP = platforms.filter((p) => !isPlatform(p));
  const badT = tiers.filter((t) => !isTier(t));
  const badD = divisions.filter((d) => !(DIVISIONS as readonly string[]).includes(d));
  const badC = classes.filter((c) => c !== 'otp' && c !== 'main');
  const badS = [...sources, ...draftSources].filter((s) => s !== 'otp' && s !== 'main' && s !== 'harvested');
  if (badP.length || badT.length || badD.length || badC.length || badS.length) {
    throw new Error(`Invalid scrape scope: ${[...badP, ...badT, ...badD, ...badC.map((c) => `pilot class ${c}`), ...badS.map((s) => `build source ${s}`)].join(', ')}`);
  }
  if (!platforms.length || !tiers.length) throw new Error('Scrape scope needs at least one platform and one tier');
  if (!sources.length || !draftSources.length) throw new Error('Scrape scope needs at least one build source and one draft source');

  const num = (v: unknown, d: number) => (Number.isFinite(Number(v)) && Number(v) > 0 ? Number(v) : d);
  const bool = (v: unknown, d: boolean) => (typeof v === 'boolean' ? v : d);
  const hotfixes = Array.isArray(input.hotfixes)
    ? (input.hotfixes as Partial<ManualHotfix>[]).filter((h) => h && typeof h.patch === 'string' && typeof h.date === 'string' && Number.isFinite(Date.parse(h.date)))
      .map((h) => ({ patch: h.patch!, date: h.date!, ...(h.note ? { note: String(h.note) } : {}) }))
    : base.hotfixes;
  /** {"KEY": number ≥ 0}, upper-cased keys when `upper`. */
  const weights = (v: unknown, d: Record<string, number>, upper: boolean) => (v && typeof v === 'object' && !Array.isArray(v)
    ? Object.fromEntries(Object.entries(v as Record<string, unknown>).filter(([, w]) => Number.isFinite(Number(w)) && Number(w) >= 0)
      .map(([k, w]) => [upper ? k.toUpperCase() : k, Number(w)]))
    : d);
  return {
    platforms: [...new Set(platforms)] as Platform[],
    tiers: [...new Set(tiers)] as Tier[],
    divisions: divisions as Division[],
    maxLeaguePagesPerDivision: num(input.maxLeaguePagesPerDivision, base.maxLeaguePagesPerDivision),
    otpMinMasteryPoints: num(input.otpMinMasteryPoints, base.otpMinMasteryPoints),
    mainMinMasteryPoints: num(input.mainMinMasteryPoints, base.mainMinMasteryPoints),
    masteryTopCount: Math.min(10, Math.round(num(input.masteryTopCount, base.masteryTopCount))),
    pilotClasses: [...new Set(classes)] as PilotClass[],
    harvestIrelias: bool(input.harvestIrelias, base.harvestIrelias),
    historyWindow: input.historyWindow === 'newest' || input.historyWindow === 'patch' ? input.historyWindow : base.historyWindow,
    matchIdsPerPlayer: Math.min(100, num(input.matchIdsPerPlayer, base.matchIdsPerPlayer)),
    maxMatchesPerPilot: Math.min(1000, num(input.maxMatchesPerPilot, base.maxMatchesPerPilot)),
    buildSources: [...new Set(sources)] as ScrapeScope['buildSources'],
    lookupHarvestedRanks: bool(input.lookupHarvestedRanks, base.lookupHarvestedRanks),
    ownGames: bool(input.ownGames, base.ownGames),
    hotfixes,
    draftSources: [...new Set(draftSources)] as ScrapeScope['draftSources'],
    regionWeights: weights(input.regionWeights, base.regionWeights, true),
    ownRegionBoost: num(input.ownRegionBoost, base.ownRegionBoost),
    patchWeights: weights(input.patchWeights, base.patchWeights, false),
  };
}

/** scrape.config.json (user copy first, see appConfig.ts), then SCRAPE_PLATFORMS / SCRAPE_TIERS env overrides. Re-read on every call. */
export function loadScope(file = scopeFile()): ScrapeScope {
  let fromFile: Record<string, unknown> = {};
  try { fromFile = JSON.parse(fs.readFileSync(file, 'utf8')) as Record<string, unknown>; } catch { /* defaults */ }
  return parseScope({
    ...fromFile,
    ...(list(process.env.SCRAPE_PLATFORMS) ? { platforms: list(process.env.SCRAPE_PLATFORMS) } : {}),
    ...(list(process.env.SCRAPE_TIERS) ? { tiers: list(process.env.SCRAPE_TIERS) } : {}),
  });
}
