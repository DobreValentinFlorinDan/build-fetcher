# Measurements

## Learning

Agent B, 2026-11-24. The build machine had 4 cores (Intel Xeon @ 2.10 GHz), 15 GB of memory and no GPU. Other
agents were compiling at the same time, so timings are indicative to about ±30%.

### Command

```
cargo run --release -p bf-learn --example measure -- 20000 2000 2000      # matches, validation matches, records per update
cargo run --release -p bf-learn --example measure -- 6000 1000 2000 1     # the same on 6,000 matches with 1 thread
cargo run --release -p bf-learn --example measure -- 6000 1000 2000 4     #                                 4 threads
cargo run --release -p bf-learn --example measure -- 6000 1000 500 4      # smaller updates (500 records)
```

### Synthetic set

`bf_learn::synthetic::generate` (seed 1): 20,000 training matches (90% with timelines) and 2,000 held-out
validation matches over four game versions (16.20 to 16.22, with a 16.22 hotfix). Champions come from per-role
pools of 5-7; each player gets a build plan from per-role menus that respect item exclusivity. The winner is drawn
from sigmoid(Σ team effects) with logistic noise, and gold, CS, level, kills, towers, dragons and barons drift toward
the winner. Planted effects:

| effect | size |
|---|---|
| Irelia TOP, Conqueror against Darius (Grasp against Darius, or Conqueror against others: nothing) | +0.45 logit (≈ +11 pp) |
| Top/jungle Maw of Malmortius against a magic-heavy team (otherwise −0.1) | +0.6 logit |
| Irelia rushing Blade of the Ruined King (vs Trinity Force) | +0.3 logit |
| Jinx with Lulu on her team (no step-1 feature can express this) | +0.5 logit |
| Ivern jungle picked rarely (thin data) | ~50 games per 6,000 matches |

The validation set gives 104,260 examples (10 players × the minutes 2, 8, 14, 20, 26, 32, 38 that fit in each game).

### Held-out metrics (20,000 matches, 10 updates of 2,000 records, 4 threads)

| model | log loss | Brier | calibration error 0-10 / 10-20 / 20-30 / 30+ min |
|---|---|---|---|
| baseline: champion-role win rate | 0.68988 | 0.24837 | 0.0052 / 0.0052 / 0.0047 / 0.0087 |
| step 1: logistic per champion-role, shrunk to global | 0.49759 | 0.16845 | 0.0089 / 0.0213 / 0.0135 / 0.0322 |
| step 1 + step 2: shared network on top | **0.49672** | **0.16800** | 0.0058 / 0.0189 / 0.0099 / 0.0288 |

- The baseline's calibration error is low because it predicts nearly the same number for everyone: calibration of a
  near-constant is trivially good. The guardrail therefore compares calibration with a cluster-robust noise floor
  (what a perfectly calibrated model with the same predictions shows by sampling alone; teammates share a label, so
  each match-team counts once). At this validation size the floors were about 0.009 / 0.015 / 0.012 / 0.023.
- Step 2 shipped in updates 2 and 6, each time beating step 1 on log loss, Brier and every calibration window. On
  the 6,000-match runs it never beat step 1 by the guardrail's rules, so step 1 alone was served there.
- Accepted / rejected updates (logistic, network): 1 ✓✗, 2 ✓✓, 3 ✓✗, 4 ✗✗, 5 ✗✗, 6 ✓✓, 7-10 ✗✗. Once the model has
  converged, the strict "no metric worse" rule rejects most updates on noise (log loss 0.49786 vs 0.49759). That is
  the rule as specified; it keeps a good model but can stall learning on new patches. A small tolerance
  (`Guardrail::log_loss_slack`, e.g. 1e-4) or a larger validation sample would let it move; left at 0 for the lead
  to decide.

### Planted effects recovered (20,000 matches)

| effect | tier A (model) | tier B (counts) |
|---|---|---|
| Conqueror vs Grasp, Irelia against Darius | Conqueror first, +0.026 vs −0.023 (4.9 pp) | Conqueror first, +0.073 vs −0.031 (10.4 pp) |
| Blade of the Ruined King vs Trinity first | (counts in both tiers) | BotRK first, +0.065 vs −0.029 |
| Maw of Malmortius against a magic-heavy team | (counts in both tiers) | Maw first among late items, +0.047 |
| Ivern jungle (thin) | tier B (too few examples for the model) | options flagged thin, basis "champion", WPA shrunk toward 0 |

The model's rune gap is smaller than the counts' because it is learnt with L2 and shrinkage, and given the game
state. On 4,000 matches the test suite checks that the model ranks Conqueror first against Darius by more than
4 pp and the two keystones within half that against Garen.

### Time and memory (CPU)

| run | update | time | of which training / evaluation |
|---|---|---|---|
| 20k matches, 4 threads | first (cold start: 6 + 4 epochs) | 6.2 s | 5.3 s / 0.5 s |
| 20k matches, 4 threads | incremental, 2,000 records + 60,000 replayed examples, both steps | 4.1-4.5 s | 3.2-3.5 s / 0.7 s |
| 6k matches, 1 thread | incremental, 2,000 records | 10.2-10.4 s | 9.2-9.4 s / 0.8 s |
| 6k matches, 4 threads | incremental, 2,000 records | 7.8-7.9 s | 7.1-7.2 s / 0.4 s |
| 6k matches, 4 threads | incremental, 500 records | 1.6-3.4 s | |

- The 1-thread and 4-thread runs end with bit-identical metrics (log loss 0.49763 both), as the determinism test
  requires. 4 threads are only ~1.3× faster: the gradient scatter and the optimiser steps are kept sequential so
  sums never depend on the thread count.
- Memory: the measuring process held the synthetic set (426 MB for 22,000 matches with full timelines) before
  training; training added about 250 MB at 2,000 records per update (peak 756 MB, resident 606-685 MB). Most of it is
  the examples (the new ones, 60,000 replayed, and the 104,000 validation examples); it scales with records per
  update and the validation size. With 500 records per update and 1,000 validation matches the process peaked at
  382 MB including 6,000 matches of synthetic data. The models themselves are 14 MB (step 1 with optimiser state)
  and 1.3 MB (step 2) in memory, 2.9 MB for all checkpoints on disk (zstd).
- A first update from 500 records was rejected four times before one beat the baseline on every metric (the replay
  sample grows meanwhile): cold start wants a first batch of ~2,000 records.

### Recommendation latency (release, after training)

| call | time |
|---|---|
| `recommend`, tier A (model: 5 sections × 8 options × 64 situations) | 2.9-5.7 ms |
| `recommend`, tier B (counts only) | 0.1-0.2 ms |
| `live` (model and counts) | 0.6-1.1 ms |

### Not verified

- **Real data.** Everything above is synthetic, with effects planted where the features can find them. Real
  matches have confounding (players choose builds because of the game), a different signal-to-noise ratio and
  correlated choices; the counts' WPA and the model's estimates have not been checked against real games.
- **GPU.** There is no GPU on the build machine. The GPU path (`backend::CudaAccelerator` through
  `bf_core::gpu::cuda`, the shipped similarities kernel) was not run; real-GPU timing and agreement were not
  measured. The checks and the fallback were exercised with a stand-in accelerator in the tests. bf-core has no GPU
  kernel for the training forward pass, so training runs on the processor even with a GPU.
- **burn / candle** were not evaluated: the network is ~52,000 parameters in plain Rust and deterministic as it is.
- **Windows.** Measured on Linux only; peak memory comes from /proc (not reported on Windows).
