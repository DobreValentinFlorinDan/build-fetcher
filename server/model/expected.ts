// Runes, summoners, the start and the skill order scored against each game's chance to win before it started (0.9.0),
// instead of against the average. Today a pre-game choice is scored by the result minus the overall win rate (there is
// no game state yet), so a rune taken mostly into hard matchups, on a weak patch or by weaker pilots looks worse than it
// is. The expected win rate here comes from what is known before the game: the draft (context.ts), the lane, the
// patch, the region, the pilot's ladder standing and Irelia mastery, and the lane opponent's own effect. A choice is
// then credited only with how much more often than that its games were won (regression adjustment).
//
// Statistically: a ridge logistic regression with the lane opponent as a random effect (its effect shrunk towards 0,
// the MAP of a normal prior), fitted by Newton's method. The pre-game choices themselves are in the fit too, and the
// expected win rate is read with them at their average mix: left out, the pilot's strength would soak up part of the
// effect of the choices strong pilots favour (adjusting the result alone biases a choice's effect towards zero by as
// much as the choice follows the pilot or the draft). A game's own expected win rate comes from a model fitted without
// its pilot's games (cross-fitting by pilot), so no game's result leaks into its own baseline. Off unless the
// evaluation finds it better (model/evaluate.ts, docs/BUILD-SOURCING.md §9.10).
import { DRAFT_DIMS, scaleOf, standardise, type Scale } from './context.ts';
import type { PreparedGame } from './dataset.ts';
import { solveSpd } from './logistic.ts';

/**
 * Ridge penalties: on the draft, pilot, patch and region coefficients (the features are on a scale of about 1), and on
 * each lane opponent's effect on the log-odds (10 is a prior spread of ±0.32, about ±8 points).
 */
export interface Penalty { lambda: number; opponentLambda: number }

export interface ExpectedOptions {
  /** The penalties tried, from flexible to rigid: the one with the lowest cross-fitted log loss is used. */
  penalties: Penalty[];
  /** Ridge penalty on the pre-game choices' coefficients: light, so their effects stay theirs (see above). */
  choiceLambda: number;
  /** Folds for cross-fitting, by pilot. */
  folds: number;
  /** Fewer games than this: no model (scores stay against the average). */
  minGames: number;
  /** A pre-game option with fewer games than this gets no column of its own (it counts with the decision's rare ones). */
  minChoiceGames: number;
}
export const EXPECTED_DEFAULTS: ExpectedOptions = {
  penalties: [{ lambda: 1, opponentLambda: 10 }, { lambda: 10, opponentLambda: 30 }, { lambda: 100, opponentLambda: 100 }, { lambda: 1000, opponentLambda: 1000 }],
  choiceLambda: 1, folds: 5, minGames: 200, minChoiceGames: 30,
};

/** The pre-game choices in the fit, read at their average mix (the decisions scoring against the draft changes). */
const CHOICES: { key: string; of: (g: PreparedGame) => string | null }[] = [
  { key: 'keystone', of: (g) => String(g.perks[0]) },
  { key: 'secondaryTree', of: (g) => String(g.sub) },
  { key: 'summoners', of: (g) => g.summoners },
  { key: 'start', of: (g) => g.start || null },
  { key: 'maxFirst', of: (g) => g.maxFirst },
];

/** A game's patch age: 0 on its region's current patch (weights.ts patchAger). */
export type AgeOf = (g: Pick<PreparedGame, 'platform' | 'patch'>) => number;

/** What turns a game into features: fixed from the games a model is fitted on, so every fold reads them alike. */
interface Spec { scale: Scale; regions: string[]; maxAge: number; choices: string[][] }

export interface ExpectedModel {
  spec: Spec;
  beta: Float64Array;
  /** The choice columns' means over the fitted games: the average mix the expected win rate is read at. */
  choiceMeans: Float64Array;
  /** Each lane opponent's effect on the log-odds (champion id → effect); unknown opponents add 0. */
  opponents: Map<number, number>;
  games: number;
  penalty: Penalty;
}

/** The feature names, for the report and the tests. */
export function featureNames(spec: Spec): string[] {
  return [
    'intercept', ...DRAFT_DIMS.map((d) => d.key), 'opponent unknown', 'mid lane', 'apex tier', 'apex LP', 'tier unknown',
    'mastery', 'mastery unknown', 'source main', 'source harvested',
    ...Array.from({ length: spec.maxAge }, (_, i) => `patch age ${i + 1}`), ...spec.regions.slice(1).map((r) => `region ${r}`),
    ...CHOICES.flatMap((c, i) => spec.choices[i].map((v) => `${c.key} ${v}`)),
  ];
}

const APEX = new Set(['CHALLENGER', 'GRANDMASTER', 'MASTER']);
const MAX_AGE = 5;

function specOf(games: PreparedGame[], ageOf: AgeOf, o: ExpectedOptions): Spec {
  const choices = CHOICES.map((c) => {
    const n = new Map<string, number>();
    for (const g of games) { const v = c.of(g); if (v !== null) n.set(v, (n.get(v) ?? 0) + 1); }
    return [...n.entries()].filter(([, k]) => k >= o.minChoiceGames).map(([v]) => v).sort();
  });
  const counts = new Map<string, number>();
  for (const g of games) counts.set(g.platform, (counts.get(g.platform) ?? 0) + 1);
  // The most common region is the reference (no feature of its own); the rest in order of games, then name.
  const regions = [...counts.entries()].sort((a, b) => b[1] - a[1] || a[0].localeCompare(b[0])).map(([r]) => r);
  let maxAge = 0;
  for (const g of games) maxAge = Math.max(maxAge, Math.min(MAX_AGE, finiteAge(ageOf(g))));
  return { scale: scaleOf(games.map((g) => g.draft), DRAFT_DIMS.length), regions, maxAge, choices };
}

const finiteAge = (a: number) => (Number.isFinite(a) && a >= 0 ? a : MAX_AGE);

/**
 * One game's features (featureNames' order). Unknown draft dimensions count as average (0 once standardised). The
 * choice columns hold the game's own choices to fit, and `choiceMeans` (the average mix) to read the expected win rate.
 */
function featuresInto(out: Float64Array, at: number, g: PreparedGame, spec: Spec, ageOf: AgeOf, choiceMeans: Float64Array | null = null) {
  let i = at;
  out[i++] = 1;
  for (const z of standardise(g.draft, spec.scale)) out[i++] = Number.isFinite(z) ? z : 0;
  out[i++] = g.opponent === null ? 1 : 0;
  out[i++] = g.lane === 'MIDDLE' ? 1 : 0;
  const apex = g.tier !== null && APEX.has(g.tier);
  out[i++] = apex ? 1 : 0;
  out[i++] = apex && g.lp !== null ? Math.log1p(Math.max(0, g.lp)) / 7 : 0;
  out[i++] = g.tier === null ? 1 : 0;
  out[i++] = g.mastery !== null ? Math.log1p(Math.max(0, g.mastery)) / 14 : 0;
  out[i++] = g.mastery === null ? 1 : 0;
  out[i++] = g.source === 'main' ? 1 : 0;
  out[i++] = g.source === 'harvested' ? 1 : 0;
  const age = Math.min(spec.maxAge, finiteAge(ageOf(g)));
  for (let a = 1; a <= spec.maxAge; a++) out[i++] = age === a ? 1 : 0;
  for (let r = 1; r < spec.regions.length; r++) out[i++] = g.platform === spec.regions[r] ? 1 : 0;
  let c = 0;
  CHOICES.forEach((choice, k) => {
    const v = choiceMeans ? null : choice.of(g);
    for (const option of spec.choices[k]) { out[i++] = choiceMeans ? choiceMeans[c] : v === option ? 1 : 0; c++; }
  });
}

const choiceWidth = (spec: Spec) => spec.choices.reduce((s, c) => s + c.length, 0);
const widthOf = (spec: Spec) => 1 + DRAFT_DIMS.length + 9 + spec.maxAge + Math.max(0, spec.regions.length - 1) + choiceWidth(spec);

/** The games as rows: features, the lane opponent's index (-1: none) and the result. */
function design(games: PreparedGame[], spec: Spec, ageOf: AgeOf) {
  const d = widthOf(spec);
  const X = new Float64Array(games.length * d);
  const opponentIds: number[] = [];
  const index = new Map<number, number>();
  const group = new Int32Array(games.length);
  const y = new Float64Array(games.length);
  games.forEach((g, r) => {
    featuresInto(X, r * d, g, spec, ageOf);
    if (g.opponent === null) group[r] = -1;
    else {
      let k = index.get(g.opponent);
      if (k === undefined) { k = opponentIds.length; index.set(g.opponent, k); opponentIds.push(g.opponent); }
      group[r] = k;
    }
    y[r] = g.win;
  });
  return { X, d, group, opponentIds, y };
}

const sigmoid = (z: number) => (z >= 0 ? 1 / (1 + Math.exp(-z)) : Math.exp(z) / (1 + Math.exp(z)));

/**
 * Maximises Σ loglik(yᵣ | xᵣ·β + u[group ᵣ]) − Σⱼ λⱼ/2·βⱼ² − λᵤ/2·‖u‖² over the rows in `use` (all when null), by
 * Newton's method; `lambda` is one penalty for every coefficient but the intercept, or one per coefficient (the
 * intercept's is ignored). A row has at most one group, so the groups' block of the Hessian is diagonal: each step
 * solves the dense block's Schur complement (d × d) and then every group's effect directly.
 */
export function fitGrouped(X: Float64Array, d: number, group: Int32Array, groups: number, y: Float64Array, lambda: number | ArrayLike<number>,
  groupLambda: number, use: Uint8Array | null = null, maxIterations = 30, tolerance = 1e-8) {
  const penalty = (i: number) => (typeof lambda === 'number' ? lambda : lambda[i]);
  const n = y.length;
  const beta = new Float64Array(d), u = new Float64Array(groups);
  let iterations = 0, converged = false;
  for (let it = 1; it <= maxIterations; it++) {
    iterations = it;
    const A = new Float64Array(d * d), gb = new Float64Array(d);
    const B = new Float64Array(d * groups), D = new Float64Array(groups), gu = new Float64Array(groups);
    for (let r = 0; r < n; r++) {
      if (use && !use[r]) continue;
      const off = r * d, k = group[r];
      let eta = k >= 0 ? u[k] : 0;
      for (let i = 0; i < d; i++) eta += X[off + i] * beta[i];
      const p = sigmoid(eta), w = p * (1 - p), res = y[r] - p;
      for (let i = 0; i < d; i++) {
        const xi = X[off + i];
        if (xi === 0) continue;
        gb[i] += res * xi;
        const wxi = w * xi;
        for (let j = 0; j <= i; j++) A[i * d + j] += wxi * X[off + j];
        if (k >= 0) B[i * groups + k] += wxi;
      }
      if (k >= 0) { D[k] += w; gu[k] += res; }
    }
    for (let i = 0; i < d; i++) for (let j = 0; j < i; j++) A[j * d + i] = A[i * d + j];
    for (let i = 1; i < d; i++) { const l = penalty(i); A[i * d + i] += l; gb[i] -= l * beta[i]; }
    A[0] += 1e-9;
    for (let k = 0; k < groups; k++) { D[k] += groupLambda; gu[k] -= groupLambda * u[k]; }
    // Schur complement of the groups' (diagonal) block: S = A − B D⁻¹ Bᵀ, right side gb − B D⁻¹ gu.
    const S = A, rhs = gb;
    for (let k = 0; k < groups; k++) {
      const inv = 1 / D[k];
      for (let i = 0; i < d; i++) {
        const bik = B[i * groups + k];
        if (bik === 0) continue;
        rhs[i] -= bik * inv * gu[k];
        for (let j = 0; j < d; j++) S[i * d + j] -= bik * inv * B[j * groups + k];
      }
    }
    const db = solveSpd(S, rhs, d);
    let biggest = 0;
    for (let i = 0; i < d; i++) { beta[i] += db[i]; biggest = Math.max(biggest, Math.abs(db[i])); }
    for (let k = 0; k < groups; k++) {
      let s = gu[k];
      for (let i = 0; i < d; i++) s -= B[i * groups + k] * db[i];
      const du = s / D[k];
      u[k] += du;
      biggest = Math.max(biggest, Math.abs(du));
    }
    if (biggest < tolerance) { converged = true; break; }
  }
  return { beta, u, iterations, converged };
}

function fitRows(rows: ReturnType<typeof design>, spec: Spec, o: ExpectedOptions, penalty: Penalty, use: Uint8Array | null, games: number): ExpectedModel {
  const width = choiceWidth(spec), from = rows.d - width;
  const penalties = Float64Array.from({ length: rows.d }, (_, i) => (i >= from ? o.choiceLambda : penalty.lambda));
  const f = fitGrouped(rows.X, rows.d, rows.group, rows.opponentIds.length, rows.y, penalties, penalty.opponentLambda, use);
  // The average mix of choices among the fitted games.
  const choiceMeans = new Float64Array(width);
  let n = 0;
  for (let r = 0; r < rows.y.length; r++) {
    if (use && !use[r]) continue;
    n++;
    for (let c = 0; c < width; c++) choiceMeans[c] += rows.X[r * rows.d + from + c];
  }
  for (let c = 0; c < width; c++) choiceMeans[c] /= Math.max(1, n);
  return { spec, beta: f.beta, choiceMeans, opponents: new Map(rows.opponentIds.map((id, k) => [id, f.u[k]])), games, penalty };
}

/**
 * Fits the expected win rate on `games` (the user's own never: they would tilt the high-elo model) with `penalty`
 * (crossFitted chooses one); null when too few.
 */
export function fitExpected(games: PreparedGame[], ageOf: AgeOf, penalty: Penalty, o: ExpectedOptions = EXPECTED_DEFAULTS): ExpectedModel | null {
  const fit = games.filter((g) => g.source !== 'me');
  if (fit.length < o.minGames) return null;
  const spec = specOf(fit, ageOf, o);
  return fitRows(design(fit, spec, ageOf), spec, o, penalty, null, fit.length);
}

/** A game's expected win rate before it started, with the average mix of pre-game choices (never its own). */
export function expectedOf(m: ExpectedModel, g: PreparedGame, ageOf: AgeOf): number {
  const x = new Float64Array(m.beta.length);
  featuresInto(x, 0, g, m.spec, ageOf, m.choiceMeans);
  let eta = g.opponent !== null ? m.opponents.get(g.opponent) ?? 0 : 0;
  for (let i = 0; i < x.length; i++) eta += x[i] * m.beta[i];
  return sigmoid(eta);
}

/** FNV-1a, as the evaluation's folds. */
function hash(s: string) {
  let h = 0x811c9dc5;
  for (let i = 0; i < s.length; i++) { h ^= s.charCodeAt(i); h = Math.imul(h, 0x01000193) >>> 0; }
  return h;
}

/**
 * Every game's expected win rate from a model fitted without its pilot's games (`o.folds` folds by pilot), aligned
 * with `games`, and the penalty used: of `o.penalties`, the one whose cross-fitted predictions (with each game's own
 * choices) predict the results best. NaN for the user's own games (the personal layer keeps scoring them against the
 * average) and when there are too few games to fit: scoreGames then scores against the average, as before 0.9.0.
 */
export function crossFitted(games: PreparedGame[], ageOf: AgeOf, o: ExpectedOptions = EXPECTED_DEFAULTS): { values: Float64Array; penalty: Penalty | null } {
  const empty = { values: new Float64Array(games.length).fill(NaN), penalty: null };
  const at = games.flatMap((g, i) => (g.source === 'me' ? [] : [i]));
  if (at.length < o.minGames) return empty;
  const fit = at.map((i) => games[i]);
  const spec = specOf(fit, ageOf, o);
  const rows = design(fit, spec, ageOf);
  const foldOf = fit.map((g) => hash(`expected:${g.pilot}`) % o.folds);
  const x = new Float64Array(rows.d);
  let best: { values: Float64Array; penalty: Penalty; logLoss: number } | null = null;
  for (const penalty of o.penalties) {
    const values = new Float64Array(games.length).fill(NaN);
    let loss = 0, n = 0;
    for (let f = 0; f < o.folds; f++) {
      const use = Uint8Array.from(foldOf, (k) => (k === f ? 0 : 1));
      const trained = use.reduce((s, k) => s + k, 0);
      if (trained < o.minGames || trained === fit.length) continue;
      const m = fitRows(rows, spec, o, penalty, use, trained);
      fit.forEach((g, j) => {
        if (foldOf[j] !== f) return;
        values[at[j]] = expectedOf(m, g, ageOf);
        // How well this penalty predicts the held-out result, with the game's own choices (what the fit learns).
        featuresInto(x, 0, g, spec, ageOf);
        let eta = g.opponent !== null ? m.opponents.get(g.opponent) ?? 0 : 0;
        for (let i = 0; i < x.length; i++) eta += x[i] * m.beta[i];
        const p = Math.min(1 - 1e-6, Math.max(1e-6, sigmoid(eta)));
        loss -= g.win ? Math.log(p) : Math.log(1 - p);
        n++;
      });
    }
    if (n && (!best || loss / n < best.logLoss)) best = { values, penalty, logLoss: loss / n };
  }
  return best ? { values: best.values, penalty: best.penalty } : empty;
}
