# Build Fetcher

A Windows app that recommends the best build for any League of Legends champion in any role:
- runes;
- summoner spells;
- skill order;
- starting items;
- item path;
- boots;
- late-game swaps.

It learns from every new high-elo match it fetches, and runs on one PC.

- **In champ select** it detects your champion and role and shows its build at once. The build updates as picks come
  in. One click imports the rune page and an item set into the League client.
- **In game** an overlay shows several next-buy and counter-item options, re-ranked from the live game. Each option
  comes with its evidence. Nothing pops up and nothing tells you what to do.
- **Every recommendation shows its evidence:** plain games and wins, the sample size, the win probability added, and
  a flag when data are thin. When data are thin it falls back from the matchup to the enemy's damage profile, then
  to the lane, then to the champion.

Build Fetcher is local only. The only things it talks to:
- the Riot web API, with your own API key;
- Data Dragon;
- the League client's local API (LCU);
- the in-game Live Client Data API.

Nothing is sent anywhere else: no account, no server, no telemetry. It never reads game memory and injects nothing
(Vanguard-safe).

Build Fetcher isn't endorsed by Riot Games and doesn't reflect the views or opinions of Riot Games or anyone
officially involved in producing or managing Riot Games properties. Riot Games, and all associated properties are
trademarks or registered trademarks of Riot Games, Inc.

## Install and first run

1. Run `Build-Fetcher-Setup-1.0.0.exe` from the GitHub release. It installs per user, with no admin rights.
2. **API key.** If Irelia Companion is installed with a key, Build Fetcher offers "Use the same key as Irelia
   Companion". On your click it copies only that value: Irelia Companion's file is opened read-only and never changed.
   Otherwise paste a key from https://developer.riotgames.com. If Riot rejects the key (401/403), Settings says so
   and links to the portal.
3. **Region.** Choose your region; EUN1, EUW1 and KR are fetched by default. Fetching then starts in the background.

Settings, the database, Data Dragon, model checkpoints and logs live in `%APPDATA%\build-fetcher`. The uninstaller
keeps that folder. Delete it by hand to start over.

## How it works

```
Riot API ──► fetch scheduler ──► ingestion + data-quality filters ──► store (SQLite)
 (your key)   per-routing-value     one record per match, all 10        tier B counts, tier A records,
              budget, shared-key    players counted                     validation sample
              aware                                                            │
                                                                               ▼
dashboard / champ select / overlay ◄── recommender ◄── learner (CPU; GPU-checked similarities)
          (127.0.0.1 only)                              guarded, versioned checkpoints
```

### Which games are used

- **Queue and rank:** ranked solo/duo on Summoner's Rift only, from Master+ on EUN1, EUW1 and KR.
- **Widening:** a champion-role pair with too few games is widened to Diamond automatically.
- **Your own games** come first, and count more in the model.
- **Dropped games:**
  - remakes;
  - games with an AFK or leaver;
  - games whose roles can't be assigned with confidence (`teamPosition` checked against the timeline's positions);
  - malformed or partial answers.

  Every drop is counted by reason, in the log and on the Fetching page.
- **Patch and hotfix** come from each game's version. Recommendations weigh recent patches and hotfixes, and rank,
  more.

### Two tiers of data

- **Tier A, your pool:**
  - your top 5 champions by games played;
  - the champions you add in Settings;
  - any champion you play 3 times in the current patch.

  These keep detailed records with timelines and get the learned model, aware of the draft and the game state.
- **Tier B, every other champion and role:** counts only. They are kept per patch × champion × role × rank × context
  (any opponent, by lane opponent, by enemy damage profile) × option: games, wins and the win probability added.
  Tier B is ready the moment you lock any champion.

One match fetch feeds tier B for all ten champions. Timelines are fetched for tier A games plus a 10% sample, used to
learn item and skill order.

### Sharing the key with Irelia Companion

Both apps use the same key, so they share one rate limit. Build Fetcher reads Riot's `X-App-Rate-Limit` and
`X-App-Rate-Limit-Count` headers on every answer, so requests made by Irelia Companion are counted too. It never
starts a request that would pass the key's limit, follows `Retry-After` on a 429, and budgets each routing value
separately: EUN1 and EUW1 share EUROPE, and KR uses ASIA. The Fetching page shows when another app is using the key.
Fetching pauses in champ select and in game.

### Learning

The model predicts the win chance from:
- the champion, role, both team comps and the lane opponent;
- runes, summoners, items owned and their order;
- the game state: time, gold, CS and level difference to the lane opponent, kills, towers, dragons and barons.

It is retrained incrementally as games arrive, with warm-started mini-batches, and only while the PC is idle.

1. **Step 1:** a logistic model per champion-role, shrunk toward a global model.
2. **Step 2:** a small shared network on top, with champion, item, rune, summoner and skill-order embeddings.

**Guardrails.** Every update is checked on held-out games: log loss, Brier score, and calibration error per 10-minute
window. An update that makes any of them worse is rejected and the previous checkpoint is kept. Checkpoints are
versioned, and Settings rolls back to an earlier one in one click. The log says what each update changed.

**Determinism.** Results on the CPU are deterministic for the same data. The GPU path (NVIDIA, through the driver
loaded at run time) is checked against the CPU on a sample, and any mismatch falls back to the CPU.

### Disk

The default budget is 2 GB, adjustable in Settings → Storage. Raw API answers are never kept. Each match becomes
compact, quantized, zstd-compressed rows, and counts. After each training cycle the app keeps only:
- the checkpoints;
- the counts;
- a fixed-size validation sample;
- the records still inside the learning window.

It deletes the rest, trims the oldest patches while over the budget, compacts the database, and logs the bytes freed.

## Development

The repository is a Rust workspace with a React dashboard (Vite + Tailwind) and a Tauri v2 shell.

| Path | What |
|---|---|
| `crates/core` (`bf-core`) | carried over from Irelia Companion: codec, correctly rounded math, Riot client + GCRA limiter, Data Dragon, item catalog, CUDA loader, test kit; `build.rs` is the contract between the parts |
| `crates/store` (`bf-store`) | settings and key import, the on-disk format, ingestion and filters, fetch scheduler, clean-up |
| `crates/learn` (`bf-learn`) | features, models, guardrails, checkpoints, backends, recommender |
| `crates/server` (`bf-server`) | axum REST + `/ws` on 127.0.0.1, LCU, champ select, importer, Live Client, wiring of the parts |
| `crates/desktop`, `desktop/tauri` | the shell (settings, keep awake, overlay placement) and the Tauri app (tray, overlay, NSIS) |
| `web/` | dashboard and overlay page |

```bash
npm ci
```

```bash
npm test             # cargo test --workspace (without the Tauri app, which needs WebKitGTK on Linux)
```

```bash
npm run test:web     # typecheck + web unit tests
```

```bash
npm run dev          # headless server (bf-server) + dashboard on http://localhost:5173
```

```bash
BF_FAKES=1 npm run dev   # the dashboard with sample data, no database or key needed
```

The tests run offline, against a Data Dragon snapshot (`fixtures/`) and synthetic match-v5 games
(`bf_core::testkit::Synth`, `bf_learn::synthetic`). No test uses a real key.

- `docs/DESIGN.md`: the design and the hand-over log.
- `docs/MEASUREMENTS.md`: measured disk size, throughput, model quality and timings, with the commands used and what
  was not verified.

Releases are made only by the Release workflow: Actions → Release → Run workflow on `main`. It builds the NSIS
installer and publishes it with `docs/releases/<version>.md` as the notes.

## Credits

Build Fetcher started from Irelia Companion 2.2.0, by the same author. See NOTICE.
