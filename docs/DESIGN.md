# Build Fetcher 1.0: design

Build Fetcher recommends a build for any champion in any role: runes, summoner spells, skill order, starting items,
item path, boots and late-game swaps. It learns from new matches as they arrive and runs on one Windows PC.

It started from a copy of irelia-companion 2.2.0 (see NOTICE). This file is the plan, and the hand-over between the
people and agents working on it.

## Rules that bind every part

- **Local only.** The app talks to four things and nothing else:
  - the Riot web API, with the user's own key;
  - Data Dragon;
  - the League client's LCU API on 127.0.0.1;
  - the Live Client Data API on 127.0.0.1:2999.

  No telemetry and no server. Crash reports stay on disk.
- **Vanguard-safe.** No game memory access and no injection.
- **The API key** never goes into the repository, a log line, a test, an error message, a crash report or the
  dashboard's JSON. Show it only through `bf_core::config::mask_key`. Tests use obviously fake keys
  ("RGAPI-00000000-test").
- **Irelia Companion's folder** (`%APPDATA%\irelia-companion`) is opened read-only. Only `config.json` → `riotApiKey`
  is read, and only after the user clicks. Nothing is ever written there.
- **Data folder** is `%APPDATA%\build-fetcher`:
  - `settings.json` holds the settings;
  - `data/` holds the database, Data Dragon and checkpoints;
  - `logs/` holds the logs.

  `BF_CONFIG_DIR` and `BF_DATA_DIR` override these, for tests.
- **Hidden names are never shown.** Champ select data become champions and roles only. Ranked solo/duo champ select
  hides other players' names, and Riot requires apps not to reveal them. 1.0 never reads names or puuids from champ
  select, except the user's own.
- **The overlay highlights choices; it does not dictate them.** Riot's game-integrity policy says products "should
  not remove game decisions, but may highlight decisions that are important and give multiple choices". So:
  - in-game advice is always several ranked options, with evidence;
  - nothing pops up or alerts on game state;
  - no enemy cooldown or timer tracking.
- **Legal notice.** The README and the dashboard's About carry Riot's "isn't endorsed by Riot Games" notice.
- **Every change is measured:** results, disk size, time, memory. Say what was not verified.

## Riot API facts this design relies on

These come from research on 2026-11-24. The developer portal was not reachable from the build machine, so some
points are from search snippets and still need checking on the portal.

- **Development and personal keys:** 20 requests / 1 s and 100 requests / 2 min. These are counted per routing value:
  `europe` and `asia` each have their own limit, as do `eun1`, `euw1` and `kr`. Method limits come on top.
  - Development keys expire every 24 h.
  - A personal key is for the owner's own use, which is our case.
- **Headers:**
  - `X-App-Rate-Limit: 20:1,100:120` gives the limits.
  - `X-App-Rate-Limit-Count: 3:1,57:120` gives the counts Riot has seen, from every app that uses the key. This is how
    we see Irelia Companion's usage.
  - `X-Method-Rate-Limit` and `X-Method-Rate-Limit-Count` work the same way for methods.
  - On a 429, `Retry-After` is in seconds, and `X-Rate-Limit-Type` is `application`, `method` or `service`. A service
    429 can come without `Retry-After`; back off exponentially.
- **match-v5** is on the regional hosts. EUN1 and EUW1 go to `europe.api.riotgames.com`; KR goes to
  `asia.api.riotgames.com`.
  - `/lol/match/v5/matches/by-puuid/{puuid}/ids?queue=420&type=ranked&startTime=&start=&count=`
  - `/lol/match/v5/matches/{id}`
  - `/lol/match/v5/matches/{id}/timeline`
- **league-v4** is on the platform hosts (`eun1`, `euw1`, `kr`.api.riotgames.com).
  - `challengerleagues`, `grandmasterleagues` and `masterleagues` `/by-queue/RANKED_SOLO_5x5`
  - `/entries/RANKED_SOLO_5x5/DIAMOND/{I..IV}?page=`
  - Entries carry `puuid`; `summonerId` is deprecated, so key everything on puuid.

## Workspace

```
crates/core    bf-core   carried over: codec, jsmath, js, util, sys, app_config, config, riot/{client, rate_limiter,
                         routing}, ddragon (all champions), engine/{exclusivity, item_catalog, items}, gpu/{cuda, log},
                         testkit (synthetic match-v5 + timeline JSON, FakeRiot, the Data Dragon snapshot);
                         build.rs  = THE CONTRACT (types + traits below)
crates/store   bf-store  Agent A
crates/learn   bf-learn  Agent B
crates/server  bf-server Agent C (+ integration): axum + /ws, LCU, Live Client, importer, wiring store → learn → UI
crates/desktop bf-desktop Agent C: shell logic without a window toolkit
desktop/tauri  build-fetcher Agent C: Tauri v2 app, tray, overlay, NSIS
web/           Agent C: React dashboard + overlay page
fixtures/      Data Dragon snapshot 16.19.1 (all 173 champions, items, runes, summoners)
```

Dependency rule:
- `store` and `learn` depend on `core` only, never on each other.
- `server` depends on all three and wires `bf_store`'s `DataSource` implementation into `bf_learn`'s `BuildSource`.

## The contract (`crates/core/src/build.rs`)

**Types:**
- **Identities:** `Role`, `Patch`, `GameVersion`, `Platform` (→ `Routing`), `RankBand`, `DamageProfile`.
- **`BuildOption { kind, key }`:** canonical keys, built only through its constructors.
- **`MatchRecord` / `PlayerRecord` / `Frame` / `TeamEvent`:** the compact form of a match. This is what the store
  keeps and the learner reads.
- **`OptionCounts`, `CountQuery`, `CountContext`, `PairVolume`:** tier B counts.
- **`DraftContext`, `LiveContext`** go in; **`BuildRecommendation`** (sections of `RankedOption`s) and **`LiveAdvice`**
  come out.
- **Status for the UI:** `Activity`, `FetchStatus`, `BudgetStatus`, `StorageStatus`, `ModelStatus`,
  `CheckpointInfo`, `Metrics`.

**Traits:**
- `DataSource` is implemented by the store and read by the learner.
- `BuildSource` is implemented by the learner and read by the server.

**Changing the contract:** add fields or variants only. Never rename or remove one without the integrator's
agreement, and write each change in the hand-over section of this file.

## Agent A: store (`crates/store`)

1. **Identity and settings**
   - `settings.rs`: typed settings over `bf_core::app_config`:
     - regions (default EUN1, EUW1, KR)
     - rank floor (Master+, widening to Diamond)
     - disk budget (2 GB)
     - tier A champions, added by the user and detected
     - GPU on/off
     - learning on/off
     - the user's Riot ID / puuid
     - `riotApiKey`
   - Defaults, validation, round-trip tests.
2. **Key import** (`key_import.rs`)
   - `irelia_key_available() -> KeyOffer`: `Available` / `Missing` / `Empty` / `Unreadable(reason)`. It returns
     neither the key nor any other field.
   - `import_irelia_key() -> Result<()>` runs on the user's click. It reads only `riotApiKey` from
     `%APPDATA%\irelia-companion\config.json` (`BF_IRELIA_CONFIG_DIR` overrides the folder for tests) and writes it
     into Build Fetcher's settings.
   - `set_key(new)` stores a pasted key.
   - Tests cover a present, missing, empty and unreadable file. They also prove nothing was written to Irelia's file:
     bytes and mtime unchanged, the file opened read-only, and no other field read or copied.
3. **On-disk format**
   - SQLite (bundled rusqlite), `data/build-fetcher.db`, with a schema version.
   - Tables:
     - `matches(seq, match_id UNIQUE, patch, build, platform, rank, start_ms, has_timeline, blob)`. The blob is a
       `MatchRecord` in a compact binary form (quantized: gold to 10s, seconds to whole), zstd-compressed with
       `bf_core::codec`.
     - `counts(patch, champion, role, rank, opponent, profile, kind, key, games, wins, expected_wins)`, with a primary
       key over the context.
     - `seen(match_id)`: dedupe for matches processed into counts but not kept as records.
     - `validation(match_id, blob)`: the fixed-size held-out sample.
     - `players(puuid, platform, rank, last_checked_ms)`: the sourcing pool.
     - `meta`.
   - Raw API answers are never stored. A small, capped answer cache is allowed (≤ 20 MB; LRU on bytes).
4. **Ingestion**
   - match-v5 JSON (+ timeline) → validated → `MatchRecord` → counts for all 10 players.
   - Records are kept only for tier A champions and the timeline sample.
   - Dedupe by match id across players and regions.
   - Patch and hotfix come from `gameVersion`.
5. **Data quality**
   - Keep only queue 420 on map 11.
   - Drop:
     - remakes (< 300 s, or `gameEndedInEarlySurrender`);
     - games with an AFK or leaver (`teamEarlySurrendered`, a player with 0 gold after 10 min or not in the timeline,
       `leaver`-style flags);
     - games whose roles can't be assigned with confidence. `teamPosition` must be complete and unique per team, and
       when a timeline exists it is checked against positions at 2-10 min, with jungle and lane spots.
   - Reject malformed or partial answers.
   - Count every drop by reason, and expose the counts in `FetchStatus.dropped` and the log.
6. **Fetch scheduler with the shared-key budget**
   - Read `X-App-Rate-Limit` **and `X-App-Rate-Limit-Count`** on every answer, per routing value.
   - Our own count versus Riot's count shows how much another app spent. Never start a request that Riot's count
     says would exceed the limit. Follow `Retry-After` on a 429.
   - Budget each routing value separately (europe for EUN1+EUW1, asia for KR, and each platform host for league-v4).
   - Order of work:
     1. the user's own new games (EUN1);
     2. tier A champions;
     3. thin champion-role pairs, widening to Diamond automatically;
     4. popular pairs.
   - Pause in champ select and in game (an `Activity` watch).
   - Timelines are fetched for tier A games plus a sample for tier B.
   - Tier A is the user's top 5 by games played, plus the ones added in Settings, plus any champion played 3 times in
     the current patch.
   - `status() -> FetchStatus` reports requests used, matches per hour and data age per pair.
   - Tests:
     - the budget with a fake Riot that returns count headers;
     - a second app spending the same key, simulated: the fake's count rises without our requests;
     - 429 with and without `Retry-After`;
     - the priority order;
     - the pause.
7. **Clean-up** (`cleanup.rs`)
   - Called after each training cycle with the learner's window. Keep:
     - the checkpoint (the learner's folder);
     - the counts;
     - the validation sample;
     - records inside the learning window.
   - Delete everything else, and trim the oldest patches while over the disk budget. Then `VACUUM` or incremental
     vacuum, and report the bytes freed.
   - `storage_status() -> StorageStatus`.
8. `impl DataSource for Store`.
9. **Measure** on a synthetic set (testkit-generated matches at scale, e.g. 50k): bytes per match (records and
   counts), ingestion time, and memory. Extrapolate requests and matches per day from the budget.

## Agent B: learning (`crates/learn`)

1. **Features** from `MatchRecord` for a player:
   - champion, role, ally and enemy comps, lane opponent;
   - runes, summoners, items owned and their order;
   - game state at a minute t: time, gold, CS and level difference to the lane opponent, kills, towers, dragons,
     barons.

   Training examples are (record, player, minute) samples. Label = the player's team won. The user's own games
   (`is_me`) get more weight.
2. **Model steps**, each shipped only if it beats the previous one on held-out games:
   1. Logistic regression per champion-role, with L2 and a shrinkage prior to the global model. Warm-started
      mini-batch updates (SGD/Adam), not refits from scratch.
   2. A small shared network: shared layers for all champions, with champion and item embeddings. It is plain Rust
      first; burn or candle are adopted only if measured better (MIT/Apache only; ask before adding a paid or
      closed-source dependency).
3. **Guardrails**
   - Every update is evaluated on `DataSource::validation_records`: log loss, Brier, and calibration error per
     10-minute window.
   - If any metric gets worse, the update is rejected and the previous checkpoint is kept.
   - Checkpoints are versioned files under `data/checkpoints/` and can be rolled back with one call.
   - A learning log says what each update changed.
4. **Determinism and backends**
   - On the CPU, results are deterministic for the same data: fixed seeds and fixed summation order (no
     thread-count-dependent reductions).
   - The GPU path goes through `bf_core::gpu::cuda` (NVIDIA, loaded at run time) or a measured cross-vendor library.
   - It is checked against the CPU on a sample within a stated tolerance. Any mismatch, or no GPU, falls back to the
     CPU, which is identical in function.
   - Heavy work runs only when `Activity::Idle`. Time and memory per update go in the log.
5. **Recommendations** (`impl BuildSource`)
   - For each `OptionKind`, rank options by predicted WPA in contexts like the user's (tier A: the model; tier B:
     counts with `expected_wins`).
   - Show plain counts and sample sizes beside them. Weight by recency (patch and hotfix) and rank.
   - Fall back matchup → profile → lane → champion with shrinkage when data are thin, and flag `thin`.
   - `live()` re-ranks the next buys and counter items from `LiveContext`. It always returns several options.
6. **Tests:**
   - the guardrails reject a worse update;
   - rollback works;
   - determinism;
   - CPU versus GPU (or a stand-in accelerator) agreement, and the fallback;
   - recommendations for a thin-data champion fall back and flag.
7. **Measure** on a synthetic set: held-out log loss and calibration of each model step, and update time and memory on
   the CPU. The GPU is not available on the build machine; say so.

## Agent C: app (`crates/server`, `crates/desktop`, `desktop/tauri`, `web/`)

1. **Bring the carried-over crates back into the workspace** under the new names: `bf-server`, `bf-desktop`, and
   `build-fetcher` (the Tauri app). Keep:
   - the LCU connector, https and importer;
   - the Live Client tracker and the live simulator;
   - game mode, keep awake and the overlay placement.

   Remove the Irelia-specific parts.
2. **Identity:**
   - the Tauri `productName` "Build Fetcher", identifier `com.buildfetcher.app`, version 1.0.0;
   - installer name `Build-Fetcher-Setup-1.0.0.exe`, tray name;
   - the NSIS hooks: no 0.x Irelia removal;
   - `package.json` name `build-fetcher`.
3. **Server:** axum REST and `/ws`, against the contract.
   - `AppState` holds `Arc<dyn BuildSource>` and the store's status.
   - Until integration, use a stub `BuildSource` and fake statuses.
   - Activity detection: champ select and game from the LCU and the Live Client; idle from the OS. It feeds the
     scheduler and the learner.
4. **Champ select**
   - Detect the user's champion and role (`assignedPosition`, else by the champion's most likely role).
   - Show its build at once, from tier A or tier B, and update it as picks come in.
   - On the user's click, import runes (a rune page) and an item set for any champion.
5. **In game:** the overlay shows next buys and counter items for any champion, re-ranked from Live Client data.
   Several options with evidence, no alerts.
6. **Settings:**
   - the API key: "Use the same key as Irelia Companion" when offered, "Paste a new key", and a masked display;
   - a 401/403 shown with a link to developer.riotgames.com;
   - regions and rank;
   - the disk budget (Storage, with sizes);
   - the tier A list;
   - GPU on/off;
   - learning on/off;
   - "Roll back model";
   - shared-key usage shown.
7. **First run:** set up the key as above, then the region. Fetching starts in the background.
8. **Dashboard:**
   - a request meter per routing value;
   - matches per hour;
   - data age per champion;
   - model status and log.
9. **Tests:**
   - the importer for any champion;
   - the champ-select parsing never exposes names;
   - the key import routes;
   - the API contract shapes.

## Integration (lead)

- Merge A, B and C. Wire `Store` → `Learner` → server, and the scheduler and learner to `Activity`.
- Get `cargo test --workspace` passing on Linux and Windows CI.
- Measure on a synthetic set.
- Write docs/releases/1.0.0.md, README and NOTICE.
- Open a PR to main. Merge only after the user approves.

## Hand-over log

- (user, 2026-11-24) Confirmed: the overlay shows several ranked options with their evidence. It never gives a single
  instruction and never pops up or alerts.
- (lead) Copy of irelia-companion 2.2.0 committed verbatim (04f0b5e). The next commit trims it to the reusable
  engineering, adds the contract, and adds the store and learn skeletons. Server, desktop and tauri are not yet in
  the workspace members; Agent C restores them.
- (C) `crates/server` (bf-server), `crates/desktop` (bf-desktop) and `desktop/tauri` (build-fetcher) are workspace
  members again; `desktop/tauri` is not a default member (no WebKitGTK needed for `cargo test` on Linux). No change to
  `crates/core`.
  - The server reaches the store, the learner and the key through `crates/server/src/handles.rs`: `StoreHandle`
    (fetch/storage status, volumes, settings view, activity, own account, game ended), `LearnerHandle` (status,
    rollback, activity), `KeyHandle` (Irelia offer, import, paste, masked key, key error), plus `BuildSource`.
    In-memory fakes and a stub `BuildSource` live in `fakes.rs`. The lead implements the traits over `bf_store` /
    `bf_learn` and returns them from `crates/server/src/wiring.rs::default_handles`, used by both `bf-server` and the
    desktop app.
  - Activity reaches the store and the learner through `App::activity_changed` (`set_activity`), and as a
    `tokio::sync::watch` from `App::activity.subscribe()`.
  - `LiveContext`'s pairs (`towers`, `dragons`, `barons`) are filled as (the user's team, the enemy team);
    `team_kills` / `enemy_kills` the same way.
  - The dashboard sends `AppSettings.firstRunComplete = true` when the first run ends; fetching may start then.
- (Agent A, store) `crates/store` delivered; `crates/core/src/build.rs` (the contract) is **unchanged**. Changes to
  other parts of `crates/core`, all additive except where noted:
  - `riot/rate_limiter.rs`: `observe_counts` reads X-App-Rate-Limit-Count / X-Method-Rate-Limit-Count; a request
    never starts when Riot's count (every app on the key) plus ours since plus the other app's expected use until
    the bucket ends would exceed the limit; Riot's fixed buckets are tracked (`first_sent`); the pace slows by the
    other app's rate. New: `parse_counts`, `WindowUsage`, `RateLimiter::app_windows`. Existing tests unchanged.
  - `riot/transport.rs` (new): `Transport` (one GET), `RawAnswer`, `HttpTransport` (reqwest; errors without URL).
  - `riot/client.rs`: `RiotClient<T: Transport = HttpTransport>` (so `RiotClient` alone still names the HTTP one;
    `RiotClient::new` unchanged); `with_transport(key, limits, pacing, transport, clock)`, `transport()`,
    `budget_statuses()`, `budget_scope()`; count headers fed to the limiter on every answer. `RiotApi` gains two
    methods with default bodies: `league_page` (one page of a Diamond ladder) and `budgets`.
  - `app_config.rs`: `read_config_at(file)` and `update_config_at(file, patch)` (the old functions call them).
  - `testkit.rs`: `make_match` also writes `mapId: 11`, `gameStartTimestamp`, `gameEndTimestamp` and
    `teamEarlySurrendered`; `make_timeline_for(m, spec, participant)` features any participant;
    `testkit::Synth` (seeded synthetic games at scale, any champion/role featured) and `testkit::SimRiot` (a
    simulated Riot with fixed-window counts and a second app on the key).
  - Wiring for the server: see the module comment of `crates/store/src/lib.rs`; measurements in
    docs/MEASUREMENTS.md ("Store").
- (Agent B) `crates/learn` delivered. **Contract change (additions only):** `DamageProfile::magic_share_of(data,
  champion)`, `DamageProfile::magic_share(data, champions)`, `DamageProfile::from_magic_share(share)` and
  `DamageProfile::of_team(data, champions)` in `crates/core/src/build.rs`. A champion's magic share is
  magic² / (attack² + magic²) from Data Dragon's 0-10 ratings; a team is Magic at ≥ 0.65, Physical at ≤ 0.35.
  **The store should compute `CountContext::enemy_profile` with `DamageProfile::of_team`** so counts and the learner
  mean the same thing. Nothing renamed or removed.
  - What the server calls: `Learner::open(dir, static_data, LearnConfig)`; `learner.train_step(&dyn DataSource,
    &Budget { max_records, deadline, activity })` in the background (it returns `NotIdle` unless `activity()` is
    `Idle`, and stops between mini-batches when it changes, the deadline passes or `learner.interrupt()` is called);
    `learner.status() -> ModelStatus`; `learner.rollback(version)`; `learner.set_learning(bool)`;
    `Recommender::new(Arc<dyn DataSource>, Some(learner), static_data, RecommendConfig)` as `Arc<dyn BuildSource>`.
    `LearnConfig::use_gpu` is the Settings switch. `Learner::disk_bytes()` is Storage's "checkpoints".
  - The checkpoint directory (`data/checkpoints/`) holds `manifest.json`, `vNNNNNN.logistic.bin` /
    `vNNNNNN.shared-nn.bin` (zstd), `contexts.bin` and `learning.log`. The store's clean-up must keep it.
  - The cursor is `records_after`'s sequence number: records are read once. A rejected update still advances it.
  - Tier A ranks the pre-game decisions (runes, summoners, skill max, starting items) by the model; the item path is
    ranked by counts in both tiers (the model's item effects are conditional on game state, which `live` uses).
  - Measurements and what was not verified: docs/MEASUREMENTS.md, "Learning".
  - Open points for the lead: the strict guardrail (log loss and Brier may not rise at all) rejects most updates once
    the model has converged; consider `Guardrail::log_loss_slack` ≈ 1e-4 or a larger validation sample. bf-core has
    no GPU kernel for the training forward pass, so only the recommender's similarities can run on a GPU.
- (lead, integration, 2026-11-25)
  - **Merged:** A (`agent-a/store`), B and C. The server wires the real parts in `crates/server/src/wire_store.rs`,
    `wire_learn.rs` and `wiring.rs` (`BF_FAKES=1` keeps the fakes).
    - The store opens once Data Dragon is loaded.
    - Fetching starts after the first run, once a key is set.
    - Training runs on its own thread only while idle, and clean-up runs after each training cycle.
    - The store's baseline for tier B counts stays the champion-role win rate. The model's prediction already
      includes the build options, so using it as the expected win would cancel the effect the counts measure.
  - **Damage profile:** the store now uses `DamageProfile::of_team` (one rule for counts and queries).
  - **Learner calibration** (found at integration on the store's synthetic games: every first update rejected for
    under-confidence late in the game):
    - a temperature line over game time, fitted on 1 record in 7 held out of training;
    - changed only when a cluster-robust Wald test passes, with at least 250 held-out matches.
    - Checkpoint format BFL2.
    - See MEASUREMENTS.md, "Integration".
  - **User decisions:**
    - (1) GPU training: a CUDA forward pass for training and evaluation (hand-written `logits.ptx`, checked by an
      interpreter test and at run time against the processor), off by default.
    - (2) The guardrail stays strict ("any metric worse → reject"); the validation sample is doubled to 4,000 games.
  - **Build tool not used:** compiling the kernel with NVIDIA's NVRTC from PyPI was not done. It is a closed-source
    binary, and the user's rules ask before such tools, so the PTX was written by hand.
  - **The key** is read only from Build Fetcher's settings: no `RIOT_API_KEY` variable, and no `.env` file.
- (lead, 2026-11-25, after 1.0.0) **Export and import** (Settings → Storage; asked for by the user).
  - **Contents:** `crates/store/src/backup.rs` writes one zip:
    - `manifest.json`;
    - `settings.json` (no key unless asked; no `dataDir`);
    - `data/build-fetcher.db`: a `VACUUM INTO` snapshot taken on the store's reading connection, so fetching goes on;
    - `data/checkpoints/*`, without `.tmp` files.
  - **Zip handling:** irelia-companion's zip module (ZIP64, CRC-checked) is back in `bf_core::zip`.
  - **Import is staged, not applied live.** It is checked and unpacked into `<settings folder>/import-pending/`. It is
    applied by `wiring.rs` at the next start, before anything opens the database.
    - The replaced data goes to `before-import/`, one copy.
    - The archive's settings replace this PC's, except `dataDir`, and the key when the archive has none.
  - **Checks before staging:**
    - the manifest says the app is Build Fetcher, with a known format;
    - only known entries with plain names (no path escape);
    - CRCs;
    - SQLite `integrity_check`;
    - no newer schema;
    - settings are a JSON object.
  - **Interfaces:**
    - routes `POST /api/backup/export`, `POST /api/backup/import`, `GET /api/backup/pending`, `POST /api/backup/cancel`;
    - `StoreHandle::{export_backup, stage_import, pending_import, cancel_import}`;
    - desktop bridge `backup.pickSave`, `backup.pickOpen` and `restart` (native dialogs, origin-checked).
- (lead, 2026-11-25, 1.1.0) **The learner waits for enough data; Pause and Resume for fetching and learning.**
  - **Waiting.** `Budget` gains `first_min_records` (2,000), `min_records` (500), `max_wait` (6 h) and
    `min_validation` (100), set in `wire_learn.rs`.
    - With fewer new records than the minimum, `train_step` returns `Outcome::Waiting` and saves nothing, so the
      records stay unread.
    - The reason it waits is the first line of the model status.
    - Before this, an idle PC trained on whatever had arrived in the last seconds; those tiny updates were rejected
      and their records still marked as read, so a first model could take very long or never arrive.
    - `Budget::records()` has no minimums (tests and measurements).
  - **Pause and Resume.**
    - Store setting `fetching` (default true): `Fetcher::set_user_paused` cancels requests in flight. Status reads
      "paused by you".
    - `LearnerHandle::set_learning` interrupts an update in progress.
    - Buttons on the Fetching and Model pages, and toggles in Settings.
- (lead, 2026-11-25, 1.1.1) **Scores that mean something early.**
  - `Metrics.matches` (contract, added with `serde(default)`): distinct test matches. `Metrics.games` keeps counting
    examples (player × minute); the Model page shows matches.
  - **Store:** `validation_fast_until` (300) and `validation_fast_one_in` (3). The held-out sample takes 1 timeline game
    in 3 until it holds 300 matches, then 1 in 10.
  - **`Learner::rescore`:** scores the models in force on the current held-out sample when it has more matches than
    when they were scored. Called while idle and waiting, at most hourly. Only the metrics change, and a log line says
    so.
- (lead, 2026-11-25, 1.1.1) **Recommender ranking, after a user report (Ahri mid).** Options with 1–25 games
  outranked options with hundreds.
  - **Causes:**
    - the role level was shrunk toward the champion's all-role estimate (cross-role leakage);
    - shrinkage was weak (40 pseudo-games);
    - ranking was by point estimate alone.
  - **Now:**
    - `RecommendConfig.shrink` = 250, from the binomial noise: 0.25 / (3 pts)².
    - The role is shrunk toward 0, and toward the champion's other roles only when the role has fewer than
      `role_min_games` (200).
    - A thin option (under `min_games` in its basis) ranks after the supported ones; thin ones are ordered by games.
      The same holds for live next buys.
    - Options show their own role's games; the dashboard shows the played win rate and the estimate, labelled "est.".
  - Regression test: `options_rank_on_their_own_role_and_thin_ones_never_lead` (fails on 1.1.0).
- (lead, 2026-11-25, 1.2.0) **The enemy's items, continuous learning, gold in the overlay.** Brief:
  `docs/plans/1.2.0.md`.
  - **Riot policy, read on 2026-11-25** (portal reachable this time):
    - The general policies (May 29, 2025) say products "should not remove game decisions, but may highlight decisions
      that are important and give multiple choices".
    - The League game policy lists as unapproved: "Apps that dictate player decisions", and "Products may not provide
      any game-session-specific information that would be previously unknown to the player".
    - **So:**
      - the overlay uses only what the player sees: their gold and items, and every player's items from the Live
        Client (the scoreboard);
      - the enemy's gold is never shown or estimated;
      - three options, best first, each with a reason.
    - The League page's approved list names "Game overlays that provide static data that is available prior to the
      game". In-game advice is not listed either way. 1.0 already gave it, as ranked options; 1.2.0 keeps that form.
  - **Model, enemy items** (`features.rs`, `logistic.rs`):
    - `Situation.enemy_items` and `enemy_tags`, both `serde(default)`. Tags are 7 shares of the enemy team's build
      items, counted against at least 8 items, × 4: armor, magic resist, healing, anti-heal, AP, AD, health.
    - Features go to a new region after the pair region (2^16 slots): a tag level ([31]), the champion-role × tag
      ([32], signed, `l2_pair`), and each of the player's items × tag for armor, MR, healing, anti-heal and health
      ([33], `TrainOptions.l2_enemy` = 3e-3).
    - The region starts at zero, so 1.1 checkpoints load unchanged.
    - The step 2 network is unchanged.
    - Synthetic: `SyntheticConfig.armor_effect` plants `ITEM_VS_ARMOR`. It is off by default, so the earlier sets
      and their tests are unchanged.
  - **Continuous learning** (`learner.rs`, `wire_learn.rs`):
    - **`Learner::absorb`** trains an in-memory `Draft` (both steps, warm, replay 1:1) on whatever new records exist.
      - `ContinuousOptions`: at least 1 game, step × 0.1, 1 pass, as measured.
      - It runs unless the user is in champ select or a game.
      - It returns `Waiting` before the first model, which still comes from `train_step` on 2,000 games while idle.
    - **`Learner::check`** (`CheckPolicy`: every 150 games or 1 h, at least 100 validation matches, reset after 8
      rejections):
      - the calibration is fitted on the draft's own held-out records;
      - then `judge_logistic`, and `judge_net` when step 1 was accepted (both split out of `train_step`, which
        behaves as before).
      - **Accept:** the manifest cursor moves to the draft's.
      - **Reset:** the cursor moves too, and the games count as read.
    - A restart drops the draft; games since the last accepted model are learnt again. A rollback drops the draft.
    - **Server cycle:** absorb up to 200 games per call, then check; poll every 5 s while games arrive.
      - Re-scoring and clean-up run idle only, each at most hourly.
      - `set_activity` interrupts only for champ select and a game.
  - **Overlay:** `LiveAdvice.buys: Vec<BuyNote>` (contract, `serde(default)`).
    - Each note gives the cost from here, counting owned components once (`remaining_cost`), the gold short, the
      most expensive affordable part of the recipe, and a reason from the enemy's items (`enemy_reason`).
    - The overlay shows 3 next buys, the first marked, each with "Buy now (cost)" or "N short · part now (cost)".
  - **Settings:** `timelineSample` default 0.25 (0.1 before) for new settings files; a choice of 10/25/50/100% in
    Settings → Regions and rank.
  - Measurements: MEASUREMENTS.md, "1.2.0".
- (lead, 2026-11-26, 1.2.1) **The timeline share reaches the settings API.**
  - **User report:** choosing a share showed "Settings: unknown field `timelineSample`".
  - **Cause:** 1.2.0 added the setting to `bf_store::settings`, the fetcher and the page, but not to the server's
    `AppSettings` view and `SettingsPatch` (`deny_unknown_fields`) in `handles.rs`.
  - **Now:**
    - both carry `timeline_sample`;
    - `routes::validate_patch` accepts 0–1;
    - `RealStore` saves it and logs the change.
  - **Tests:** `settings_round_trip_and_validation` (stand-in store) and `real_key_handles_import_from_irelia_without_touching_it`
    (real store to file).
  - **Lesson:** 1.2.0's UI change was not clicked in a browser before release. This one was, on the real server (see
    the release notes).
- (lead, 2026-11-26, 1.2.2) **A calibration pool for continuous learning.**
  - **User report:** every check rejected the draft on calibration (10–30 min, 0.10–0.17 against 0.03–0.08),
    although its log loss was far better (0.62–0.65 against 0.684). The draft was reset twice.
  - **Cause:** `Draft.cal` (held-out calibration examples) lived and died with each draft, at most about 174 matches
    under 8 × 150 games. `fit_calibration` needs `CALIB_MIN_GAMES` (250), so a draft's temperature was never fitted.
  - **Now:** `State.cal_pool` holds the newest `CAL_POOL_MATCHES` (2,000) calibration matches, never trained on.
    - Fed by `absorb` and `train_step`.
    - Refilled once per run from the last `CAL_POOL_SCAN` (20,000) records up to the cursor (`pool_fill`).
    - `check` fits the draft's temperature on the pool before the guardrail. The guardrail is unchanged (the
      user's choice: strict).
  - **Not verified:** at the user's request (no new tests or measurements); CI ran the existing ones.
- (lead, 2026-11-26, 1.3.0) **Stats, combat math and a dormant team network** (brief: `docs/plans/1.3.0.md`).
  - **Data:** Data Dragon only.
    - `ItemMeta.stats` holds 20 stats read from the description's `<stats>` block. The `stats` object lacks
      penetration, lethality, haste and omnivamp, so it is only the fallback.
    - `ItemMeta.effects` holds flags from tags and text.
    - `ChampionMeta.base` holds base stats and growth, with League's growth formula.
    - Ability ratios are not in Data Dragon (zeros and `{{ }}` placeholders). The wiki and CommunityDragon were
      not used: the user's source rule, and Riot's "supported services for data ingestion".
    - The test snapshot (16.19.1) now keeps full item descriptions and champion stats.
  - **`combat.rs`:** a 36-number vector per player and minute: 20 build stats per 1,000 gold, and 16 combat terms
    listed in `TERM_NAMES`. Build gold and raw effective health were dropped because they grow with game time and act
    as a clock.
    - Samples compute it from frames and purchase logs.
    - The overlay computes it from `LiveContext.players`, new in the contract with `serde(default)`, filled by
      `live_context.rs`. It is recomputed per candidate item, with the candidate's components removed.
    - Stored draft contexts carry none.
  - **Logistic:** a stat region after the enemy region (2^16, starting at zero).
    - Shared: one weight per combat number ([40], `l2_pair`).
    - Champion-role taste: one weight per build stat ([41], signed, `l2_pair`).
    - 1.2 checkpoints load padded and unchanged.
  - **Team network** (`teamnet.rs`, step 3):
    - Shape: a player network (51 → 32 → 24), then the user's output and the allies' and enemies' means, with the
      game state and combat terms, into a head (99 → 48 → 1). About 69k weights.
    - Its residual is added on top of steps 1 + 2. The last layer starts at zero. The gradient is checked against
      finite differences.
    - **Gate:** awake once `max(manifest.timeline_seen, stored timeline records)` reaches `team.threshold`
      (200,000). `DataSource::timeline_count` was added, with a default.
    - **Awake:** `absorb` trains a `team_draft` on new games, a backlog of `backlog_per_call` (150) stored games from
      `manifest.team_cursor` (never calibration records), and replay.
    - **`check`:** `judge_team` streams the held-out records' team examples and judges on top of steps 1 + 2 in
      force. Accepted networks become `team-nn` checkpoints.
    - **Serving:** `Served.predict_team` is used in live advice when the Live Client gives all players.
    - Rolling step 1 back drops step 3.
  - **Tests:** the armor test and the team-gate test use `noise_allowance` 1.0 for their first model only. The first
    model's calibration bound is at the noise edge on small synthetic sets (1.2.2 passed the armor test's with 0.0266
    against 0.0268), and these tests are about later steps. The guardrail tests keep the strict bound.
