// L2-regularised logistic regression, fitted by Newton's method (IRLS). Small and dependency-free: the state model has
// a few dozen coefficients and at most tens of thousands of rows.

const sigmoid = (z: number) => (z >= 0 ? 1 / (1 + Math.exp(-z)) : Math.exp(z) / (1 + Math.exp(z)));

export const dot = (a: ArrayLike<number>, b: ArrayLike<number>) => {
  let s = 0;
  for (let i = 0; i < a.length; i++) s += a[i] * b[i];
  return s;
};

export interface FitOptions {
  /** L2 penalty on every coefficient but the first (the intercept: column 0 must be 1). */
  lambda: number;
  /** Row weights (default 1). */
  weights?: ArrayLike<number>;
  maxIterations?: number;
  tolerance?: number;
}

export interface Fit { beta: number[]; iterations: number; converged: boolean }

/** Solves A x = b for a symmetric positive-definite A (Cholesky). A is d×d, row-major. */
export function solveSpd(A: Float64Array, b: Float64Array, d: number): Float64Array {
  const L = new Float64Array(d * d);
  for (let i = 0; i < d; i++) {
    for (let j = 0; j <= i; j++) {
      let s = A[i * d + j];
      for (let k = 0; k < j; k++) s -= L[i * d + k] * L[j * d + k];
      if (i === j) {
        if (s <= 0) throw new Error('matrix is not positive definite');
        L[i * d + i] = Math.sqrt(s);
      } else L[i * d + j] = s / L[j * d + j];
    }
  }
  const y = new Float64Array(d);
  for (let i = 0; i < d; i++) {
    let s = b[i];
    for (let k = 0; k < i; k++) s -= L[i * d + k] * y[k];
    y[i] = s / L[i * d + i];
  }
  const x = new Float64Array(d);
  for (let i = d - 1; i >= 0; i--) {
    let s = y[i];
    for (let k = i + 1; k < d; k++) s -= L[k * d + i] * x[k];
    x[i] = s / L[i * d + i];
  }
  return x;
}

/** Maximises Σ wᵢ·loglik(yᵢ | xᵢ·β) − λ/2·‖β₁..‖². `X` rows must start with a 1 (intercept). */
export function fitLogistic(X: ArrayLike<number>[], y: ArrayLike<number>, o: FitOptions): Fit {
  const n = X.length;
  if (!n) throw new Error('no rows to fit');
  const d = X[0].length;
  const beta = new Float64Array(d);
  const maxIt = o.maxIterations ?? 30;
  const tol = o.tolerance ?? 1e-8;
  for (let it = 1; it <= maxIt; it++) {
    const H = new Float64Array(d * d);
    const g = new Float64Array(d);
    for (let r = 0; r < n; r++) {
      const x = X[r];
      const sw = o.weights ? o.weights[r] : 1;
      if (!sw) continue;
      const p = sigmoid(dot(x, beta));
      const resid = sw * (y[r] - p);
      const w = sw * p * (1 - p);
      for (let i = 0; i < d; i++) {
        const xi = x[i];
        if (xi === 0) continue;
        g[i] += resid * xi;
        const wxi = w * xi;
        for (let j = 0; j <= i; j++) H[i * d + j] += wxi * x[j];
      }
    }
    for (let i = 0; i < d; i++) for (let j = 0; j < i; j++) H[j * d + i] = H[i * d + j];
    for (let i = 1; i < d; i++) { H[i * d + i] += o.lambda; g[i] -= o.lambda * beta[i]; }
    H[0] += 1e-9;   // keeps the intercept's pivot positive when every outcome is the same
    const step = solveSpd(H, g, d);
    let max = 0;
    for (let i = 0; i < d; i++) { beta[i] += step[i]; max = Math.max(max, Math.abs(step[i])); }
    if (max < tol) return { beta: [...beta], iterations: it, converged: true };
  }
  return { beta: [...beta], iterations: maxIt, converged: false };
}

export const predictLogistic = (beta: ArrayLike<number>, x: ArrayLike<number>) => sigmoid(dot(beta, x));

/** Brier score and log loss of probabilities `p` against outcomes `y`. */
export function scoreProbabilities(p: ArrayLike<number>, y: ArrayLike<number>) {
  let brier = 0, logLoss = 0;
  for (let i = 0; i < p.length; i++) {
    const q = Math.min(1 - 1e-12, Math.max(1e-12, p[i]));
    brier += (q - y[i]) ** 2;
    logLoss -= y[i] * Math.log(q) + (1 - y[i]) * Math.log(1 - q);
  }
  return { brier: brier / p.length, logLoss: logLoss / p.length, n: p.length };
}

/** Calibration table: predictions in `bins` equal-width buckets, with the mean prediction and the observed rate. */
export function calibration(p: ArrayLike<number>, y: ArrayLike<number>, bins = 10) {
  const rows = Array.from({ length: bins }, (_, i) => ({ from: i / bins, to: (i + 1) / bins, n: 0, predicted: 0, observed: 0 }));
  for (let i = 0; i < p.length; i++) {
    const b = rows[Math.min(bins - 1, Math.floor(p[i] * bins))];
    b.n++; b.predicted += p[i]; b.observed += y[i];
  }
  return rows.filter((r) => r.n > 0).map((r) => ({ ...r, predicted: r.predicted / r.n, observed: r.observed / r.n }));
}
