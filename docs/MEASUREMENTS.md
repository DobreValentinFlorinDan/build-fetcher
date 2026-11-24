# Measurements

Each section says how it was measured (exact commands), on what, and what was not verified.

## Store

Machine: the build machine (Linux container, 4 cores, no GPU), release build, 2026-11-24. Synthetic data from
`bf_core::testkit::Synth` (seed 7): 50,000 games over 3 patches (16.17-16.19) and 3 platforms, 10% with a full
timeline, 3% with one defect for the filters, Irelia featured in 5% of games and tier A.

### Commands

```
cargo build --release -p bf-store --example measure
time target/release/examples/measure 50000 7 0.1        # batches of 1000 games per transaction
target/release/examples/measure 50000 7 0.1 50          # batches of 50 (closer to the fetcher, which ingests one game at a time)
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
