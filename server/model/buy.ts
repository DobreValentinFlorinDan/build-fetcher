// Stage C3: the best purchase for your current gold, toward the next completed item. Shop advice only: what to buy
// with the gold you have, never when to go back or what to do. docs/BUILD-SOURCING.md §10.
//
// With enough gold, the item itself. Otherwise its missing parts (its recipe, minus what you own) that you can afford,
// in the order high-elo Irelias of this lane bought them in the stored games: for each game that finished the item,
// the first of the still-missing parts it bought is that game's choice, and games against your lane opponent count
// three times. The most common choice wins, unless another one did clearly better than the game state predicted
// (win-probability-added, the same measure as the draft model's). The rest of your gold goes to the next parts.
import type { BestBuy, Lane } from '../../shared/types.ts';
import type { StaticData } from '../ddragon.ts';
import type { ScoredGame } from './dataset.ts';
import { STATE_FEATURES, winProbabilityOf, type StateModel } from './state.ts';

/** A choice must lead by this much WPA, both on this many games, to beat the more common one. */
const WPA_LEAD = 0.02;
const WPA_MIN_GAMES = 30;
const MATCHUP_WEIGHT = 3;

/** One stored game that finished `target`: when it first bought each part of it, and the chance to win then. */
interface Path { lane: Lane; opponent: number | null; win: number; parts: { id: number; p: number }[] }

/** Every part of an item's recipe, recursively (repeats kept: two Long Swords are two parts). */
function partsOf(data: StaticData, id: number): number[] {
  return (data.items.get(id)?.from ?? []).flatMap((c) => [c, ...partsOf(data, c)]);
}

/**
 * A node of the target's recipe tree you don't own: its price now (its combine cost plus its missing parts) and its
 * missing children. Owned items cover the first matching node, each owned item once.
 */
interface Missing { id: number; cost: number; children: Missing[] }

export function missingTree(data: StaticData, target: number, owned: number[]): Missing | null {
  const pool = [...owned];
  const walk = (id: number): Missing | null => {
    const at = pool.indexOf(id);
    if (at >= 0) { pool.splice(at, 1); return null; }
    const item = data.items.get(id);
    const from = item?.from ?? [];
    const combine = (item?.goldTotal ?? 0) - from.reduce((s, c) => s + (data.items.get(c)?.goldTotal ?? 0), 0);
    const children = from.map(walk).filter((m): m is Missing => m !== null);
    return { id, cost: combine + children.reduce((s, c) => s + c.cost, 0), children };
  };
  return walk(target);
}

/** Every missing node below the root, each with its ancestors and descendants (bought parts exclude both). */
function candidates(root: Missing) {
  const out: { node: Missing; related: Set<Missing> }[] = [];
  const below = (n: Missing): Missing[] => n.children.flatMap((c) => [c, ...below(c)]);
  const visit = (n: Missing, ancestors: Missing[]) => {
    for (const c of n.children) {
      out.push({ node: c, related: new Set([...ancestors, n, ...below(c)].filter((x) => x !== root)) });
      visit(c, [...ancestors, n].filter((x) => x !== root));
    }
  };
  visit(root, []);
  return out;
}

/** The stored paths to each target item, built once per model (the live poll asks every second). */
export class BuyIndex {
  private paths = new Map<number, Path[]>();

  constructor(private scored: ScoredGame[], private state: StateModel | null, private data: StaticData) {}

  private pathsTo(target: number): Path[] {
    let paths = this.paths.get(target);
    if (paths) return paths;
    const parts = new Set(partsOf(this.data, target));
    const width = STATE_FEATURES.length;
    paths = [];
    for (const { g } of this.scored) {
      if (g.source === 'me') continue;
      const done = g.items.find((c) => c.item === target);
      if (!done) continue;
      // The chance to win just before each purchase: the last state-model frame before it (frames start at 1:00).
      const pAt = (ts: number) => {
        const i = Math.floor(ts / 60_000) - 1;
        if (!this.state || i < 0) return this.state?.base ?? 0.5;
        if (g.pAt) return i < g.pAt.length ? g.pAt[i] : this.state.base;
        if (!g.frames || (i + 1) * width > g.frames.length) return this.state.base;
        return winProbabilityOf(this.state, g.frames.subarray(i * width, (i + 1) * width));
      };
      // Its parts in the order they were first bought (by time: the stored list need not be sorted), before it was done.
      const seen = new Set<number>();
      const bought: Path['parts'] = [];
      for (const [id, ts] of [...g.purchases].filter(([, ts]) => ts <= done.t).sort((a, b) => a[1] - b[1])) {
        if (parts.has(id) && !seen.has(id)) { seen.add(id); bought.push({ id, p: pAt(ts) }); }
      }
      paths.push({ lane: g.lane, opponent: g.opponent, win: g.win, parts: bought });
    }
    this.paths.set(target, paths);
    return paths;
  }

  /** The best purchase for `gold` toward `target`, or null when nothing of it is affordable (or it is owned). */
  best(q: { lane: Lane; opponent: number | null; target: number; owned: number[]; gold: number }): BestBuy | null {
    const name = (id: number) => this.data.items.get(id)?.name ?? `#${id}`;
    const root = missingTree(this.data, q.target, q.owned);
    if (!root) return null;
    const gold = Math.floor(q.gold);
    if (root.cost <= gold) {
      return { target: q.target, items: [q.target], cost: root.cost, gold, finishes: true, games: 0, share: null, lift: null,
        reason: `Enough gold to finish ${name(q.target)}` };
    }
    const paths = this.pathsTo(q.target).filter((p) => p.lane === q.lane);
    let open = candidates(root);
    const items: number[] = [];
    let left = gold, first: Pick<BestBuy, 'games' | 'share' | 'lift' | 'reason'> | null = null;

    while (open.length) {
      const affordable = open.filter((c) => c.node.cost <= left);
      if (!affordable.length) break;
      // Each game's choice: the first of the still-open parts it bought.
      const ids = new Set(open.map((c) => c.node.id));
      // w: the weighted vote (matchup games count three times) that ranks; n and m: plain game counts for the text.
      const votes = new Map<number, { w: number; n: number; m: number; wpa: number }>();
      let games = 0, matchupGames = 0;
      for (const p of paths) {
        const choice = p.parts.find((x) => ids.has(x.id));
        if (!choice) continue;
        const vsOpponent = q.opponent !== null && p.opponent === q.opponent;
        const v = votes.get(choice.id) ?? { w: 0, n: 0, m: 0, wpa: 0 };
        v.w += vsOpponent ? MATCHUP_WEIGHT : 1; v.n += 1; v.m += vsOpponent ? 1 : 0; v.wpa += p.win - choice.p;
        votes.set(choice.id, v);
        games++;
        if (vsOpponent) matchupGames++;
      }
      const score = (c: typeof open[number]) => votes.get(c.node.id)?.w ?? 0;
      const ranked = [...affordable].sort((a, b) => score(b) - score(a) || b.node.cost - a.node.cost);
      let pick = ranked[0];
      const mean = (id: number) => { const v = votes.get(id); return v && v.n ? v.wpa / v.n : 0; };
      const pv = votes.get(pick.node.id);
      const better = ranked.find((c) => {
        const v = votes.get(c.node.id);
        return c !== pick && v && pv && v.n >= WPA_MIN_GAMES && pv.n >= WPA_MIN_GAMES && mean(c.node.id) - mean(pick.node.id) >= WPA_LEAD;
      });
      if (better) pick = better;

      if (!first) {
        const v = votes.get(pick.node.id);
        const vsText = v && matchupGames ? ` (${v.m} of ${matchupGames} against your opponent)` : '';
        first = !v ? { games: 0, share: null, lift: null, reason: `No stored games show an order here: ${name(pick.node.id)} spends the most of your gold` }
          : better ? { games, share: v.n / games, lift: mean(pick.node.id),
            reason: `Bought first less often than ${name(ranked[0].node.id)}, but those games went better (estimate)` }
          : { games, share: v.n / games, lift: mean(pick.node.id),
            reason: `${Math.round((v.n / games) * 100)}% of ${games} high-elo games buy ${name(pick.node.id)} first toward ${name(q.target)}${vsText}` };
      }
      items.push(pick.node.id);
      left -= pick.node.cost;
      open = open.filter((c) => c !== pick && !pick.related.has(c.node));
    }
    if (!items.length || !first) return null;
    return { target: q.target, items, cost: gold - left, gold, finishes: false, ...first };
  }
}
