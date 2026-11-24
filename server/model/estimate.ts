// Scores the options of one decision by their mean win-probability-added, shrunk in layers so thin samples lean on
// broader ones: both lanes → this lane → games with a similar context (kernel-weighted) → this exact matchup. Each
// layer moves toward its own data by n / (n + k), with n the effective sample size (similarity counted as a fraction
// of a game).
import type { Lane } from '../../shared/types.ts';
import { kernel, kernelAt, type Contexts } from './context.ts';

export interface Observation {
  option: string;
  wpa: number;
  win: number;
  /** Pilot × region × patch weight, after the pilot cap. */
  w: number;
  lane: Lane;
  opponent: number | null;
  pilot: string;
  /** Standardised context (NaN where unknown)... */
  z?: ArrayLike<number>;
  /** ...or row `zRow` of `zs` (a draft model's contexts, 0.14.0: no array per observation). */
  zs?: Contexts;
  zRow?: number;
  /** The game's index in the draft model, for similarities the caller computed once (Query.sims). */
  row?: number;
  /** Patches since the game's (0: its region's current patch), for patch memory's data check (weights.ts consistent). */
  age?: number;
}

export interface EstimateOptions {
  /** Kernel bandwidth in standard deviations; Infinity turns the context layer off. */
  h: number;
  /** Prior strength of each layer, in games. */
  k: number;
  /** Options need this many games in the lane... */
  minGames: number;
  /** ...and this share of the lane's weight, to be offered. */
  minShare: number;
}

export const ESTIMATE_DEFAULTS: EstimateOptions = { h: 0.7, k: 100, minGames: 5, minShare: 0.03 };

export interface Query {
  lane: Lane;
  opponent: number | null;
  z: ArrayLike<number>;
  /**
   * Each game's similarity to this query (by Observation.row) at bandwidth `h`, when the caller computed it once for
   * many decisions: recommendDraft's ~20 decisions share one. Used for the full kernel at that bandwidth only.
   */
  sims?: {
    h: number; values: ArrayLike<number>;
    /**
     * Grouped drafts (0.20.0, model/groups.ts; an evaluation trial): each game's group (by row) and each group's
     * similarity, `values` holding every game's group's. The sums then add up per group (cellSums).
     */
    groups?: { of: Int32Array; sims: Float64Array };
  };
  /**
   * The sums already added up for this query (same lane, opponent, context and bandwidth), by observations: the
   * evaluation tries three priors per bandwidth on each held-out game, and the prior doesn't change them.
   */
  memo?: Map<Observation[], unknown>;
}

export interface OptionEstimate {
  option: string;
  /** Shrunk mean WPA: how much more often than expected games with this option were won. */
  theta: number;
  low: number;
  high: number;
  /** The lane-level estimate (no context, no matchup): what the pick is compared against for reasons. */
  laneTheta: number;
  /** Without the matchup layer. */
  contextTheta: number;
  nEff: number;          // effective games behind it in the context layer
  matchupGames: number;
  games: number;         // games in the lane with this option
  pilots: number;
  winRate: number;       // context-weighted raw win rate
}

export interface DecisionEstimate {
  options: OptionEstimate[];
  /** Effective games of the whole decision in the context layer (under 30: too thin to adjust). */
  nEff: number;
  sigma: number;
}

/**
 * Weighted sums. `k` is a similarity in [0, 1] counted as a fraction of a game: the effective size (Σwk)² / Σw²k
 * shrinks when a draft is unlike every stored game, where a plain relative weight would not.
 */
class Acc {
  sw = 0; sw2 = 0; swx = 0; swin = 0; n = 0;
  add(w: number, x: number, win: number, k = 1) {
    if (w <= 0 || k <= 0) return;
    const wk = w * k;
    this.sw += wk; this.sw2 += w * wk; this.swx += wk * x; this.swin += wk * win; this.n++;
  }
  get mean() { return this.sw > 0 ? this.swx / this.sw : 0; }
  get nEff() { return this.sw2 > 0 ? (this.sw * this.sw) / this.sw2 : 0; }
}

const towards = (a: { nEff: number; mean: number }, prior: number, k: number) => (a.nEff * a.mean + k * prior) / (a.nEff + k);

/** Whether any context dimension of a query is known. */
function anyKnown(z: ArrayLike<number>) {
  for (let i = 0; i < z.length; i++) if (!Number.isNaN(z[i])) return true;
  return false;
}

/**
 * One lane's observations, indexed once: the sums that don't depend on the query (all lanes, this lane, pilots) and
 * the observations as flat arrays, so each query is one tight loop over them. A decision is estimated for every
 * held-out game of an evaluation and every live poll, on the same observations.
 */
interface LaneIndex {
  /** obs.length when indexed: a grown array is indexed again. */
  size: number;
  /** This lane's options, in the order first seen. */
  options: string[];
  all: Acc[];
  lane: Acc[];
  pilots: number[];
  /** This lane's observations, in order: option index, weight, WPA, win, opponent, context, game (-1: none). */
  opt: Int32Array;
  w: Float64Array;
  wpa: Float64Array;
  win: Float64Array;
  opponent: (number | null)[];
  /** The same opponents as numbers (-1: none), for the fast loop. */
  opp: Int32Array;
  /** Each observation's context: its array, or its row (zRow ≥ 0) of a model's contexts. */
  z: (ArrayLike<number> | Contexts)[];
  zRow: Int32Array;
  row: Int32Array;
  /** Every observation has its game's row (its similarity can come from Query.sims). */
  allRows: boolean;
  /** What doesn't depend on the query, summed in observation order as the loop would: all options' weight and WPA spread. */
  laneWeight: number;
  sq: number;
  sqW: number;
  /** Without context every similarity is 1: the context sums are the lane's, and their totals these. */
  tsw: number;
  tsw2: number;
  /** Without context, the matchup's sums per opponent (computed on first use). */
  byOpponent: Map<number, MatchupSums>;
  /**
   * The same observations grouped by option, in order within each (option k's are `start[k]` to `start[k + 1]`): each
   * option's sums then add up in local variables, in the order the loop above would add them.
   */
  start: Int32Array;
  gw: Float64Array;
  gwpa: Float64Array;
  gwin: Float64Array;
  gopp: Int32Array;
  grow: Int32Array;
  /** Grouped drafts: the cells for one grouping (cellsOf), built on first use. */
  cells?: Cells;
}

/**
 * Grouped drafts (0.20.0, model/groups.ts): a lane's observations as cells, per option one per group present, holding
 * the sums of their observations with a positive weight; the same observations by option and opponent, for the exact
 * matchup layer; and per group the context total's sums. Built once per lane index and grouping.
 */
interface Cells {
  of: Int32Array;
  /** Option k's cells are cellStart[k] to cellStart[k + 1]. */
  cellStart: Int32Array;
  cellGroup: Int32Array;
  sw: Float64Array; sw2: Float64Array; swx: Float64Array; swin: Float64Array;
  /** Option k against one opponent: positions (in the grouped arrays) from..to of `matchup`, by key k·2^16 + opponent. */
  matchup: Int32Array;
  byOpponent: Map<number, [number, number]>;
  /** Per group: Σw and Σw² over the lane's observations with a positive weight. */
  tw: Float64Array; tw2: Float64Array;
}

function cellsOf(idx: LaneIndex, of: Int32Array): Cells {
  if (idx.cells?.of === of) return idx.cells;
  const n = idx.options.length;
  let groups = 0;
  for (let p = 0; p < idx.grow.length; p++) groups = Math.max(groups, of[idx.grow[p]] + 1);
  const cellStart = new Int32Array(n + 1);
  const cellGroup: number[] = [], sw: number[] = [], sw2: number[] = [], swx: number[] = [], swin: number[] = [];
  const at = new Int32Array(groups).fill(-1);
  const tw = new Float64Array(groups), tw2 = new Float64Array(groups);
  const keys: number[] = [], pos: number[][] = [];
  const keyAt = new Map<number, number>();
  for (let k = 0; k < n; k++) {
    const touched: number[] = [];
    for (let p = idx.start[k]; p < idx.start[k + 1]; p++) {
      const opp = idx.gopp[p];
      if (opp >= 0) {
        const key = k * 65536 + opp;
        let x = keyAt.get(key);
        if (x === undefined) { x = keys.length; keyAt.set(key, x); keys.push(key); pos.push([]); }
        pos[x].push(p);
      }
      const w = idx.gw[p];
      if (w <= 0) continue;
      const g = of[idx.grow[p]];
      let c = at[g];
      if (c < 0) { c = at[g] = cellGroup.length; touched.push(g); cellGroup.push(g); sw.push(0); sw2.push(0); swx.push(0); swin.push(0); }
      sw[c] += w; sw2[c] += w * w; swx[c] += w * idx.gwpa[p]; swin[c] += w * idx.gwin[p];
      tw[g] += w; tw2[g] += w * w;
    }
    for (const g of touched) at[g] = -1;
    cellStart[k + 1] = cellGroup.length;
  }
  const matchup = new Int32Array(pos.reduce((t, l) => t + l.length, 0));
  const byOpponent = new Map<number, [number, number]>();
  let to = 0;
  keys.forEach((key, x) => { const from = to; for (const p of pos[x]) matchup[to++] = p; byOpponent.set(key, [from, to]); });
  idx.cells = {
    of, cellStart, cellGroup: Int32Array.from(cellGroup), sw: Float64Array.from(sw), sw2: Float64Array.from(sw2),
    swx: Float64Array.from(swx), swin: Float64Array.from(swin), matchup, byOpponent, tw, tw2,
  };
  return idx.cells;
}

/**
 * The sums querySums adds up, from the cells: a group's similarity times its cell's sums for the context layer, the
 * matchup's observations one by one (with their group's similarity), and the context total per group.
 */
function cellSums(idx: LaneIndex, groups: { of: Int32Array; sims: Float64Array }, opponent: number, o: GroupedOut): { tsw: number; tsw2: number } {
  const cells = cellsOf(idx, groups.of), gs = groups.sims;
  const n = idx.options.length;
  for (let k = 0; k < n; k++) {
    let sw = 0, sw2 = 0, swx = 0, swin = 0;
    for (let c = cells.cellStart[k]; c < cells.cellStart[k + 1]; c++) {
      const sim = gs[cells.cellGroup[c]];
      if (sim <= 0) continue;
      sw += sim * cells.sw[c]; sw2 += sim * cells.sw2[c]; swx += sim * cells.swx[c]; swin += sim * cells.swin[c];
    }
    o.csw[k] = sw; o.csw2[k] = sw2; o.cswx[k] = swx; o.cswin[k] = swin;
    let ms = 0, ms2 = 0, msx = 0, mswn = 0, m = 0, seen = 0;
    const range = opponent >= 0 ? cells.byOpponent.get(k * 65536 + opponent) : undefined;
    if (range) {
      seen = 1;
      for (let x = range[0]; x < range[1]; x++) {
        const p = cells.matchup[x], w = idx.gw[p], sim = gs[groups.of[idx.grow[p]]];
        if (w <= 0 || sim <= 0) continue;
        const wk = w * sim, wpa = idx.gwpa[p], win = idx.gwin[p];
        ms += wk; ms2 += w * wk; msx += wk * wpa; mswn += wk * win; m++;
      }
    }
    o.msw[k] = ms; o.msw2[k] = ms2; o.mswx[k] = msx; o.mswin[k] = mswn; o.mn[k] = m; o.mseen[k] = seen;
  }
  let tsw = 0, tsw2 = 0;
  for (let g = 0; g < cells.tw.length; g++) { const sim = gs[g]; if (sim > 0) { tsw += sim * cells.tw[g]; tsw2 += sim * cells.tw2[g]; } }
  return { tsw, tsw2 };
}

/** Per option: weighted sums of one opponent's games (as the loop accumulates them), how many, and whether any. */
interface MatchupSums { sw: Float64Array; sw2: Float64Array; swx: Float64Array; swin: Float64Array; n: Int32Array; seen: Uint8Array }

const indexes = new WeakMap<Observation[], Map<Lane, LaneIndex>>();

function laneIndex(obs: Observation[], lane: Lane): LaneIndex {
  let byLane = indexes.get(obs);
  if (!byLane) { byLane = new Map(); indexes.set(obs, byLane); }
  const cached = byLane.get(lane);
  if (cached && cached.size === obs.length) return cached;
  const allOf = new Map<string, Acc>();
  for (const ob of obs) {
    let a = allOf.get(ob.option);
    if (!a) { a = new Acc(); allOf.set(ob.option, a); }
    a.add(ob.w, ob.wpa, ob.win);
  }
  const inLane = obs.filter((ob) => ob.lane === lane);
  const at = new Map<string, number>();
  const idx: LaneIndex = {
    size: obs.length, options: [], all: [], lane: [], pilots: [],
    opt: new Int32Array(inLane.length), w: new Float64Array(inLane.length), wpa: new Float64Array(inLane.length),
    win: new Float64Array(inLane.length), opponent: new Array(inLane.length), opp: new Int32Array(inLane.length),
    z: new Array(inLane.length), zRow: new Int32Array(inLane.length), row: new Int32Array(inLane.length), allRows: true,
    laneWeight: 0, sq: 0, sqW: 0, tsw: 0, tsw2: 0, byOpponent: new Map(),
    start: new Int32Array(0), gw: new Float64Array(0), gwpa: new Float64Array(0), gwin: new Float64Array(0), gopp: new Int32Array(0), grow: new Int32Array(0),
  };
  const pilots: Set<string>[] = [];
  inLane.forEach((ob, j) => {
    let k = at.get(ob.option);
    if (k === undefined) {
      k = idx.options.length;
      at.set(ob.option, k);
      idx.options.push(ob.option); idx.all.push(allOf.get(ob.option)!); idx.lane.push(new Acc()); pilots.push(new Set());
    }
    idx.lane[k].add(ob.w, ob.wpa, ob.win);
    pilots[k].add(ob.pilot);
    idx.opt[j] = k; idx.w[j] = ob.w; idx.wpa[j] = ob.wpa; idx.win[j] = ob.win; idx.opponent[j] = ob.opponent;
    if (ob.zs) { idx.z[j] = ob.zs; idx.zRow[j] = ob.zRow!; } else { idx.z[j] = ob.z ?? []; idx.zRow[j] = -1; }
    idx.opp[j] = ob.opponent ?? -1;
    idx.row[j] = ob.row ?? -1;
    if (idx.row[j] < 0) idx.allRows = false;
    const w = ob.w, wpa = ob.wpa;
    idx.laneWeight += w;
    idx.sq += w * wpa * wpa; idx.sqW += w;
    if (w > 0) { idx.tsw += w; idx.tsw2 += w * w; }
  });
  idx.pilots = pilots.map((p) => p.size);
  // Grouped by option, in order within each.
  const n = idx.options.length, len = inLane.length;
  idx.start = new Int32Array(n + 1);
  for (let j = 0; j < len; j++) idx.start[idx.opt[j] + 1]++;
  for (let k = 0; k < n; k++) idx.start[k + 1] += idx.start[k];
  const next = idx.start.slice(0, n);
  idx.gw = new Float64Array(len); idx.gwpa = new Float64Array(len); idx.gwin = new Float64Array(len);
  idx.gopp = new Int32Array(len); idx.grow = new Int32Array(len);
  for (let j = 0; j < len; j++) {
    const at = next[idx.opt[j]]++;
    idx.gw[at] = idx.w[j]; idx.gwpa[at] = idx.wpa[j]; idx.gwin[at] = idx.win[j]; idx.gopp[at] = idx.opp[j]; idx.grow[at] = idx.row[j];
  }
  byLane.set(lane, idx);
  return idx;
}

/**
 * Estimates for every option of a decision given `obs`. `only` limits the work to some options and `dim` the kernel to
 * one context dimension (the reason search). `obs` is indexed on first use: don't change the array afterwards (other
 * than growing it, which re-indexes).
 */
/** The sums a query adds up: per option in the context layer and the matchup's, and the context total. */
interface Sums {
  c: { sw: Float64Array; sw2: Float64Array; swx: Float64Array; swin: Float64Array };
  m: MatchupSums;
  tsw: number; tsw2: number;
  laneWeight: number; sq: number; sqW: number;
}

const emptySums = (n: number): MatchupSums => ({
  sw: new Float64Array(n), sw2: new Float64Array(n), swx: new Float64Array(n), swin: new Float64Array(n), n: new Int32Array(n), seen: new Uint8Array(n),
});

/** Without context: one opponent's matchup sums, accumulated in observation order exactly as the general loop does. */
function opponentSums(idx: LaneIndex, opponent: number): MatchupSums {
  let m = idx.byOpponent.get(opponent);
  if (m) return m;
  m = emptySums(idx.options.length);
  for (let j = 0; j < idx.opt.length; j++) {
    if (idx.opp[j] !== opponent) continue;
    const k = idx.opt[j];
    m.seen[k] = 1;
    const w = idx.w[j];
    if (w <= 0) continue;
    const wk = w * 1, wpa = idx.wpa[j], win = idx.win[j];
    m.sw[k] += wk; m.sw2[k] += w * wk; m.swx[k] += wk * wpa; m.swin[k] += wk * win; m.n[k]++;
  }
  idx.byOpponent.set(opponent, m);
  return m;
}

const totals = new Float64Array(2);

/**
 * The loop every held-out game of an evaluation runs for each decision: each observation weighted by its game's
 * similarity to the query, summed per option and for the matchup (`opponent`; -2 for none, never an observation's -1).
 * Option by option over the grouped arrays, so each sum stays in a local variable: the same additions in the same
 * order as one pass in observation order, several times faster (no sum waits on the one before it in memory).
 */
function groupedLoop(start: Int32Array, W: Float64Array, WPA: Float64Array, WIN: Float64Array, opp: Int32Array, row: Int32Array,
  sims: Float64Array, opponent: number, csw: Float64Array, csw2: Float64Array, cswx: Float64Array, cswin: Float64Array,
  msw: Float64Array, msw2: Float64Array, mswx: Float64Array, mswin: Float64Array, mn: Int32Array, mseen: Uint8Array) {
  for (let k = 0; k + 1 < start.length; k++) {
    let sw = 0, sw2 = 0, swx = 0, swin = 0, ms = 0, ms2 = 0, msx = 0, mswn = 0, n = 0, seen = 0;
    for (let p = start[k]; p < start[k + 1]; p++) {
      const w = W[p], sim = sims[row[p]];
      // The exact matchup, in drafts like this one: the most specific layer is a subset of the similar games.
      const matchup = opp[p] === opponent;
      if (matchup) seen = 1;
      if (w <= 0 || sim <= 0) continue;
      const wk = w * sim, wpa = WPA[p], win = WIN[p];
      sw += wk; sw2 += w * wk; swx += wk * wpa; swin += wk * win;
      if (matchup) { ms += wk; ms2 += w * wk; msx += wk * wpa; mswn += wk * win; n++; }
    }
    csw[k] = sw; csw2[k] = sw2; cswx[k] = swx; cswin[k] = swin;
    msw[k] = ms; msw2[k] = ms2; mswx[k] = msx; mswin[k] = mswn; mn[k] = n; mseen[k] = seen;
  }
}

/** The arrays groupedLoop reads: a lane's observations grouped by option (LaneIndex has them). */
export interface GroupedArrays {
  start: Int32Array; gw: Float64Array; gwpa: Float64Array; gwin: Float64Array; gopp: Int32Array; grow: Int32Array;
}
/** What groupedLoop fills, per option: the sums over the draft's similar games (c…), and over the exact matchup (m…). */
export interface GroupedOut {
  csw: Float64Array; csw2: Float64Array; cswx: Float64Array; cswin: Float64Array;
  msw: Float64Array; msw2: Float64Array; mswx: Float64Array; mswin: Float64Array; mn: Int32Array; mseen: Uint8Array;
}
export const groupedOut = (k: number): GroupedOut => ({
  csw: new Float64Array(k), csw2: new Float64Array(k), cswx: new Float64Array(k), cswin: new Float64Array(k),
  msw: new Float64Array(k), msw2: new Float64Array(k), mswx: new Float64Array(k), mswin: new Float64Array(k), mn: new Int32Array(k), mseen: new Uint8Array(k),
});
/** groupedLoop on the processor, over a lane's grouped arrays: what the GPU's sums are checked against. */
export function groupedSums(g: GroupedArrays, sims: Float64Array, opponent: number, o: GroupedOut) {
  groupedLoop(g.start, g.gw, g.gwpa, g.gwin, g.gopp, g.grow, sims, opponent, o.csw, o.csw2, o.cswx, o.cswin, o.msw, o.msw2, o.mswx, o.mswin, o.mn, o.mseen);
}

/**
 * Use GPU (0.19.0, server/gpu/): the sums groupedLoop adds up, one thread per option, in the same order and with each
 * product and sum rounded on its own, so the same numbers. `run` fills `out` (true) or leaves it to the processor
 * (false). One call in SUMS_VERIFY_EVERY is also worked out here and compared bit for bit: a difference stops the
 * GPU's sums for the rest of the evaluation (`failed`) and the processor's numbers are used.
 */
export interface SumsAccelerator {
  run(g: GroupedArrays, sims: Float64Array, opponent: number, out: GroupedOut): boolean;
  failed(detail: string): void;
}
export const SUMS_VERIFY_EVERY = 500;
let sumsAccelerator: SumsAccelerator | null = null;
let sumsRuns = 0;
export function setSumsAccelerator(a: SumsAccelerator | null) { sumsAccelerator = a; sumsRuns = 0; }

/** The first difference between two sets of sums (bit for bit), or null. */
export function firstSumsDifference(a: GroupedOut, b: GroupedOut): string | null {
  for (const f of ['csw', 'csw2', 'cswx', 'cswin', 'msw', 'msw2', 'mswx', 'mswin'] as const) {
    for (let k = 0; k < a[f].length; k++) if (!Object.is(a[f][k], b[f][k])) return `${f}[${k}] is ${a[f][k]} on the GPU and ${b[f][k]} on the processor`;
  }
  for (const f of ['mn', 'mseen'] as const) {
    for (let k = 0; k < a[f].length; k++) if (a[f][k] !== b[f][k]) return `${f}[${k}] is ${a[f][k]} on the GPU and ${b[f][k]} on the processor`;
  }
  return null;
}

/** The context total over every option, in observation order (the order its rounding depends on). */
function contextTotals(W: Float64Array, row: Int32Array, sims: Float64Array, out: Float64Array) {
  let tsw = 0, tsw2 = 0;
  for (let j = 0; j < W.length; j++) {
    const w = W[j], sim = sims[row[j]];
    if (w <= 0 || sim <= 0) continue;
    const wk = w * sim;
    tsw += wk; tsw2 += w * wk;
  }
  out[0] = tsw; out[1] = tsw2;
}

/**
 * Every option's sums for a query. Three ways to the same numbers: without context, from what the index holds (the
 * lane sums and, per opponent, the matchup's); with the query's similarities by game, one tight loop; else (the reason
 * search, live context, a subset of options) the general loop.
 */
function querySums(idx: LaneIndex, q: Query, o: EstimateOptions, hasContext: boolean, sims: ArrayLike<number> | null,
  use: boolean[] | null, dim: number): Sums {
  const n = idx.options.length;
  const groups = sims ? q.sims?.groups : undefined;
  if (groups && hasContext && idx.allRows && !use) {
    const c = { sw: new Float64Array(n), sw2: new Float64Array(n), swx: new Float64Array(n), swin: new Float64Array(n) };
    const m = emptySums(n);
    const out: GroupedOut = { csw: c.sw, csw2: c.sw2, cswx: c.swx, cswin: c.swin, msw: m.sw, msw2: m.sw2, mswx: m.swx, mswin: m.swin, mn: m.n, mseen: m.seen };
    const t = cellSums(idx, groups, q.opponent ?? -2, out);
    return { c, m, tsw: t.tsw, tsw2: t.tsw2, laneWeight: idx.laneWeight, sq: idx.sq, sqW: idx.sqW };
  }
  if (!hasContext && !use) {
    const c = {
      sw: Float64Array.from(idx.lane, (a) => a.sw), sw2: Float64Array.from(idx.lane, (a) => a.sw2),
      swx: Float64Array.from(idx.lane, (a) => a.swx), swin: Float64Array.from(idx.lane, (a) => a.swin),
    };
    const m = q.opponent !== null && q.opponent >= 0 ? opponentSums(idx, q.opponent) : emptySums(n);
    return { c, m, tsw: idx.tsw, tsw2: idx.tsw2, laneWeight: idx.laneWeight, sq: idx.sq, sqW: idx.sqW };
  }
  const c = { sw: new Float64Array(n), sw2: new Float64Array(n), swx: new Float64Array(n), swin: new Float64Array(n) };
  const m = emptySums(n);
  let tsw = 0, tsw2 = 0;
  if (sims instanceof Float64Array && idx.allRows && !use) {
    const opponent = q.opponent ?? -2;
    const out: GroupedOut = { csw: c.sw, csw2: c.sw2, cswx: c.swx, cswin: c.swin, msw: m.sw, msw2: m.sw2, mswx: m.swx, mswin: m.swin, mn: m.n, mseen: m.seen };
    const accelerator = sumsAccelerator;
    if (accelerator?.run(idx, sims, opponent, out)) {
      if (++sumsRuns % SUMS_VERIFY_EVERY === 1) {
        const cpu = groupedOut(n);
        groupedSums(idx, sims, opponent, cpu);
        const bad = firstSumsDifference(out, cpu);
        if (bad) {
          sumsAccelerator = null;
          accelerator.failed(bad);
          for (const f of Object.keys(cpu) as (keyof GroupedOut)[]) (out[f] as Float64Array).set(cpu[f] as Float64Array);
        }
      }
    } else groupedSums(idx, sims, opponent, out);
    contextTotals(idx.w, idx.row, sims, totals);
    return { c, m, tsw: totals[0], tsw2: totals[1], laneWeight: idx.laneWeight, sq: idx.sq, sqW: idx.sqW };
  }
  let laneWeight = 0, sq = 0, sqW = 0;
  for (let j = 0; j < idx.opt.length; j++) {
    const k = idx.opt[j];
    if (use && !use[k]) continue;
    const w = idx.w[j], wpa = idx.wpa[j];
    laneWeight += w;
    sq += w * wpa * wpa; sqW += w;
    const sim = !hasContext ? 1 : sims && idx.row[j] >= 0 ? sims[idx.row[j]]
      : idx.zRow[j] >= 0 ? kernelAt(idx.z[j] as Contexts, idx.zRow[j], q.z, o.h, dim) : kernel(idx.z[j] as ArrayLike<number>, q.z, o.h, dim);
    const matchup = q.opponent !== null && idx.opponent[j] === q.opponent;
    if (matchup) m.seen[k] = 1;
    if (w <= 0 || sim <= 0) continue;
    const wk = w * sim, win = idx.win[j];
    c.sw[k] += wk; c.sw2[k] += w * wk; c.swx[k] += wk * wpa; c.swin[k] += wk * win;
    tsw += wk; tsw2 += w * wk;
    if (matchup) { m.sw[k] += wk; m.sw2[k] += w * wk; m.swx[k] += wk * wpa; m.swin[k] += wk * win; m.n[k]++; }
  }
  return { c, m, tsw, tsw2, laneWeight, sq, sqW };
}

export function estimateDecision(obs: Observation[], q: Query, o: EstimateOptions, only?: Set<string>, dim = -1): DecisionEstimate {
  const hasContext = Number.isFinite(o.h) && (dim >= 0 ? !Number.isNaN(q.z[dim]) : anyKnown(q.z));
  const idx = laneIndex(obs, q.lane);
  const sims = hasContext && dim < 0 && q.sims?.h === o.h ? q.sims.values : null;
  const n = idx.options.length;
  const use = only ? idx.options.map((x) => only.has(x)) : null;
  // Context-weighted sums per option (each like Acc.add with the similarity as k), the matchup's, and the total.
  const memoKey = q.memo && !use && dim < 0 && (!hasContext || sims) ? obs : null;
  let sums = memoKey ? q.memo!.get(memoKey) as Sums | undefined : undefined;
  if (!sums) {
    sums = querySums(idx, q, o, hasContext, sims, use, dim);
    if (memoKey) q.memo!.set(memoKey, sums);
  }
  const { c, m, tsw, tsw2, laneWeight, sq, sqW } = sums;
  // WPA has mean ~0 by construction; its spread sets the interval width.
  const sigma = Math.max(0.3, sqW > 0 ? Math.sqrt(sq / sqW) : 0.5);
  const at = (s: { sw: Float64Array; sw2: Float64Array; swx: Float64Array }, k: number) => ({
    mean: s.sw[k] > 0 ? s.swx[k] / s.sw[k] : 0, nEff: s.sw2[k] > 0 ? (s.sw[k] * s.sw[k]) / s.sw2[k] : 0,
  });

  const options: OptionEstimate[] = [];
  let laneSw = 0, laneSw2 = 0;
  for (let k = 0; k < n; k++) {
    if (use && !use[k]) continue;
    const l = idx.lane[k];
    laneSw += l.sw; laneSw2 += l.sw2;
    if (l.n < o.minGames || l.sw < o.minShare * laneWeight) continue;
    const thetaAll = towards(idx.all[k], 0, o.k);
    const laneTheta = towards(l, thetaAll, o.k);
    const ck = at(c, k);
    const contextTheta = hasContext ? towards(ck, laneTheta, o.k) : laneTheta;
    const mk = m.seen[k] ? at(m, k) : null;
    const theta = mk ? towards(mk, contextTheta, o.k) : contextTheta;
    const nEff = Math.max(hasContext ? ck.nEff : l.nEff, mk?.nEff ?? 0);
    const half = 1.96 * sigma / Math.sqrt(nEff + o.k);
    const wr = hasContext ? { sw: c.sw[k], swin: c.swin[k] } : l;
    options.push({
      option: idx.options[k], theta, low: theta - half, high: theta + half, laneTheta, contextTheta,
      nEff: hasContext ? ck.nEff : l.nEff, matchupGames: mk ? m.n[k] : 0, games: l.n, pilots: idx.pilots[k],
      winRate: wr.sw > 0 ? wr.swin / wr.sw : 0,
    });
  }
  options.sort((a, b) => b.theta - a.theta || b.games - a.games);
  const total = tsw2 > 0 ? (tsw * tsw) / tsw2 : 0;
  return { options, nEff: hasContext ? total : laneSw ** 2 / Math.max(1e-9, laneSw2), sigma };
}
