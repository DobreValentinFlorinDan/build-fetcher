# Evaluate builds at 100,000+ games: the plan, and grouped drafts (0.20.0)

## The problem

A full evaluation (server/model/evaluate.ts) compares every held-out game with every stored game: each stored game's
similarity to the held-out draft (context.ts `kernel`: a Gaussian kernel on the mean squared standardised distance over
the 12 draft dimensions), then, per decision, the similarity-weighted sums per option and for the exact matchup
(estimate.ts). That is held-out × stored games per fold and per setting tried (bandwidth and prior grid, patch memory's
fades, fine-tuning), so the time grows with the **square** of the games. The quick update (0.16.0, docs/EVAL-UPDATE.md)
avoids re-measuring old games between full evaluations, but every full evaluation still pays the square, and reading
the games (decompressing and replaying every timeline) is paid by both.

Measured before 0.20.0 on the user's data: a full evaluation took 1,931 s at 82,069 games on 2 threads; a quick update
151–231 s (68 s with nothing new, mostly reading).

## The plan, in four steps

| Step | What | Exact? | Release |
|---|---|---|---|
| 0 | Time by step in every report, to measure where the time goes on real data | yes | **0.20.0** |
| 1 | Feature files: each game's model inputs stored once, compactly, appended as games arrive; reading skips the timelines | yes | next |
| 2 | Running totals: the context-free layers (all lanes, lane, matchup) and the folds (totals minus the fold) from additive sums | yes | next |
| 3 | **Grouped drafts**: the stored drafts in M groups, comparisons per group | no: measured | **0.20.0, as a trial** |
| 4 | Prequential scoring (test-then-train): each new game scored once by the model as it stood, never re-measured | no: measured | after 3 is adopted |

Steps 1 and 2 keep every report identical to the last digit. Steps 3 and 4 change the numbers slightly by design, so
each runs beside the exact evaluation and is adopted only within limits set before the comparison (below).

## Grouped drafts (step 3, 0.20.0)

**How:** the evaluated games' draft contexts are standardised and put into M groups by k-means (k-means++ seeding,
Lloyd's iterations on a sample, then every game assigned to its nearest centre; deterministic for a seed). Games are
only grouped with games that know the same dimensions, so a group's centre knows what its games know. Each draft model
built from those games (draft.ts `buildDraftModel`) then holds, per group, the centre in its own standardisation; a
query computes M similarities (one per centre) instead of one per stored game. In estimate.ts, a lane's observations
become cells, per option one per group, holding Σw, Σw², Σw·WPA and Σw·win of their observations: the context layer's
sums are then Σ over cells of the group's similarity × the cell's sums, which is exactly the per-game sum with every
game at its group's similarity (tested to rounding). The exact matchup layer stays per game: its observations are
indexed by opponent and summed one by one with their group's similarity. Weights (pilot cap, patch, region) are the
model's own, unchanged: cells are built per model.

**Why it is sound:** this is binned kernel estimation (Silverman 1982; Fan & Marron 1994; Wand 1994): the error comes
from treating a group's games as if they sat at its centre, and it shrinks with the groups' spread against the
bandwidth (0.7 sd by default). The report gives that spread.

**Where it applies:** only an evaluation trial (`EvalOptions.grouping`). Champ select, the overlay, the model thread and
the live replay of an evaluation stay exact. A trial is always a full evaluation, saved as `eval-grouped-<stamp>`;
it never becomes `latest.json` (the app's verdicts), the base of a quick update, or a point of the evaluation history.

### Measured (synthetic, Linux, one thread)

Per query (one decision's estimate, 8 options, 60 opponents), 150,000 drafts drawn around 300 compositions, a real
option effect on one dimension, 200 queries:

| Groups | Grouping | Exact query | Grouped query | Faster | Same top pick | Same top pick where it leads by > 0.5 pts | Largest move |
|---:|---:|---:|---:|---:|---:|---:|---:|
| 1,024 | 7.6 s | 7.8 ms | 0.25 ms | 31× | 186 / 200 | 139 / 139 | 0.61 pts |
| 2,048 | 27 s | 7.6 ms | 0.38 ms | 20× | 187 / 200 | 139 / 139 | 0.54 pts |
| 4,096 | 91 s | 7.0 ms | 0.74 ms | 10× | 189 / 200 | 139 / 139 | 0.52 pts |
| 8,192 | 184 s | 7.9 ms | 1.7 ms | 5× | 190 / 200 | 139 / 139 | 0.40 pts |

The picks that differ are near-ties: wherever the exact top option led by more than half a point, the grouped one
agreed every time. More groups buy little accuracy for much more time, so the default is about 40 games per group, at
most 1,024 (groups.ts `autoGroups`); `--grouped N` tries another number. Without grouping, the exact query grew from
2.7 ms at 50,000 drafts to 7.6 ms at 150,000 and 20.5 ms at 300,000; grouped, 0.23, 0.37 and 0.93 ms.

A whole evaluation (npm run eval:builds' steps, patch memory off as there is one patch), synthetic games
(server/test/modelFixtures.ts: two enemy compositions and five opponents, so very few distinct drafts):

| Games | Exact | Grouped | Verdict and lift |
|---:|---:|---:|---|
| 6,000 | 51.9 s | 21.4 s (−59 %) | passed, +3.57 pts (3.18 to 3.98) in both |

These synthetic drafts are easy to group; real drafts are not. **The trial on your data is what decides.**

### Running the trial

```
npm run eval:builds -- --compare-grouped
```

runs a full exact evaluation (saved as usual), then the grouped trial on the same games, and saves the two side by side
as `eval-compare-<stamp>.txt` next to the reports (`%APPDATA%\irelia-companion\evals`): time, every verdict, the lift
and its interval, Brier scores, the chosen settings, and per decision the lift and how many held-out games followed
the draft model's pick (a proxy for how often the two pick the same). `--grouped [N]` runs the trial alone. Close the
app first, or leave it idle: the trial runs on the same cores.

### Adopting it

Grouping becomes the default for full evaluations only if, on your data:

1. every verdict is the same as the exact evaluation's (draft model, patch memory, runes against the draft,
   fine-tuning, late-game swaps), or one that differs had an exact interval touching zero;
2. the draft model's lift moves by at most 0.1 pts, and no decision's by more than 0.25 pts;
3. it takes at least 50 % less time.

If they hold, the next release makes it the default (with "Full evaluation (exact)" kept as its own action). If not,
more groups (`--grouped 2048`) is the next thing to measure.

## Time by step (step 0, 0.20.0)

Every report (the app's and `npm run eval:builds`') ends with **Time by step**: each step's time and share, then the
same by kind (a step's folds added up), largest first. The JSON report holds them as `timings`, and the middleware's
log a line `[eval] time: …` with the three largest. On the 6,000 synthetic games above, fine-tuning took 53 % of the
exact run and the bandwidth/prior grid 22 %: these are where grouping saves most, and where your report will show
whether reading (step 1) matters next.

## Next (not in 0.20.0)

- **Step 1, feature files.** A binary file per patch and region beside the database (no new dependency): per game the
  lane, opponent, champions, each decision's option as a small number, the game-state features at each purchase, win,
  pilot and time; a few hundred bytes a game against the ~6.5 KB a thread holds today. New games go into a small new
  file, merged now and then; a feature version (Data Dragon, capture, model logic) rebuilds them. What depends on the
  whole pool (the champion profiles behind the 12 draft dimensions, each fold's game-state model) is worked out at
  evaluation time from what is stored. Reading falls from about a minute to seconds (an estimate), and less memory per
  thread means more threads in the app.
- **Step 2, running totals.** With the feature files, the context-free sums kept per lane × option × opponent × patch,
  added to as games arrive; a fold's model is the totals minus that fold's. The per-pilot cap is not additive and needs
  its own handling.
- **Step 4, prequential scoring** (Dawid 1984; Gama et al. 2013): each new game scored by the model built from the
  games before it (its own pilot's left out), then added; gates by the same pilot bootstrap on the accumulated scores;
  the full K-fold kept as a check once a patch.
