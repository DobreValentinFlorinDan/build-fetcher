import type { Lane } from '../../shared/types.ts';
import type { ChampionMeta } from '../ddragon.ts';

/** Classes that usually play a lane. Only consulted when the database has never seen any of the picks there. */
const LANE_CLASSES: Record<Lane, string[]> = { TOP: ['Fighter', 'Tank'], MIDDLE: ['Mage', 'Assassin'] };

/** A lane's shares are taken over at least this many games, so a lane with a handful can't claim a pick on one match. */
const MIN_SHARE_GAMES = 20;

const NONE = new Map<number, number>();

/**
 * The likely lane opponent among the enemy picks (ranked hides enemy roles): the pick Irelia faced most often
 * in that lane (`faced`: champion id → games), else the first pick whose main class fits the lane, else one
 * whose second class does, else the first pick.
 */
export function guessLaneOpponent(
  enemyIds: number[],
  lane: Lane,
  faced: Map<number, number>,
  champions: Map<number, Pick<ChampionMeta, 'tags'>>,
): number | null {
  const ranked = [...enemyIds].sort((a, b) => (faced.get(b) ?? 0) - (faced.get(a) ?? 0));
  if (faced.get(ranked[0])) return ranked[0];
  const classes = LANE_CLASSES[lane];
  const tags = (id: number) => champions.get(id)?.tags ?? [];
  return enemyIds.find((id) => classes.includes(tags(id)[0]))
    ?? enemyIds.find((id) => tags(id).some((t) => classes.includes(t)))
    ?? enemyIds[0] ?? null;
}

/**
 * The likely opponent for each of `lanes`, never the same pick for two lanes. Each lane guesses on its own
 * (guessLaneOpponent); the lane whose guess fits it best keeps that pick, and the other lanes guess again
 * without it. Fit is the share of the lane's games Irelia played against the pick, then whether its main class,
 * or any class, plays the lane; on a tie the earlier lane in `lanes` keeps it. A lane left without a pick gets null.
 */
export function guessLaneOpponents(
  enemyIds: number[],
  lanes: readonly Lane[],
  faced: Partial<Record<Lane, Map<number, number>>>,
  champions: Map<number, Pick<ChampionMeta, 'tags'>>,
): Partial<Record<Lane, number | null>> {
  const fit = (id: number, lane: Lane) => {
    const games = faced[lane] ?? NONE;
    const total = Math.max(MIN_SHARE_GAMES, [...games.values()].reduce((sum, n) => sum + n, 0));
    const tags = champions.get(id)?.tags ?? [];
    const classes = LANE_CLASSES[lane];
    return [(games.get(id) ?? 0) / total, classes.includes(tags[0]) ? 2 : tags.some((t) => classes.includes(t)) ? 1 : 0];
  };
  const byFit = (a: number[], b: number[]) => b[0] - a[0] || b[1] - a[1];

  const out: Partial<Record<Lane, number | null>> = {};
  let pool = [...enemyIds];
  let open = [...lanes];
  while (open.length) {
    const guesses = open
      .map((lane) => ({ lane, id: guessLaneOpponent(pool, lane, faced[lane] ?? NONE, champions) }))
      .filter((g): g is { lane: Lane; id: number } => g.id !== null);
    if (!guesses.length) break;
    const { lane, id } = guesses.sort((a, b) => byFit(fit(a.id, a.lane), fit(b.id, b.lane)))[0];   // stable: ties keep lane order
    out[lane] = id;
    pool = pool.filter((x) => x !== id);
    open = open.filter((l) => l !== lane);
  }
  for (const lane of open) out[lane] = null;
  return out;
}
