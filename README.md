# Irelia Companion

Local League of Legends dashboard for Irelia OTPs. It reads your champion select from the League client (LCU), picks your lane opponent, and recommends runes, items, and skill order for your lane: top or mid. The recommendations are aggregated from high-elo Irelia one-trick games on the **current patch**, which a background scraper collects through the Riot API. It also collects Irelia mains' games, other players' Irelia games found in fetched matches, and your own games, each tagged by source, with every participant and a compact timeline per game.

**Draft-adjusted builds** (Stage B, [docs/BUILD-SOURCING.md §9](docs/BUILD-SOURCING.md#9-stage-b-as-built)) score every decision (each rune row, each item slot, boots, skills) by how much more often than the game state predicted high-elo games like yours were won: same lane, similar enemy and ally comps, the same opponent, weighted by pilot, patch and region. They say what changed and why ("enemy magic damage 62%"), fall back to the lane baseline when the evidence is thin, re-rank the next item in game from the Live Client data, and compare your own games with the high-elo ones. They are off by default until `npm run eval:builds` shows they beat the lane baseline; the dashboard's **Draft-adjusted / Lane baseline** switch overrides that.

The app has three tabs, and the overlay:
- **Build.** The build for your lane and matchup: runes, skills, items and, in draft-adjusted mode, what the draft changed and why. The matchup follows the League client: your assigned lane and likely lane opponent in champ select, the enemy in your position in game. Click an enemy chip, search a champion or switch Top / Mid to override it (until the draft and its game are over). In game, a live panel shows CS, vision and gold against your lane opponent, with the next item to buy.
- **Your games.** Your item timings against the high-elo median, what works for you, and a post-game review of each game: what the app showed, what you bought, how it went.
- **Data.** Fetching (Start / Stop), what is stored, the patch per region, the draft model, patch memory and their evaluation, archiving old patches, and maintenance (rebuild from cache, data audit).
- **Overlay** (desktop app). The same matchup and build in game: whatever the dashboard shows, the overlay shows.

**Stage C** ([docs/BUILD-SOURCING.md §10](docs/BUILD-SOURCING.md#10-stage-c-as-built-070)): in champ select, one click saves the build shown as an "Irelia Companion" rune page and item set in the League client (only its own, never yours, never in game); before the game, how the matchup tends to go (lane gold at 10 and 15 minutes, win rate by game length); in game, the best purchase for your current gold toward the next item, and who is ahead from the scoreboard.

```bash
npm install        # the dashboard's tools (Vite, React, TypeScript) and the Tauri CLI
npm run dev        # API server (Rust, irelia-server) on :5174 + dashboard on http://localhost:5173
npm test           # cargo test: storage, codec, Riot API pacer, sourcing, engine, model, evaluation, live tracker,
                   # backups, Slim, desktop logic (settings, keep awake, leftover middleware)
npm run test:web   # the dashboard: typecheck and its build-plan tests
npm run audit:data # read-only audit of a data folder (no API calls): coverage, data quality, yield model
                   # (the installed app runs the same audit: Data → Run data audit)
npm run eval:builds # offline evaluation of the draft-adjusted builds against the lane baseline (grouped
                   # cross-validation by pilot); a passed gate makes them the default. --quick for a fast run
                   # (the installed app runs the same: Data → Evaluate builds). Every report ends with the time
                   # each step took. --compare-grouped runs the grouped-drafts trial beside an exact evaluation
                   # (docs/EVAL-SCALING.md)
```

Since 1.0 the app is written in Rust (`crates/core`, `crates/server`, `crates/desktop`, `desktop/tauri`); 0.22.0's TypeScript (`server/`, its tests) stays as the specification and for the parity harness (`parity/`, `REWRITE-STATUS.md`), which checks that 1.0's evaluation and builds match 0.22.0's to the last digit. `npm run test:ts` runs 0.22.0's own tests.

Tests run offline: the engine reads a trimmed Data Dragon snapshot (`server/test/ddragon-snapshot.json`) instead of the CDN, so a new patch never changes their results. `node scripts/ddragon-snapshot.mjs [version]` refreshes it. CI (`.github/workflows/ci.yml`) runs `cargo test` and the dashboard's typecheck, tests and build on every pull request, on Linux and Windows, and builds the desktop app on Windows.

## Desktop app

Irelia Companion also ships as a Windows desktop app (Tauri, WebView2) with an **in-game overlay**, a **live CS / vision / gold tracker** and a **storage manager**. The server runs inside the app's own process. Architecture, the Vanguard-safety rules for the overlay, the gold model and verification steps are in [docs/DESKTOP.md](docs/DESKTOP.md).

```bash
npm run desktop       # development build of the app (data in ./data, dashboard from ./dist)
npm run dist          # installer: target/release/bundle/nsis/ (CI renames it Irelia-Companion-Setup-<version>.exe)
```

The installed app keeps everything in `%APPDATA%\irelia-companion\` (`config.json`, `data\`, `logs\`). The Riot API key is entered in **⚙ Settings**, and the data folder can be moved to another drive from there.

Put your key in `.env` (see `.env.example`). Riot **development keys expire every 24h**. When the key expires, the scraper stops and shows "invalid API key" on the Data tab.

## Architecture

```
 League Client (LCU)          Riot API (kr + asia, ...)           Data Dragon
  wss://127.0.0.1:<port>       league-v4, mastery-v4, match-v5    items, runes, champs, patch
        │ WAMP events                  │ cache-first, rate-limited        │ hourly patch check
        ▼                              ▼                                  ▼
 ┌─────────────── Node middleware (server/) ─────────────────────────────────────────┐
 │ index.ts           REST /api/* · WebSocket /ws (state, matchup, build, live)       │
 │ matchup.ts         the lane / opponent / draft every window shows, and your picks  │
 │ lcu/connector.ts   lockfile / process discovery → champ-select + gameflow events  │
 │ lcu/liveClient.ts  polls :2999 Live Client API; CS / vision / gold-diff tracker    │
 │ engine/            exclusivity · validator (patch + removed items) · recommend ·   │
 │                    adaptive (live counter-items)                                   │
 │ db.ts              SQLite (node:sqlite, WAL), one connection per thread            │
 ├─ worker threads (workers.ts): the main thread never waits on them ─────────────────┤
 │ scraper/worker.ts  one lane per Riot host, backfill · sourcing, capture, riot/*    │
 │ model/worker.ts    lane baseline · Stage B model (profiles, game-state model,      │
 │                    draft-aware builds, live "Next", personal layer)                │
 │ jobWorker.ts       one-shot: build evaluation, data audit                          │
 └─────────────────────────────────────┬──────────────────────────────────────────────┘
                                       │ REST + WS (Vite proxy)
                     Vite + React + Tailwind (web/): Build · Your games · Data · Overlay
```

**Threads.** The main thread only talks: to the League client, the Live Client, the dashboard and the overlay. Everything slow runs in a worker thread with its own database connection. The scraper owns every Riot call; the model worker builds the draft model (a few seconds on a large database, never during champ select or a game) and answers draft, live and personal requests; the job worker runs one evaluation or audit and exits. Requests to the model time out instead of hanging, and a worker that exits unexpectedly starts again by itself, so Start / Stop always answers.

**Pushed, not polled.** The server resolves the matchup (`server/matchup.ts`) from champ select, the live game and your picks, asks the model worker for its build, and pushes both over `/ws` whenever the matchup, the mode or the data changes. The dashboard and the overlay render what they receive; your picks go back with `PUT /api/matchup` (`{"lane"}`, `{"opponentId"}` (null: all matchups), `{"follow":true}`, `{"regions"}`).

### Scrape scope: regions, tiers and sources (`scrape.config.json`)

```json
{ "platforms": ["KR", "EUW1", "EUN1", "NA1", "JP1"], "tiers": ["CHALLENGER", "GRANDMASTER", "MASTER"], "divisions": ["I","II","III","IV"],
  "maxLeaguePagesPerDivision": 1, "otpMinMasteryPoints": 150000, "mainMinMasteryPoints": 100000, "masteryTopCount": 3,
  "pilotClasses": ["otp", "main"], "harvestIrelias": true, "historyWindow": "patch", "maxMatchesPerPilot": 300,
  "matchIdsPerPlayer": 20, "buildSources": ["otp"], "lookupHarvestedRanks": true, "ownGames": true, "hotfixes": [] }
```

- **Platforms.** Any key of `PLATFORMS` in [server/riot/routing.ts](server/riot/routing.ts) works: KR, JP1, EUW1, EUN1, TR1, RU, ME1, NA1, BR1, LA1, LA2, OC1, SG2, TW2, VN2. Each one maps to its regional cluster, for example `KR → asia.api.riotgames.com` (match-v5) and `kr.api.riotgames.com` (league and mastery). To add a new Riot shard, add one line to that table.
- **Tiers.** The crawler loops over the `tiers` array. Apex tiers (Challenger, Grandmaster, Master) use the single league endpoint. `DIAMOND`, `EMERALD`, and lower tiers are paged per division through `league-v4/entries`, up to `maxLeaguePagesPerDivision` pages (205 players each).
- **Pilots.** `pilotClasses` picks whose match histories are scanned: `otp` (Irelia #1 with `otpMinMasteryPoints`) and `main` (`mainMinMasteryPoints` among their top `masteryTopCount` masteries). Only pilots who played Irelia since their region's patch began are scanned (mastery `lastPlayTime`), most recent first.
- **History window.** `"patch"` pages every match id since the region's patch start (up to `maxMatchesPerPilot`); `"newest"` reads the `matchIdsPerPlayer` newest, as before.
- **Harvesting.** With `harvestIrelias`, another player's TOP / MIDDLE Irelia in a fetched match is stored too (one timeline call), and that player becomes a candidate pilot.
- **Build sources.** `buildSources` decides which games feed the lane builds: `otp` by default (the baseline); `main` and `harvested` can be added. Your own games (`ownGames`) never feed them.
- **Hotfixes.** `hotfixes: [{ "patch": "16.19", "date": "…", "note": "…" }]` records hotfixes that keep the build number.
- **Reloading and overrides.** The file is re-read on every loop iteration, so edits apply without a restart. `SCRAPE_PLATFORMS` and `SCRAPE_TIERS` in `.env` override it.

### Disk-cached data layer (`data/cache/<kind>/<platform>/<key>.json.gz`)

| kind | endpoint | TTL |
|---|---|---|
| `match`, `timeline` | match-v5 | forever (immutable) |
| `league` | league-v4 (apex list / entries pages) | 6 h |
| `mastery` | champion-mastery-v4 top | 3 days |
| `matchIds` | match-v5 by-puuid ids (key holds queue, startTime, start, count) | 10 min |
| `account` | account-v1 region of your PUUID | 7 days |

Every Riot call reads the cache first. Writes are atomic (temp file, then rename), so the worker and a restart never see half-written files. Changing the engine or wiping the SQLite DB costs no API budget for data that's already cached.

### Theme

Tailwind v4 via `@tailwindcss/vite`. The red/black design tokens are defined in `@theme` in [web/src/styles.css](web/src/styles.css): `ink-*` (blacks), `blood-*` (reds), `ash-*` (text), `ember` (hairlines), and `gold-400` (high win rate). In Tailwind v4 this block is the Tailwind configuration, so there's no `tailwind.config.js`. Newer panels use utility classes; the older dashboard CSS sits in `@layer components` and reads the same tokens.

1. **Static data and patch.** `ddragon.ts` loads `versions.json`. The current patch is `major.minor` of the newest version, and the previous patch is the next one in the list. All JSON is cached per version under `data/ddragon/`. An hourly check spots new patches, rebuilds the exclusivity index, and tells the scraper.
2. **Scraper (worker thread).** Every Riot host works its own queue at its own pace ([docs/BUILD-SOURCING.md §5](docs/BUILD-SOURCING.md#5-sourcing)):
   - **Platform hosts** (`kr.api…`: league, mastery): reseed the scope's ladders every 6 h. Then check mastery, in this order: players seen playing Irelia, pilots whose activity is due (every 3 days), unchecked ladder players, then everyone else (every 14 days).
   - **Regional hosts** (`asia.api…`: match-v5): your own games first, then whatever is worth the most expected Irelia games per request. That is a harvested game's timeline, a queued match, or reading the history of the most promising due pilot (every 45 min).
   - **Fetching.** Match ids are paged from the region's patch start, newest first, and a pilot's queue stops at the first older-patch match. A timeline is fetched only for fresh Irelia games that weren't remade, and every match is recorded with all ten participants, so nothing is fetched twice.
   - **Start/Stop.** The Data tab's button (and the Build tab's Match data card) starts and stops the lanes (`/api/scraper/start|stop`). It answers at once; nothing else runs in this thread.
3. **Patch-aware validation** (`engine/validator.ts`). A game is used only if its patch is its region's current one: the newest patch seen in that platform's own games, with Data Dragon as the fallback (`server/patches.ts`). A region still on the old patch therefore keeps its games current until it moves.
   - **Previous patch.** Allowed at 0.35 weight while the current patch has fewer than 40 games.
   - **Dropped games.** Remakes (the early-surrender vote or under 5 minutes), games where a player left (the timeline shows their XP stop), and games that used a rune, stat shard or build item that no longer exists or can't be bought.
   - **Item classes** (completed, tier-2 boots, starter) are recomputed from **current** item data on every request.
4. **Exclusivity** (`engine/exclusivity.ts`). Groups come from Data Dragon's `<passive>` tags: Immolate (Sunfire Aegis / Hollow Radiance / Bami's Cinder), Lifeline, Spellblade, Cleave, Manaflow, Annul, Glory, and Eternity. On top of those are the Boots group and fixed groups (Last Whisper, Quicksilver, support quest). Removed items are caught by flags (`purchasable`, `inStore`, map 11) and by a blocklist, `REMOVED_ITEMS`. Opportunity (6701) is still listed in Data Dragon, but with `purchasable=false`. The engine drops conflicting core options and filters 4th, 5th, and 6th item options against the chosen core and boots. Live counter-item suggestions are checked against your current inventory, so if you own Sterak's, Maw is reported as "blocked by Lifeline" and the next option is offered.
5. **Recommendation** (`engine/recommend.ts`). Options are weighted tallies ranked by the **Wilson lower bound** of win rate, so a 2-0 option can't beat a 60%-over-80-games option. Builds are per lane (`/api/recommendation?lane=TOP|MIDDLE`): each stored game keeps Riot's `teamPosition`. Matchup games in that lane have weight 1. If there are fewer than 15 of them, the lane's other games are blended in at weight 0.15. If the lane itself has fewer than 30 games, the other lane's games are blended in at weight 0.15 too, and the UI says so. Jungle, bot and support games are never used. Details in [docs/DESKTOP.md §7](docs/DESKTOP.md#7-lanes-top-and-mid).
6. **Matchup** (`server/matchup.ts`). Your lane is your in-game position, else your assigned champ-select lane; blind pick has none, so the lane you picked last is used. Ranked hides enemy roles, so the likely lane opponent is the enemy pick that Irelia faced most often **in that lane** in the database (each stored game's lane opponent is the enemy who stood nearest Irelia from 3:00 to 14:00, which beats Riot's position labels in lane swaps). Without data, the first Fighter or Tank (top) or Mage or Assassin (mid) is used. Top and mid never get the same pick. In game the enemy in your position is the opponent; the champ select is kept through the game for games without positions, and the database guess covers the rest. Click any enemy chip or search a champion to override; the Top / Mid switch overrides the lane. Your picks hold until the draft and its game are over, and **Follow the draft / the game** goes back.

## Database schema (`data/irelia.db`, schema v5)

| table | key columns | purpose |
|---|---|---|
| `meta` | `key`, `value` | `schema_version`, `seeded_at:<platform>:<tier>`, `scraper_enabled`, `me_puuid` / `me_platform` (`last_scan[:<platform>]` rows from the streamed scan before 0.6.0 are kept, unused) |
| `players` | `puuid` PK, `platform` ("KR"), `tier`, `league_points`, `is_otp` (NULL = unchecked), `irelia_mastery`, `last_checked`, `last_match_scan`; v3: `pilot_class` (otp / main / none), `irelia_points`, `irelia_last_play`, `irelia_rank`, `found_by` (ladder / harvest), `history_matches`, `history_irelia`, `rank_checked` | ladders, harvested players and pilot status |
| `matches` | `match_id` PK, `patch`, `game_version`, `queue_id`, `game_creation`, `has_irelia`; v3: `remake`, `leaver`, `end_result`, `detail_version` | every match seen (dedupe, patch, quality) |
| `irelia_games` | `match_id` PK → matches (cascade), `patch`, `win`, `position` (Riot `teamPosition`), `opponent_champion_id` (the lane opponent: the timeline's when known), `summoner1/2`, `primary_style`, `sub_style`, `perks` JSON, `stat_perks` JSON, `skill_order` "QEWQ…", `purchases` JSON `[[itemId, ms]]` (undo-resolved), `final_items` JSON, K/D/A; v3: `participant_id`, `position_opponent_id`, `timeline_opponent_id`, `pilot_source` (otp / main / harvested / me), `pilot_tier`, `pilot_lp`, `pilot_mastery`, `pilot_last_play`, `pilot_top1` | raw facts the engine aggregates |
| `participants` | (`match_id`, `participant_id`) → matches (cascade) | every player of every stored match: champion, team, position, win, items, damage to champions by type, taken, mitigated, healing, shields, CC |
| `timelines` | `match_id` → matches (cascade) | one compact JSON per Irelia game: per-minute gold / XP / level / CS of all ten, positions to 15:00, item, kill and objective events |
| `game_versions` | (`platform`, `game_version`) | first and last game per build: the current patch per region and hotfix builds |
| `champion_patch_stats` | (`patch`, `champion_id`, `position`) | per-champion totals kept when old patches are purged |
| `match_queue` | `match_id` | matches and harvested timelines waiting for a regional host, scored by expected Irelia games per request |
| `rec_log` | `id`, `session` | v4: builds shown to you (champ select, in game), linked to your game with what you bought and the result |
| `archive` | `match_id`, `patch` | v5: matches of patches older than patch memory's window, each stored whole as gzipped JSON (the **Archive patches older than the window** switch; switched off, they are put back) |

The schema upgrades in place, additively. Matches stored before v3 get their participants, quality flags and timelines from the disk cache with no API calls: the backfill starts by itself, or use **Rebuild from cache** on the Data tab (`POST /api/db/backfill`, `{"force":true}` redoes everything).

**Delete games of old patches** (Settings → Storage, with a confirmation) deletes matches of patches older than patch memory's window (cascade), keeping every patch within six of a region's current one, and keeps their per-champion totals in `champion_patch_stats`. The **Archive patches older than the window** switch (Data → Patches) moves the same matches into `archive` instead, compressed, now and as patches leave the window; switched off, it brings them back (docs/BUILD-SOURCING.md §9.8).

## Testing without the client

```bash
curl -X POST localhost:5174/api/dev/simulate-draft -H "Content-Type: application/json" -d "{\"enemies\":[122,64,103,222,412]}"
```

This shows a simulated champion select (Irelia top, enemy roles hidden like in ranked) until a real one starts. Add `"lane":"MIDDLE"` for mid, or `"lane":""` for blind pick. Send `{"enemies":[]}` to clear it.

```bash
curl -X POST localhost:5174/api/dev/simulate-live -H "Content-Type: application/json" -d "{\"startAt\":420}"
```

This streams a synthetic in-game session (Irelia vs Darius top; add `"lane":"MIDDLE"` for Irelia vs Syndra mid) through the live tracker, the Build tab's live panel and the desktop overlay. `"positions":false` leaves every position empty, as customs do: the opponent then comes from the (simulated) champ select or the database. Send `{"stop":true}` to end it.

## Config (`.env`)

`RIOT_API_KEY`, `RIOT_RATE_LIMITS` (personal or production keys: the starting limits, refined from Riot's rate-limit headers; see [docs/BUILD-SOURCING.md §5.6](docs/BUILD-SOURCING.md#56-api-keys)), `RIOT_CONCURRENCY` (Riot requests in flight per host, 1–3, default 3; requests stay paced either way), `SCRAPE_PLATFORMS` / `SCRAPE_TIERS` (override `scrape.config.json`), `LEAGUE_PATH` (non-default install location).

Per-user settings live in `%APPDATA%\irelia-companion\config.json`: `dataDir` (set by Settings → Storage), `riotApiKey` (set by Settings; wins over `.env`), `buildMode` (the Draft-adjusted / Lane baseline switch), `patchMemory` (the Data tab's patch memory switch; unset follows the evaluation), `archiveOldPatches` and `trimCacheToWindow` (the archive and cache switches), `lane` (the lane you picked last), `lcuImport` (the rune page and item set the import created, the only ones it replaces) and `desktop` (overlay and hotkeys). `npm run eval:builds` saves its reports in `evals\` next to it. A `scrape.config.json` next to it overrides the repo copy. `IRELIA_DATA_DIR` pins the data folder for one process (and disables moving it), and `IRELIA_CONFIG_DIR` relocates the settings folder (useful for tests). `DB_PATH` is gone: the database is always `<data folder>/irelia.db`.

## Legal

Irelia Companion isn't endorsed by Riot Games and doesn't reflect the views or opinions of Riot Games or anyone officially involved in producing or managing Riot Games properties. Riot Games, and all associated properties are trademarks or registered trademarks of Riot Games, Inc.
