// Who counts as an Irelia player, and how much each next Riot request is expected to yield.
// Pure functions: the worker (sourcing.ts) feeds them champion-mastery-v4 answers and database rows.
import type { PilotUpdate, PlayerRow } from '../db.ts';
import type { MasteryDto } from '../riot/client.ts';
import type { ScrapeScope } from '../scrapeScope.ts';

/**
 * Classifies a player from their top masteries (best first) and, when Irelia is not among them, her own mastery
 * entry (by-champion; only fetched for players already seen playing her). Class:
 *  - otp:  Irelia is #1 with at least otpMinMasteryPoints (the definition before v3)
 *  - main: at least mainMinMasteryPoints on Irelia without being an OTP
 *  - none: anyone else
 * Whether a pilot is scanned also depends on activity (`isActive`): Irelia played since the current patch began.
 */
export function classifyPilot(
  top: MasteryDto[], irelia: MasteryDto | null, championId: number,
  scope: Pick<ScrapeScope, 'otpMinMasteryPoints' | 'mainMinMasteryPoints'>,
): PilotUpdate {
  const idx = top.findIndex((m) => m.championId === championId);
  const entry = idx >= 0 ? top[idx] : irelia?.championId === championId ? irelia : null;
  const points = entry?.championPoints ?? 0;
  const rank = idx >= 0 ? idx + 1 : null;
  const pilotClass = rank === 1 && points >= scope.otpMinMasteryPoints ? 'otp'
    : points >= scope.mainMinMasteryPoints ? 'main' : 'none';
  return { pilotClass, ireliaPoints: points, ireliaLastPlay: entry?.lastPlayTime ?? null, ireliaRank: rank };
}

/** Played Irelia since `since` (the current patch's start on their platform). Unknown activity counts as active. */
export const isActive = (p: Pick<PlayerRow, 'irelia_last_play'>, since: number) =>
  p.irelia_last_play === null || p.irelia_last_play === undefined || p.irelia_last_play >= since;

// ------------------------------------------------------------------------------------------- expected yield

/** Prior share of a pilot's ranked games that are Irelia games we keep, by how they were found. */
const PRIOR_IRELIA_SHARE: Record<string, number> = { otp: 0.7, main: 0.35, harvested: 0.3, me: 0.5 };
const PRIOR_WEIGHT = 5;   // the prior counts like this many matches of the pilot's own history
/** Prior ranked games per hour of a pilot (about 6 a day). */
const PRIOR_GAMES_PER_HOUR = 0.25;
/** Expected new matches the first time a pilot's history is read this patch. */
const FIRST_SCAN_MATCHES = 30;

/** Expected share of this pilot's matches that are Irelia games, shrunk toward the prior of their class. */
export function ireliaShare(p: Pick<PlayerRow, 'history_matches' | 'history_irelia' | 'pilot_class'>, source = p.pilot_class ?? 'otp') {
  const prior = PRIOR_IRELIA_SHARE[source] ?? PRIOR_IRELIA_SHARE.harvested;
  return ((p.history_irelia ?? 0) + prior * PRIOR_WEIGHT) / ((p.history_matches ?? 0) + PRIOR_WEIGHT);
}

/**
 * Expected Irelia games per API call of fetching one listed match: it costs the match, plus a timeline when it is
 * an Irelia game (share p), and yields p games.
 */
export const matchScore = (p: number) => p / (1 + p);

/** A harvested game's match is already on disk: one timeline call, one Irelia game. */
export const HARVEST_SCORE = 1;
/** The user's own games come first on their regional host (a few dozen calls a patch). */
export const OWN_GAMES_SCORE = 2;

/**
 * Expected Irelia games per API call of reading a pilot's match history now: one id call reveals g new matches,
 * which then cost g × (1 + p) calls and yield g × p games. g grows with the time since the last scan.
 */
export function historyScore(p: PlayerRow, now: number, maxMatches: number, source = p.pilot_class ?? 'otp') {
  const share = ireliaShare(p, source);
  const g = p.last_match_scan
    ? Math.min(maxMatches, Math.max(0.5, ((now - p.last_match_scan) / 3600_000) * PRIOR_GAMES_PER_HOUR))
    : Math.min(maxMatches, FIRST_SCAN_MATCHES);
  return (share * g) / (1 + g * (1 + share));
}
