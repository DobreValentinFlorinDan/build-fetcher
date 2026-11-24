// Evaluate builds on the GPU, second kernel (0.19.0): the weighted sums estimate.ts groupedLoop() runs on the
// processor for every question the evaluation asks, to the same last digit. One thread per option: its games are
// added in the same order, each product and sum rounded on its own (never fused), so the numbers are the processor's.
extern "C" {

struct SumsParams {
  unsigned long long start;  // const int[K + 1]: where each option's games start (they are grouped by option)
  unsigned long long w;      // const double[N]: each game's weight
  unsigned long long wpa;    // const double[N]
  unsigned long long win;    // const double[N]
  unsigned long long opp;    // const int[N]: each game's lane opponent (-1: none)
  unsigned long long row;    // const int[N]: each game's row in `sims`
  unsigned long long sims;   // const double[]: each stored game's similarity to the draft
  unsigned long long out;    // double[8 K], then int[2 K]: see below
  int options;               // K
  int opponent;              // the draft's lane opponent (-2: none)
};

__constant__ SumsParams params;

// out: the doubles field by field (sw, sw2, swx, swin over all the draft's similar games, then the same four for the
// exact matchup), K of each; then the ints: the matchup's game count per option, and whether the option was seen in it.
__global__ void sums() {
  const int k = blockIdx.x * blockDim.x + threadIdx.x;
  const int K = params.options;
  if (k >= K) return;
  const int *start = (const int *)params.start;
  const double *W = (const double *)params.w, *WPA = (const double *)params.wpa, *WIN = (const double *)params.win;
  const int *opp = (const int *)params.opp, *row = (const int *)params.row;
  const double *sims = (const double *)params.sims;
  const int opponent = params.opponent;
  double sw = 0.0, sw2 = 0.0, swx = 0.0, swin = 0.0, ms = 0.0, ms2 = 0.0, msx = 0.0, mswn = 0.0;
  int n = 0, seen = 0;
  for (int p = start[k]; p < start[k + 1]; p++) {
    const double w = W[p], sim = sims[row[p]];
    const bool matchup = opp[p] == opponent;
    if (matchup) seen = 1;
    if (w <= 0.0 || sim <= 0.0) continue;
    const double wk = __dmul_rn(w, sim), wpa = WPA[p], win = WIN[p];
    sw = __dadd_rn(sw, wk); sw2 = __dadd_rn(sw2, __dmul_rn(w, wk));
    swx = __dadd_rn(swx, __dmul_rn(wk, wpa)); swin = __dadd_rn(swin, __dmul_rn(wk, win));
    if (matchup) {
      ms = __dadd_rn(ms, wk); ms2 = __dadd_rn(ms2, __dmul_rn(w, wk));
      msx = __dadd_rn(msx, __dmul_rn(wk, wpa)); mswn = __dadd_rn(mswn, __dmul_rn(wk, win));
      n++;
    }
  }
  double *od = (double *)params.out;
  od[k] = sw; od[K + k] = sw2; od[2 * K + k] = swx; od[3 * K + k] = swin;
  od[4 * K + k] = ms; od[5 * K + k] = ms2; od[6 * K + k] = msx; od[7 * K + k] = mswn;
  int *oi = (int *)(od + 8 * K);
  oi[k] = n; oi[K + k] = seen;
}

}
