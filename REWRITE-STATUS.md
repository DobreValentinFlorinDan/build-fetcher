# Irelia Companion 1.0 — Rust rewrite: status

> **2.0.0 (2026-11-11).** `main` was put back to 0.22.0 (PR #58) and the 1.0.0-1.1.2 releases withdrawn. 2.0.0 is the
> 1.1.2 code carried over, with one change: `pow` is the app's own, correctly rounded (`crates/core/src/jsmath.rs`),
> instead of the platform C library's, so no result depends on Windows' UCRT or glibc. Checked: 1,020,000 powers
> against 400-bit arithmetic (`parity/pow-fixtures.py`), exact ties and special cases; 221 tests; against 0.22.0 run
> in Electron 44 the full evaluation (2,024 numbers), the quick update (1,866) and 200 drafts (93,666) are bit for
> bit, 10.0 s vs 17.9 s and 118 MB vs 338 MB. Results must equal 0.22.0's verdicts and strings exactly and its
> numbers within 1e-9 (`parity/compare.mjs`); bit equality is no longer a requirement. The sections below are the
> 1.x history.

Branch: `rewrite/rust-1.0` (from `main` at `2d6a856`, the 0.22.0 release, tag `v0.22.0`). `main` and the 0.x app stay
untouched until 1.0 is ready. **Read this file first; it is the hand-over between sessions.**

The TypeScript code and its tests are the specification. Every Rust module names the TS module it ports, and keeps its
names (snake_case) so the two can be read side by side.

## Architecture (as decided)

| Part | 0.22.0 | 1.0 |
|---|---|---|
| Shell | Electron 44 (main + utility process) | Tauri v2 (`desktop/tauri`), WebView2. Not a default workspace member: Linux builds and tests never need WebKit |
| Backend | Express + ws in a utility process, worker threads, child processes | `crates/server`: axum + tokio, **same REST + `/ws` API** (`shared/types.ts`, `server/index.ts`, `server/routes/`). Runs in the Tauri process (one process; no 4 GB heap, no child processes), or headless (`irelia-server` binary) for development, CI and the parity harness |
| Slow work | worker threads + processes (`workers.ts`, `evalTeam.ts`) | `std::thread` per worker (scraper, model, job) + rayon for evaluation and prepare |
| Storage | `node:sqlite` | `rusqlite` (bundled SQLite), the same `irelia.db` schema v5+ and migrations, the same `cache/riot-cache.db`, `config.json`, `evals/`, backups |
| Codec | `node:zlib` zstd, raw-content dictionaries | `zstd` crate (libzstd, BSD), same blob format `[1][dict id u16 LE][frame]` |
| Riot API | `fetch` + own pacer | `reqwest` (rustls) + port of `rateLimiter.ts` |
| Model + evaluation | TS, threads/processes | Rust, struct-of-arrays, rayon; same algorithms, same order of floating-point sums |
| GPU (CUDA) | `server/gpu/` via koffi | 1.1.0: `crates/core/src/gpu/`, `nvcuda.dll` loaded at run time (libloading), 0.22.0's PTX kernel for the draft similarities, the same self-check and 1-in-500 verification; the weighted sums stay on the processor (0.22.0's were behind `IRELIA_GPU_SUMS=1`, off) |
| Web UI | React + Vite (`web/src`) | unchanged; served by axum from the built `dist/`. `window.irelia` (preload) re-implemented as an init script over Tauri IPC |

Why one process instead of a middleware process: Electron needed it for crash isolation and V8's heap; Rust has
neither problem. Leftover handling (0.14.2) becomes a single-instance guard plus ending a 0.x middleware left by
`middleware.pid`.

Workspace layout:

```
Cargo.toml                 workspace (default-members: crates/*)
crates/core                lib `irelia`: codec, db, config, riot, scraper, engine, model, eval, transfer, zip, slim, …
crates/server              lib + bin `irelia-server`: axum routes, /ws, jobs, matchup, lcu, live client, auto eval;
                           bins `eval-builds`, `audit-data`
parity/                    harness: runs 0.22.0's TS evaluation and the Rust one on the same database, compares
crates/desktop             the shell's logic without a window toolkit (settings, keep awake, leftover 0.x
                           middleware, /ws feed, overlay placement, renderer priority, log), with its tests
desktop/tauri              Tauri v2 app (tray, hotkeys, overlay, window.irelia bridge, splash, NSIS config)
```

## Milestones (each testable on its own)

Status: ☐ not started · ◐ in progress · ☑ done (with its tests passing on Linux; Windows via CI)

### M0 — Scaffolding ☑
- Workspace, CI (`cargo test` + web tests on ubuntu-latest and windows-latest), toolchain pinned (`rust-toolchain.toml`).
- TS baseline: `npm test` of 0.22.0 runs here (Node 22) for reference.

### M1 — Storage and codec ☑
Ports: `codec.ts`, `config.ts`, `appConfig.ts`, `fsUtil.ts`, `db.ts` (schema v1→v5+, every migration, `timeline_json`
SQL function, `Db.tx` BEGIN IMMEDIATE with busy timeouts, WAL/NORMAL), `storage.ts`, `riot/diskCache.ts` (cache DB,
legacy gzip files, TTLs, `entries_size`), `zip.ts`, `participantIds` layout, `patches.ts`, `scrapeScope.ts`.
Tests: `storage.test.ts`, `zip.test.ts`, `participantIds.test.ts`, `scope.test.ts`, `convert.test.ts` (part),
`dbLocks.test.ts`. Cross-check: a DB + cache written by 0.22.0 (TS) read by Rust and vice versa.

### M2 — Riot client and sourcing ☑
Ports: `riot/routing.ts`, `riot/rateLimiter.ts`, `riot/client.ts`, `scraper/*` (sourcing, pilots, pool, ingest,
backfill, account, worker lanes, controller), `capture.ts` (v3), `engine/parseMatch.ts`, `backfill`, `convert.ts`.
Tests: `rateLimiter.test.ts`, `pool.test.ts`, `sourcing.test.ts`, `capture.test.ts`, `backfill.test.ts`,
`account.test.ts`, `lanes.test.ts` (DB part).

### M3 — Engine ☑
Ports: `ddragon.ts`, `engine/exclusivity.ts`, `validator.ts`, `recommend.ts`, `adaptive.ts`, `laneOpponent.ts`,
`itemCatalog.ts`, `shared/buildPlan.ts` (Rust copy for tests; the web keeps the TS one), `matchup.ts`.
Tests: `engine.test.ts`, `buildPlan.test.ts`, `lanes.test.ts`, `matchup.test.ts` on the Data Dragon snapshot.

### M4 — Model ☑
Ports: `model/logistic.ts`, `state.ts`, `dataset.ts`, `context.ts`, `profiles.ts`, `weights.ts`, `draft.ts`,
`estimate.ts`, `expected.ts`, `personal.ts`, `live.ts`, `swaps.ts`, `buy.ts`, `facts.ts`, `groups.ts`, `service.ts`.
Tests: `model.test.ts`, `live.test.ts`, `swaps.test.ts`, `estimate.test.ts`, `expected.test.ts`, `memory.test.ts`,
`stagec.test.ts`, `groups.test.ts`, `service.test.ts`.

### M5 — Evaluation and the parity harness ☑
Ports: `evaluate.ts`, `evalBase.ts` (quick update), `evalTeam.ts` (→ rayon, same chunk order), `runEval.ts`,
`evalHistory.ts`, `evalTravel.ts`, `evalCompare.ts`, `autoEval.ts`, report text + JSON, Time by step.
Tests: `evaluate.test.ts`, `evalUpdate.test.ts`, `evalTeam.test.ts`, `runEval.test.ts`, `evalHistory.test.ts`,
`evalTravel.test.ts`, `autoEval.test.ts`.
**Parity harness** (`parity/`): builds the synthetic DB (`server/test/modelFixtures.ts`), runs 0.22.0's TS
evaluation and the Rust one, compares every verdict (identical) and every number (≤ 1e-9 relative), and lane builds +
draft recommendations for 200 sampled drafts (identical). Same on a copy of a real data folder when provided.
Measure: full evaluation time and peak memory, TS vs Rust, same machine.

### M6 — Server API ☑
Ports: `server/index.ts` (every route, `/ws` events `ServerEvent`), `routes/*`, `jobs.ts`, `progress.ts`,
`workers.ts` (threads with restart), `gameMode.ts`, `audit.ts`, `lcu/connector.ts`, `lcu/liveClient.ts`,
`lcu/importer.ts`, `lcu/simulateLive.ts`, `lcu/https.ts`. The web UI runs against it unchanged.
Tests: `liveClient.test.ts`, `importer.test.ts`, `progress.test.ts`, `workers.test.ts`, `gameMode.test.ts`, plus an
API contract test (every route answers in the shape of `shared/types.ts`).

### M7 — Tauri shell and overlay ☑
Ports: `desktop/main.ts`, `overlay.ts`, `settings.ts`, `keepAwake.ts`, `leftover.ts`, `serverProcess.ts` (→ in-process),
`preload.ts` (→ init script). Tray, hotkeys (RegisterHotKey via global-shortcut plugin, only in game), overlay
(transparent, always on top, click-through, non-focusable, created per game), keep awake (SetThreadExecutionState),
game mode (below-normal priority), splash. Tests: `settings.test.ts`, `keepAwake.test.ts`, `leftover.test.ts`
(Windows parts in CI).

### M8 — Backups and the rest ☑
Ports: `transfer.ts`, `transfers.ts`, `autoExport.ts`, `routes/backup.ts` (ZIP export/import into a chosen folder,
per-computer pruning, offers, blending pools), `slim.ts` / `slimControl.ts` / `slimPart.ts` / `slimReads.ts` (Slim
stored data with rollback), `oldPatches.ts` (archive/purge), compaction, moving the data folder, `audit.ts` (data
audit), storage manager, settings. Tests: `transfer.test.ts`, `slim.test.ts`, `oldPatches.test.ts`, `evalTravel.test.ts`.

### M9 — Release ◐
Version 1.0.0 and 1.1.0 released, `docs/releases/1.0.0.md` and `1.1.0.md`, `release.yml` (Tauri NSIS: per-user,
shortcuts, uninstaller keeps `%APPDATA%\irelia-companion`), PR to `main`, release workflow via `workflow_dispatch`.
1.1.1 is the Electron 44 math fix (`docs/releases/1.1.1.md`); 1.1.2 fixes the Show overlay button
(`docs/releases/1.1.2.md`).

## Progress log

- (session 1) Plan written. Branch created.
- (session 1) M0 workspace; M1 core storage (db.rs schema v6 + migrations + archive + participants-by-ids, codec,
  disk cache, storage manager, zip) with tests; capture/parseMatch/ingest; engine (exclusivity, validator, recommend)
  with engine.test.ts ported; model (logistic, state, dataset, context, profiles, weights, estimate, draft, groups,
  expected, swaps, live stability); evaluate.rs + run_eval.rs (format_eval, format_timings).
- V8 math: Math.exp/log/log1p/hypot/pow ported from Node 22's V8 fdlibm (jsmath.rs), checked bit for bit against
  Node (tests/jsmath.rs); JS number formatting / toFixed / JSON key order in js.rs. (1.1.1 replaces the first three
  and pow with Electron 44's versions; see Session 4.)
- **Parity harness** (parity/): make-db.ts writes a synthetic data folder with 0.22.0's own storage code (2,400 games,
  5 patches, 3 regions, top/mid, your games, leavers, late sells); ts-eval.ts runs 0.22.0's evaluation; the Rust
  `parity_eval` bin runs 1.0's; compare.mjs checks every number (1e-9 rel), string, key order and text line.

## Session 3 (this session)

- M7: `crates/desktop` (settings.ts, keepAwake.ts, leftover.ts, overlay placement, the /ws feed of main.ts, log) with
  settings/keepAwake/leftover tests ported (keep awake: SetThreadExecutionState on a thread of its own instead of the
  PowerShell helper; the Windows-only test checks `powercfg /requests`). `desktop/tauri`: server in-process (own tokio
  runtime handed to Tauri), splash (`splash/index.html`), dashboard window, overlay (transparent, on top, not
  focusable, click-through, created per game, position saved), hotkeys via the global-shortcut plugin only in game,
  tray, single instance, dialogs, `window.irelia` as `src/bridge.js` over Tauri IPC (capability
  `capabilities/bridge.json` for http://127.0.0.1:*, each command re-checks the page's origin), leftover 0.x
  middleware ended at start, NSIS per-user with `windows/hooks.nsh` removing a 0.x install first (its uninstall key
  is electron-builder's GUID `5990d65b-…`), data kept. Smoke-tested on Linux under Xvfb (IRELIA_SELFTEST=1 in a debug
  build prints the bridge's answers): bridge calls, settings saved, overlay opened on a simulated game, hotkeys
  registered, overlay closed after it, position saved. CI builds the app and the NSIS installer on Windows (artifact
  `installer`, about 7 MB).
- Electron files removed (desktop/*.ts, electron-builder.yml, scripts/bundle.mjs); package.json scripts for 1.0
  (`npm test` = cargo test, `npm run test:web`, `npm run test:ts` = 0.22.0's suite, `dist` = tauri build); version
  1.0.0 in Cargo.toml, tauri.conf.json, package.json; README and docs/DESKTOP.md (section 0) for 1.0;
  docs/releases/1.0.0.md written.
- `eval-builds` and `audit-data` as Rust bins (scripts/*.ts), with the grouped-drafts trial and evalCompare.ts; the
  grouped trial is bit for bit 0.22.0's (246 numbers).
- CI (`ci.yml`): typecheck + build-plan tests + Vite build + `cargo test --no-fail-fast` on ubuntu and windows; on
  Windows also `tauri build --bundles nsis`, the installer kept as an artifact. `release.yml`: version check (three
  files), notes from docs/releases/<v>.md with size and SHA-256 filled in, NSIS build renamed
  `Irelia-Companion-Setup-<v>.exe`, `gh release create v<v>`.
- rusqlite 0.32 → 0.41 (bundled SQLite 3.46 → 3.54; 0.22.0's Node had 3.53): with 3.46 the cache's size query
  could not use `entries_size` as a covering index (it read every blob).
- Bugs found by the ported tests and fixed: turning Archive old patches on could deadlock (the database lock taken
  twice); a step going over the folds reported its progress from 0 again at each fold.
- Measurements for the release notes (9,600-game synthetic pool, 4 threads both; idle server; warm start).

## Session 4 (this session)

- **1.1.1: the math functions as Electron 44 computes them.** 0.22.0 ran in Electron 44 (V8 15.2), which takes
  `exp`, `log` and `log1p` from LLVM's libc (correctly rounded) and `pow` from the C library (V8's
  `use_std_math_pow`, on by default). 1.0 and 1.1.0 had ported Node 22's V8 fdlibm instead: a few percent of calls
  were one ulp off. That could move an evaluation number and made Use GPU's self-check reject an NVIDIA card (its
  kernel's `exp` is correctly rounded, as Electron's).
- `crates/core/src/jsmath.rs`: `exp`, `log` and `log1p` in double-double (`exp` is the GPU kernel's `crExp`, with
  Dekker products; `log` and `log1p` are a Newton step on it), a second double-double stage, then 300-bit
  `dashu-float` for the rare uncertain cases. `log1p`'s `|x| < 2^-10` series avoids the 300-bit path (before it,
  1M values took 20 s). `pow` is V8's four special cases and then `f64::powf` (the platform C library); `hypot`
  is unchanged.
- Fixtures regenerated from Electron 44 by `parity/math-fixtures.mjs`; the committed set is 20,020 rows per
  function. Checked 1,020,040 values each for `exp`, `log` and `log1p` (all bit for bit; `log1p` now under a
  second in total) and 1,000,000 each for `pow` and `hypot` on Linux (`pow` glibc's).
- Windows `pow`: Electron 44 links the **static** Universal CRT, whose `pow` differs from the system
  `ucrtbase.dll`'s on 159 of 1,000,000 sampled inputs. `.cargo/config.toml` builds Windows MSVC with
  `-C target-feature=+crt-static`; `crates/core/tests/pow_windows.rs` checks 20,000 Electron 44 rows plus those
  159 on every Windows build.
- The 300-bit fallback now handles non-finite and out-of-domain inputs (`log(0)`, `log1p(x < -1)`, `exp(±∞)`)
  instead of panicking; `the_slow_path_rounds_as_the_fast_one` passes.
- Version 1.1.1 in `Cargo.toml`, `package.json`, `package-lock.json` and `desktop/tauri/tauri.conf.json`;
  `docs/releases/1.1.1.md`.
- Live overlay/client check on 1.1.0 (the overlay and connector code 1.1.1 keeps): the overlay opened in a real
  game, showed live performance/build order, updated between 4:53 and 5:06, and the window had click-through
  (`WS_EX_TRANSPARENT`), `WS_EX_NOACTIVATE` and topmost. The "Irelia Companion" rune page and item set were in the
  client, and the importer tests (`cargo test -p irelia-server --lib lcu::importer`) passed.
- The client connector now polls the gameflow phase (and champ select while in it) every 5 s beside the WAMP
  events, and `set` drops unchanged states. A long-running 1.1.0 process had stayed at `ChampSelect` for hours while
  the client was in a game, so the overlay never opened; a missed/replayed event cannot leave it there now.

## Session 5

- **1.1.2: the Show overlay button.** A sync Tauri command runs in the Windows IPC callback on the main thread;
  `overlay_set_visible` created the overlay WebView from there, which deadlocked. The window existed but stayed
  hidden (`about:blank`, 200x240), and the button's promise never resolved. Both overlay window commands are
  `async fn` now, so the work leaves the IPC callback. The tray/hotkey/automatic-game paths were already on the
  main thread.
- Reproduced through WebView2 CDP: `window.irelia` and `overlay.state()` answered, clicking "Show overlay now"
  created only a hidden `Irelia Companion Overlay` window and hung. After a page reload the bridge section was
  present, confirming the renderer side was fine.
- 1.1.2 in `Cargo.toml`, `package.json`, `package-lock.json` and `desktop/tauri/tauri.conf.json`;
  `docs/releases/1.1.2.md`.

## Test port coverage (TS test → Rust)

Ported: engine (core/tests/engine.rs), scope, liveClient, storage, oldPatches (server/tests/old_patches.rs, through
the app), rateLimiter, pool, capture, sourcing + account, model (server/tests/model.rs), evaluate, evalUpdate,
evalHistory, progress, service, matchup, importer, autoEval, gameMode, convert (server/tests/convert.rs),
participantIds, slim (core/tests/slim.rs + server/tests/slim.rs), zip, transfer, dbLocks, runEval, evalTravel,
desktop settings/keepAwake/leftover.
1.1.0: lanes and backfill (server/tests), live, stagec, swaps, estimate, expected, memory, groups, evalTeam,
buildPlan's Rust copy (core/tests/build_plan.rs), gpu (core/tests/gpu.rs), workers (server/tests/workers.rs, through
the app). Every TS test file now has its Rust counterpart.
`server/tests/common/mod.rs` starts the app on a temporary data folder for tests through the HTTP API.

Cases left out, each said at the top of its file: 0.22.0's process team and worker-thread internals (evalTeam,
workers: 1.x runs evaluations on a rayon pool in one process), frameSlabs (expected: a V8 memory layout) and the
weighted-sums accelerator (estimate: off in 0.22.0). Test-only hooks added for the ports: `EvalInput::prepared_hook`
(0.x's `prepare` option) and `ModelClient::crash_for_tests`.

## Next (in order)

1. 1.1.2: CI green on the PR, merge, then `release.yml` on main through the API (workflow_dispatch; pushing tags
   is blocked by the environment's proxy). If a run fails within seconds with no logs, tell the user (Actions
   budget) instead of retrying.
2. Use GPU has been tested here without a card (no driver: refused with the reason; the hook and the checks by
   tests). Its first run on an NVIDIA card is the user's: with 1.1.1's math the self-check's `exp` is Electron 44's,
   so the card should no longer be refused for a last-bit `exp` difference.

## Open questions

- WebView2 (Microsoft's, closed source, part of Windows 10/11) is what Tauri uses on Windows; it came with the
  architecture the user chose (Tauri v2), and the user acknowledged it. Said in the PR and the notes.
- Decisions taken without changing user data: evals/base.bin is written in 1.0's own binary form (0.22's was
  V8-serialized). A base is only ever reused by the app version that wrote it (its key holds the version), so 1.0's
  first evaluation is a full one either way, and 0.22.0 reading 1.0's base.bin treats it as none. Everything in the
  data folder (irelia.db, cache, ddragon) and config.json / latest.json / reports keep their 0.22.0 form.

## Parity results

Synthetic data folder (parity/make-db.ts 2400 7), Linux container, 4 cores:

| Run | Numbers compared | Differences | TS 0.22.0 (1 thread) | Rust 1.0 |
|---|---:|---:|---:|---:|
| Quick evaluation (--quick) | 1,866 | 0 (all bit for bit) | 3.0 s, 217 MB | 2.0 s, 101 MB |
| Full evaluation, 1 thread | 2,024 | 0 (all bit for bit) | 17.4 s, 310 MB | 9.8 s, 118 MB |
| Full evaluation, 4 threads | 2,024 | 0 (all bit for bit) | — | 4.6 s, 119 MB |

Math (1.1.1), Electron 44 fixtures from `parity/math-fixtures.mjs`, 1,000,000 random rows plus specials each:

| Function | Values compared | Differences |
|---|---:|---:|
| `exp` | 1,020,040 | 0 (all bit for bit) |
| `log` | 1,020,040 | 0 (all bit for bit) |
| `log1p` | 1,020,040 | 0 (all bit for bit) |
| `pow` (Linux, glibc) | 1,000,000 | 0 (all bit for bit) |
| `pow` (Windows, static UCRT) | 20,159 | 0 (all bit for bit) |
| `hypot` | 1,000,000 | 0 (all bit for bit) |

`pow` is the platform C library's. Windows used the system `ucrtbase.dll` in the first check, which differs from
Electron's static Universal CRT on 159 of the 1,000,000 rows; with `+crt-static` the app uses Electron's, and the
Windows fixture (20,000 random rows plus those 159) passes.

| 200 sampled drafts, lane build + draft-adjusted build (detail estimates) | 93,666 | 0 (all bit for bit) | model 1.0 s | model 0.6 s |
| The same with patch memory on | 93,664 | 0 (all bit for bit) | model 1.3 s | model 0.8 s |

Commands (IRELIA_CONFIG_DIR pointing at an empty folder so no user scope applies):

```
npx tsx parity/make-db.ts /tmp/p/synth 2400 7
npx tsx parity/ts-eval.ts /tmp/p/synth /tmp/p/ts.json [--quick]
cargo run --release -p irelia-server --bin parity_eval -- /tmp/p/synth /tmp/p/rs.json [--quick] [--threads 4]
node parity/compare.mjs /tmp/p/ts.json /tmp/p/rs.json
npx tsx parity/ts-drafts.ts /tmp/p/synth /tmp/p/ts-d.json 200 [--memory]
cargo run --release -p irelia-server --bin parity_drafts -- /tmp/p/synth /tmp/p/ts-d.json /tmp/p/rs-d.json [--memory]
node parity/compare-drafts.mjs /tmp/p/ts-d.json /tmp/p/rs-d.json
```

(Times and peak RSS measured on this synthetic folder; TS ran without its thread team.)

Larger pool, both with their threads (session 3): `parity/make-db.ts /tmp/p/big 9600 7`, `eval-builds` (Rust) and
`scripts/eval-builds.ts` (TS, its process team) on copies, peak RSS summed over the process tree:

| Run | Numbers compared | Differences | TS 0.22.0 (4 threads, processes) | Rust 1.0 (4 threads) |
|---|---:|---:|---:|---:|
| Full evaluation, 9,600 games (7,056 evaluated) | 244 + report text | 0 (bit for bit) | 50.3 s steps, 1,440 MB | 25.8 s steps, 297 MB |
| Grouped drafts trial, 2,400 games | 246 + report text | 0 (bit for bit) | — | — |

Idle server after a minute on the 9,600-game folder: TS 277 MB (node, tsx wrappers excluded), Rust 243 MB. Warm start
until /api/health answers: TS 0.86 s (tsx), Rust 0.3–0.5 s.
