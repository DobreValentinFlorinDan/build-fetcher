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
- **In game** an overlay shows three next-buy options and counter items, re-ranked from the live game: your items,
  the enemy's items as the scoreboard shows them, and the game state. Each option shows its evidence, the gold you
  still need (or "Buy now"), the part of its recipe your gold covers now, and what in the enemy's build makes it fit.
  Nothing pops up and nothing tells you what to do.
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

### Pausing

- **Fetching:** the Fetching page has **Pause fetching** and **Resume fetching**. Pausing stops new requests at once,
  and the choice is kept after a restart. Fetching also pauses on its own in champ select and in game.
- **Learning:** the Model page has **Pause learning** and **Resume learning**.

The same switches are in Settings.

### Moving to another PC, or keeping a copy

Settings → Storage → **Export…** writes one zip with your settings, games, counts and model. The Riot API key is
included only if you tick "Include my Riot API key". Data Dragon and logs are left out. The app keeps fetching and
learning meanwhile.

On the other PC, **Import…** checks the zip, then applies it at the next start (**Restart now**):
- The data that was there is kept aside in `%APPDATA%\build-fetcher\before-import` until the next import.
- Without a key in the zip, that PC keeps its own key.
- A zip that isn't a Build Fetcher backup, or that comes from a newer version, or holds a damaged database or
  unexpected files, is refused before anything is changed.

## How it works

```
Riot API ──► fetch scheduler ──► ingestion + data-quality filters ──► store (SQLite)
 (your key)   per-routing-value     one record per match, all 10        tier B counts, tier A records,
              budget, shared-key    players counted                     validation sample
              aware                                                            │
                                                                               ▼
dashboard / champ select / overlay ◄── recommender ◄── learner (CPU; optional GPU, checked against the CPU)
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

One match fetch feeds tier B for all ten champions. Timelines are fetched for tier A games plus a sample of the rest
(25% by default; Settings → Regions and rank), used to learn item and skill order and what the enemy had built.

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
- the enemy team's items at that minute: what share of their build is armor, magic resist, healing, anti-heal,
  ability power, attack damage and health, and how each of your items does against each of those;
- the game state: time, gold, CS and level difference to the lane opponent, kills, towers, dragons and barons.

Every player of every stored game counts, winners and losers alike: an item's effect shows only next to the games it
lost. The first model is trained on a batch of 2,000 new games while the PC is idle. After that, each new game is
learnt as it arrives, in a few milliseconds, whenever you're not in champ select or a game. Every 150 games the
updated model is checked against the one in force on held-out games, and it replaces it only if no score got worse.
The Model page says how many games were learnt since the last accepted model.

1. **Step 1:** a logistic model per champion-role, shrunk toward a global model.
2. **Step 2:** a small shared network on top, with champion, item, rune, summoner and skill-order embeddings.

**Guardrails.** Every update is checked on held-out games: log loss, Brier score, and calibration error per 10-minute
window. An update that makes any of them worse is rejected and the previous checkpoint is kept. Checkpoints are
versioned, and Settings rolls back to an earlier one in one click. The log says what each update changed.

**Calibration.** The model's confidence is calibrated with a temperature that varies over game time. It is fitted on
recent records held out from training (never on the validation games), and it changes only when the evidence is
statistically clear.

**Determinism and GPU.** Results on the CPU are deterministic for the same data, whatever the number of threads.
Settings → GPU (NVIDIA, off by default) moves the training and evaluation forward pass and the draft-similarity sums
onto the card, through the driver loaded at run time. Nothing extra needs installing. The GPU's results are checked
against the CPU before use and on a sample while it runs; any mismatch or error falls back to the CPU for good.

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
