// Stage C2: how a matchup tends to go, from the stored high-elo games of this lane against this opponent: the gold
// difference at 10 and 15 minutes (timelines) and the win rate by game length, each with how many games it rests on.
// Shown before the game only (champ select and the dashboard), never in game. docs/BUILD-SOURCING.md §10.
import type { GoldAt, Lane, MatchupBest, MatchupFacts, MatchupOption } from '../../shared/types.ts';
import type { PreparedGame, ScoredGame } from './dataset.ts';

/** Game-length buckets in minutes; the last one is open-ended. */
export const LENGTH_BUCKETS: [number, number | null][] = [[0, 25], [25, 30], [30, 35], [35, null]];

const median = (xs: number[]) => {
  const s = [...xs].sort((a, b) => a - b);
  return s.length % 2 ? s[(s.length - 1) / 2] : (s[s.length / 2 - 1] + s[s.length / 2]) / 2;
};

function goldAt(values: (number | null)[]): GoldAt | null {
  const xs = values.filter((v): v is number => v !== null);
  return xs.length ? { median: Math.round(median(xs)), ahead: xs.filter((x) => x > 0).length / xs.length, games: xs.length } : null;
}

/** Null without an opponent or with fewer than `minGames` games against it in this lane (your own games never count). */
export function matchupFacts(games: PreparedGame[], lane: Lane, opponent: number | null, minGames = 3): MatchupFacts | null {
  if (opponent === null) return null;
  const vs = games.filter((g) => g.source !== 'me' && g.lane === lane && g.opponent === opponent);
  if (vs.length < minGames) return null;
  const rate = (gs: PreparedGame[]) => (gs.length ? gs.filter((g) => g.win).length / gs.length : 0);
  return {
    lane, opponentId: opponent, games: vs.length, pilots: new Set(vs.map((g) => g.pilot)).size, winRate: rate(vs),
    gold10: goldAt(vs.map((g) => g.lane10)), gold15: goldAt(vs.map((g) => g.lane15)),
    byLength: LENGTH_BUCKETS.map(([from, to]) => {
      const inside = vs.filter((g) => g.duration / 60 >= from && (to === null || g.duration / 60 < to));
      return { from, to, games: inside.length, winRate: rate(inside) };
    }).filter((b) => b.games > 0),
  };
}

/** Fewer games against the opponent than this: no "did best" (too noisy). An option needs BEST_MIN_OPTION of its own. */
export const BEST_MIN_MATCHUP = 15;
export const BEST_MIN_OPTION = 5;
/** Prior games towards 0: an option's lift is ΣWPA / (games + 20), so a few lucky games can't top the list. */
export const BEST_PRIOR = 20;

/**
 * The start (one) and the first completed items (up to two) that did best against `opponent` in this lane: each
 * option's win-probability-added in these games (pre-game for the start, from the state when it was bought for the
 * item: ScoredGame), shrunk towards 0, best first. Only options you can still buy (`buyable`); your own games never
 * count. Null without an opponent or with fewer than BEST_MIN_MATCHUP games.
 */
export function matchupBest(scored: ScoredGame[], lane: Lane, opponent: number | null, buyable: (id: number) => boolean): MatchupBest | null {
  if (opponent === null) return null;
  const vs = scored.filter((s) => s.g.source !== 'me' && s.g.lane === lane && s.g.opponent === opponent);
  if (vs.length < BEST_MIN_MATCHUP) return null;
  function rank<T>(rows: { key: string; value: T; wpa: number }[]): MatchupOption<T>[] {
    const by = new Map<string, { value: T; n: number; sum: number }>();
    for (const r of rows) {
      const e = by.get(r.key) ?? { value: r.value, n: 0, sum: 0 };
      e.n++; e.sum += r.wpa;
      by.set(r.key, e);
    }
    return [...by.values()].filter((e) => e.n >= BEST_MIN_OPTION)
      .map((e) => ({ value: e.value, games: e.n, lift: e.sum / (e.n + BEST_PRIOR) }))
      .sort((a, b) => b.lift - a.lift || b.games - a.games);
  }
  const starts = rank(vs.filter((s) => s.g.start && s.g.start.split(',').every((id) => buyable(Number(id))))
    .map((s) => ({ key: s.g.start, value: s.g.start.split(',').map(Number), wpa: s.wpa0 })));
  const firsts = rank(vs.filter((s) => s.g.items[0] && buyable(s.g.items[0].item))
    .map((s) => ({ key: String(s.g.items[0].item), value: s.g.items[0].item, wpa: s.itemWpa[0] })));
  if (!starts.length && !firsts.length) return null;
  return { start: starts[0] ?? null, first: firsts.slice(0, 2) };
}
