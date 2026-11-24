# Desktop app, in-game overlay, live tracker and storage

This covers the Windows desktop build: the shell, the installer, the in-game overlay (and how it stays Vanguard-safe), the live CS / vision / gold tracker, and the storage manager.

## 0. Since 1.0: Rust and Tauri

1.0 is a rewrite of 0.22.0 in Rust with the same features, the same data and the same results (REWRITE-STATUS.md). What
section 1 says about the Electron shell is 0.x history; the rest of this document (overlay rules, tracker, storage,
keep-awake conditions) still holds, with the differences below.

```
IreliaCompanion.exe (Tauri v2: desktop/tauri/src/main.rs)
 ├─ the server (crates/server) on its own tokio runtime: axum REST + /ws on 127.0.0.1:<port>, the same API as 0.x
 │    ├─ fetching lanes (tokio tasks), the model thread (restarted after a crash), job threads (evaluation, audit),
 │    │  Slim stored data, conversion; evaluations and preparing games on every core with rayon
 │    └─ one SQLite connection per thread (rusqlite, the same irelia.db, WAL, BEGIN IMMEDIATE)
 ├─ WebView2  http://127.0.0.1:<port>/          dashboard (Build, Your games, Data, Settings), window.irelia bridge
 ├─ WebView2  http://127.0.0.1:<port>/overlay   in-game HUD (transparent, click-through, never focused)
 └─ Tray      open / overlay on-off / click-through / quit
```

- **One process.** 0.x ran its middleware in a separate process because of Electron (crash isolation, V8's 4 GB heap).
  Rust has neither limit, so the server runs in the app's own process: no middleware to start, wait for or restart.
  `middleware.pid` is no longer written; a 0.x middleware left running is still ended at start by its PID (only if
  the PID is still that executable), and the installer removes a 0.x install first, keeping `%APPDATA%\irelia-companion`.
- **window.irelia** is an initialization script over Tauri's IPC (desktop/tauri/src/bridge.js) with the same calls as
  0.x's preload. Only the app's own pages get it (http://127.0.0.1:<port>), the capability allows only the app's own
  commands, and every command checks the page's origin again.
- **Keeping the computer awake:** the app holds `ES_CONTINUOUS | ES_SYSTEM_REQUIRED` (and `ES_DISPLAY_REQUIRED` with
  "Keep the screen on too") itself, on a thread of its own, instead of through a PowerShell helper. Same conditions,
  same states; `powercfg /requests` lists `IreliaCompanion.exe` under SYSTEM (CI checks the same call on windows-latest).
- **Overlay:** the same window rules (section 3). Tao shows a non-focusable window with `SW_SHOWNOACTIVATE`; the HUD's
  opacity is applied to the page (a transparent WebView2 window draws through DirectComposition). While click-through
  is on (2.0), the app reads where the cursor is every 100 ms (the window's cursor position: no mouse hook, no input
  read) and sends it to the page, which shows the tooltip of what is under it as hovering would; clicks always go to
  the game.
- **Champ select and games:** the app's process runs at below-normal priority (it now holds the server), and so does
  the dashboard's WebView2 renderer (2.0: WebView2's process list says which renderer holds which page,
  `ICoreWebView2Environment13::GetProcessExtendedInfos`); the overlay's stays at normal, as in 0.22.0.
- **Use GPU (2.0):** the NVIDIA driver (`nvcuda.dll`) is loaded at run time, only when an evaluation starts with
  Use GPU on or Test GPU is pressed, and 0.22.0's PTX kernel runs the draft similarities; no CUDA toolkit or extra
  file ships. The same checks as 0.22.0: test drafts before a thread uses it, one pass in 500 compared, any difference
  sends the thread back to the processor. The weighted sums stay on the processor (0.22.0 had them behind
  `IRELIA_GPU_SUMS=1`, off because slower).
- **Logs:** `%APPDATA%\irelia-companion\logs\middleware.log` (the same file name), rotated at 5 MB.

## 1. Architecture

**Choice: Electron 44 + electron-builder (NSIS), with the existing Express middleware run unchanged in an Electron utility process.**

| | Electron + electron-builder (chosen) | Tauri |
|---|---|---|
| Runs the existing middleware | Yes: Electron 44.9.3 ships **Node 24.22** (0.14.1; 24.21 crashed now and then as worker threads ended), and `node:sqlite` plus `worker_threads` work in a utility process (verified) | Needs a Node sidecar binary (~80 MB) or a Rust rewrite of `server/` |
| Overlay primitives | `transparent`, `setIgnoreMouseEvents({forward})`, `setAlwaysOnTop('screen-saver')`, `focusable:false`, `globalShortcut` | Available, but transparency/click-through on Windows needs extra plugins and Rust glue |
| Installer | NSIS with Start Menu + desktop shortcut, per-user (no admin) | NSIS / WiX |
| Size | ~112 MB installer, ~370 MB installed | Smaller shell, but similar once a Node sidecar is added |

Tauri only wins on size, and that win disappears once a Node runtime has to ship next to it. With Electron, one runtime serves the UI, the overlay and the middleware, and no native modules have to be rebuilt.

```
IreliaCompanion.exe (Electron main: desktop/main.ts)
 ├─ utilityProcess: dist-server/index.mjs   Express REST + /ws on 127.0.0.1:<port>
 │    ├─ worker thread: dist-server/worker.mjs         Riot API scraper
 │    ├─ worker thread: dist-server/model-worker.mjs   lane baseline + draft model
 │    │    └─ worker threads: dist-server/prepare-worker.mjs   a build's games prepared on several cores (0.15.0), and an evaluation's (0.15.1)
 │    ├─ worker thread: dist-server/job-worker.mjs     one-shot: evaluation, data audit
 │    │    └─ worker threads: dist-server/eval-worker.mjs   the evaluation's other threads (0.11.0), one per spare core
 │    ├─ worker thread: dist-server/gpu-worker.mjs     Test GPU, and the clean-up after a stopped evaluation (0.13.0)
 │    ├─ worker thread: dist-server/convert-worker.mjs earlier versions' data into the smaller format, while idle (0.14.0)
 │    └─ worker thread: dist-server/slim-worker.mjs    Slim stored data: backup, results before and after (0.14.0)
 │         └─ worker threads: dist-server/slim-part-worker.mjs   its slimming on several cores (0.14.4)
 ├─ BrowserWindow  http://127.0.0.1:<port>/          dashboard (Build, Your games, Data, Settings)
 ├─ BrowserWindow  http://127.0.0.1:<port>/overlay   in-game HUD (transparent, click-through)
 └─ Tray           open (a new dashboard window) / overlay on-off / click-through / quit
```

- **Process management** ([desktop/serverProcess.ts](../desktop/serverProcess.ts)). The middleware asks for port **45174**. A stable port keeps the dashboard origin (and its localStorage) stable. If that port is taken, the server falls back to any free port and reports the port it actually got through `process.parentPort` (`{type:'ready', port}`). If it crashes, it restarts on the same port, at most 3 times a minute; open windows reconnect through `/ws` without reloading. Its output goes to `%APPDATA%\irelia-companion\logs\middleware.log`, rotated at 5 MB. Since 0.14.2 its PID is kept in `%APPDATA%\irelia-companion\middleware.pid` while it runs ([desktop/leftover.ts](../desktop/leftover.ts)): a middleware left running by an earlier start (the app's process ended by hand or crashed, so it held the port and the database and every later start exited) is ended by that PID before the next one starts, only if the PID's image is still the app's executable. The middleware also checks every 5 s that the app's process (`IRELIA_PARENT_PID`) is there, and ends itself if it isn't. The app waits for the middleware's `ready` for as long as it hears from it (0.14.3): until it listens, the middleware says what its start is doing every 2 s (`{type:'starting', text}`, shown on the splash), and only 90 s without a word ends it. Before 0.14.3 the start had 90 s in all, and a Slim stored data rollback longer than that was ended part-way at every start.
- **Worker threads** ([server/workers.ts](../server/workers.ts)). The middleware's main thread only talks to the League client, the Live Client, the dashboard and the overlay; Riot API traffic, the model and the one-shot jobs each have a thread with its own database connection, so none of them can stall the UI or Start / Stop. A worker that exits unexpectedly is started again (after 1 s, then up to a minute after repeated failures); requests to the model time out rather than hang.
- **One database, several connections** (0.8.5). The main thread and the fetching thread write; the model and job threads only read (WAL mode, so reads never wait).
  - **Every write transaction takes the write lock first** (`Db.tx`: `BEGIN IMMEDIATE`) and waits for it. Before 0.8.5 a transaction began deferred, read, then wrote. When another thread wrote meanwhile, SQLite failed it at once with "database is locked": it can't wait then, since what it read is already out of date. Fetching's history and own-games transactions read first, so archiving, restoring or deleting old patches made them fail. One such error also stayed under Start fetching until the next Start; it now goes once that host's next job succeeds.
  - **How long a write waits:** 5 s on the main thread (a wait there holds up the dashboard and the overlay), 30 s on the fetching thread (Delete games of old patches holds the lock for seconds on a large folder).
  - **Archive and restore** pause 150 ms between batches of 200 matches (`BATCH_GAP_MS`). SQLite retries a waiting write at least every 100 ms, so the write gets in between batches instead of waiting behind all of them.
  - **Commits don't wait for the disk** (0.8.6, `PRAGMA synchronous = NORMAL`, WAL's recommended mode; the WAL is flushed at each checkpoint). With FULL, every commit held the write lock through a disk flush. On a Windows CI runner a writer committing every 20 ms then held the lock almost all the time, and the archive's batch, polling for it every 100 ms, timed out after 5 s with "database is locked" (it failed 1 of the 4 Windows runs since the test was added). Reproduced on Linux with every flush slowed to 20 ms: the archive failed every time with FULL and passes with NORMAL, also with 50 ms flushes. An app crash loses nothing; a power cut can roll back the last commits, never corrupt the database.
  - **Opening:** only the main thread creates and migrates the schema. The other threads start after it and write nothing when they open (before 0.8.5 each ran the schema's fixes, which are writes).
  - **Tested:** `server/dbLocks.test.ts` runs a writer thread shaped like fetching while the archive switch archives and restores 1,200 matches, and while they are deleted; it gets no error. With 0.8.4's code the same test fails with "database is locked after 0 ms".
- **Keeping the computer awake** (0.7.4; fixed for Modern Standby PCs in 0.8.3, [desktop/keepAwake.ts](../desktop/keepAwake.ts)).
  - The main process follows the middleware's `scraper` and `jobs` status on `/ws`. It acts while fetching runs, or an evaluation you started (0.19.0; not one the app starts by itself), and **Settings → Keep the computer awake while fetching or evaluating** is on (the default).
  - **Electron's blocker alone wasn't enough.** `powerSaveBlocker('prevent-app-suspension')` is a `PowerRequestExecutionRequired` request (Chromium's `power_save_blocker_win.cc`). Windows treats that as system-required only on PCs without Modern Standby. On Modern Standby PCs (most current Windows 11 laptops and many desktops), 0.7.4–0.8.2 let the PC sleep when idle anyway.
  - **Since 0.8.3, a hidden PowerShell holds the request.** It calls `SetThreadExecutionState(ES_CONTINUOUS | ES_SYSTEM_REQUIRED)` (Windows' own call, nothing installed) on its one thread and waits on stdin. The app ends stdin to let go, and a closed pipe does the same if the app exits or crashes. The command line has no double quotes, so it needs no escaping. Electron's blocker stays alongside it.
  - **Verifying it:** while fetching runs, `powercfg /requests` (as administrator) lists `powershell.exe` under SYSTEM. CI checks this on windows-latest.
  - **Retries:** if the helper stops by itself, the app tries again after 30 s, 2 min and 10 min. If PowerShell can't start (missing, or blocked by policy), it reports that and doesn't retry.
  - **Keep the screen on too** (off by default) switches Electron's blocker to `prevent-display-sleep`, a display request Windows always honours, for PCs where the helper is refused.
  - **Status:** Settings shows whether it is active right now, starting, or refused (and why), and the tray tooltip says so too.
  - **Released** when fetching stops (Stop, or an expired API key), when the middleware can't be reached, and when the app quits.
  - **Limits:** Windows still sleeps when you put it to sleep (lid, power button, Start → Sleep). On battery, a Modern Standby PC ends such requests about five minutes after its sleep timeout.
- **Champ select and games** (0.14.0; the matchup's phase, the same signal that pauses an evaluation's other threads):
  - **Pause fetching during games** (Settings → While you play, on by default; `pauseFetchingInGame` in config.json): the scraper worker stops its lanes and any backfill, so there are no Riot API requests and no database writes. Fetching stays on (the Data tab says "Paused for the game"; Stop still stops it) and resumes when the game ends, with your new game fetched then. Keep-awake follows fetching, so it lets go during the game.
  - **Below-normal priority** for the middleware process (`os.setPriority`, server/gameMode.ts) and the dashboard's renderer if it's open (desktop/main.ts, from the `matchup` event); normal again after. A failure is logged and ignored.
  - **Clean-up when champ select starts:** every worker thread (scraper, model) runs a full garbage collection (a `gc` message, answered `gc-done`), then the main thread; then, on Windows, the process's working set is trimmed (`SetProcessWorkingSetSize(-1, -1)` through koffi, loaded only there). The working set is measured before and after: a trim that frees under 50 MB isn't done again that session.
  - The dashboard window was already destroyed when closed to the tray (0.7.3), and the overlay already exists only during a game (0.6.0).
- **Memory** (0.7.3, measured on 4,400 synthetic games, the middleware process with all its threads):
  - **Idle:** about 170–210 MB once settled (0.8.1; about 230 MB before), measured on 4,400 test games.
    - The model thread keeps about 14 MB of live data for those games (27 MB with patch memory on), down from 27 MB (54 MB) in 0.8.0.
    - Once a model is built, it drops each purchase's state features and raw live context, which only the build reads. The state frames go too: the best buy for your gold keeps only the win probability at each one (0.8.1).
    - The model thread collects its garbage after each rebuild (`server/gc.ts`), so the previous model doesn't linger.
    - It also keeps the lane builds per lane and opponent until the data changes, so a matchup change doesn't re-read every stored game.
    - V8 hands back what a burst of requests leaves about 30 s after it ends.
    - A rebuild reads the stored games one at a time (`Db.modelGamesIter`), so their raw timelines don't all sit in memory at once. Since 0.7.4 its peak is a third lower: +116 MB instead of +170 MB on 4,400 games.
    - **When (0.15.0):** new data rebuilds the model once it is 1 % of the games the model was built from (at least 50), after half an hour at the soonest, or after six hours with any change (`dataDue`, `model/service.ts`). Before, every change rebuilt it after 5 minutes: about every 10 minutes while fetching, a minute each at 118,000 games. Your own new game, a new patch and a Data tab switch still rebuild it soon. No build starts in queue (`Matchmaking`, `ReadyCheck`), champ select or a game, nor while Evaluate builds runs (0.15.1: they share one 4 GB heap in the app, and a build holds two models for a moment; the first build doesn't wait).
    - **How (0.15.0):** from 4,000 games a build prepares its games on helper threads (`model/prepareTeam.ts`, `prepare-worker.mjs`: every core but one, as free memory allows at 512 MB each, at most 6), 250 at a time, put back in the order they were read: the same model as on one thread (tested byte for byte). The model thread answers requests with the model it has meanwhile, where a build used to hold every request up to a minute. Queue, champ select or a game stop a build between batches; the model stays as it was. A game's replay (`state.ts statesAt`) also does less: inventories from the one replay, values summed again only when they change. On the 54,000-game test database with compressed timelines (30,000 model games; Linux, 4 cores): 17.7 s in 0.14.4, 15.4 s on one thread, 8.2 s with 3 helpers, the same games byte for byte. The model thread reading and receiving the games is what's left: on more cores, about 2-3 times faster in all (estimate).
    - **The lane builds (0.15.0):** the games they're made from are read once per data version and kept 3 minutes after the last use, with their validation per patch admission (`recommend`'s `memo`): the next opponent in champ select costs the build itself, not another read and validation of every stored game.
  - **At 54,000 games** (0.9.0 and 0.9.1, the test database with patch memory on; measured on Linux):
    - **The middleware idle** (0.9.1): about 450 MB, against about 830 MB in 0.9.0. 114 MB of it is the middleware with no games; the model thread holds 212 MB of live data, the main thread 15–19 MB and fetching 8 MB.
    - **What 0.9.0 kept besides the live data:**
      - **V8's heap pages:** after a rebuild V8 kept the pages the build had grown into, about 400 MB beyond the live data. Since 0.9.1 the model thread runs V8's thorough collection after each rebuild (and once more 2 s later) and whenever it lets a cache go; that compacts the heap and hands the empty pages back (`collectGarbage(true)` in `server/gc.ts`, half a second each, never during champ select or a game).
      - **Native allocations:** about 200 MB that the system allocator kept after the build freed 54,000 small arrays of game-state frames. Since 0.9.1 the frames come from 16 MB blocks (`frameSlabs`, `model/dataset.ts`), which go back to the system when freed; the evaluation uses them too.
      - **The champ-select caches:** new games refresh the dashboard's build about every 30 s while fetching, so the draft caches of 0.9.0 (about 230 MB) were never idle long enough to go. Since 0.9.1 they are kept in champ select only.
      - **The in-game re-rank's observations:** let go when the game ends since 0.9.1.
    - **Evaluate builds:** while it runs (about 15–20 min there), the middleware holds about 1.5–1.65 GB. Its thread ends with it and V8's part goes; on Linux the allocator still kept about 280 MB of what it freed (736 MB afterwards). Windows' allocator was not measured. With Follow the evaluation on it runs by itself when you have been idle for 10 minutes, at most every 6 hours.
    - **A rebuild** takes 15–35 s in the model thread, never during champ select or a game; the middleware peaks at about 1.15 GB meanwhile.
    - **Champ select:** the first draft takes about 1–1.5 s, the next ones 12–34 ms. The games' weights and each decision's observations are kept between its drafts and let go 3 minutes after the last one. Before 0.9.0 every draft took 1–3 s.
    - **In game:** "Next" takes 8–31 ms per update (the target is under 50 ms).
  - **Evaluate builds:** about 415 MB at its peak on 4,400 games (0.8.1; 590–660 MB in 0.8.0, 1 GB before 0.7.3), about 1.8 GB on 54,000 games (the evaluation alone, in a process of its own; 0.8.6, over 2.0 GB in 0.8.5), back to idle within a minute or two of the end. It takes about 13.5 min on those 54,000 games on one core (10.3 in 0.8.6, before runes against the draft were tested too; 36 min before 0.8.6). With Follow the evaluation on it also runs by itself (0.8.4): at most every 6 hours, never during champ select or a game ([BUILD-SOURCING.md §9.6](BUILD-SOURCING.md)).
    - It holds one cross-validation fold at a time.
    - It fits the game-state model on views of the stored frames instead of copies, and drops the frames once that is done.
    - It reads the stored games one at a time and drops each raw timeline once its game is prepared; the live replay reads its sample again.
    - It collects its garbage before each step: most of the old peak was garbage V8 had not collected yet.
    - Its draft recommendations reuse one similarity buffer instead of allocating one each.
  - **Windows:**
    - Closing the dashboard to the tray now closes it instead of hiding it, so its Chromium renderer's memory is freed. The tray opens a new one.
    - The overlay window exists only while a game runs.
- **Bundling** ([scripts/bundle.mjs](../scripts/bundle.mjs)). esbuild produces `dist-server/{index,worker,model-worker,prepare-worker,job-worker,eval-worker,gpu-worker,convert-worker,slim-worker,slim-part-worker}.mjs` (ESM, because of top-level await) and `dist-desktop/{main,preload}.cjs`. Sandboxed preloads must be CommonJS. Every npm package is bundled, so the installed app ships **no `node_modules`**, which is why `package.json` lists everything under `devDependencies`, except one: **koffi** (0.13.0, Use GPU's way to the NVIDIA driver, a C function interface with prebuilt Node-API modules, MIT). Its JavaScript and the prebuilt module for the platform npm installed (`@koromix/koffi-win32-x64` on the release build) are copied to `dist-server/native/` and loaded at run time, only when Use GPU is on, and (0.14.0) on Windows when champ select starts, for the working-set trim (kernel32's `SetProcessWorkingSetSize`).
- **Security.** All windows use `contextIsolation`, `sandbox` and no `nodeIntegration`. The preload exposes a fixed `window.irelia` API ([shared/desktop.ts](../shared/desktop.ts)). IPC is accepted only from the middleware origin. `openFolder` refuses anything that is not a directory, because `shell.openPath` on a file would run it. External links open in the default browser. Navigation away from the app origin and all permission requests are denied. The middleware listens on 127.0.0.1 only.
- **Evaluate builds in processes of its own** (0.22.0, [server/jobs.ts](../server/jobs.ts), [server/model/evalTeam.ts](../server/model/evalTeam.ts) `leadProcessTeam`). Every thread of the middleware shares one 4 GB pointer-compression heap, so at 170,000 games an evaluation fit on one or two threads. The evaluation's job therefore runs as a child process: the app's own executable with `ELECTRON_RUN_AS_NODE=1` running `dist-server/job-worker.mjs`, which gets its boot and the champ-select flag as messages. Its helpers are processes too (`dist-server/eval-process.mjs`), each reading the evaluation's snapshot from a temporary file. Rank 0 hands out the chunks over each process's message channel and applies the results in game order, so the report is the same as on one thread. `IRELIA_EVAL_PROCESSES=0` keeps threads; `=1` forces processes outside the app (tests).
- **One per-user folder**, `%APPDATA%\irelia-companion\`, holds `config.json` (settings), `timings.json` (how long each long action took here, for its next estimate; 0.10.0), `data\` (default data location), `logs\` and Chromium's profile. The uninstaller keeps it.

## 2. Vanguard and Riot policy guidelines

The overlay is an **ordinary desktop window that happens to sit on top of the game**. That is the only reason it is safe alongside Vanguard, so these rules are hard constraints for future changes:

1. **No contact with the game process.** No DLL injection, no `OpenProcess`/`ReadProcessMemory`, no DirectX/Present hooks, no Overwolf-style in-process overlay. This also means it **cannot draw over exclusive Fullscreen**: League has to run in **Borderless** or **Windowed** mode.
2. **Official data only.** Data comes from the League Client API (lockfile, champ select), the Live Client Data API (`https://127.0.0.1:2999`, which only exposes what the player can see on the scoreboard), the Riot Web API and Data Dragon. Never sniff packets and never read memory.
3. **No input to the game.** No synthetic keystrokes or clicks, and no auto-buy, auto-level or auto-anything. The overlay is click-through by default (`setIgnoreMouseEvents(true, {forward: true})`), non-focusable (`focusable:false`, `showInactive()`) and never steals focus.
4. **Hotkeys via `RegisterHotKey`** (Electron `globalShortcut`), **not** a low-level keyboard hook (`SetWindowsHookEx`). They are registered **only while a game is running** and released when it ends, so they never shadow other apps. `Alt+Tab` is reserved by Windows and is rejected in Settings. The defaults are `Shift+Tab` (show/hide) and `Ctrl+Shift+Tab` (click-through on/off), and both can be rebound.
5. **Small footprint.** The overlay window is created when a game starts and destroyed when it ends. It hugs the HUD (the HUD reports its height and the window resizes), so no large transparent surface is composited over the game.
6. **Riot game-integrity policy.** Show only information the player already has (scoreboard items, KDA, CS, vision score), plus estimates derived from it. Never add enemy summoner/ultimate cooldown timers, jungler tracking, fog-of-war reveals, or anything that de-anonymizes champ select. Build and item recommendations are allowed; "do X now" calls are not. The gold-difference estimate follows this rule: it uses only scoreboard data and labels itself as an estimate (`~`, `±`, confidence). So does the draft-adjusted "Next" item (Stage B, [BUILD-SOURCING.md §9.5](BUILD-SOURCING.md#95-live-re-ranking-b5-modellivets)): it is re-ranked from scoreboard items, levels, CS, kills and the objective events, shows only which item to buy and why, and labels its lift as an estimate. The game-state win probability behind it is never shown in game. Stage C keeps to the same rules ([BUILD-SOURCING.md §10](BUILD-SOURCING.md#10-stage-c-as-built-070)): the only writes to the League client (a rune page and an item set) happen on your click, during champ select only, and never in game; matchup facts are shown before the game only; in game the overlay adds the best purchase for your current gold (shop advice) and who is ahead from the scoreboard (item value, kills), labelled as estimates.

## 3. Live performance tracker ([server/lcu/liveClient.ts](../server/lcu/liveClient.ts))

The tracker polls `/liveclientdata/allgamedata` once per second during a game and every 3 s otherwise. Polls never overlap. It sends `LivePerformance` inside the existing `live` WebSocket event to the dashboard's Build tab and the overlay.

- **You and your lane opponent.** You are the `allPlayers` entry matching `activePlayer` by Riot ID, else by game name; the overlay's inventory and the tracker use the same rule (`identify()` in [server/engine/adaptive.ts](../server/engine/adaptive.ts)). Your lane opponent is the app's matchup opponent ([server/matchup.ts](../server/matchup.ts)): the enemy with your `position`, so top and mid both work. When the API reports no position (practice tool, some custom games), it is champ select's likely opponent (the draft is kept through its game), else the enemy Irelia faced most in your lane in the database; an opponent you pick in the dashboard wins over all of them. The tracker, the threat summary and "Next" all use the same opponent.
- **CS.** `scores.creepScore` (minions plus monsters; the API only reports the combined value), CS per minute, and the difference against the lane opponent. If every value seen is a multiple of 10, the tracker flags `csGranularity: 10` and the UI prefixes CS with `≈`.
- **Vision.** `scores.wardScore` (vision score), per minute and as a difference, plus Control Wards and the trinket in your inventory. The API has no ward-placed or ward-killed events, so per-ward counts are not available.
- **Gold difference.** Only your own unspent gold is visible. The opponent's is hidden by fog of war, so it is estimated:
  - **inventory value**: the Data Dragon total cost of the visible items. This is exact.
  - **income model**: 500 starting gold, plus passive income (2.04 g/s from 1:05), plus 20 g per CS, plus kill bounties replayed from the event feed (death-streak decay, shutdown bonus, first blood, 50 % assist share), plus turret, inhibitor and Baron gold.
  - **self-calibration**: each tick compares your exact net worth (items + unspent gold) with the model's figure for you. The ratio (clamped to 0.75–1.15 and faded in after 1:00) corrects the opponent's modelled income for consumables, sold items and bounty rules the model doesn't cover.
  - opponent net worth = `max(inventory, calibrated income)`. The UI shows the difference with `±uncertainty` and a confidence level: high when visible items explain almost all of it, low when a lot of unspent gold is being guessed. The exact inventory difference is shown next to it.
- Constants live in `GOLD` and can be tuned. A sample is stored every 15 s of game time (up to one hour) for the sparklines.

**Without League:** `POST /api/dev/simulate-live` (body `{"startAt":420}`, or `{"stop":true}` to end) streams a synthetic Irelia vs Darius top game through the real pipeline; add `"lane":"MIDDLE"` for Irelia vs Syndra mid. You start with the trinket and 500 gold and buy the start at 0:20, which shows the overlay's **Buy first** (§8). At 24:00 your Plated Steelcaps become Armored Advance (the free tier-3 upgrade), which exercises the overlay's upgrade rule (§8). The desktop app reacts to it exactly as it does to a real game: the overlay appears and the hotkeys register.

## 4. Storage manager

`config.json` (`%APPDATA%\irelia-companion\config.json`, or `IRELIA_CONFIG_DIR`) holds `dataDir`, `riotApiKey` and `desktop` (overlay and hotkey settings). Each process merges only its own keys and writes the file atomically.

| Endpoint | |
|---|---|
| `GET /api/estimates` | How long each long action would take if started now, in ms (estimates: this computer's last run, scaled to the data as it is now; null when there is nothing to go on): `evaluate`, `audit`, `purge`, `archive`, `restore`, `trim`. The Storage card's own (`compact`, `clear-cache`, `clear-expired`, `ddragon-old`, `move`) come with `/api/storage/stats` (`estimates`). While an action runs, its status carries `progress` (`{fraction, etaMs, startedAt, at}`): the jobs, the model's rebuild, the backfill, the old-patch switches (pushed as `old-patches` over `/ws`), a move (`storage`), and the Storage card's actions (`storage-action`) |
| `GET /api/system` | This computer, for Evaluate builds (0.11.0): the processor's name, its logical cores, the memory (`total`, `available` now without swapping, the `reserve` always left free: 15 % of it, at least 1.5 GB), and the threads an evaluation started now would use with the memory each one beyond the first would take (`perThread`, an estimate; `measured` once an evaluation here has measured it). Read on this computer for the Data tab, never sent anywhere |
| `GET /api/gpu`, `PUT /api/gpu` `{useGpu}`, `POST /api/gpu/test`, `DELETE /api/gpu/log` | Settings → GPU (0.13.0, [server/gpu/](../server/gpu/)): Use GPU (`useGpu` in config.json, read when an evaluation starts), Test GPU (refused while Evaluate builds runs, and in champ select or a game) and the GPU log, `%APPDATA%\irelia-companion\gpu.log` (its last 400 lines; kept under 512 KB). Only this computer's GPU is involved: nothing is sent anywhere |
| `GET /api/storage/stats` | Size and file count per category (SQLite DB + WAL, Riot API cache per kind, Data Dragon per patch, other files), rows and size per database table, and free space on the drive. Since 0.14.0 the cache's parts are its database's entries per kind (plus any files of an earlier version not converted yet, and the database's own index and free pages); `conversion` is the background conversion's status, and `suggestCompact` (`{freeBytes}`) says Compact database is worth running once |
| `GET /api/storage/slim`, `POST /api/storage/slim` `{action}` | Slim stored data (0.14.0, [server/slim.ts](../server/slim.ts)): its status, and `start`, `rollback` (a verified run) or `keep` (delete its backup). Refused in champ select or a game, during another storage operation, while the conversion still has work, and while Evaluate builds or the data audit runs. While it runs, `POST /api/model/evaluate`, `/api/db/audit` and `/api/db/purge-stale` answer 409 and no automatic evaluation starts (the results it compares must not change meanwhile); pushed as `slim` over `/ws` |
| `GET /api/storage/inspect?dir=` | Pre-flight check: path is absolute and ≤140 characters (Windows' 260-character path limit), not inside or around the current folder, writable, enough free space, and whether it already holds data |
| `POST /api/storage/clear` | `riot-expired` (only entries past their TTL; matches and timelines are kept), `riot-cache`, `ddragon-old` (every version but the one in use and the newest of the six patches before it, whose item and Irelia files patch memory reads), `compact-db` (0.14.0: participants by integer ids when they're over a quarter of the database and it makes them smaller, then `auto_vacuum = INCREMENTAL`, VACUUM and a WAL checkpoint) |
| `GET/PUT /api/old-patches` | The archive and cache switches (`{"archive": true}`, `{"trimCache": true}`; server/oldPatches.ts): the answer comes once the change is applied. Moves and compaction wait while one runs, and it waits for them |
| `POST /api/storage/migrate` | `{targetDir, mode}`. `move`: copy, verify, switch, then delete the old copy. `copy`: keep the old copy. `adopt`: use the data already in that folder |
| `GET/PUT /api/settings` | Riot API key. The installed app has no `.env`, so the key is entered in Settings. A saved key wins over `.env`. Also your server (`{myPlatform}`) and, since 0.14.0, `{pauseFetchingInGame}` |

Moving the folder: stop the worker threads (the scraper, every Riot cache and DB writer; the model and any job, which hold the database open), copy the DB with SQLite `VACUUM INTO` (a consistent, compacted snapshot that includes the WAL), copy cache files with their mtimes (so cache TTLs survive), then verify every file size, SQLite `quick_check`, and the row count of every table. Next, write `config.json`, swap the live DB connection (`swappableDb`), delete the old copy (move only), and start the workers again on the new folder. **Compact database** pauses them too: `VACUUM` needs the database to itself. Other `/api` routes answer 503 while this runs. The API cache's database is copied the same way (`VACUUM INTO`) and checked with `quick_check` and its entry count. Only the app's own entries (`irelia.db*`, `cache/`, `ddragon/`) are copied or deleted, so a folder shared with other files is never wiped. If anything fails before the switch, the partial copy is removed and nothing changes.

**What's stored, and how** (0.14.0):
- **Riot API cache:** one SQLite file, `cache/riot-cache.db` (WAL, 16 KB pages, incremental auto-vacuum), one row per response keyed by kind, scope and key, with the time it was stored (the TTLs count from it). Each response is a zstd frame (level 9; 0.14.0 wrote level 15, which reads the same) with a dictionary made from a match and a timeline of the cache itself (server/codec.ts: `[1][dictionary id][frame]`; the dictionaries are kept in `codec_dicts`, so a later one never orphans older rows). Before 0.14.0 it was one gzip file per response under `cache/<kind>/<scope>/`: those are read until they're converted. Since 0.14.1 an index of each entry's size (`entries_size`, schema version 2), built once by the fetching thread, gives the Storage card its sizes without reading the entries.
- **Timelines in the database:** `timelines.data_z` holds the capture compressed the same way (with a dictionary made from a capture of this database), `data` is '' then; every query reads `timeline_json(data, data_z)`, a SQL function registered on each connection. The archive still stores timelines as JSON inside its gzip, and puts them back compressed.
- **The background conversion** (server/convert.ts, convertWorker.ts): while the app is idle, earlier versions' cache files go into the cache's database and plain timelines into `data_z`, a batch at a time. Each item is read back and compared before its old copy is deleted; an unreadable file is left where it is. It stops in champ select and games, during a folder move and Compact, and resumes after; the Storage card shows its progress.
- **Space comes back by itself:** `auto_vacuum = INCREMENTAL` and `journal_size_limit` (64 MB); `incremental_vacuum` runs after archiving, deleting old patches, trimming the cache and the conversion. A database from before 0.14.0 needs one full VACUUM for it: the Storage card suggests Compact database once the conversion is through.
- **Participants by ids** (Compact database, only when `participants` and its index are over a quarter of the database, and kept only when the result is smaller): each PUUID and match id stored once (`puuid_ids`, `match_ids`), `participant_rows` refers to them, and `participants` is a view with the old columns, so every reader is unchanged. The rebuild compares every row both ways before the old table goes; what it did is in `meta` (`participants_layout`).
- **Slim stored data** (Settings → Storage): the one action that deletes stored data. (a) Raw cached timelines older than the newest two patches whose games are captured; (b) the fields of cached matches and timelines no capture reads (found by running the captures on each response through a recording Proxy, and kept only when every capture gives the same result from the slim copy); (c) in the archive, the other nine players' columns that neither the draft model nor the per-champion sums read. First it checks the free space and backs up (`backup-0.14.0/`: a `VACUUM INTO` copy with its SHA-256, the affected cache entries moved into `cache-entries.db`, a manifest), records a quick evaluation and 200 drafts' outputs (lane baseline, draft build, live "Next" at five moments), slims, and runs the same again. Any difference: the database file is restored from the copy and checked by SHA-256, and the cache entries go back. The same results: the backup stays until **Roll back** (row by row, so games fetched since stay) or **Keep the slimmer data**. A run cut short is undone at the next start, before the database opens. Fetching pauses while it runs. Since 0.14.4 step 4 (b) slims on several threads (`slimPart`, server/slimPart.ts: every core but the writing one, as the free memory allows at 384 MB each, at most 8): the games go out 50 at a time, and the slim thread alone writes what comes back, a transaction per 50 games; the blobs are the same on any number of threads. Since 0.15.2, where every thread shares one 4 GB heap (the app), the slimming threads together stay within 1.5 GB of it (4 threads), the check's quick evaluation runs on the slim thread alone (the same results; each more thread took about 1.3 GB at 121,000 games, and 0.15.1 ran out of memory before slimming), its parts' leftovers are collected between them, and the model thread lets go of its request caches as it starts (it keeps its model). The card shows each step's phases (0.14.2): what they count (MB of the database copy, cache entries, evaluation steps, drafts, games, archived games) and their time left, an estimate from each phase's own pace; the check's phases start from what the same phase of the baseline took.

`IRELIA_DATA_DIR` pins the folder for a process and disables moving it. `IRELIA_DEFAULT_DATA_DIR` only changes the default; the desktop app sets it to `%APPDATA%\irelia-companion\data`.

## 5. Riot API pacing ([server/riot/rateLimiter.ts](../server/riot/rateLimiter.ts))

Riot counts requests per key in windows per routing value, so the platform host (`kr.api…`: league, mastery) and the regional host (`asia.api…`: match-v5) each have their own budget and their own limiter. A development key allows 20 requests per second and 100 per 2 minutes. The scraper runs one lane per host, so every host is kept busy at once ([BUILD-SOURCING.md §5.4](BUILD-SOURCING.md#54-one-lane-per-riot-host)), and the Match Database panel lists each host's task and queue. Settings shows when the key was saved: a development key stops working 24 h after it was generated, a personal key never does ([§5.6](BUILD-SOURCING.md#56-api-keys)).

- **Even spacing instead of bursts.** Each window asks for a spacing of `windowMs / floor(limit × 0.9)`, and the largest one wins. For a development key that is 120 s / 90, so **one request every 1.33 s per host**. Each window is enforced with GCRA, which allows up to 3 back-to-back starts after an idle spell. A sliding log per window stays as the hard backstop: never more than `floor(limit × 0.9) + burst − 1` requests in any window, and never more than Riot's limit.
- **Method limits.** `X-Method-Rate-Limit` paces each endpoint family (league, mastery, match IDs, match, timeline) on its host, on top of the app limit. A request needs a slot from both.
- **Time limit and dropped connections.** A request (headers and body) is abandoned after 30 s. A timeout or a network error, such as a connection that died while the computer slept, is retried on a new connection after 1, 2, 4 and 8 s (5 attempts), like a 5xx. Before, such a request could hang for minutes. A stop by the caller is never retried.
- **429s.** Retry-After is always honoured. An `application` or `method` 429 also drops that scope's pace to 80 % and lets it climb back over the next minute. A `service` 429 (or one without a type) means Riot's backend is busy, so only that host waits (Retry-After, else 1, 2, 4 … up to 30 s) and the pace stays the same. After a forced wait, requests resume at the normal spacing, with no catch-up burst.
- **Concurrency.** Up to 3 requests are in flight per host (`RIOT_CONCURRENCY`, 1–3). They queue in order and their start times are still paced, so concurrency hides network latency but never raises the rate. The background loop's mastery checks and match parsing commit results in their original order. The database therefore ends up exactly as it would with one request at a time, and stopping drops the requests still waiting for a slot.
- **Same budget, no stalls.** A development key still gets 90 requests per 2 minutes. Before, the scraper sent ~90 requests in a few seconds and then stalled for up to 2 minutes. Now requests flow steadily: in a simulated 10-minute run, the longest gap while work remained dropped from ~107 s to ~1.3 s. The other side of this is that a short burst of work no longer finishes at once: each uncached request takes about 1.3 s.
- **Live status.** The worker checks its limiters 5 times a second and posts a small `ratelimit` snapshot over `/ws` only when something changed. The full status report keeps its 2 s cadence. The Match Database bars (Data tab) animate between snapshots and show the pace. The panel tells apart **Pacing (next request in X ms)**, **Rate limited by Riot (Retry-After)** and **Up to date – no new work** (the background loop's 60 s idle sleep).

## 6. Build and verify (Windows 11)

```bash
npm install
npm run typecheck        # server + web + desktop
npm test                 # 142 tests, offline (Data Dragon snapshot): engine (incl. lanes), overlay build plan, lanes (champ select, DB, simulator),
                         # routing/scope/cache, Riot API pacer, live tracker + gold model, storage migration, capture, sourcing
                         # (patch window, pilots, harvesting, per-host lanes, own games), v2 → v3 migration, backfill, audit,
                         # draft model + evaluation, matchup, worker threads (restart after an exit), Stage C (import, facts, best buy)
npm run build            # typecheck + Vite + esbuild bundles (dist/, dist-server/, dist-desktop/)
```

Run the desktop shell in development (Vite hot reload, tsx-watched middleware):

```bash
npm run desktop:dev
```

Run the built app without installing it (spawns `dist-server` itself; uses `./data` and `.env`):

```bash
npm run desktop
```

Build the installer → `release/Irelia-Companion-Setup-0.5.1.exe` (plus `release/win-unpacked/` for a no-install run):

```bash
npm run dist
```

Install it locally: run `release\Irelia-Companion-Setup-0.5.1.exe`. It's unsigned, so SmartScreen asks first: choose *More info → Run anyway*. The installer is per-user (no admin), lets you pick the folder, and creates **Irelia Companion** in the Start Menu and on the desktop. Uninstall from *Settings → Apps*.

First run of the installed app:
1. **⚙ Settings → Riot API key**: paste your key (development keys expire every 24 h).
2. Optional: **Settings → Storage → Change data location → Browse…**. To reuse the data gathered in development, pick the `data` folder of your development checkout and choose **Use the existing data there**. To start somewhere else, choose **Move**.
3. Set League to **Borderless** (*Settings → Video → Window Mode*).

Check the overlay and tracker without a game: start the app and run the command below. The overlay should appear on the right edge, click-through. Press `Ctrl+Shift+Tab` to unlock and drag it, and `Shift+Tab` to hide it. Send `{"stop":true}` to end the simulated game.

```bash
curl -X POST localhost:45174/api/dev/simulate-live -H "Content-Type: application/json" -d "{\"startAt\":420}"
```

For mid, send `{"startAt":1380,"lane":"MIDDLE"}`: the overlay's title reads **Mid · vs Syndra**. At 24:00 your boots become Armored Advance and the boots slot stays filled: ✓ when the build's boots are Plated Steelcaps, ⇄ for any other pair.

Check a storage move through the API (the Settings UI does the same):

```bash
curl "localhost:45174/api/storage/inspect?dir=D:%5CGames%5CIreliaData"
```

```bash
curl -X POST localhost:45174/api/storage/migrate -H "Content-Type: application/json" -d "{\"targetDir\":\"D:\\\\Games\\\\IreliaData\",\"mode\":\"move\"}"
```

```bash
cat "$APPDATA/irelia-companion/config.json"
```

## 7. Lanes (top and mid)

Irelia is played top and mid, so builds are collected and recommended per lane. The lane is decided **per game**: every stored game keeps Riot's `teamPosition` (`irelia_games.position`, stored since the first schema, so old games need no re-download). OTP discovery stays lane-free (champion mastery has no lane), and so does fetching: match-v5 can't filter match IDs by position, so a game's lane is only known once it is downloaded.

- **Engine** ([server/engine/recommend.ts](../server/engine/recommend.ts)). A game's weight is patch weight × lane weight × matchup weight. Games in the requested lane count 1. The other lane's games blend in at `otherLaneWeight` (0.15) only while the lane has fewer than `minLaneGames` (30) valid games, and not at all after that. Matchups only count games in the same lane; below `minMatchupGames` (15) the lane's other matchups blend in at 0.15, as before. The previous patch is admitted on the lane's own current-patch count. Jungle, bot, support and unknown-position games never feed a lane build. The win rate only counts the lane's own games, and only those against the chosen opponent when one is set (`—` when there are none), so blended games never change it. The validation counts (used, stale, invalid) cover the games the build can use: the lane's, plus the other lane's while it blends in.
- **API.** `GET /api/recommendation?enemy=&lane=TOP|MIDDLE` (`MID` works too; without `lane` every position is mixed, as before). The result's `sample` says `laneGames`, `otherLaneGames` and `usedLaneFallback`, and the UI shows a note whenever the other lane was blended in. `/api/db/stats` splits each patch's games by position.
- **Stored lane opponent.** Each stored game's lane opponent is the enemy who stood nearest Irelia between 3:00 and 14:00 (timeline positions); Riot's `teamPosition` match is kept next to it and used when there is no timeline ([BUILD-SOURCING.md §4.3](BUILD-SOURCING.md#43-the-real-lane-opponent)).
- **Champ select** ([server/lcu/connector.ts](../server/lcu/connector.ts)). Your lane is the assigned position (ranked and normal draft). Blind pick, ARAM and customs have none, so the lane you picked last is used (saved in `config.json`). The likely lane opponent is worked out for each lane from the enemy **picks everyone can see**: an enemy whose role is visible, else, among the picks whose role is hidden, the pick Irelia faced most often in that lane in the database, else the first pick whose class fits (Fighter/Tank top, Mage/Assassin mid). The two lanes never get the same pick: a pick both would take stays in the lane where it makes up the larger share of Irelia's games (a share counts at least 20 games), else the lane its class plays, else top, and the other lane takes its next guess, or none until another enemy picks ([server/engine/laneOpponent.ts](../server/engine/laneOpponent.ts)). It never looks anyone up.
- **UI.** The server resolves one matchup for every window ([server/matchup.ts](../server/matchup.ts)) and pushes it with its build over `/ws`. The Build tab has a Top / Mid switch that follows the game, then champ select, and says which ("from champ select", "set by hand (champ select: Mid)"); a manual choice holds until the draft and game end. The overlay shows the same matchup and build, titled **Top · vs Darius · draft-adjusted** / **Mid · vs Syndra · lane build**, so an opponent or lane picked in the dashboard, or the Draft-adjusted / Lane baseline switch, reaches it at once. The live panels name the lane, and the Data tab shows top / mid counts per patch.
- **Simulators.** `POST /api/dev/simulate-draft` takes `"lane"` (`"MIDDLE"`, or `""` for blind pick; default `TOP`), and `POST /api/dev/simulate-live` takes `"lane":"MIDDLE"` and `"positions":false` (no positions, as in customs).

## 8. Overlay build plan ([shared/buildPlan.ts](../shared/buildPlan.ts))

The overlay lists the plan (the three core items with the boots where high-elo games finish them, then the 4th / 5th / 6th pick) and ticks what you already own, using the inventory from the Live Client API (any order). A later slot whose top option is already planned, or blocked by a planned item's unique group, takes its next option. Each owned item fills at most one entry, matched in this order:

| You own | Entry | On screen |
|---|---|---|
| the planned item | done | greyed, ✓ |
| an upgrade of it (its recipe chain, including `specialRecipe` transforms such as Muramana) | upgraded | the owned item, greyed, ✓; hover: "Armored Advance (upgraded Plated Steelcaps)" |
| for the boots slot: any finished boots (tier 2, or upgraded from tier 2) | swap | the owned item, greyed, ⇄; hover: "Mercury's Treads instead of Plated Steelcaps" |
| a finished item of the same unique group (you can no longer buy the planned one), or an item the recommendation lists for that slot | swap | as above |
| some of its components | building | dashed border; Next shows the gold still needed (price minus the components you own, as the shop counts them) |

**The start** (0.11.0): above the plan, the overlay shows the build's starting items (its top option: "Doran's Blade + Health Potion ×2", or Long Sword and potions), as the Build tab's Starting Items card does. In a game it is titled **Buy first** until you own one of its items or 1:30 has passed (potions may be drunk already), then **Start**, greyed with ✓, and it goes at 3:00. Until the start is bought, the best buy for your gold ("With 500g: …") waits: the start is the buy.

**Boots' place** (0.7.1): the engine records, for each game, how many completed items were finished before its tier-2 boots, and the weighted median (`bootsAfter`, 0–3) places the boots in the plan, the saved item set and the Build tab's Boots card ("after item 1"). Before, boots always came after the third item. When the boots are the plan's next entry, they are "Next" in draft-adjusted mode too (the live re-ranking only ranks completed items), and the best buy aims at them.

**Next** is the first entry that is neither done, upgraded nor swapped. Tier-3 boots (Armored Advance, Chainlaced Crushers, Gunmetal Greaves, …) cost nothing extra (`gold.base` 0 in Data Dragon) and replace the tier-2 item id in your inventory. Recommendations never contain them, because the engine records the first tier-2 boots bought. Gunmetal Greaves has no `Boots` tag in Data Dragon, so boots are also recognised by what they upgrade (`/api/meta` serves every Summoner's Rift item with its recipe, unique groups and a boots flag).

## 9. Your own games

When the League client connects, the middleware reads your account: `puuid` from `/lol-summoner/v1/current-summoner` and the platform from `/lol-platform-config/v1/namespaces/LoginDataPacket/platformId`. Both endpoints are unofficial; without the platform, it asks Riot's account-v1.

The worker then fetches your ranked solo and flex Irelia games of the current and previous patch, with their timelines, through the same pacer, and stores them with source `me`. It looks again 2, 6 and 15 minutes after each game ends (match-v5 has a finished game a few minutes later) and every 30 minutes while fetching runs. Your games never feed the high-elo builds or the draft model; the personal layer uses them (dashboard → **Your games**: your item timings against high-elo, what works for you, and a post-game review; [BUILD-SOURCING.md §9.4](BUILD-SOURCING.md#94-personal-layer-b4-modelpersonalts)). `"ownGames": false` in `scrape.config.json` turns this off.

