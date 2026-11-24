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
