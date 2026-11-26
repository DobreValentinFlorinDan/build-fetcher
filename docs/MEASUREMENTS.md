# Build Fetcher 1.0: measurements

Each part records what it measured, how, and what was not verified.

## App

Measured on 2026-11-24 on the Linux build machine (4 cores, no GPU, Rust 1.97.0, Node 22.23.5), with the in-memory
fakes and the stub `BuildSource` (`crates/server/src/fakes.rs`): the real store and learner were not wired in yet, so
these figures are the server's own cost, before the database and the model.

**Server, headless (`bf-server`, release build):**

```
cargo build --release -p bf-server
BF_CONFIG_DIR=<tmp>/cfg BF_DATA_DIR=<tmp>/data PORT=5199 BF_WEB_DIR=$PWD/dist ./target/release/bf-server
```

A Python script started the process, polled `GET /api/health` every 5 ms, then read `VmRSS` from `/proc/<pid>/status`.

| what | result |
|---|---|
| start → first `/api/health` answer | 72 ms (the listener is up before Data Dragon loads; it loads in the background) |
| resident memory right after start | 21 MB |
| resident memory idle after 30 s, Data Dragon 16.23.1 loaded (173 champions, 863 items), no League client | 41 MB |
| `GET /`, `GET /overlay` | serve the built dashboard (`dist/index.html`) |

Data Dragon was fetched from the CDN on the first start (cold cache in `<tmp>/data/ddragon`).

**Web bundle (`npm run build`, Vite 8):**

| file | size | gzip |
|---|---|---|
| dist/index.html | 0.39 kB | 0.26 kB |
| dist/assets/index-*.css | 16.69 kB | 4.09 kB |
| dist/assets/index-*.js (React 19 + dashboard + overlay) | 263.71 kB | 80.97 kB |

No web fonts or other remote assets; icons come from Data Dragon's CDN at run time.

**Tests:** `cargo test -p bf-server -p bf-desktop` (38 + 9 tests), `npm run test:web` (typecheck and 6 unit tests).

**Not verified here:**

- The Windows UI: the Tauri app (`desktop/tauri`) is not built on Linux (no WebKitGTK on this machine); only Windows CI
  builds it and its NSIS installer. The tray, single instance, splash, window placement, WebView2 renderer priority and
  the installer's per-user install and uninstall (keeping `%APPDATA%\build-fetcher`) were not run.
- A real League client: champ select, the gameflow phase and the import were tested against recorded-shape fixtures
  and a fake LCU (`crates/server/src/lcu/importer.rs`, `api_tests.rs`), not a running client. Whether the client accepts
  each rune page and item set as sent was not checked.
- A real game: the Live Client parser was tested on a fixture shaped like `/liveclientdata/allgamedata` and on the
  simulator (`POST /api/dev/simulate-live`); the overlay on top of a running game (topmost, click-through, focus) was not
  seen.
- Windows' idle time (`GetLastInputInfo`): compiled only for Windows; on Linux the activity treats the PC as idle.
- Memory and start time with the real store (SQLite) and learner wired in, and the desktop app's memory with WebView2.

## Store

Machine: the build machine (Linux container, 4 cores, no GPU), release build, 2026-11-24. Synthetic data from
`bf_core::testkit::Synth` (seed 7): 50,000 games over 3 patches (16.17-16.19) and 3 platforms, 10% with a full
timeline, 3% with one defect for the filters, Irelia featured in 5% of games and tier A.

### Commands

```
cargo build --release -p bf-store --example measure_store
time target/release/examples/measure_store 50000 7 0.1        # batches of 1000 games per transaction
target/release/examples/measure_store 50000 7 0.1 50          # batches of 50 (closer to the fetcher, which ingests one game at a time)
cargo test -p bf-core --lib riot::client -- --nocapture # request counts under the shared-key budget (simulated Riot)
cargo test -p bf-store --test scheduler -- --nocapture  # the whole fetcher on the simulated Riot
```

`/usr/bin/time` is not installed on the build machine; peak memory is the process's RSS sampled after each batch
(`bf_core::sys::own_rss`).

### Ingestion and size (50,000 games)

| figure | value |
|---|---|
| outcomes | 48,060 counted (7,609 kept as records: 4,427 with a timeline, the rest tier A without), 463 to the validation sample, 1,477 dropped (queue 296, remake 321, leaver 288, roles 278, malformed 294: every defect put in was caught, by the right reason) |
| raw match-v5 JSON these games are (compact) | 796.8 MB |
| database on disk | 45.9 MB (48,111,616 bytes) |
| by part (SQLite dbstat) | counts 35.1 MB, records 8.0 MB, validation 0.7 MB, pool/dedupe 2.0 MB, free 0.1 MB |
| count rows | 681,508 (+ 2,624 option keys, 1,185 champion-role-patch volumes) |
| bytes per count row (counts, options and volumes pages / count rows) | 53.9 |
| count rows per counted game | 14.2 (sublinear: rows aggregate; 38.7 per game at 5,000 games) |
| database bytes per counted game | 992 |
| record blob (zstd, dictionary) | 1,256 B with a timeline, 324 B without; 1,101 B per record with its table and index |
| validation record | 1,548 B (with table overhead) |
| ingestion throughput, batches of 1000 | 2,804 games/s (17.8 s; generation 6.4 s more) |
| ingestion throughput, batches of 50 | 1,062 games/s (47.1 s) |
| peak RSS, batches of 1000 | 218 MB, nearly all of it the generated JSON of one batch (≈16 MB of JSON as `serde_json::Value`s); 60 MB at the end |
| peak RSS, batches of 50 | 51.6 MB |
| total run (generation + ingestion + clean-ups), batches of 1000 | 28.5 s wall |

### Clean-up (same database)

| step | time | freed | after |
|---|---|---|---|
| learner's window 16.18-16.19 (records of 16.17 deleted: 2,510) | 77 ms | 2.6 MB | 43.2 MB |
| budget 25.9 MB (60% of the above): 16.17 then 16.18 trimmed (454,015 count rows) | 720 ms | 25.9 MB | 17.4 MB, under budget |

The validation sample (463 records) was intact after both. Space is returned with `PRAGMA incremental_vacuum` and
a WAL checkpoint, so clean-up never needs free disk space for a copy of the database.

### Requests and matches per day (theory, per routing value)

Limits: 100 requests / 120 s per routing value for a personal or development key (Riot's documentation as
summarised in docs/DESIGN.md; europe = EUN1 + EUW1 together, asia = KR). That is 72,000 requests/day at most. The
pacer keeps to 90 per 120 s plus a burst of 3 (safety 0.9), i.e. ≈ 64,800/day. Measured on the simulated Riot
(`riot::client` tests): 455 requests in 10 minutes alone (≈ 65,500/day, peak 92 in any bucket); with a second app
on the key spending 40 per 2 minutes, 252 of ours beside its 201 in 10 minutes, no 429 for either app, and the
pace back to full (219 in the next 5 minutes) once it stopped.

Request mix per new game (estimates, not measured on live data):

| part | requests per game | why |
|---|---|---|
| match | 1 | |
| timeline | ≈ 0.33 | every game with a tier A champion (5 tier A champions among 10 picks: ≈ 26% of games) + 10% of the rest |
| match-id list | ≈ 0.25 | 20 ids per list; most ids of a Master+ player checked every 6 h are new or already seen from other players: assumed 1 list per 4 new games |
| league-v4 | ≈ 0 | 3 apex ladders per platform per day (+8 Diamond pages when widening), on the platform hosts' own budgets |

≈ 1.58 requests per game → ≈ 41,000 games/day per routing value with the key to ourselves (≈ 82,000/day for
europe + asia). With Irelia Companion spending 40 requests per 2 minutes on the same routing value: (92 − 40) per
120 s ≈ 37,400 requests/day → ≈ 23,700 games/day for that routing value. With no tier A champions (timelines for
10% only): ≈ 1.35 requests per game → ≈ 48,000 games/day alone.

Disk: at ≈ 1 KB per counted game (and falling as rows aggregate), 2 GB holds well over 2 million games of counts;
the default budget is not the limit for the first weeks of a patch.

### Not verified

- **No real Riot key and no real fetching**: nothing here contacted Riot. Request rates come from the simulated
  Riot (`bf_core::testkit::SimRiot`, which models fixed 120 s / 1 s buckets per routing value starting with their
  first request, as Riot documents), not from Riot. Whether Riot's buckets behave exactly so, and how
  X-App-Rate-Limit-Count counts requests that got a 429, is not verified.
- **The request mix** (timelines and id lists per game) is an estimate; the share of new ids per list depends on
  real players' activity.
- **Record sizes are for synthetic games**: frames have jitter added, but real timelines (more events, uneven
  purchases, ward and level-up noise) will be larger. Expect 2-3 KB per timeline record; not measured.
- The leaver rule on end-of-game numbers (`timePlayed` under 80% of the game; under 170 gold/min with under
  1.5 CS/min) and the lane areas used to check roles are judgement calls tested on synthetic data only.
- Windows file-system behaviour (sizes, the read-only open of Irelia Companion's folder) was tested on Linux only;
  the permission-denied case can't be produced as root and was skipped there.

## Learning

Agent B, 2026-11-24. The build machine had 4 cores (Intel Xeon @ 2.10 GHz), 15 GB of memory and no GPU. Other
agents were compiling at the same time, so timings are indicative to about ±30%.

### Command

```
cargo run --release -p bf-learn --example measure_learn -- 20000 2000 2000      # matches, validation matches, records per update
cargo run --release -p bf-learn --example measure_learn -- 6000 1000 2000 1     # the same on 6,000 matches with 1 thread
cargo run --release -p bf-learn --example measure_learn -- 6000 1000 2000 4     #                                 4 threads
cargo run --release -p bf-learn --example measure_learn -- 6000 1000 500 4      # smaller updates (500 records)
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

These are the numbers after integration, with the calibration line added (see "Integration" below). The figures Agent B
measured before it are in the second table for comparison.

| model (after integration, 2026-11-25) | log loss | Brier | calibration error 0-10 / 10-20 / 20-30 / 30+ min |
|---|---|---|---|
| baseline: champion-role win rate | 0.68988 | 0.24837 | 0.0052 / 0.0052 / 0.0047 / 0.0087 |
| step 1: logistic per champion-role, shrunk to global | 0.49708 | 0.16815 | 0.0059 / 0.0176 / 0.0138 / 0.0209 |
| step 1 + step 2: shared network on top | **0.49697** | **0.16814** | 0.0044 / 0.0186 / 0.0127 / 0.0219 |

| model (Agent B, before integration) | log loss | Brier | calibration error 0-10 / 10-20 / 20-30 / 30+ min |
|---|---|---|---|
| step 1 | 0.49759 | 0.16845 | 0.0089 / 0.0213 / 0.0135 / 0.0322 |
| step 1 + step 2 | 0.49672 | 0.16800 | 0.0058 / 0.0189 / 0.0099 / 0.0288 |

- On this set the calibration line found no significant change (the Wald test never passed), so the model is the
  same kind as before. The differences come from training on 6 of every 7 new records (the 7th is held out for the
  calibration). Results: 6 of 10 updates accepted (logistic in 1, 2, 3, 8, 9; network in 3, 6, 9), against 4 of 10
  before.
- The baseline's calibration error is low because it predicts nearly the same number for everyone: calibration of a
  near-constant is trivially good. The guardrail therefore compares calibration with a cluster-robust noise floor
  (what a perfectly calibrated model with the same predictions shows by sampling alone; teammates share a label, so
  each match-team counts once). At this validation size the floors were about 0.009 / 0.015 / 0.012 / 0.023.
- Once the model has converged, the strict "no metric worse" rule rejects updates on noise (update 5: log loss
  0.49874 vs 0.49837). The user chose to keep the rule strict and double the store's validation sample (2,000 to 4,000
  games) instead; this measurement uses its own 2,000-match validation set.

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
  and 1.3 MB (step 2) in memory, 2.9 MB for all checkpoints on disk (zstd; 4.2 MB after integration's run, with more accepted versions).
- A first update from 500 records was rejected four times before one beat the baseline on every metric (the replay
  sample grows meanwhile): cold start wants a first batch of ~2,000 records.

### Recommendation latency (release, after training)

| call | time |
|---|---|
| `recommend`, tier A (model: 5 sections × 8 options × 64 situations) | 2.9-5.7 ms (7.2 ms after integration) |
| `recommend`, tier B (counts only) | 0.1-0.2 ms (0.5 ms after integration) |
| `live` (model and counts) | 0.6-1.1 ms (1.0 ms after integration) |

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

## Integration (lead)

The lead, 2026-11-25. Same machine: 4 cores, 15 GB, no GPU. The three parts run wired together, as `wiring.rs`
connects them.

### Store → learner → recommender on the store's synthetic games

```
cargo test --release -p bf-server --test integration -- --nocapture
```

The input is 4,000 `bf_core::testkit::Synth` games: half with timelines, Irelia mid in a third of them, 3% with a
defect. They go through the real `Store`, then the real `Learner` trains on it, then the real `Recommender` answers.

| step | result |
|---|---|
| ingestion | 4,000 games in 5.1 s; 2,460 records kept (tier A + timelines), 117 dropped by the filters; 50 re-sent games all duplicates |
| first update (2,000 records) | **accepted**: log loss 0.699 → 0.486, Brier 0.253 → 0.163; calibration temperature 1.43 + 0.50 per 10 minutes |
| second update (460 records) | rejected (log loss 0.4885 > 0.4856): the strict rule |
| recommendations | Irelia mid from the model (tier A), ≥ 5 sections with options; the thinnest pair (Nocturne mid, 8 games) from counts with 45 options flagged thin |
| clean-up after training | 30.3 MB → 20.5 MB (9.8 MB freed) in 84 ms; validation sample intact |

**What integration found and fixed.** On these games every first update was rejected at first. Log loss and Brier
improved a lot, but calibration error per window rose from about 0.03 to 0.08-0.11: the game state is very
informative late in Synth's games, and one update's Adam steps left the model under-confident there. The fix is a
calibration line over game time, (a0 + a1 · t) · z, fitted on 1 record in 7 held out of training (never the
validation sample).
- It changes only when a Wald test with a cluster-robust covariance (matches as clusters) passes at 95%, with at
  least 250 held-out matches.
- Two earlier attempts each fixed one synthetic set and broke the other. A per-window fit counted examples one by one
  and overfitted small windows (one match gives dozens of examples with one outcome). A faster learning rate for the
  state weights made small early updates over-confident on the learner's own set.

### The app, headless, with the real parts

```
cargo build --release -p bf-server
BF_CONFIG_DIR=<empty folder> BF_IRELIA_CONFIG_DIR=<empty folder> PORT=5199 target/release/bf-server
```

- First `/api/health` answer **33 ms** after start.
- Data Dragon 16.23.1 (173 champions, 863 items) loaded from the CDN; then the database opened and the learner
  started on the CPU.
- Without a key, fetching shows `paused: "no API key"`.
- Resident memory **22 MB** at start and **48 MB** idle after 30 s (peak 50 MB).
- On disk: 1.2 MB (the empty database, 0.9 MB of Data Dragon JSON).

### Disk and request budget, extrapolated (not measured on real data)

These use the store's own figures, about 1 KB of database per counted game and 1.58 requests per new game.
- At the key's limit, each routing value (EUROPE, ASIA) gives about 41,000 games a day with the key to itself, and
  about 23,700 while Irelia Companion spends 40 requests every 2 minutes.
- So with both routing values the database grows by about 50-80 MB a day before clean-up.
- At the default 2 GB budget, clean-up starts trimming the oldest patches after roughly 3.5-6 weeks, two to three
  patches.
- Real timelines are bigger than synthetic ones: the store expects 2-3 KB per kept record, not measured.

### Not verified (whole project)

- **Real Riot data and 24 hours of real fetching.** No real API key was used anywhere (by rule, and none is available
  in the build environment). Requests per day, matches per day, real record sizes, the data-quality thresholds on
  real games, and model quality on real games remain to be measured on the user's PC.
- **A real GPU.** There is no GPU on the build machine.
  - The CUDA forward-pass kernel (hand-written PTX) was checked by an interpreter against the processor, bit for bit.
  - Not checked: the driver compiling it, and its speed.
  - The similarity kernel is irelia-companion's, proven there.
  - GPU update times: not measured.
- **Windows.** All measurements here are Linux. CI builds and tests on Windows (and builds the NSIS installer), but
  memory, timings, the overlay on a running game, the tray and the League client import were not exercised on
  Windows.

## 1.2.0: enemy items, continuous learning, gold in the overlay

All on Linux and synthetic data (`crates/learn/examples/measure_continuous.rs`, release build).

```
cargo run --release -p bf-learn --example measure_continuous -- 4000 6000 1 150 1
```

Each run: a first model from 4,000 games, then 6,000 more arriving one at a time, 1,500 held-out matches.

### Continuous learning against batches (same 6,000 games, armor effect planted)

| how new games are learnt | checks | accepted | final held-out log loss |
|---|---|---|---|
| 1.1: batches of 500 (`train_step`) | 12 | 2 | 0.49368 |
| one game at a time, full step, 2 passes | 40 | 0 | 0.49640 (never left the first model) |
| groups of 10, step × 0.3, 1 pass | 40 | 8 | 0.49142 |
| groups of 10, step × 0.1, 1 pass | 40 | 13 | 0.49117 |
| **one game at a time, step × 0.1, 1 pass (default)** | 40 | 12 | **0.49112** |
| one game at a time, step × 0.3, 1 pass | 40 | 5 | 0.49167 |

At the full step, one game's examples share one label per team, so every game pulled the draft its way. Each check
then found it worse than the model in force (log loss 0.505–0.519 against 0.496).

- **Time:**
  - learning one game: **5.6 ms** (both model steps; a copy of the draft is made first, so an interrupted call
    changes nothing);
  - a check: **0.48 s** on 1,500 held-out matches. The app's held-out sample grows to 4,000 matches: about 1.3 s,
    extrapolated.
- **Memory:**
  - The logistic model is 15.0 MiB in memory, 13.5 MiB before 1.2.0 (+65,536 slots × 3 numbers).
  - The draft is a second copy, and each call makes a third for a moment: about +30 MiB while learning
    continuously.
  - Process peaks in these runs: 410–630 MB. Most of it is the 10,000-game synthetic source held in memory.
- **Disk:** the largest model file is 1.2 MB, the same with and without the enemy features; only touched slots are
  written. The checkpoint folder held 2.6 MB after 12 accepted models (old ones are pruned).

### The enemy's items in the model

| synthetic set | enemy features | first model: log loss, train time | after 2,000 more games |
|---|---|---|---|
| no effect planted on enemy items | off (1.1.1 features) | 0.50490, 8.3 s | 0.50362 |
| no effect planted on enemy items | on | 0.50580, 8.8 s | 0.50427 |

- **What it costs:** where the enemy's items carry no signal, the extra inputs cost about 0.0007–0.0009 log loss
  (noise) and 6% training time.
- **Where an effect is planted** (`ITEM_VS_ARMOR`: Lord Dominik's Regards +0.6 logit against 7+ enemy armor items,
  −0.1 otherwise), the test `enemy_armor_moves_the_item_advice` passes. With 3 items, at 30 minutes, the model adds
  **+3.4 pts** for Lord Dominik's against 9 enemy armor items and **−0.7 pts** against none. That's about a quarter
  of the planted size, because L2 shrinks it.
- **What it took:**
  - **Per-item enemy features and raw item counts were dropped.** Counts grow with game time. With them the first
    model was overconfident late and failed its calibration check: 30+ min 0.054 against a 0.031 noise floor. Tags
    are now shares of the enemy's build (at least 8 items counted) × 4.
  - **Item × tag pairs are limited to armor, magic resist, healing, anti-heal and health.** Damage type already
    meets items through the damage profile.
  - **The item × tag half has its own, lighter L2 (3e-3).** At the global part's 1e-2 the planted effect came out
    at a tenth of its size.
- **Old checkpoints:** the new weights sit after the pair region and start at zero. A 1.1 checkpoint loads and
  predicts exactly as before (test `checkpoints_before_the_enemy_region_load_and_predict_unchanged`). 1.1 cannot
  load a 1.2 checkpoint: "checkpoint slot out of range".

### Request cost of timelines (estimated from the 1.0 figures, not measured on real data)

The 1.0 store figures were 1.35 requests per new game with 10% timelines (1.58 with tier A games too). Each extra
10% of timelines adds about 0.1 requests per game:

| timelines for other champions | requests per game (estimated) | games per hour, against 10% |
|---|---|---|
| 10% (before 1.2.0) | 1.35 | — |
| 25% (new default) | 1.50 | −10% |
| 50% | 1.75 | −23% |
| 100% | 2.25 | −40% |

### Not verified in 1.2.0

- **Real data:**
  - whether the enemy's items improve the model on real games;
  - how often real checks are accepted;
  - the real request cost per timeline share.
- **The overlay on Windows in a live game:** gold, the part to buy now, and reasons. CI builds it; nobody looked at
  it in a game.
- **CPU load of continuous learning while gaming outside League:** each game takes milliseconds and a check about a
  second every few minutes, but this was not measured on the user's PC.
- **The first model's calibration check stays fragile on synthetic data.** With 2,000 games, some seeds fail it on
  one window with or without the enemy features (seed 5: the 0-10 min window 0.033 against 0.026 allowed). This
  was so before 1.2.0. The app retries with the next 2,000 games.

## 1.3.0: stats, combat math, team network

Linux, synthetic data, release build.

```
cargo run --release -p bf-learn --example measure_continuous -- 4000 1500 1 150 1
```

| | 1.2.0 | 1.3.0 |
|---|---|---|
| First model (4,000 games) | 10.1 s | 10.8 s |
| Learning one game | 5.6 ms | 6.1 ms |
| A check (1,500 held-out matches) | 0.48 s | 0.79 s |
| Checks accepted on the first 1,500 new games | — | 6 of 10 (log loss 0.4965 → 0.4921) |
| Logistic model in memory | 15.0 MiB | 16.5 MiB |
| Largest model file | 1.2 MB | 1.2 MB |

- **Team network:** 68,745 weights (1.6 MiB with its optimiser state).
  - **First check**, after waking on a 5,000-game set: rejected, log loss 0.50725 against 0.50718 for steps 1 + 2.
  - **Time:** the check took 567 ms, 312 ms of it evaluation.
- **Stat features where nothing was planted on stats:** the first model's calibration in the 20-30 min window moved
  from 0.0266 (1.2.2) to 0.0278 (1.3.0) on the armor test's set; log loss 0.51840 to 0.51877.
- **Not measured:**
  - a planted stat effect (for example "attack speed pays off on champion X") learnt through the new features;
  - the team network on a large set;
  - its CPU use once awake on real data.
