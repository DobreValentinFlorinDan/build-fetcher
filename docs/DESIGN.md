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

- (lead) Copy of irelia-companion 2.2.0 committed verbatim (04f0b5e). The next commit trims it to the reusable
  engineering, adds the contract, and adds the store and learn skeletons. Server, desktop and tauri are not yet in
  the workspace members; Agent C restores them.
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
