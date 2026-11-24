import { LANE_LABEL, LANES, toLane } from '../../shared/lanes.ts';
import type { BuildRecommendation, Lane, Matchup } from '../../shared/types.ts';

/**
 * The lane an older version saved in this browser (the server keeps it now): read once, then forgotten. The dashboard
 * hands it to the server so an update keeps the lane you were on.
 */
export function takeLegacyLane(): Lane | null {
  try {
    const lane = toLane(localStorage.getItem('lane'));
    localStorage.removeItem('lane');
    return lane;
  } catch { return null; }
}

/** Where the lane comes from, in words: "from champ select", "set by hand (in game: Top)". */
export function laneSource(m: Matchup): string {
  if (m.laneFrom === 'manual' && m.detectedLane) return `set by hand (${m.phase === 'game' ? 'in game' : 'champ select'}: ${LANE_LABEL[m.detectedLane]})`;
  return m.laneFrom === 'game' ? 'from the game' : m.laneFrom === 'draft' ? 'from champ select' : 'your choice';
}

/** "Only 8 Mid games: blended with Top games at reduced weight." when the build leans on the other lane. */
export function laneNote(rec: BuildRecommendation): string | null {
  const lane = rec.lane;
  if (!lane || !rec.sample.usedLaneFallback) return null;
  const other = LANES.filter((l) => l !== lane).map((l) => LANE_LABEL[l]).join(' / ');
  const n = rec.sample.laneGames;
  if (!rec.sample.otherLaneGames) return n ? `Only ${n} ${LANE_LABEL[lane]} games so far.` : `No ${LANE_LABEL[lane]} games yet.`;
  return n
    ? `Only ${n} ${LANE_LABEL[lane]} games: blended with ${other} games at reduced weight.`
    : `No ${LANE_LABEL[lane]} games yet: built from ${other} games at reduced weight.`;
}

export { LANE_LABEL, LANES };
