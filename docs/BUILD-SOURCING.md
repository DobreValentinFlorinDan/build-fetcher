# Build sourcing v2: richer data, draft-aware builds, a personal layer, live re-ranking

Status: **Stage A shipped** (0.4.0, 0.4.1). **Stage B shipped** (0.5.0): the draft-aware model is additive, compared against the lane baseline, and off by default until `npm run eval:builds` shows it beats the baseline. **Stage C built** (0.7.0, §10): one-click import, pre-game matchup facts, and the best buy for your gold.

Today the app recommends one build per lane from about 930 high-elo Irelia games (KR, patch 16.19). That is too few, from too few players, to rank items, and one average build ignores the draft. Stage A collects many more clean games with the context a draft-aware model needs. Stage B builds that model, the personal layer and live re-ranking. Stage C adds one-click import, pre-game matchup facts and the best purchase for your gold.

| Stage | What | Where |
|---|---|---|
| A0 | CI on every PR, offline engine tests (Data Dragon snapshot) | PR `claude/ci-offline-tests` |
| A1 | This document | `docs/BUILD-SOURCING.md` |
| A2 | `npm run audit:data`: read-only measurement of your data | `server/audit.ts`, `scripts/audit-data.ts` |
| A3 | Every participant, a compact timeline per Irelia game, pilot metadata, backfill from the disk cache, retention | `server/capture.ts`, `server/db.ts`, `server/scraper/backfill.ts` |
| A4 | Remakes, leavers, the real lane opponent, the current patch per region, hotfix builds | `server/capture.ts`, `server/patches.ts`, `server/engine/validator.ts` |
| A5 | Whole-patch histories, OTPs + mains by activity, harvesting, one queue per Riot host, scope and cost, API keys | `server/scraper/{sourcing,ingest,pilots}.ts`, `scrape.config.json` |
| A6 | Your own ranked Irelia games, source `me` | `server/lcu/connector.ts`, `server/scraper/sourcing.ts` |
| A7 | Fixture tests for every item, docs; the audit also runs in the app (**Match Database → Run data audit**) | `server/*.test.ts`, `server/test/fixtures.ts` |
| B1 | Champion profiles from every stored player | `server/model/profiles.ts` |
| B2 | Game-state win-probability model, one feature function for timelines and the Live Client | `server/model/state.ts`, `logistic.ts` |
| B3 | Draft-aware recommender: per-decision WPA, layered shrinkage, pilot / patch / region weights, reasons | `server/model/{dataset,context,estimate,weights,draft}.ts` |
| B4 | Personal layer: timings, options shrunk toward high-elo, post-game review, `rec_log` (schema v4) | `server/model/personal.ts` |
| B5 | Live re-ranking of the next item, with a stability rule | `server/model/live.ts` |
| B6 | `npm run eval:builds`: grouped cross-validation by pilot, the gate | `server/model/evaluate.ts`, `scripts/eval-builds.ts` |
| B7 | Draft-adjusted / Lane baseline switch, reason chips, regions, review panel, overlay "Next" | `web/src/components/{DraftAdjust,Review}.tsx` |

---

## 1. Audit findings

Checked against `main` at `840bc64`. "Verified" means the code does what the finding says; "corrected" notes where it differs.

| # | Finding | Status | Note / what Stage A did |
|---|---|---|---|
| 1 | History stops at the 20 newest ids (`matchIdsPerPlayer`, `client.ts:143` `start=0&count=N`, no `startTime`) | Verified | `historyWindow: "patch"` pages every id since the region's patch start (§5.1). |
| 2 | OTP = Irelia #1 with ≥ 150k (`worker.ts:95`); `lastPlayTime` never read | Verified | The `top` answer the app already fetched carries `lastPlayTime`. Pilots are now OTPs and mains, and only active ones are scanned (§5.2). |
| 3 | Only the scanned player's Irelia is extracted (`parseMatch.ts:32`) | Verified | Other TOP / MIDDLE Irelias are harvested (§5.3). In ranked draft a champion appears at most once per match, so a match never holds two Irelias. |
| 4 | Each game keeps Irelia's row plus the lane opponent's id; raw JSON stays on disk forever | Verified, one correction | Every fetched **match** is cached, but a **timeline** only for stored Irelia games (the scraper fetched timelines only for those). The backfill can rebuild all participants, and per-minute frames for the stored Irelia games, with zero API calls. |
| 5 | Ranked solo only, KR only by default; each host has its own budget | Verified | Default scope is now KR + EUW1 + EUN1 + NA1 + JP1 (§5.5). Own games add flex (440). |
| 6 | The loop does one step at a time, so one host is busy at a time | Verified | Within a step requests overlapped (`inOrder`, up to 3 in flight), but ladder, mastery and match work never ran together. Now one lane per host (§5.4). |
| 7 | Dev key 100 / 2 min per host, paced to 1 / 1.33 s, expires every 24 h; `RIOT_RATE_LIMITS` + header refinement | Verified | Settings now shows when the key was saved (§5.6). |
| 8 | Remakes and early surrenders never filtered | Verified | Riot's "early surrender" (`gameEndedInEarlySurrender`) **is** the remake vote; 15-minute surrenders are real games and stay (dropping them would bias win rates). Filter: the flag, or a game under 5 minutes (§4.1). |
| 9 | Leavers not detected | Verified | Timeline-based flag (§4.2). |
| 10 | Lane opponent from `teamPosition` (`parseMatch.ts:39`) | Verified | Both are stored; the timeline's wins (§4.3). The audit counts disagreements. |
| 11 | Current patch = newest Data Dragon version (`ddragon.ts:96`) | Verified, worse than stated | The scraper's stale check (`worker.ts:114`) used it too: while Data Dragon was ahead of a region, every history scan stopped at its first match and **nothing** was collected there. Now per region (§4.4). |
| 12 | Hotfixes within a patch ignored; `game_version` stored but unused | Verified | `game_versions` records each build's first and last game per platform; manual `hotfixes` in the scope (§4.5). |
| 13 | Raw win rate ranked by Wilson (z = 1) is confounded by game state | Verified | Stage B (§9.2–9.3). Baseline kept. |
| 14 | Exact combinations: ordered 3-item core, whole rune page incl. shards, first 9/15 level-ups | Verified; line corrected | The core tuple is `recommend.ts:219` (`cores.add(g.completed.slice(0, 3))`), not `:143`. Stage B scores each decision on its own. |
| 15 | No per-player weighting | Verified | Stage A measures concentration (audit); Stage B caps pilot influence. |
| 16 | All-or-nothing matchup fallback (1 vs 0.15) | Verified | `recommend.ts:106–111`. Stage B shrinks through similar opponents. |
| 17 | `/api/recommendation` mixes every region (`index.ts:143–151`) | Verified | Stage B adds the region choice. |
| 18 | Previous patch at a fixed 0.35 while < 40 current games | Verified | Stage B sets it from a Data Dragon diff. Per-region patches already avoid "previous patch" for a lagging region. |
| 19 | Every `/api/recommendation` re-parses every stored game | Verified | Fine at 930 games; Stage B caches aggregates (§9.5). |
| 20 | Live counters: `info.attack/magic` (`adaptive.ts:76`), `HEALERS` (`:38`), fixed `COUNTERS` (`:42`) | Verified | Stage B replaces them with data profiles (§9.1). |
| 21 | Only `release.yml`; nothing checks a PR | Verified | A0: `.github/workflows/ci.yml`. |
| 22 | Engine tests download live Data Dragon | Verified | A0: committed snapshot. |

Also found while auditing:
- **Dev mode crashed the worker on Node ≥ 22.18.** Node strips types natively and loaded `worker.ts` without tsx, rejecting parameter properties. `worker-dev.mjs` registers tsx in the worker. The packaged app (esbuild bundle) was never affected.
- **Mastery rechecks dominated the platform host.** All 52k ladder players were rechecked every 3 days, about 243k calls a patch, while the sequential loop held the regional host idle during those batches. Now pilots are rechecked every 3 days and everyone else every 14, on their own lane.

## 2. Riot facts this relies on

`developer.riotgames.com` was not reachable from the build environment. The endpoints and fields below were confirmed in Riot's API reference through the [riotapi-schema](https://github.com/MingweiSamuel/riotapi-schema) OpenAPI file, which is generated daily from that reference.

| Fact | Used by |
|---|---|
| `GET /lol/match/v5/matches/by-puuid/{puuid}/ids`: `startTime` / `endTime` in **epoch seconds**, `queue`, `type`, `start` (default 0), `count` 0–100 (default 20). Riot notes that `startTime` excludes matches before 2021-06-16. | Patch window (§5.1) |
| `GET /lol/champion-mastery/v4/champion-masteries/by-puuid/{puuid}/top?count=N` (default 3) and `.../by-champion/{championId}`; `ChampionMasteryDto.lastPlayTime` is Unix **ms** | Pilot classes, activity (§5.2) |
| `GET /lol/league/v4/entries/by-puuid/{puuid}` → `LeagueEntryDTO` (`queueType`, `tier`, `rank`, `leaguePoints`) | Harvested pilots' rank (§5.3) |
| `GET /riot/account/v1/region/by-game/lol/by-puuid/{puuid}` → `{ region }` (americas / asia / europe hosts) | Own account's platform fallback (§6) |
| `ParticipantDto`: `gameEndedInEarlySurrender`, `gameEndedInSurrender`, `teamEarlySurrendered`, `physical/magic/trueDamageDealtToChampions`, `totalDamageTaken`, `damageSelfMitigated`, `totalHeal`, `totalHealsOnTeammates`, `totalDamageShieldedOnTeammates`, `timeCCingOthers`, `totalTimeCCDealt`, `timePlayed`, `challenges` | Capture, remakes |
| `InfoDto.gameDuration` is seconds since 11.20; `InfoDto.endOfGameResult` exists | Remakes |
| Timeline: `ParticipantFrameDto` `totalGold`, `currentGold`, `xp`, `level`, `minionsKilled`, `jungleMinionsKilled`, `position {x,y}`; `EventsTimeLineDto` `itemId`, `beforeId`/`afterId`, `killerId`, `victimId`, `assistingParticipantIds`, `bounty`, `shutdownBounty`, `killerTeamId`, `monsterType`/`monsterSubType`, `teamId`, `buildingType`, `towerType`, `laneType` | Timeline capture |

Not confirmable from Riot's reference, so each has a fallback:
- **`gameEndedInEarlySurrender` semantics.** Riot's field description is a copy-paste of a challenge's. That it marks the remake vote comes from how the field behaves, and the filter also drops any game under 5 minutes.
- **Event `type` values** (`ITEM_PURCHASED`, `ITEM_SOLD`, `ITEM_DESTROYED`, `ITEM_UNDO`, `CHAMPION_KILL`, `ELITE_MONSTER_KILL`, `BUILDING_KILL`, `TURRET_PLATE_DESTROYED`, `SKILL_LEVEL_UP`). The reference types them as plain strings. The app already relied on three of them, and unknown types are ignored.
- **`gameEndedInIGNBSurrender` / `teamIGNBSurrendered`** are listed but undocumented. The captured match keeps them (the raw JSON stays on disk), but nothing relies on them.
- **`endOfGameResult` values.** Stored as sent. The audit counts them per value before any filter uses them.
- **League client endpoints** (unofficial): `/lol-summoner/v1/current-summoner` → `gameName`, `tagLine` (and the client's own `puuid`, which the Riot API does not accept: fixed in 0.7.1, §6); the server from `/lol-chat/v1/me` → `platformId` (`"EUW1"`), `/riotclient/region-locale` → `region` (`"EUW"`), or the older `/lol-platform-config/v1/namespaces/LoginDataPacket/platformId` ([hextechdocs](https://hextechdocs.dev/getting-started-with-the-lcu-api/), [lcu.vivide.re](https://lcu.vivide.re/)). Without a server, account-v1 is asked, then Settings.

### Riot policy check (for Stage B)

Checked 2026-10-03 through search results; the policy pages themselves were blocked by the build environment's network policy:
- **Game integrity.** Products must not use information not present in the game client that gives a competitive edge. Build suggestions, rune pages and post-game analysis are allowed; ultimate trackers with live cooldowns are not. Enemy ultimate alerts have been restricted since 2025-03-13. Overlay and loading-screen ads have been forbidden since 2025-05-29. ([Riot developer support: League of Legends](https://support-developer.riotgames.com/hc/en-us/articles/22698698001939), as summarised by search)
- **Memory access.** From **2026-10-06** Riot blocks game-memory access by unknown third-party apps; known tools get a grace period until April 2027. Apps should use the official APIs. ([Riot support](https://support.riotgames.com/en-us/riot/performance/game-memory-access-removed-for-third-party-apps), [Vanguard FAQ](https://www.riotgames.com/en/DevRel/vanguard-faq)) The app never reads memory; live data comes only from the Live Client Data API.
- **Consequences for Stage B.** Every live input must come from the Live Client API (scoreboard-visible data), and estimates are labelled. Item recommendations, including what to buy with your current gold, are fine. "Do X now" calls, enemy cooldown timers, fog-of-war information and champ-select de-anonymisation are out. Re-check these pages before building Stage B.

**Re-checked 2026-10-03, before building Stage B.** `developer.riotgames.com`, `support-developer.riotgames.com` and `support.riotgames.com` were still blocked by the build environment, so the check went through search results again:
- Riot's [General Policies](https://support-developer.riotgames.com/hc/en-us/articles/22698591841939-General-Policies) and [League of Legends](https://support-developer.riotgames.com/hc/en-us/articles/22698698001939) pages, as summarised: no information that isn't available in the game client, no de-anonymising players who can't be identified from visible information, no historic Riot IDs.
- Enemy ultimate timers are prohibited since 2025-03-13 ([Riot Developer Relations on X](https://x.com/RiotGamesDevRel/status/1899532362637250955)).
- Game memory access by unknown third-party apps is blocked from 2026-10-06; known tools have until April 2027 to move to the official APIs ([Riot support](https://support.riotgames.com/en-us/riot/performance/game-memory-access-removed-for-third-party-apps)). The Live Client Data API stays the sanctioned source, and it only exposes the player's own view.
- Nothing found changes the rules above. What Stage B does live: it re-scores **which item to buy next** from scoreboard items, levels, CS, kills and the event feed (dragons, barons, heralds, grubs, towers, inhibitors), labels its lift as an estimate, and shows nothing else: no win probability, no "fight" / "recall", no timers, nothing about champ select beyond the picks the client shows.

## 3. Schema v3

Additive and idempotent. `openDb` creates the new tables, adds missing columns (`PRAGMA table_info` check, since `ALTER TABLE` has no `IF NOT EXISTS`), runs one-off `UPDATE`s that are no-ops the second time, and writes `meta.schema_version = 3`, all in one transaction. A v0.3.0 database upgrades in place. v0.3.0 can still open a v3 database: every new column is nullable or has a default, and it ignores the new tables.

| Table | New in v3 | Key | Purpose |
|---|---|---|---|
| `players` | `pilot_class` (otp / main / none), `irelia_points`, `irelia_last_play`, `irelia_rank`, `found_by` (ladder / harvest / me), `history_matches`, `history_irelia`, `rank_checked` | `puuid` | Pilot class and Irelia activity; the pilot's Irelia share drives queue scores |
| `matches` | `remake`, `leaver`, `end_result`, `detail_version` | `match_id` | Quality flags; `detail_version` = capture version applied (the backfill's work list) |
| `irelia_games` | `participant_id`, `position_opponent_id`, `timeline_opponent_id`, `pilot_source` (otp / main / harvested / me), `pilot_tier`, `pilot_lp`, `pilot_mastery`, `pilot_last_play`, `pilot_top1` | `match_id` | `opponent_champion_id` is now the effective lane opponent (the timeline's when known) |
| `participants` | new, `WITHOUT ROWID` | `(match_id, participant_id)` | Every player of every stored match: champion, team, position, win, level, KDA, gold, CS, items, damage to champions by type, taken, mitigated, healing (self, allies), shields on allies, CC (`timeCCingOthers`, `totalTimeCCDealt`), time played, a `challenges` subset |
| `timelines` | new | `match_id` | One compact JSON blob per Irelia game (`TimelineCapture`, below) |
| `game_versions` | new | `(platform, game_version)` | First / last game and count per build: the patch per region and hotfix builds |
| `champion_patch_stats` | new | `(patch, champion_id, position)` | Per-champion totals of purged patches |
| `match_queue` | new | `match_id` | Work for the regional hosts: matches to fetch and harvested timelines, with a score |
| `rec_log` | v4 | `id`, `session` unique | Builds the app showed you (champ select, in game) and, once your game is fetched, what you bought and the result (§9.4). Schema v4 adds only this table. |
| `archive` | v5 | `match_id` | Matches of patches older than patch memory's window, each stored whole (its rows in `matches`, `irelia_games`, `participants`, `timelines`) as gzipped JSON and out of those tables (§9.8). Schema v5 adds only this table. |
| `timelines.data_z`, `codec_dicts` | v6 (0.14.0) | `match_id`; `id` | The capture compressed (zstd with a dictionary of this database, server/codec.ts); `data` is '' then. Read through `timeline_json(data, data_z)`. Earlier rows are converted in the background (docs/DESKTOP.md §4). |
| `participant_rows`, `match_ids`, `puuid_ids` | v6, only after Compact database and only where it's smaller | `(match_ref, participant_id)`; `id` | Participants by integer ids; `participants` is then a view with the same columns (docs/DESKTOP.md §4). |

`TimelineCapture`, the timeline blob, keeps:
- per frame and player (one-minute frames), `[totalGold, currentGold, xp, level, cs]`;
- positions `[x, y]` for frames 0–15;
- item events `[t, pid, kind, itemId, afterId]` (buy, sell, destroy, undo);
- kills `[t, killer, victim, assists, bounty, shutdown]`;
- objectives `[t, team that took it, kind, subtype]`: dragons (elder separate), Baron, Herald, grubs (`HORDE`), Atakhan, towers, inhibitors and plates.

`inventoryAt(capture, pid, t)` rebuilds anyone's inventory at any time from the item events.

**Space** (0.14.0): the Riot API cache is one SQLite file of compressed responses (`cache/riot-cache.db`) instead of a gzip file each; timelines are compressed in the database; free pages go back by themselves (`auto_vacuum = INCREMENTAL`); and **Slim stored data** (Settings → Storage) can delete what no capture or model reads, behind a backup and a check of the results. Details: docs/DESKTOP.md §4.

**Rows vs blobs.**
- **Participants are rows.** Stage B's champion profiles and the retention summaries aggregate them across thousands of matches (`GROUP BY champion_id, position`), which SQL does directly. A row is about 0.6 KB, most of it the PUUID and the `challenges` subset; the audit measures it on your data.
- **Timelines are blobs.** They are always read whole, one game at a time: replaying a game's state minute by minute, or rebuilding an inventory. As rows, one game would be about 300 frame rows plus about 300 event rows with an index each, roughly 10× the size, and no query needs to filter inside a game. The blob is JSON (readable, `json_extract` works) at about 15–25 KB per game; the raw Riot timeline is about 1 MB.

**Retention.**
- "Purge stale patches" (since 0.8.1 **Delete games of old patches** in Settings → Storage, with a confirmation) keeps every patch within patch memory's window of a region's current patch (six patches), plus Data Dragon's pair. Before 0.8.0 it kept every region's current and previous patch.
- Before deleting, it adds the purged matches' per-champion totals (games, wins, seconds, damage by type, taken, mitigated, healing, shields, CC; remakes excluded) to `champion_patch_stats`, in the same transaction.
- The Storage manager lists every table's rows and size (SQLite `dbstat` when available).

**Backfill** (`server/scraper/backfill.ts`, worker thread, zero API calls):
- It starts on its own when the worker finds matches with `detail_version < CAPTURE_VERSION`. You can also run it with `POST /api/db/backfill` (`{"force":true}` redoes everything) or the **Rebuild from cache** button.
- For each batch of 50 matches it reads the cached match and timeline files, then writes participants, quality flags, the timeline, the leaver flag, both lane opponents and the pilot metadata in **one** transaction that also marks those matches done. Interrupted, it resumes; rerun, it rewrites the same rows.
- It also reads Irelia's `lastPlayTime` from the cached mastery answers and queues harvestable games from cached matches.
- Progress is reported through the scraper status (`backfill`).
- Pilot metadata for games stored before v3 is the pilot's tier and LP now, not at scan time; new games record it at scan time.

## 4. Data quality

Every fix below changes the lane builds on purpose; these are the baseline exceptions the plan allows. `npm run audit:data` reports each fix's effect on game counts and on the lane builds: it runs v0.3.0's rules, then adds the fixes one at a time (§7.3).

### 4.1 Remakes
A game is a remake when any participant has `gameEndedInEarlySurrender`, or it lasted under 300 s: the remake vote ends a game around 3:00, and no surrender is allowed before 15:00.
- New matches: remakes never cost a timeline call or become an Irelia game.
- Stored ones: `matches.remake` is set by the backfill, and the validator drops them (`remake`).
- 15-minute surrenders stay: they are real games, and dropping them would bias win rates.
- **Aborted games are dropped too:** an `endOfGameResult` other than `GameComplete`, e.g. `Abort_AntiCheatExit` (2 of 4,240 matches in the first real audit). Matches stored without the field pass.

### 4.2 Leavers
From the timeline: a player whose XP and CS stop growing and whose gold only grows by passive income (≤ 140 g/min) for **3 minutes or more** after 2:00.
- At level 18 XP stops anyway, so only CS and gold tell, and it takes **5 minutes**, longer than any death plus a standoff.
- Deaths are far shorter, and the first two minutes (walking to lane, no passive gold yet) are ignored.
- `matches.leaver = 1`, and the validator drops it (`a player left the game`).
- Participants stay for champion profiles. Matches without a timeline have `leaver = NULL` (unknown).
- The audit can compare this with `timePlayed`, which may make a cheaper check for non-Irelia matches later.

### 4.3 The real lane opponent
The enemy with the smallest **median** distance to Irelia over the frames from 3:00 to 14:00.
- Both opponents are stored (`position_opponent_id`, `timeline_opponent_id`); `opponent_champion_id` is the timeline's when there is one.
- The audit counts how often the two disagree, and the build comparison shows the effect on matchups.

### 4.4 Current patch per region
`server/patches.ts`:
- **Current patch.** A platform's current patch is the newest patch among its own games (`game_versions`), with Data Dragon as the fallback for a platform with no games yet.
- **Previous patch.** Data Dragon's previous patch when it describes the same patch, else the previous minor number.
- **Engine.** Each game is weighted by its own platform's patch pair.
- **Scraper.** A game is stale only when it is older than its platform's current patch.

### 4.5 Hotfixes
- **Builds.** `game_versions` keeps the first and last game per build (`16.19.712.4471`) per platform. Every build after a patch's first one is a hotfix moment.
- **Manual dates.** Not every hotfix changes the build number, so `scrape.config.json` `hotfixes: [{ "patch": "16.19", "date": "2026-09-26T18:00:00Z", "note": "Irelia Q" }]` adds them by hand.
- **Display.** The Match Database panel lists each region's patch with its hotfix count.
- **Use.** Stage B weights games after an Irelia or item hotfix above those before it.

## 5. Sourcing

### 5.1 The whole patch window
- **Patch start.** Riot has no "patch start" endpoint. A platform's start is bracketed between the newest previous-patch game and the oldest current-patch game seen there.
  - The window starts at the newest previous-patch game (`startKnown`).
  - With no previous-patch game, it starts one patch cycle (14 days) before the oldest current-patch game.
  - With no game at all, it starts 14 days before now.
- **Paging.** Ids are paged with `startTime` (seconds), `start` and `count=100`, newest first, until a short page, `maxMatchesPerPilot` (300), or a page made only of matches already stored.
- **Early start is cheap.** A too-early start only costs a few id pages: a pilot's fetches still stop at the first older-patch match, and older queued ids of that pilot are dropped (match ids grow over time on a platform).
- **Cache keys** hold every parameter: `{puuid}-{queue}-{startTime}-{start}-{count}`.
- **Old behaviour.** `historyWindow: "newest"` restores the 20 newest ids.

### 5.2 Who counts as an Irelia player
- **Mastery check.** One call per player: `top?count=3` (`masteryTopCount`). It returns each champion's points and `lastPlayTime`.
- **Classes:**
  - **otp**: Irelia #1 with at least `otpMinMasteryPoints` (150k), unchanged;
  - **main**: at least `mainMinMasteryPoints` (100k) without being an OTP;
  - **none**: everyone else.
- **Harvested players.** For players seen playing Irelia, a `by-champion/39` call is added when she isn't in their top 3.
- **Who is scanned.** Only pilots whose Irelia `lastPlayTime` is after their platform's patch start, most recent first. Pilots with unknown activity (checked before v3) count as active until rechecked; the backfill fills it from cached answers.
- **Recheck intervals.** Pilots every 3 days, everyone else every 14. Players checked before v3 (with the top-1 call, which can't tell a main) are rechecked once, ranked with the unchecked players.

### 5.3 Harvesting other Irelias
- **Which games.** Any Irelia in TOP or MIDDLE in a fetched match (new, or cached via the backfill) who is not the scanned pilot. Her player becomes a candidate pilot (`found_by = 'harvest'`, tier from the ladder if known, else `UNKNOWN`).
- **Cost.** On the current patch, and not remade, her game is stored as `harvested` for one timeline call. Cached matches queue that call.
- **Rank lookup.** Harvested candidates are mastery-checked first on their platform host. When they classify as pilots but are on no seeded ladder, their rank comes from league-v4 `entries/by-puuid` (`lookupHarvestedRanks`).
- **Build weight.** Unknown-tier games are weighted lower in Stage B.

### 5.4 One lane per Riot host
`server/scraper/sourcing.ts` and `worker.ts`: every platform host (`kr.api…`: ladders, mastery, ranks) and every regional host (`asia.api…`: match ids, matches, timelines) runs its own lane, paced by its own limiter.
- **Platform lane priority.** Reseed a due ladder, then mastery checks of players seen playing Irelia (harvested), then pilots whose activity is due, then harvested pilots' ranks, then unchecked ladder players (by LP), then everyone else due a 14-day recheck.
- **Regional lane priority.** Your own games first. Then whichever has more **expected Irelia games per request**:
  - a harvested timeline: 1;
  - the best queued match: p / (1 + p), where p is that pilot's Irelia share, shrunk toward a class prior (OTP 0.7, main 0.35);
  - reading the most promising due pilot's history: p·g / (1 + g(1 + p)), where g = expected new matches since their last scan.
- **Order is unchanged.** Database writes stay ordered and transactional: each job commits synchronously, with `inOrder` for batches.
- **Nothing else in this thread.** The streamed OTP scan (which paused the lanes) was removed in 0.6.0: the lanes cover it. The evaluation and the audit run in a thread of their own, so Start / Stop is always answered at once.
- **Visible per host.** The Match Database panel (Data tab) shows each host's task and queue.

### 5.5 Scope and cost
| Key | Default | |
|---|---|---|
| `platforms` | `KR`, `EUW1`, `EUN1`, `NA1`, `JP1` | Three regional hosts (asia: KR and JP1, europe: EUW1 and EUN1, americas: NA1), so roughly three times the match budget. EUN1 and JP1 were added after the first audit found pilots there (they share their region's match budget, but bring their own ladders). Any platform in `routing.ts` works. |
| `tiers` / `divisions` | Challenger, Grandmaster, Master / I–IV | Diamond I: add `"DIAMOND"` and set `"divisions": ["I"]`, with `maxLeaguePagesPerDivision` capping 205 players per page. |
| `pilotClasses` | `otp`, `main` | Whose histories are scanned. `["otp"]` reverts to OTP-only collection. |
| `harvestIrelias` | `true` | |
| `historyWindow` / `maxMatchesPerPilot` | `patch` / 300 | |
| `buildSources` | `otp` | What feeds today's lane builds (the baseline). `main` and `harvested` can be added; `me` never. |
| `lookupHarvestedRanks`, `ownGames`, `hotfixes` | `true`, `true`, `[]` | |

**Default chosen with you:** collect from every source, and keep today's lane builds on OTP games until Stage B shows that weighting the others in beats them.

Match-id pages per pilot: one for the newest-ids window, up to ⌈min(`maxMatchesPerPilot`, matches) / 100⌉ for the patch window. Match calls stay at most 2 per match, because a ranked match holds one Irelia at most, harvested or not.

### 5.6 API keys
- **Development key.** Generated at developer.riotgames.com. It expires 24 h after you generate it, and Settings now shows when the key was saved, with a reminder. Riot's published dev limits are 20 requests per second and 100 per 2 minutes, **per routing value** (each platform host and each regional host).
- **Personal key.** For a personal project: you register the product and Riot approves it. Same limits as a development key unless Riot grants more, but it **never expires**, so the lanes can run unattended.
- **Production key.** For a public product, after Riot approves it. Its limits are much higher (shown in your developer dashboard).
- **How the app applies limits.**
  - `RIOT_RATE_LIMITS` (`.env`, e.g. `500:10,30000:600`) sets the starting limits.
  - After the first response, the limiter takes the real ones from `X-App-Rate-Limit` and `X-Method-Rate-Limit` on every answer, and plans for 90 % of each window.
  - Nothing else hardcodes Riot's limits.
  - A 429 is honoured per scope (`riot/rateLimiter.ts`).
- **Rules.** Never rotate between several keys to get around limits. Applying for a production key is your call.

## 6. Your own games (A6)

- **Account** (`scraper/account.ts`). On connect, the League client's `current-summoner` gives your Riot ID. Its `puuid` is the client's own and the Riot API rejects it (the API's PUUIDs are encrypted for each application): before 0.7.1 this made the server lookup fail ("Could not tell which server your account plays on") and no own game was ever fetched. The worker now asks account-v1 `accounts/by-riot-id/{gameName}/{tagLine}` for the API's PUUID (cached 7 days), and stored client PUUIDs are ignored.
- **Server.** Settings → Your account wins when set. Otherwise the League client's chat identity, the Riot client's region name (mapped: EUW → EUW1, EUNE → EUN1, LAN → LA1, OCE → OC1…) or the older login data packet, then account-v1 `region/by-game/lol/by-puuid`. When none answers, the Data tab and Settings say so and ask for the server.
- **What is fetched.** The worker lists your ranked solo (420) and flex (440) ids since one patch cycle before your platform's current patch began, so the current and previous patch. It always fetches page 0 fresh: the 10-minute id cache would hide a game that just ended.
- **Storage.** Irelia games in any position are stored with their timelines and source `me`. Previous-patch games are kept on purpose: the personal layer compares patches.
- **Cadence.** When the account is first known, after each game (2, 6 and 15 minutes after the client reaches the end-of-game screen, since match-v5 lags), and every 30 minutes while the lanes run. With the lanes stopped, own games are still fetched directly: a few dozen calls a patch.
- **Never in the high-elo builds.** Every build query filters on `buildSources`, which can't contain `me`. A game of yours stored earlier by an OTP scan is retagged `me`.

## 7. Yield model

### 7.1 Formulas
Per full patch (`D` = 14 days), per sourcing option. Each option adds to the previous one. Parameters marked † come from `npm run audit:data` (`yield` in its JSON):

- `P`† pilots, `a`† share active this patch (cached `lastPlayTime`), `r`† ranked matches a pilot plays per day, `m = r·D`;
- `s`† Irelia share of an active pilot's matches, `h`† harvestable TOP / MID Irelias per fetched match;
- `N`† ladder players, `B` = 90 × 30 × 24 = 64,800 calls a day per host (dev key at 90 %).

Each row gives games `G` (one timeline call each), matches `M`, id pages `L`, mastery checks `C` and days to collect at a dev key:

| Option | G | M | L | C | Days |
|---|---|---|---|---|---|
| as now | P·a·min(20, m)·s | P·a·min(20, m) + P(1−a) | P | N·D/3 | (M+G+L+C)/B |
| + whole-patch pagination | P·a·m·s | P·a·m + P(1−a) | P·a·⌈m/100⌉ + P(1−a) | N·D/3 | (M+G+L+C)/B |
| + lastPlayTime filter | same | P·a·m | P·a·⌈m/100⌉ | P·D/3 + (N−P) | (M+G+L+C)/B |
| + harvesting | G + h·M | same | same | same | (M+G+L+C)/B |
| + per-host queues | same | same | same | same | max(M+G+L, C)/B |
| + EUW1, NA1 | ×k with k = 3/regions now | ×k | ×k | ×k | / 3 hosts |
| + Master / Diamond I | ×(pilots ratio)† | | | | |
| + personal / production key | same | | | | personal: no expiry; production: B × ~66 |

Inactive pilots have no current-patch games, so the `lastPlayTime` filter saves calls rather than adding games: no id list, and no older-patch match that only stops the scan. Calls per Irelia game tend to `(1 + s + s·h)/s` plus id pages.

### 7.2 Projection for your scope (illustrative, until you run the audit)
With your numbers (225 OTPs, 52,147 ladder players, 930 games on 16.19) and assumed `a = 0.55`, `r = 7`/day, `s = 0.55`, `h = 0.05`:

| Option | Games / patch | Calls / patch | Days at a dev key |
|---|---|---|---|
| as now (20 newest ids) | 1,361 (930 measured so far) | 247,515 | 3.8 |
| + whole-patch pagination | 6,670 | 262,477 | 4.1 |
| + lastPlayTime filter | 6,670 | 71,893 | 1.1 |
| + harvesting | 7,277 | 72,500 | 1.1 |
| + per-host queues | 7,277 | 72,500 | 0.8 |
| + EUW1 and NA1 | 21,830 | 111,555 | 0.3 |
| + Master / Diamond I (pilots ×2, assumed) | 43,659 | 223,111 | 0.6 |
| + production key | 43,659 | 223,111 | 0.01 |

The audit prints this table with your measured parameters; the real numbers replace these.

### 7.3 What the audit measures
All without API calls and without writing to the data folder:
- players and OTPs by platform and tier;
- matches and Irelia games by patch, lane and platform;
- the share of fetched matches that are Irelia games;
- games per pilot and the top-10 share;
- matchup coverage;
- remakes (flag and duration), end results, leavers;
- `teamPosition` vs timeline opponents;
- builds per patch per region, and the first game of each region's current patch;
- cache coverage both ways;
- harvest potential;
- disk per 1,000 games;
- the yield parameters and projection;
- **the lane builds under v0.3.0's rules and with each fix** (games used and dropped, keystone, core, boots, skill priority, win rate, top matchups).

## 8. Coverage targets: what "enough data" means

Current-patch, valid (no remakes or leavers) games from the build sources:

| Target | Top | Mid | Why |
|---|---|---|---|
| Games per lane per patch | ≥ 2,000 | ≥ 1,000 | Separating two core options 3 points of win rate apart at one standard error needs about 1,000 games each. |
| Distinct pilots per lane | ≥ 80 | ≥ 40 | Builds reflect the population, not a few accounts. |
| Top-10 pilots' share of the lane | ≤ 35 % | ≤ 45 % | With per-pilot caps (Stage B) above this. |
| Lane games in matchups with ≥ 15 games | ≥ 70 % | ≥ 60 % | Enough for the matchup layer to speak before shrinking. |
| Games within 72 h of a patch or Irelia / item hotfix | ≥ 300 | ≥ 150 | The previous patch's weight matters less. |
| Stored Irelia games with a timeline | ≥ 95 % | ≥ 95 % | Stage B's state model and WPA need them. |

## 9. Stage B as built

Every new model is additive: the lane builds (`engine/recommend.ts`, OTP games, Wilson ranking) are unchanged and stay the default until the evaluation gate (§9.6) passes. Choices the design left open are marked **(chosen)**.

### 9.1 Champion profiles (B1, `model/profiles.ts`)
Per champion and position, from `participants` of every region's current and previous patch (remakes, leavers and aborted games left out), plus `champion_patch_stats` of purged patches at half weight:
- damage mix (physical / magic / true share of damage to champions) and damage per minute;
- self-healing per minute (`totalHeal` minus healing on allies), healing and shields on allies per minute;
- CC per minute (`timeCCingOthers`), damage taken plus mitigated per minute;
- attack range from Data Dragon (`stats.attackrange`, now kept by the loader and the test snapshot); ranged above 300.

Shrinkage: champion = (n·x̄ + k·class)/(n + k), class = (n·x̄ + k·all)/(n + k), position-specific toward the champion's pooled profile, **k = 50 for every stat (chosen)**. A champion with no games takes its class's profile; with no games at all, the damage mix falls back to Data Dragon's attack / magic ratings.

The live counters (`engine/adaptive.ts`) use the profiles once at least ten champions have 20 games: the enemy damage mix weighted by each champion's damage, healers (more than twice the median champion's healing), tanks (above the top fifth for damage taken plus mitigated). Before that, the old heuristics stay.

### 9.2 Game-state model (B2, `model/state.ts`, `model/logistic.ts`)
L2-regularised logistic regression fitted by Newton's method (IRLS, Cholesky solve), from Irelia's team's side, on one row per timeline frame from 1:00 (at most 60,000 rows, an even subsample above that). The user's own games are never training rows.
- **Features** (13 differences, Irelia's team minus the enemy's): team item value, kills, team CS, average level, towers, inhibitors, dragons, elder dragons, barons, grubs and heralds, and Irelia's item value, level and CS against her lane opponent.
- **Time (chosen): interactions.** Each difference also enters multiplied by τ = min(t, 40 min)/30 min, plus τ itself: 28 coefficients. Fewer parameters than time buckets, and smooth.
- **One feature function** (`stateFeatures`) for a stored timeline (`statesAt`: item events replayed, frames, kill and objective events) and for the Live Client (`stateFromLive`: scoreboard items, levels, CS and kills; objectives from the event feed: `DragonKill` with `DragonType`, `BaronKill`, `HeraldKill`, `HordeKill`, `TurretKilled` / `InhibKilled` with the building's team). A test builds the same moment both ways and gets identical features.
- **No separate gold estimate (chosen).** The live gold estimator is a linear function of CS, kills, assists, objectives and time, which are already features, so it would only add a collinear column. Hidden `totalGold` is never used.
- **WPA.** Each decision's win-probability-added = result − p̂(state just before it): for items, the state 1 ms before the purchase (the components still owned); for runes, summoners, start and skills, the state at 0:00 (p̂ is then the base rate). Removes "ahead players buy greedier items".
- λ is chosen by held-out log loss in the evaluation (default 1).

### 9.3 Draft-aware recommender (B3, `model/draft.ts`)
- **Context** (`model/context.ts`): 12 draft dimensions, as means so a half-picked draft is comparable: enemy damage mix (damage-weighted), healing, shields, CC, tankiness and ranged share; ally damage mix and frontline; the lane opponent's damage mix, tankiness, range and healing. Champ select hides enemy roles, so teams use pooled profiles on both sides; the lane opponent uses Irelia's lane. Items add 8 live dimensions at the moment of the purchase (§9.5). Dimensions are standardised over the stored games and clamped to ±4; a dimension the games never differ in is left out of distances.
- **Kernel:** Gaussian on the mean squared standardised distance over the dimensions both sides know. **Bandwidth h and prior k by cross-validation (chosen):** grid h ∈ {0.5, 1, ∞}, k ∈ {30, 100, 300}; defaults h = 0.7, k = 100 until an evaluation has run.
- **Layers:** both lanes (toward 0) → this lane → similar drafts (kernel-weighted) → this exact matchup in similar drafts, each moving toward its own data by n/(n + k). n counts similarity as a fraction of a game, so a draft unlike every stored game leans on the lane. Interval: θ ± 1.96·σ/√(n + k), σ the lane's WPA spread (an approximation: the layers reuse the lane's games).
- **Decisions, each on its own:** keystone, primary rows 1-3 (within the keystone's tree), secondary tree, two secondary runes (different rows), three shards, summoners, start, first / second / third item, boots, items 4-6, the ability maxed first, levels 1-3. Items are offered only if purchasable and allowed next to the picks already made (exclusivity groups).
- **Pilot weights (chosen):** source otp 1, main 0.8, harvested 0.6 (`draftSources`, all three by default; `me` never); tier Challenger / Grandmaster 1, Master 0.9, Diamond 0.75, Emerald 0.6, unknown 0.5; Irelia points 0.85-1 (500k for full weight); LP 0.95-1 within the apex tiers. **Each pilot is capped at 5 % of a lane's weight**, solved exactly (water-filling); with fewer than 20 pilots the cap is an equal share.
- **Patch weights (chosen):** current patch 1; previous patch 0.8 when the Data Dragon diff shows neither Irelia nor the decision's items changed, 0.2 when one did, 0.35 when the previous version's files can't be read. `patchWeights` in the scope overrides per patch. Games before the newest hand-entered hotfix of their patch count 0.5 (`hotfixes`); new build numbers alone don't, since most hotfix builds don't touch Irelia. With patch memory (§9.8), older patches count too.
- **Regions:** all by default, your own platform ×1.5 (`ownRegionBoost`), `regionWeights` per platform; the dashboard can limit a draft to some regions.
- **Thin evidence:** a decision with fewer than 30 effective similar games keeps the lane baseline's pick, flagged.
- **Reason:** when the draft moves a pick away from the lane-level choice, the matchup if only the matchup does it, else the context dimension that alone gives the pick its largest lead ("enemy magic damage 62%").
- **Output:** the build in the lane baseline's shape (the existing UI renders it), plus per pick its lift, interval, effective games, pilots and matchup games, and the draft's most unusual dimensions. Speed: about 5 ms per draft with 800 games.
- **Cache:** the model is built from the database in its own worker thread (`model/worker.ts`; about 0.5 s for 800 games). Since 0.15.0 new data rebuilds it once it is 1 % of the model's games (at least 50), after half an hour at the soonest or six hours at the latest; your own games, a new patch and the Data tab switches at once. Never in queue, champ select or a game. From 4,000 games the build runs on helper threads while the model thread keeps answering with the model it has. Until the first build, drafts get the lane baseline alone; the draft-adjusted part is pushed when the model is ready.

### 9.4 Personal layer (B4, `model/personal.ts`)
- **Timings:** your median finishing minute of each item against the high-elo median for the same item and lane.
- **What works for you:** your mean WPA per option blended with the high-elo lane estimate, (n·yours + 30·high-elo)/(n + 30), so a few games never override it.
- **`rec_log`** (schema v4, additive): one row per champ select (what the dashboard showed, and in which mode) and per game (every "Next" shown). A row is linked to your game when it is fetched: champ select up to 20 minutes before the game, to its end. Linking also records what you bought and the result.
- **Post-game review** (dashboard → Your games): result, lane gold and CS against your opponent at 10:00 and 15:00 (from the match timeline, after the game), what you built and when against the high-elo median, and what was shown, with how much of it you followed.

### 9.5 Live re-ranking (B5, `model/live.ts`)
- **When:** every Live Client poll (1 s), in Draft-adjusted mode, in the model worker (one re-rank at a time; the live view never waits for it).
- **Inputs, scoreboard only:** the draft context from the ten champions, plus the enemies' damage mix weighted by their item value (who is ahead), the armour and magic resist their items give, how many own an anti-heal item, Irelia's item value against her lane opponent's, team item value and kill differences, and the minute. The same dimensions are recorded at every stored purchase.
- **What changes:** only the next completed-item slot is scored, among items you can still buy next to what you own.
- **Stability (chosen δ = 0.01 WPA, 3 polls):** another item replaces "Next" only after leading it by δ on three polls in a row; an item whose components you hold stays "Next"; a new slot starts fresh.
- **Speed:** observations are cached per model build; a re-rank takes a median 3.4 ms over 9,600 games (ten times today's), against the 50 ms target and a 1 s poll (tested).
- **Display:** "Next: Trinity Force · adjusted for: fed Syndra (AP)" in the overlay and the Live Game card, with the estimated lift. The lane opponent is the app's matchup opponent (`server/matchup.ts`), the same the overlay and the tracker use. A fed enemy is named only when their items are worth a quarter more than the enemy average. Build advice only.

### 9.6 Evaluation (B6, `npm run eval:builds`)
Offline, no API calls; grouped 5-fold cross-validation by pilot (FNV hash of the PUUID). It opens the database like the app (upgrading an older one in place) and writes nothing else to the data folder. The app runs the same evaluation in a thread of its own (**Data → Evaluate builds**; fetching and the dashboard carry on). The Data tab shows its step ("Step 8 of 13: Comparing builds on fold 2 of 5"), the time so far and a **Cancel** button. They come from the server (`/ws` `jobs`), so they stay when you switch tabs.
- **Automatic runs (0.8.4, `server/autoEval.ts`):** with **Follow the evaluation** on (the default), the evaluation decides patch memory, so the app runs it again by itself. Otherwise the verdict would stay as old as your last click.
  - **When:**
    - After 300 games stored since the last evaluation, or once games of a patch newer than it saw are stored. Never evaluated: once 300 games are stored.
    - At most every 6 hours, counted from the last evaluation (yours or the app's) and from the last automatic try (after a failure or a Cancel).
    - Only after 10 minutes without champ select or a game (also after the app starts), and not while the data folder moves or old patches are archived.
  - **Champ select or a game** stops it at once (nothing changes) and it starts over after the next quiet 10 minutes; a pause doesn't count as a try. A click on **Evaluate builds** while it runs makes it yours, and then nothing stops it.
  - **New games:** each evaluation records, as it starts, the newest stored game's row, the number of games and the newest patch (`latest.json` `mark`). New games are counted by row, so archiving older games hides none, or by count when that says more (Compact database renumbers rows). A `latest.json` from before 0.8.4 has no mark: its game count is read from its `dataVersion`, and its patch is the newest one first seen before it ran.
  - **Its reports:** saved like yours, as `eval-auto-<time>.txt` / `.json`. Only the newest 20 automatic reports are kept; reports of evaluations you started are never deleted.
  - **Follow the evaluation off:** Evaluate builds runs only when you click it.
  - **The Data tab** says when the next one is due ("Due (312 new games): it runs at 21:05"), labels a running one ("Automatic evaluation (312 new games) · Step 3 of 14"), and labels its report.
- **State model:** held-out Brier score, log loss and a calibration table against always predicting the training win rate, for λ ∈ {0.3, 1, 3, 10}.
- **Builds:** for each held-out game, the draft model's and the lane baseline's picks for its draft (each trained on the other folds). Per decision, lift = mean WPA of games that followed the pick − mean WPA of those that didn't (0 when either group has fewer than 10 games); a model's lift is the mean over keystone, secondary tree, summoners, start, max-first, items 1-3 and boots. 95 % interval of the difference from 1,000 pilot bootstrap resamples.
- **Brier:** each model's outcome prediction for the option the pilot took: the baseline's raw lane win rate of that option, the draft model's p̂(state) + θ.
- **Live stability:** "Next" replayed minute by minute on held-out timelines (at most 500, evenly spread): flips per game within a slot.
- **Yours:** linked `rec_log` games that followed the draft build against those that didn't, once there are 20.
- **Gate:** default only if the lift difference's 95 % interval is above zero and the Brier score is no worse, on at least 200 held-out games. The result goes to `%APPDATA%\irelia-companion\evals\latest.json`; the app reads it. h and k are picked on the same folds as the gate, a small optimism the 9-point grid keeps modest.
- **Tests:** on synthetic games where the enemy comp decides the best first item (and the matchup doesn't), the gate passes (+3.6 pts, CI +2.6 to +4.4, no "Next" flips); with no such effect it fails.
- **Speed (0.7.2):** every held-out game is compared with every training game, so the work grows with the square of the stored games.
  - Before 0.7.2: 52 s for 1,100 games, 105 s for 2,200 and 6.4 min for 4,400 (synthetic data, one core). With more games it ran for tens of minutes and looked stuck.
  - Three changes, with the report unchanged to the last digit:
    - `estimateDecision` indexes a decision's observations once per lane (sums that don't depend on the query, flat arrays) instead of rebuilding maps for every held-out game.
    - `recommendDraft` computes each game's similarity to the draft once for its ~20 decisions (`Query.sims`).
    - The bootstrap sums outcomes grouped by decision, with pilots as indices.
  - With the live replay capped at 500 games: 11 s for 1,100 games, 17 s for 2,200 and 41 s for 4,400. The 4,400-game run takes 39 s in the app.
  - `server/model/estimate.test.ts` checks the indexed estimator against the plain one on random decisions.
- **Memory (0.7.3):** the report is unchanged to the last digit; on 4,400 games the whole middleware peaks at about 700 MB instead of 1 GB during an evaluation.
  - **Folds:** they go one at a time, over three passes. The first runs the bandwidth / prior grid with every setting on each fold. The second runs the final comparison, and the third the live replay. Each pass rebuilds the fold it needs, which costs a fraction of the comparisons. Before, all five folds' draft models, weighted games and observation caches were alive together. Live heap at the heaviest step: 101 MB, was 274 MB.
  - **State model:** it is fitted on `Float32Array` views of the stored frames (`stateRows`), built once per fold for every penalty, instead of 60,000 copied arrays per fit. Only the best penalty's held-out predictions are kept.
  - **Timelines:** the raw timelines of games the live replay won't read are dropped once the folds are known, along with their participants.
  - **Large databases:** held-out predictions are appended in a loop. Spreading tens of thousands of them into `push` could overflow the call stack on a large database.
- **Memory (0.8.1):** the report is unchanged to the last digit (the 1,100- and 4,400-game reference reports, and the six-patch one with its patch-memory section). On 4,400 games the whole middleware peaks at about 415 MB during an evaluation instead of 590–660 MB, and the evaluation is about 10 % faster.
  - **Most of the peak was garbage.** With a full collection before each step, the same evaluation peaks about 40 % lower: V8 lets the heap grow to roughly twice what is alive before it collects. The middleware now lets its threads collect on purpose (`server/gc.ts`: `--expose-gc` set before any thread starts). The evaluation thread collects before each step, and the model thread after each rebuild. A full collection takes tens of milliseconds, and these happen about twenty times per evaluation.
  - **One game at a time:** the stored games are read with an iterator and prepared as they come. Each raw timeline is dropped as soon as its game is prepared, and the live replay reads its sample again (`Db.timelinesOf`), so the timelines are never all in memory at once. The participants go once read.
  - **State frames:** they are dropped once the game-state model is fitted, together with its held-out predictions (the report's part on it is made first).
  - **Similarity buffer:** every draft recommendation reused to allocate a buffer as long as the model's games (thousands per evaluation). One buffer is now reused (`similarities` in `model/draft.ts`).
- **The kept model (0.8.1):** the best buy for your gold read the stored state frames. It now reads the state model's win probability at each frame, computed once per build (`PreparedGame.pAt`), and the frames go. That is 13 times smaller, with the same answers (tested). The model kept between builds is 14 MB instead of 27 MB, and 27 MB instead of 54 MB with patch memory on.
- **Speed (0.10.0): what was tried.** Profiled on the 54,000-game test database (10.3 min, the evaluation alone peaking at 1.28 GB with a collection at each step, as in the app), three-quarters of the time is the pairwise comparison: each held-out draft's similarity to every training game (`similarities`, 38 %) and the weighted sums over them (`querySums`, 37 %).
  - **Tried and dropped, with reports the same to the last digit:**
    - Sharing a draft's similarities across the three patch-memory fades: 2 % faster, but 520 MB more at the peak.
    - Comparing runes against the draft in the same pass as the builds (its models have the same games and drafts): 6 % faster, but 220 MB more at the peak.
  - **Kept:** the similarity loop multiplies a difference by itself instead of `** 2` (the same rounding, IEEE) and takes a straight pass when every dimension is known. Reports are the same byte for byte on three databases, and time and peak are the same on the test database, whose games miss most draft dimensions.
  - **Big gains left, each with a cost:** the five folds in parallel threads (2-3× on four cores, an estimate; roughly 0.6-1 GB more while it runs) or a sample of the held-out games in the all-games passes (2-3×; different numbers). Both are off the table for now.
  - **Progress:** within the long steps it reports how far it is (held-out games done, stored games read) every half second, without the collection a step's start gets.
- **Every core (0.11.0, `model/evalTeam.ts`):** the report is unchanged to the last digit (byte for byte against 0.10.0 on the 54,000-game test database with 4 threads, and in the tests with 2 and 3 threads, a thread killed mid-run and champ select starting mid-run). On that database, the same day on a 4-core cloud machine: 12.2 min on one thread, 4.5 min on four (2.7×). The evaluation's peak goes from 1.3 to 4.9 GB.
  - **How:** the job thread reads and prepares the games and fits the game-state models as before (about a sixth of the time, on one thread). It then copies what the comparisons need into memory every thread can read (`v8.serialize` once into a `SharedArrayBuffer`), and starts the other threads (`dist-server/eval-worker.mjs`). Each takes its own copy and runs the same comparisons. At each loop over held-out games, the threads take chunks of games from a shared counter (whoever is free takes the next), the job thread gathers every chunk's results and sends them all to each thread, and every thread adds them up in game order. The sums are the same, in the same order, as on one thread, and each thread goes on from the same state.
  - **How many threads:** every logical core (`os.availableParallelism`), as long as each one beyond the first fits in the memory available now (`os.freemem`, and the process's limit where Node knows one) beyond a reserve always left free: 15 % of the installed memory, at least 1.5 GB. Nothing spills to disk. A thread's memory is measured each run with two threads or more (its heap and buffers at their peak, ×1.15 for what the process takes beyond them) and kept with the run's time (`timings.json`, `bytesPerGame`; its heap alone, `heapPerGame`, 0.19.0). Before the first measure: 6,500 bytes per game read, 5,000 of them heap (about 0.98 and 0.75 GB at 150,000 games, estimates from a thread's peak on 112,750 real games). In the desktop app every thread shares one 4 GB heap: only a thread's heap counts against what is left of it, its buffers count against the memory alone (0.19.0; before, both did: at about 150,000 games the 8,000-byte default could leave room for the job thread alone). If available memory drops below half the reserve while it runs, one thread stops at each loop until it doesn't.
  - **Champ select or a game:** the other threads stop at once, so their memory goes back to the game, and the job thread finishes alone (an automatic evaluation stops entirely, as before). A thread that stops or dies leaves its games to the job thread, which works out every chunk it had taken and not sent.
  - **Estimates:** the time is scaled to the threads by Amdahl's law, with a sixth of it on one thread (`EVAL_SERIAL_SHARE`, measured above), from the last run's own threads.
  - **The Data tab** shows the processor, its threads and the memory (`GET /api/system`), how many threads Evaluate builds would use and the memory each takes, and "· 4 threads" while it runs.
- **Less memory per thread (0.12.0):** the report is unchanged to the last digit (byte for byte against 0.11.0 on the 54,000-game test database with 1 and 4 threads and on the 6,300-game one with 1 and 4, and in the tests). On that database with 4 threads, the same day: the evaluation's peak went from 4.8 to 2.3 GB, each other thread's from 967 to 427 MB (10,700 bytes per game read, against 23,500), and the time from 7.4 to 6.5 min. With the same free memory, about twice as many threads fit.
  - **Results stay with the job thread:** at each loop the others send their games' results and go on; only the job thread adds them up, in game order as before. What steers later work it shares with them (`Team.share`): the bandwidth and prior the grid chose, patch memory's fade per patch, and the expected win rates. Before, every thread received and added up every result (the outcomes alone are tens of MB per thread).
  - **One fold's models at a time:** each fold's work is a call of its own (`compareFold`, `gridFold`, `scoringFold`, …), and the heap is collected before the next fold's models are built (`collect`: the job thread and the others). A loop suspended at an `await` kept the last fold's models until the next ones were built, and V8 let the heap grow to about twice what was alive before it collected. Patch memory early in a patch now takes the two-patch model's picks first and builds patch memory's model after (`sidePicks`), instead of holding both.
  - **The purchases' chances, once:** scoring a game's purchases with a fold's state model read each purchase's state (28 numbers per item, `ItemChoice.x`), and every thread had a copy. Each purchase's win probability under each fold's model is now computed once, after the state models are fitted (`purchaseChances`, `scoreGames`' arithmetic), in memory every thread reads (a `SharedArrayBuffer` handed over beside the copy), and the states go. The purchase log goes too (only the personal layer reads it). Each thread's copy of the games: 289 → 180 MB at 54,000 games.
  - **Shared out too:** the bootstrap's resamples (four intervals of 1,000 each) and the expected win rate's five per-fold fits run on every thread. Resample b draws its pilots where it would in the one sequence (the generator adds a constant per draw, so b's draws start b × pilots draws in), and the deltas are sorted together as before. On 4 threads: the interval 6.3 → 1.7 s, the expected win rate 17 → 6 s.
  - **A check per model, not per draft:** `similarities` checked every stored game's context length on every draft (`zDraft.some`, 4 % of the time on one thread); it is checked once per model (`widthOf`), the arrays being fixed once built, like the flat copy beside them.
  - **Planning:** a thread's measure says which layout it was taken with (`THREAD_LAYOUT`, `timings.json` `layout`); 0.11.0's (twice as much) is not used. The process grows by about what a thread measures (402 MB per thread against 427 measured, from the whole process's peak: 1.12 GB on one thread, 2.32 GB on four), so the margin on the measure is 15 % (was 25 %). Before the first measure: 10,500 bytes per game read (10,000 since 0.14.0).
- **Flat contexts (0.14.0):** a draft model's standardised contexts are three flat `Float64Array`s (`Contexts`: the draft one per game, the item and boots ones with each game's first row, `itemRow` / `bootsRow`) instead of an array per game and per item, and `similarities()` reads the draft one directly, where it kept a flat copy of the arrays beside them. An observation names its context by row (`zs`, `zRow`; `kernelAt` is `kernel()` on a row). The report is unchanged byte for byte (54,000-game test database, 1 and 4 threads, against 0.13.0); a thread's measure went from 9,758 to 9,413 bytes per game read, so measures are taken anew (`THREAD_LAYOUT` 3).
  - **One thread:** the same time as 0.11.0 (back to back on the 6,300-game database: 1.04 and 0.97 min against 1.06 and 0.96): the collections cost about what the purchases' chances and the per-model check save. 17.7 min on the 54,000 games that day, a slower day for that machine than the figures above.
- **Use GPU (0.13.0, Settings → GPU, off by default):** each stored game's similarity to a draft (`similarities`, about two fifths of an evaluation's time at 54,000 games) is worked out on an NVIDIA graphics card, with the same numbers, in every thread of the evaluation. The report is unchanged to the last digit.
  - **How:** a CUDA kernel ([server/gpu/similarities.cu](../server/gpu/similarities.cu)), shipped as PTX that the driver compiles for the card, called through the driver's own library (`nvcuda.dll`) with koffi. Nothing to install beyond the NVIDIA driver (CUDA 12.0 or later, compute capability 5.2 or later).
  - **The same numbers:** the distances are the processor's subtractions, products and sums in the same order, each rounded on its own (no fused multiply-add), and the same divisions. Electron's `Math.exp` is correctly rounded (V8 uses LLVM libc's; checked: 200,000 of 200,000 values), so the kernel's exp is correctly rounded too: double-double to about 2^-75, rounded only when everything within 2^-70 rounds the same. The rare game where that can't be told (about one in 100,000), or whose exponent is out of range, is worked out on the processor.
  - **Checked:** before each evaluation the GPU works out 36,000 test similarities and every one is compared with the processor's; while it runs, one pass in 500 is compared too. A difference, an error or a lost GPU sends that thread back to the processor for the rest of the evaluation, and the GPU log says what happened. Plain Node's `Math.exp` (fdlibm, not correctly rounded) fails the check, so `npm run eval:builds` stays on the processor. Before release, the kernel ran on the processor with a stand-in driver under Electron 44: 434,000 similarities and a 3-thread evaluation the same to the last digit.
  - **Champ select or a game:** the GPU isn't used (the processor takes over each pass), so the card is the game's. Each thread loads the kernel on the card's shared context; when the evaluation ends, or after it is stopped, the context is reset, which frees what every thread put on the card.
  - **The weighted sums (0.19.0):** a second kernel ([server/gpu/sums.cu](../server/gpu/sums.cu)) adds each option's games in the processor's order, one thread per option, with unfused multiplies and adds (the same bits), for lanes of 20,000+ games. A test lane is checked before each evaluation and one call in 500 while it runs; any difference turns only the sums back to the processor (`IRELIA_GPU_MIN_OBSERVATIONS` overrides the threshold). **Off unless `IRELIA_GPU_SUMS=1`:** on a real laptop card (RTX 4050) a full evaluation crawled with it. The order makes it serial within an option: at 120,000 observations whose largest option holds 25,600, about 9 ms a pass (an estimate) against 1.3 ms on one processor thread (measured), and every evaluation thread waits its turn on the card.
  - **Speed (0.13.0, similarities only):** the processor still did the rest, so expect a quarter to a third less time than without it on the same threads (an estimate); Test GPU logs how much faster the similarities are on this computer.
- **0.15.1:** the report is unchanged byte for byte (54,000-game test database, 4 threads, against 0.15.0): 419 → 360 s that day (−14 %).
  - **The reading on helper threads:** the stored games are read and replayed on the model build's helpers (`model/prepareTeam.ts`, from 4,000 games), put back in the order they were read: 28.5 → 10.8 s, where three cores waited before. Champ select or a game midway: the batches out come back, the rest is read on the job thread.
  - **Fine-tuning's item prior** works out the four item decisions only (`SCORED_ITEMS`); the other five are the chosen model's picks, which the same weights and prior give them: 86 → 75 s.
  - **Patch ages** are worked out once per pair of patches (`patchAges`), not looked up in the patch list for every observation (1.8 % of the time): patch memory's two passes on every held-out game 108 → 85 s.
  - **The thread plan (desktop app):** the job thread's heap is collected before it is measured. The frames and purchases just dropped counted as held (873 MB at 54,000 games, 227 MB alive), and every thread shares one 4 GB heap there: 1.6 → 2.0 GB left for the other threads, one more at 0.5 GB each. What the app's other threads hold (main, model, scraper) is measured every 2 s while it runs (`setOtherHeap`), a quarter and 256 MB more kept for them, at least 1 GB as before (`otherThreadsHeap`). No model build starts meanwhile (`model/worker.ts`), since it would add a second model to that heap; the first build doesn't wait. The middleware's log shows the plan (`[eval] threads: …`).
- **0.15.2: lane baselines once per fold, for every thread.** At a fold's first pass, the lane baseline of each held-out game's lane and opponent is worked out once over the team (`baselinesOf`, `team.forEach` then `team.share`) and handed to every thread, with the validated training games kept (`recommend`'s memo, as the model thread keeps them). Before, each thread built those its own games needed, mostly the same ones, validating every training game again for each. The report is unchanged byte for byte (the 54,000-game test database with its lane opponents spread over 120 champions, 2 threads): 837 → 673 s, the settings grid 212 → 74 s. On 24,000 training rows one opponent took 426 ms, 226 ms with the memo. In the first evaluation on 0.15.1 at 121,000 games (2 threads), each fold of the grid took about 57 s beyond its games (an estimate from its steps), likely much of it these baselines.
- **0.16.0: quick updates between full evaluations** ([docs/EVAL-UPDATE.md](EVAL-UPDATE.md)). A full evaluation saves its base next to its report (`evals/base.bin`, `base.json`: every held-out game's outcomes, the settings it chose, salted hashes of the match and pilot IDs and of what each game held). A quick update (`EvalOptions.update`, [server/model/evalBase.ts](../server/model/evalBase.ts)) reads the games as a full one does, rebuilds each fold's models from all its training games with the base's settings, works out the held-out games the base hasn't measured, adds their outcomes and decides every gate again with the same rules and bootstrap; the grids and the live replay are kept from the base. With nothing new it is the full evaluation again, number for number. `planEvaluation` picks quick or full (a new patch or a region moving, a changed app, Data Dragon, capture, data folder or setting, 20 % more games or 30 % more of the current patch's since the last full one, a week, 2 % of the base's games gone or rewritten); a verdict the new games move clearly across its line goes on to a full evaluation, one they only move onto it keeps the full evaluation's. On the user's 82,000 games a quick update took 151–231 s against 1,039–1,931 s in full (2 threads) and stayed within 0.015 pts of its base; 118 new games of a young patch moved a full evaluation itself by 0.05 pts, so early in a patch every evaluation stays full.
- **0.17.0: late-game swaps** ([docs/SWAPS.md](SWAPS.md)). At shop visits from 20:00 with every slot full (`server/model/swaps.ts`), each held legendary or tier-2 boots kept, sold for the next legendary finished within 10 min, or sold for nothing; a game's decision points share one game's weight. Scored like purchases (result − the state model's win chance just before the visit), estimated per held item (`adviseSwap`, keeping as the reference, a swap only with a clear lead over keeping), gated by Evaluate builds (`evaluateSwaps`: following the advice against keeping every item, pilot bootstrap; quick updates add new games' situations). On the user's 82,069 games keeping did 7.2 pts better than selling, state-adjusted; the gate failed, so the Items view keeps the Sixth Item unless the Data tab's Late-game swaps switch says On.
- **Speed (0.8.6):** the report is unchanged to the last digit (byte for byte on three test databases: 4,400 games of one patch, 4,400 over six patches and 6,300 over six patches; chosen setting, lift, interval, Brier scores and patch memory's verdict on 54,000 games). On a test database of 54,000 games over six patches (30,000 in the two newest), one core: 35.8 min in 0.8.5, 10.3 min now. The final comparison went from 8.3 to 2.5 min, patch memory on every held-out game from 15.5 to 4.5 min, the grid from 4.5 min to 35 s, patch memory's grid from 3.4 min to 67 s, early in a patch from 82 to 28 s and the live replay from 125 to 34 s.
  - **Why it was slow:** each held-out game's ~20 decisions were estimated against every training game, and the evaluation does that on all 30,000 held-out games twice (the final comparison, then patch memory with ~24,000 more games from older patches), besides the grid and the replay. Those two passes took 8.3 and 15.5 min.
  - **Only the scored decisions** (`recommendDraft` `extra.decisions`): the evaluation reads nine (`EVAL_DECISIONS`); the others keep the baseline's pick unestimated. None of the nine depends on another decision's pick.
  - **Grouped by option** (`LaneIndex.start`, `gw`, `gwpa`, …): each option's observations sit together, so a query's sums for one option stay in local variables (`groupedLoop`). Within an option they're added in observation order, and the context total in observation order too (`contextTotals`), so every sum rounds as before.
  - **Without context** (an infinite bandwidth) the sums don't depend on the draft: the lane's are kept in the index, and the matchup's per opponent (`opponentSums`).
  - **The grid, game by game:** a draft's sums at one bandwidth are the same for every prior, so the three priors share them (`Query.memo`), and its similarities (`similarities`: a flat array, the query's known dimensions only when no stored game misses one). Each setting's outcomes still come in game order.
  - **The live replay:** each minute's similarities come from the same flat array (an observation's `row` is its context's index in its item slot), so it takes the grouped loop too.
  - **Memory:** the grouped arrays add about 40 bytes per observation to a fold's index, and the evaluation now indexes nine decisions instead of about 20. Peak: 1.8 GB on the 54,000 games, where 0.8.5 was past 2.0 GB five minutes in; 518 MB against 0.8.5's 477 MB on 2,200 games, 517 against 494 MB on 3,500.
  - `server/model/estimate.test.ts` checks the kept sums against a fresh estimate for each prior, and the flat similarities against `kernel()`.

- **Runes against the draft (0.9.0):** compared after patch memory, with its own gate (§9.10).
- **History (0.9.0):** the Data tab's **Evaluation history** card reads every saved report (the newest 30; `model/evalHistory.ts`, `GET /api/model/evaluations`). It draws one chart per question, on a shared axis of evaluations: draft-adjusted against the lane baseline, patch memory against two patches, runes against the draft against the overall win rate.
  - **Each evaluation** is its difference in lift with the 95 % interval, against zero; the newest value is labelled.
  - **Verdicts:** a filled green dot is better (the gate passed); a hollow grey ring, no clear difference; a red triangle, worse (the whole interval below zero). Shape and label carry the verdict as well as colour: green and red are close for colour-blind readers (the palette validator puts them in its warning band on the dark panel).
  - **Each chart** says how often the option was better in the last 10 evaluations and how long the newest verdict has held. Hover or keyboard focus shows the interval, when, how many games and the reason; **As a table** lists every number.

### 9.7 UI (B7)
- **Build tab:** a Draft-adjusted / Lane baseline switch (your choice is kept; unset, it follows the gate), with the model's evaluation status; "Draft adjustments": the draft's unusual dimensions, each changed pick with its reason chip, lift and evidence, picks that kept the baseline for lack of data, and region toggles.
- **Your games tab:** timings, what works for you, and the post-game review.
- **Data tab:** the model's state, **Evaluate builds** (it also runs by itself with Follow the evaluation on, §9.6), the switches **Patch memory** (§9.8), **Runes against the draft** (§9.10) and **Follow the evaluation**, which lets the evaluation decide both, and the **Evaluation history** (§9.6). The Patches card has the archive switch.
- **Switches:** every on / off setting in the app is the same switch (`Switch`, `SwitchRow` in `web/src/components/common.tsx`): the label and what it does on the left, the switch on the right. Settings' desktop options use it too.
- **Progress bars (0.10.0):** every long action shows the same bar (`ProgressBar`, `Segments` in `web/src/components/Progress.tsx`): a row of small vertical segments that fill from the left, the one at the front pulsing while it runs (a soft sweep when the work can't be counted; still with reduced motion). Beside it: what it is doing, the time it has run and its time left, always marked as an estimate. The same segments are the API rate windows' meters and the overlay's gold-towards-the-next-item meter (no time there: nothing in game says when).
- **Estimates (0.10.0, `server/progress.ts`):** each finished action's time and size (games, matches, bytes or files) are kept in `timings.json` next to the settings (this computer only, the newest five per action). Before an action starts, its button says how long it should take ("Takes about 10 min (estimate)"): the last run here, scaled to the size it has now (the evaluation by games^1.3, the rest in proportion). While it runs, the time left is that minus the time passed, until its own pace (time so far over the share done) can say more: halfway, its pace alone. The evaluation's share done weighs each step by what it took last time (its steps differ a hundredfold) and counts the held-out games within a step. Before an action has ever run here, the evaluation and the compaction start from what they took on the test database; the others show "estimating the time left…" until their pace says something (a few seconds). Covered: Evaluate builds, the data audit, Rebuild from cache, model rebuilds, archiving / restoring old patches, trimming the API cache, compacting the database, clearing the API cache (all or the expired answers), removing old Data Dragon patches, deleting old patches' games, moving the data folder, and each region's fetch queue at its pace (an estimate the queue keeps outgrowing while fetching runs).
- **The model's switches (0.9.2):** the main thread alone saves them (`config.json`) and shows them at once (`withSwitches`, `model/service.ts`); the model thread only reads them. It rebuilds with them unless champ select or a game is on. A switch moved during a build waits for that build to end (35–45 s at 54,000 games) and is what the next one is for (each build reads its settings once, at its start). Switches moved together take one rebuild, none if they cancel out. Until the model has it, the switch says so (`inModel`: "Updating the model…", or "at its next update" while champ select or a game holds it).
- **Overlay and Live Game card:** the mode you chose. Draft-adjusted: "Next" with its reason. Lane baseline: the build plan's next item and counter items for the enemy team. The overlay shows the dashboard's matchup and build (pushed over `/ws`), so the switch, a lane or an opponent picked in the dashboard reaches it at once.
- **Dock:** removed in 0.6.0. Its streamed OTP scan predated the per-host lanes, and its lane builds are the Build tab's.

### 9.8 Patch memory and the archive (0.8.0)
Games of patches before the previous one, kept instead of discarded, while what changed since counts less. Additive and off by default: off, the model is 0.7.4's to the last digit (the evaluation reports of the test databases are identical).
- **Window:** today's patch and the five before it (`MEMORY_PATCHES = 6`, about three months), counted on Data Dragon's version list per region (a region a patch behind counts from its own patch). Further back, a game weighs nothing.
- **Weights (`model/weights.ts`):** current and previous patch as in §9.3. A game *a* ≥ 2 patches back weighs decay^(a−1) × the diff weight, where the diff compares the game's patch with today's: 0.8 when neither Irelia (stats, spells, passive) nor the decision's items (gold, stats, description) changed in between, 0.2 when one did, 0.35 when a patch's Data Dragon files can't be read. The fingerprints of the last seven patches come from Data Dragon's own files (`loadPatchHistory`, `HISTORY_PATCHES`, cached in the data folder; no other source): the window plus one, because on patch day Data Dragon is a patch ahead of the servers and a region's window then reaches one patch further back (before 0.8.2, that oldest patch counted as unknown). A hand-entered `patchWeights` entry wins and fades the same way.
- **Data check (`consistent`):** per lane and option, today's games against older ones on mean WPA. When both sides have at least 30 effective games and differ by |z| > 2, the older games keep (2/z)² of their weight, so today's results lead. It catches what Data Dragon can't show: hidden changes, bug fixes, other champions' changes. It sees an option's average only: an effect that reversed within some drafts but not on average is left to the evaluation.
- **What stays on two patches:** the game-state model (WPA), the champion profiles, the matchup facts, the best buy for your gold, the Your games report and the post-game review's high-elo timings (since 0.8.2; before, the review compared with all six patches). Only the draft-aware picks and live "Next" read the older games.
- **Evaluation (§9.6, its own section in the report):** on the same held-out games and folds, the draft model with patch memory against the one without it (both with the chosen bandwidth and prior, each fold's state model). Older games of a test fold's pilots are never training games. The fade per patch is chosen from {0.5, 0.75, 0.9} on the grid's subsample. Two conditions:
  - **All games:** every training game.
  - **Early in a patch:** a quarter of the current patch's training games (the same quarter for both models), which is when older patches matter most.
- **Gate:** on by itself (Data tab: **Follow the evaluation** switched on, the default) only with at least 200 older games, a lift no worse than without it with all games, a Brier score no worse, and a lift difference whose 95 % pilot-bootstrap interval is above zero with all games or early in a patch. Moving the **Patch memory** switch makes the choice yours (Follow the evaluation goes off); off is 0.7.4's model. Since 0.8.4, Follow the evaluation also has the evaluation run again by itself after 300 new games or a new patch (§9.6), so its verdict doesn't go stale.
- **Tests:** older patches that still hold pass the gate (synthetic, early in a patch +3.7 pts, CI +2.0 to +5.0); older patches where the two first items swapped roles fail it (worse with all games, worse Brier).
- **Speed and memory (synthetic, 4,400 games over six patches):** the evaluation takes 31 s with its patch-memory section; the model with patch memory builds in 2.0 s instead of 1.1 s (twice the games) and keeps 27 MB instead of 14 MB (0.8.1; see §9.6 Memory).
- **Storage:**
  - Two switches, both off by default (`server/oldPatches.ts`, `GET / PUT /api/old-patches`). Each applies when switched on, and again whenever a patch leaves the window (checked a minute after start and hourly; the automatic run waits while champ select or a game is on, since its batches run on the main thread). One operation at a time; none while the data folder moves or the database is compacted.
  - **Archive patches older than the window** (Data → Patches) stores each such match whole (its rows in every table) as gzipped JSON in `archive`, then deletes it from the working tables, keeping its per-champion totals like a deletion. It runs 200 matches per transaction (`BEGIN IMMEDIATE`), so the app stays responsive, with a 150 ms pause between batches (0.8.5) so that fetching's writes get in ([DESKTOP.md §1](DESKTOP.md)). About 4.5 KB per match on the test data, against about 13 KB in the tables. Switched off, every archived match is put back as it was and its totals taken back out (tested row for row); a match is removed from the archive only once it is back. Your own games are archived like any other; archived games leave the Your games list until restored. A database archived with 0.8.0's one-off button counts as switched on.
  - **Delete games of old patches** (Settings → Storage, with a confirmation) deletes them instead; per-champion totals are kept.
  - The database file shrinks only after **Compact database** (Settings → Storage); otherwise new games reuse the space.
  - **Keep the API cache to the six-patch window** (Settings → Storage) deletes cached matches and timelines of games older than the window. The stored or archived match's patch decides, else the cached match's own game version; files it can't place stay. It runs again only once the window has moved (`meta.cache_trimmed_to`). Matches and timelines never expire otherwise, so this is what keeps the cache from growing without end.
  - Nothing is deleted without a click and a confirmation.

### 9.9 Known limits
- The state model learns from Irelia games' timelines only (about 30 frames each); team-level effects are shared across champions, Irelia-specific ones are not separated.
- Pre-game decisions have no state to adjust for: their WPA is the result minus the base rate, so for runes and summoners the draft model differs from the baseline only through context weighting and shrinkage. With **Runes against the draft** on (§9.10) it is the result minus the game's expected win rate before it started.
- Some clients report other players' CS in steps of 10; stored timelines are exact.
- `HordeKill` is assumed to be the Live Client's voidgrub event; unknown events are ignored, so a different name only drops grubs from the live state.
- A rebuild takes about a minute at 118,000 games on one core. Since 0.15.0 it runs on helper threads and requests are answered meanwhile with the model in use; before, they waited for it (up to 30 s, then "The model is busy").

### 9.10 Runes and summoners against the draft (0.9.0, `model/expected.ts`)
- **Why:** a pre-game choice (keystone, rows, secondary tree and runes, shards, summoners, start, skill order) has no game state to be judged against, so it was scored by the result minus the overall win rate. A choice taken mostly into hard matchups, on a weak patch or by weaker pilots was blamed for them; one favoured by strong pilots was credited for them.
- **Expected win rate before the game:** a ridge logistic regression on what is known before the game:
  - the 12 draft dimensions (an unknown one counts as average), the lane, the patch age and the region;
  - the pilot's ladder standing (apex tier and LP), Irelia mastery and source;
  - each lane opponent's own effect, shrunk towards zero (a random effect). It is fitted by Newton's method, with the opponents' diagonal block solved through its Schur complement.
- **The choices are in the fit too:** the expected win rate is read with them at their average mix. Left out, the pilot's strength soaks up part of the effect of the choices strong pilots favour. Adjusting the result alone biases a choice's effect towards zero by as much as the choice follows the pilot or the draft: on the synthetic test, −4 points recovered of a true −8.
- **No leakage:** each game's expected win rate comes from a model fitted without its pilot's games (5-fold cross-fitting by pilot). The penalties (draft, pilot, patch and region; lane opponent) are chosen by cross-fitted log loss from (1, 10), (10, 30), (100, 100) and (1000, 1000). The evaluation records the choice, and the app uses it.
- **What changes:** with it on, each pre-game decision's WPA is the result minus the game's expected win rate. Items keep the game-state model's WPA. Your own games keep the overall rate, so the personal layer is unchanged.
- **Evaluated** (Evaluate builds, after patch memory): the draft model with it against without it, on the same held-out games, with the same bandwidth, prior, state models and lane baselines.
  - Only pre-game picks can differ, so the five scored pre-game decisions are compared (keystone, secondary tree, summoners, start, skill maxed first).
  - **One yardstick for both:** how much more often than its expected win rate a held-out game that followed the pick was won, minus those that didn't. The expected win rate is the fold's, fitted on its training games only. So neither model is credited for the matchups its picks fall into.
  - The report also gives the old yardstick (the result minus the overall win rate), the Brier score of each model's win chance for the option taken, and whether the expected win rate predicts held-out results better than the overall one.
- **Gate:** on only when the expected win rate predicts held-out results better than the overall win rate (log loss), the lift difference's 95 % interval (pilot bootstrap) is above zero, and the Brier score is no worse. Off until then.
- **Switch:** Data tab, **Runes against the draft**, under the same **Follow the evaluation** as patch memory (§9.8). Following, the last verdict decides; moving the switch makes both choices yours (patch memory keeps what it was).
- **Tested** (`model/expected.test.ts`):
  - The fit matches a plain ridge logistic regression with one column per opponent.
  - A game's own result never moves its expected win rate.
  - **Synthetic pilots:** strong pilots take Ignite in 85 % of their games (weaker ones in 15 %), and Ignite costs 8 points. Against the overall win rate Ignite looks about 6 points better; against the expected win rate it costs about 8 (−8.1 on average over eight seeds). The gate passes in 5 of 8 seeds at 1,600 games and stays off in the others.
  - On the synthetic test databases, where neither the draft nor the pilot predicts the result, the expected win rate is no better than the overall one, so it stays off. The rest of the report is byte for byte unchanged on three test databases.
- **Cost:** on the 54,000-game test database the evaluation takes 13.5 min instead of 10.3 (10 s to fit the expected win rates, 2.3 min to compare on every held-out game), with the same peak memory (1.8 GB). The rest of the report is the same to the last digit. A model build with it on fits five more models, a few seconds at that size.

### 9.11 Fine-tuning (0.15.0, `model/evaluate.ts` evaluateTuning)
- **What:** three settings that were set by hand, each tried against the chosen draft model on the same held-out games, state models and lane baselines:
  - **Pilot standing's strength:** the pilot weight (source, tier, mastery, LP) raised to 0, 0.5 or 2 (1: as before; 0: every pilot alike).
  - **The item decisions' own prior:** core items, boots and items 4-6 (and the live "Next") with another of the grid's priors than runes, summoners, the start and skills.
  - **A fade within the current patch:** a game of its region's current patch counts half after 7 or 21 days before the newest game (older patches: patch memory).
- **How:** each setting alone on the grid's subsample; the better value of each, together, on every held-out game, with the pilot bootstrap. The lift's yardstick is the usual one, with the chosen model's picks in the baseline's place.
- **Gate:** better than the chosen model with 95 % confidence and a Brier no worse. Until an evaluation passes it, the app uses today's settings; once passed, the Data tab says what it uses.
- **Within each pilot (reported only):** the lifts compared within each pilot (followed against not followed by the same pilot, weighted by n₁n₀/(n₁+n₀)), so a model isn't credited for the pilots who tend to follow it being better players. Reported next to the lift; not part of a gate yet.
- **Cost:** on the 54,000-game test database (Linux, 4 threads) the evaluation took 295 s with fine-tuning against 253 s without (+17 %; on this synthetic data no setting changed a pick, so the confirmation on every held-out game didn't run: with one, add about one more comparison on every held-out game).
- **The grid's subsample (0.15.0):** the bandwidth, the prior and fine-tuning's settings are chosen on up to 10,000 held-out games, not 2,000: under 2 % of the games at 118,000. The evaluation then took 386 s (+31 %), the same choice on the test data.

## 10. Stage C as built (0.7.0)
Rules (docs/DESKTOP.md §2): writes to the League client only on your click, only during champ select, never in game; in game, build and shop advice only, from the Live Client data alone; no new data sources.

### 10.1 One-click import (C1, `lcu/importer.ts`)
- **What:** the build shown (draft-adjusted or lane, whichever you chose) becomes the **"Irelia Companion"** rune page, made current, and the **"Irelia Companion · Top vs Darius"** item set: start, core and boots in order, the three best 4th / 5th / 6th items, control wards and potions. The item set shows in the in-game shop.
- **When:** the Build tab's **Save runes and items to the client** button, in champ select. `POST /api/lcu/import` refuses outside a real `ChampSelect` phase, during a game, from any page that is not the app's own (`Origin` other than localhost / 127.0.0.1), and when the build changed since you saw it (the dashboard sends the key of the build it shows).
- **Only its own:** it replaces the rune page whose id it saved (or one named exactly "Irelia Companion") and the item set with its saved uid (or titled "Irelia Companion …"); the ids are kept in `config.json` (`lcuImport`). With no free rune page it says so and changes nothing of yours; the item set is saved either way. The item-set endpoint replaces the whole list, so every other set is sent back unchanged.
- **Endpoints** (unofficial, from the League client): `GET/POST /lol-perks/v1/pages`, `DELETE /lol-perks/v1/pages/{id}`, `GET /lol-perks/v1/inventory`, `GET /lol-summoner/v1/current-summoner`, `GET/PUT /lol-item-sets/v1/item-sets/{summonerId}/sets`. Tested against a fake client; a client update that changes them makes the import fail with the client's message, nothing else.

- **"vs" blocks (0.9.0, `matchupBest` in `model/facts.ts`):** after the build's start, the start that did best against your lane opponent in this lane; after the core, the first completed items that did best (up to two), titled with the opponent and the games behind them ("vs Darius: best start (24 games)").
  - "Did best" is each option's win-probability-added in those games, shrunk towards zero: the sum divided by its games plus 20, so a few lucky games can't top the list. The start counts from before the game, the item from the game state when it was bought.
  - Options need 5 games, the matchup 15. Only items you can still buy count; your own games never do.
  - The Matchup card shows them in champ select ("Did best in this matchup").

### 10.2 Matchup facts (C2, `model/facts.ts`)
- From the stored high-elo games of this lane against this opponent (your own never): Irelia's total gold minus her lane opponent's at 10:00 and 15:00 (timeline frames; median and the share of games she was ahead), the win rate, and the win rate by game length (< 25, 25–30, 30–35, 35+ minutes), each with its number of games. At least 3 games.
- **Before the game only:** the Build tab's Matchup card in champ select and outside games (the Live Game card replaces it in game), and one line in the overlay in champ select.

### 10.3 Best buy for your gold (C3, `model/buy.ts`)
- **Target:** the next completed item: "Next" in draft-adjusted mode, else the build plan's next item for your inventory.
- **With enough gold** (its price minus the parts you own, each owned item counted once): the item itself.
- **Otherwise:** its missing parts that you can afford. For every stored game in your lane that finished the item, the first still-missing part it bought is that game's choice; games against your lane opponent count three times. The most common choice wins, unless another affordable part was chosen in at least 30 games, as was the common one, and its games beat the game-state prediction by 2 points more (win-probability-added at the purchase, §9.2). The rest of your gold goes to the next parts the same way. On a first back this is the first core item's parts.
- **Shown:** "With 1,250g: Sheen + Long Sword" in the overlay's build order and the Live Game card, with how often high-elo Irelias buy it first, and the estimated lift in the tooltip. Shop advice only: never when to go back or what to do.
- **Who is ahead** (`engine/adaptive.ts` `liveSummary`): team item value and kill differences, and the most fed enemy (AP / AD) and ally, from the scoreboard only (Data Dragon prices of the items everyone can see on Tab). A player counts as most fed only with items worth a quarter more than their team's average. Labelled as estimates.

### 10.4 Known limits
- Component order needs component purchases in the stored games: games stored before v3 without a timeline still have their purchase list, but games whose purchase list only holds completed items fall back to "the part that spends most of your gold".
- Beyond the "vs" blocks, the item set's blocks are fixed; the League client may cap rune page, item set and block names.

## 11. Defaults
- **Sources:** collect from every source; builds stay OTP-only (`buildSources: ["otp"]`), agreed with you.
- **Default scope:** KR + EUW1 + EUN1 + NA1 + JP1, Challenger + Grandmaster + Master (EUN1 and JP1 added in 0.4.1).
- **Main threshold:** 100k Irelia points, Irelia in the top 3 masteries (harvested players: any rank, via by-champion).
- **History window:** the whole patch, up to 300 ids per pilot.
- **Rechecks:** pilots every 3 days, others every 14.
- **Remakes:** the flag or < 5 minutes. 15-minute surrenders kept.
- **Leavers:** no XP, no CS and ≤ 140 g/min for 3 minutes after 2:00 (5 minutes at level 18).
- **Lane opponent:** median distance from 3:00 to 14:00.
- **Own games:** current and previous patch, solo and flex, any position.
- **Runes against the draft (0.9.0):** off until an evaluation finds it better (Follow the evaluation); the expected win rate's penalties by cross-validation. "vs" blocks: 15 games in the matchup, 5 per option, a 20-game prior.
- **Champ select (0.9.0):** the draft model keeps the games' weights and decision observations between drafts, and lets them go 3 minutes after the last.
- **Stage B:** draft model off by default until the gate passes; draft sources otp, main, harvested (1 / 0.8 / 0.6); 5 % pilot cap; previous patch 0.8 / 0.2 by the Data Dragon diff; own region ×1.5; bandwidth 0.7 and prior 100 until evaluated; fallback under 30 effective games; "Next" changes after leading by 0.01 for 3 polls; personal prior 30 games.

## 12. Backing up the pool (0.19.0)

The high-elo pool can be saved as a ZIP into a folder the user picks (a Google Drive folder synced to the computer, say), once a week and on **Back up now**; each computer keeps its own 3 newest there (0.20.1: before, a computer counted the others' backups too and deleted them). The ZIP holds `manifest.json` and a copy of the database with other players' IDs blanked: the pool's pilots (the one-tricks and mains it follows) and the Irelia players of its games keep theirs, ladder-only players are left out. It is written as `.partial` and renamed when finished, so a syncing folder never uploads half a file.

Another computer pointed at the same folder is offered the newest backup made on a different computer, and imports it only when asked: games it doesn't have are added, nothing stored is replaced, and the pool's builds then include them. Nothing about builds changes otherwise: they always come from the pool.

A per-player data set (one player's games only) was built for 0.18.0 and never released: a single player's few games were too thin to build from, so 0.19.0 dropped it.
