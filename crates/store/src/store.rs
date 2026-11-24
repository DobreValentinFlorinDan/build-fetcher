//! The store: ingestion into the database, and the learner's view of it (`impl DataSource`).
//!
//! One writer connection (ingestion, clean-up) and one reader connection (the learner, the dashboard), both on the
//! same WAL-mode file, so reads never wait for a long ingestion batch.

use crate::db::{self, meta_get, meta_set, profile_code, profile_from};
use crate::options::{enemy_profile, player_options};
use crate::parse::{self, Drop, Parsed};
use crate::record::{patch_code, patch_from, platform_code, rank_code, rank_from, RecordCodec};
use anyhow::{anyhow, Result};
use bf_core::build::{BuildOption, CountContext, CountQuery, DataSource, MatchRecord, OptionCounts, OptionKind, PairVolume, Patch, RankBand, Role};
use bf_core::codec::Dictionaries;
use bf_core::ddragon::StaticData;
use parking_lot::{Mutex, RwLock};
use rusqlite::{params, Connection, OptionalExtension};
use serde_json::Value;
use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// A win chance for player `i` of a record before the game (the model's), used as the count's baseline. Without one
/// the store uses the champion-role's own win rate on that patch so far, shrunk toward 50% (see `pair_rate`).
pub trait Baseline: Send + Sync {
    fn win_chance(&self, record: &MatchRecord, player: usize) -> Option<f64>;
}

#[derive(Clone, Debug)]
pub struct StoreOptions {
    /// Size of the held-out validation sample.
    pub validation_cap: usize,
    /// One timeline game in this many (by match id hash) goes to validation while it has room.
    pub validation_one_in: u64,
    /// Records packed before the zstd dictionary is made from them.
    pub dictionary_after: usize,
    /// A champion-role with fewer games than this on the newest patch is thin (targeted sourcing, widening).
    pub thin_games: u32,
    /// Players remembered per thin or tier A champion-role (targeted sourcing).
    pub player_champs_per_pair: usize,
    /// Prior games (half wins) shrinking the champion-role baseline.
    pub baseline_prior_games: f64,
}

impl Default for StoreOptions {
    fn default() -> Self {
        StoreOptions { validation_cap: 2000, validation_one_in: 10, dictionary_after: 64, thin_games: 300, player_champs_per_pair: 40, baseline_prior_games: 20.0 }
    }
}

/// One answer to ingest: the match (and its timeline when fetched), the rank band it was sourced from, and the
/// user's puuid (their games go to the Mine band and are kept).
pub struct Ingest<'a> {
    pub m: &'a Value,
    pub timeline: Option<&'a Value>,
    pub rank: RankBand,
    pub me: Option<&'a str>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Outcome {
    Counted { kept: bool, timeline: bool },
    Validation,
    Duplicate,
    Dropped(Drop),
}

type CountKey = (i64, u8, i64, u8, i64, u8, i64);

struct Writer {
    db: Connection,
    codec: RecordCodec,
    option_ids: HashMap<(u8, String), i64>,
    /// (champion, role, patch) → (games, wins), all ranks: the baseline when no model gives one.
    pairs: HashMap<(i64, u8, i64), (u32, u32)>,
    validation: usize,
    kept: usize,
    samples: Vec<Vec<u8>>,
}

struct Reader {
    db: Connection,
    codec: RecordCodec,
}

pub struct Store {
    pub dir: PathBuf,
    pub db_path: PathBuf,
    pub opts: StoreOptions,
    w: Mutex<Writer>,
    r: Mutex<Reader>,
    dicts: Dictionaries,
    data: RwLock<Arc<StaticData>>,
    tier_a: RwLock<HashSet<i64>>,
    baseline: RwLock<Option<Arc<dyn Baseline>>>,
    drops: Mutex<BTreeMap<Drop, u64>>,
    log: Mutex<VecDeque<String>>,
    pub(crate) last_cleanup_freed: Mutex<Option<u64>>,
}

const LOG_LINES: usize = 300;

pub fn kind_code(k: OptionKind) -> u8 {
    OptionKind::ALL.iter().position(|x| *x == k).unwrap() as u8
}
pub fn kind_from(c: i64) -> Option<OptionKind> {
    OptionKind::ALL.get(c as usize).copied()
}
fn role_code(r: Role) -> u8 {
    r.index() as u8
}

/// FNV-1a: a stable hash of the match id (the validation sample must not change between runs or versions).
pub fn id_hash(s: &str) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in s.bytes() {
        h ^= b as u64;
        h = h.wrapping_mul(0x0100_0000_01b3);
    }
    h
}

impl Store {
    /// Opens (or creates) `dir/build-fetcher.db`.
    pub fn open(dir: &Path, data: Arc<StaticData>, opts: StoreOptions) -> Result<Store> {
        let db_path = dir.join("build-fetcher.db");
        let w = db::open(&db_path)?;
        let r = db::open(&db_path)?;
        let dicts = Dictionaries::new(&w);
        let codec = RecordCodec::new(dicts.current())?;
        let mut option_ids = HashMap::new();
        {
            let mut st = w.prepare("SELECT id, kind, key FROM options")?;
            for row in st.query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?, r.get::<_, String>(2)?)))? {
                let (id, kind, key) = row?;
                option_ids.insert((kind as u8, key), id);
            }
        }
        let mut pairs = HashMap::new();
        {
            let mut st = w.prepare("SELECT champion, role, patch, SUM(games), SUM(wins) FROM pairs GROUP BY champion, role, patch")?;
            for row in st.query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?, r.get::<_, i64>(2)?, r.get::<_, i64>(3)?, r.get::<_, i64>(4)?)))? {
                let (c, ro, p, g, wi) = row?;
                pairs.insert((c, ro as u8, p), (g as u32, wi as u32));
            }
        }
        let validation: i64 = w.query_row("SELECT COUNT(*) FROM validation", [], |r| r.get(0))?;
        let kept: i64 = w.query_row("SELECT COUNT(*) FROM matches", [], |r| r.get(0))?;
        let mut drops = BTreeMap::new();
        for d in Drop::ALL {
            if let Some(n) = meta_get(&w, &format!("drop.{}", d.as_str())).and_then(|v| v.parse().ok()) {
                drops.insert(d, n);
            }
        }
        let tier_a: HashSet<i64> = meta_get(&w, "tierA").and_then(|v| serde_json::from_str(&v).ok()).unwrap_or_default();
        Ok(Store {
            dir: dir.to_path_buf(),
            db_path,
            opts,
            w: Mutex::new(Writer { db: w, codec, option_ids, pairs, validation: validation as usize, kept: kept as usize, samples: vec![] }),
            r: Mutex::new(Reader { db: r, codec: RecordCodec::new(None)? }),
            dicts,
            data: RwLock::new(data),
            tier_a: RwLock::new(tier_a),
            baseline: RwLock::new(None),
            drops: Mutex::new(drops),
            log: Mutex::new(VecDeque::new()),
            last_cleanup_freed: Mutex::new(None),
        })
    }

    pub fn set_static_data(&self, data: Arc<StaticData>) {
        *self.data.write() = data;
    }
    pub fn static_data(&self) -> Arc<StaticData> {
        self.data.read().clone()
    }

    /// The integrator's hook: model win chances as the counts' baseline from now on (None: back to the
    /// champion-role's own rate). Counts already written keep the baseline they were written with.
    pub fn set_baseline(&self, b: Option<Arc<dyn Baseline>>) {
        *self.baseline.write() = b;
    }

    /// Tier A champions (records are kept for their games). Saved, so ingestion right after a restart knows them.
    pub fn set_tier_a(&self, champions: &HashSet<i64>) -> Result<()> {
        *self.tier_a.write() = champions.clone();
        let mut v: Vec<i64> = champions.iter().copied().collect();
        v.sort();
        meta_set(&self.w.lock().db, "tierA", &serde_json::to_string(&v)?)
    }
    pub fn tier_a(&self) -> HashSet<i64> {
        self.tier_a.read().clone()
    }

    pub fn log(&self, line: String) {
        let mut l = self.log.lock();
        if l.len() >= LOG_LINES {
            l.pop_front();
        }
        l.push_back(format!("{} {line}", bf_core::util::now_ms()));
    }
    /// The newest log lines (ingestion drops, clean-up, the fetcher's notes), oldest first.
    pub fn log_lines(&self) -> Vec<String> {
        self.log.lock().iter().cloned().collect()
    }

    /// Every drop so far by reason (kept across restarts).
    pub fn drops(&self) -> Vec<(String, u64)> {
        self.drops.lock().iter().map(|(d, n)| (d.as_str().to_string(), *n)).collect()
    }

    /// Was this match id processed already (counted, kept, in validation or dropped)?
    pub fn is_seen(&self, match_id: &str) -> Result<bool> {
        let r = self.r.lock();
        let found = r.db.prepare_cached("SELECT 1 FROM seen WHERE match_id = ?")?.query_row([match_id], |_| Ok(())).optional()?.is_some();
        Ok(found)
    }

    /// Match ids processed, by outcome: (counted, of which kept as records, in validation, dropped).
    pub fn seen_counts(&self) -> Result<(u64, u64, u64, u64)> {
        let r = self.r.lock();
        let mut out = (0, 0, 0, 0);
        let mut st = r.db.prepare("SELECT outcome, COUNT(*) FROM seen GROUP BY outcome")?;
        for row in st.query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?)))? {
            let (o, n) = row?;
            match o {
                0 => out.0 += n as u64,
                1 => {
                    out.0 += n as u64;
                    out.1 += n as u64
                }
                2 => out.2 += n as u64,
                _ => out.3 += n as u64,
            }
        }
        Ok(out)
    }

    /// Ingests answers in one transaction. Parsing runs on all cores first; the database work is sequential.
    pub fn ingest(&self, items: &[Ingest]) -> Result<Vec<Outcome>> {
        use rayon::prelude::*;
        let parsed: Vec<Result<Parsed, parse::Rejected>> = items.par_iter().map(|it| parse::parse(it.m, it.timeline, it.rank, it.me)).collect();
        let data = self.static_data();
        let tier_a = self.tier_a();
        let baseline = self.baseline.read().clone();
        let mut w = self.w.lock();
        let w = &mut *w;
        let tx = w.db.transaction()?;
        let mut outcomes = Vec::with_capacity(items.len());
        let mut counts: HashMap<CountKey, (u32, u32, f64)> = HashMap::new();
        let mut pair_agg: HashMap<(i64, u8, i64, u8), (u32, u32, i64)> = HashMap::new();
        let mut batch_ids: HashSet<String> = HashSet::new();
        let mut drops: Vec<(Drop, String)> = Vec::new();
        let mut champs_seen: Vec<(i64, u8, String, u8, u8, i64)> = Vec::new();
        {
            let mut is_seen = tx.prepare_cached("SELECT 1 FROM seen WHERE match_id = ?")?;
            let mut mark = tx.prepare_cached("INSERT OR IGNORE INTO seen(match_id, patch, outcome) VALUES(?, ?, ?)")?;
            for (it, p) in items.iter().zip(parsed) {
                let id = parse::match_id_of(it.m).map(String::from);
                if let Some(id) = &id {
                    if batch_ids.contains(id) || is_seen.query_row([id], |_| Ok(())).optional()?.is_some() {
                        outcomes.push(Outcome::Duplicate);
                        drops.push((Drop::Duplicate, id.clone()));
                        continue;
                    }
                    batch_ids.insert(id.clone());
                }
                let p = match p {
                    Ok(p) => p,
                    Err(e) => {
                        if let Some(id) = &id {
                            let patch = it.m["info"]["gameVersion"].as_str().and_then(Patch::parse).map(patch_code).unwrap_or(0);
                            mark.execute(params![id, patch, 3])?;
                        }
                        drops.push((e.reason, format!("{}: {}", id.as_deref().unwrap_or("?"), e.detail)));
                        outcomes.push(Outcome::Dropped(e.reason));
                        continue;
                    }
                };
                if let Some(t) = &p.timeline_dropped {
                    drops.push((Drop::Timeline, format!("{}: {}", p.record.match_id, t.detail)));
                }
                let r = p.record;
                let patch = patch_code(r.version.unwrap().patch);
                let mine = r.rank == Some(RankBand::Mine);
                let tl = r.has_timeline();
                // The held-out sample: timeline games, by a fixed hash of the id, never the user's own.
                let to_validation = tl && !mine && id_hash(&r.match_id) % self.opts.validation_one_in.max(1) == 0 && {
                    if w.validation < self.opts.validation_cap {
                        true
                    } else {
                        // Full: a game from a newer patch replaces the oldest-patch one, so the sample stays current.
                        let oldest: Option<(String, i64)> = tx.query_row("SELECT match_id, patch FROM validation ORDER BY patch, start_ms LIMIT 1", [], |x| Ok((x.get(0)?, x.get(1)?))).optional()?;
                        match oldest {
                            Some((old, op)) if op < patch => {
                                tx.execute("DELETE FROM validation WHERE match_id = ?", [&old])?;
                                w.validation -= 1;
                                true
                            }
                            _ => false,
                        }
                    }
                };
                if to_validation {
                    let blob = w.codec.pack(&r)?;
                    tx.execute("INSERT INTO validation(match_id, patch, start_ms, blob) VALUES(?, ?, ?, ?)", params![r.match_id, patch, r.start_ms, blob])?;
                    mark.execute(params![r.match_id, patch, 2])?;
                    w.validation += 1;
                    outcomes.push(Outcome::Validation);
                    continue;
                }
                // Counts, all ten players.
                let rank = rank_code(r.rank);
                for (i, pl) in r.players.iter().enumerate() {
                    let role = role_code(pl.role.unwrap());
                    let base = baseline.as_ref().and_then(|b| b.win_chance(&r, i)).unwrap_or_else(|| {
                        let (g, wi) = w.pairs.get(&(pl.champion, role, patch)).copied().unwrap_or((0, 0));
                        (wi as f64 + 0.5 * self.opts.baseline_prior_games) / (g as f64 + self.opts.baseline_prior_games)
                    });
                    let opponent = r.lane_opponent(i).map(|o| o.champion).unwrap_or(0);
                    let profile = profile_code(Some(enemy_profile(&r, i, &data))) as u8;
                    for o in player_options(pl, &data, tl) {
                        let oid = option_id(&tx, &mut w.option_ids, &o)?;
                        // The "any" total, the lane matchup and the enemy damage profile (a missing one isn't counted
                        // twice as "any").
                        let contexts = [(0, 0), (opponent, 0), (0, profile)];
                        for (k, (opp, prof)) in contexts.into_iter().enumerate() {
                            if k > 0 && (opp, prof) == (0, 0) {
                                continue;
                            }
                            let e = counts.entry((pl.champion, role, patch, rank, opp, prof, oid)).or_default();
                            e.0 += 1;
                            e.1 += pl.win as u32;
                            e.2 += base;
                        }
                    }
                    let e = pair_agg.entry((pl.champion, role, patch, rank)).or_default();
                    e.0 += 1;
                    e.1 += pl.win as u32;
                    e.2 = e.2.max(r.start_ms);
                    let g = w.pairs.entry((pl.champion, role, patch)).or_default();
                    g.0 += 1;
                    g.1 += pl.win as u32;
                    // Who plays thin and tier A pairs: where targeted sourcing looks.
                    if !mine && !p.puuids[i].is_empty() && (tier_a.contains(&pl.champion) || g.0 < self.opts.thin_games) {
                        champs_seen.push((pl.champion, role, p.puuids[i].clone(), platform_code(r.platform), rank, r.start_ms));
                    }
                }
                let keep = mine || tl || r.players.iter().any(|pl| tier_a.contains(&pl.champion));
                if keep {
                    let blob = w.codec.pack(&r)?;
                    tx.execute(
                        "INSERT INTO matches(match_id, patch, build, platform, rank, start_ms, has_timeline, blob) VALUES(?, ?, ?, ?, ?, ?, ?, ?)",
                        params![r.match_id, patch, r.version.unwrap().build, platform_code(r.platform), rank, r.start_ms, tl, blob],
                    )?;
                    w.kept += 1;
                    if w.codec.dictionary_id() == 0 && w.samples.len() < self.opts.dictionary_after {
                        w.samples.push(crate::record::encode(&r));
                    }
                }
                mark.execute(params![r.match_id, patch, keep as i64])?;
                outcomes.push(Outcome::Counted { kept: keep, timeline: tl });
            }
        }
        {
            let mut up = tx.prepare_cached(
                "INSERT INTO counts(champion, role, patch, rank, opponent, profile, option, games, wins, expected_wins) VALUES(?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
                 ON CONFLICT DO UPDATE SET games = games + excluded.games, wins = wins + excluded.wins, expected_wins = expected_wins + excluded.expected_wins",
            )?;
            for (k, v) in &counts {
                up.execute(params![k.0, k.1, k.2, k.3, k.4, k.5, k.6, v.0, v.1, v.2])?;
            }
            let mut pu = tx.prepare_cached(
                "INSERT INTO pairs(champion, role, patch, rank, games, wins, newest_ms) VALUES(?, ?, ?, ?, ?, ?, ?)
                 ON CONFLICT DO UPDATE SET games = games + excluded.games, wins = wins + excluded.wins, newest_ms = MAX(newest_ms, excluded.newest_ms)",
            )?;
            for (k, v) in &pair_agg {
                pu.execute(params![k.0, k.1, k.2, k.3, v.0, v.1, v.2])?;
            }
            let mut pc = tx.prepare_cached(
                "INSERT INTO player_champs(champion, role, puuid, platform, rank, games, last_ms) VALUES(?, ?, ?, ?, ?, 1, ?)
                 ON CONFLICT DO UPDATE SET games = games + 1, last_ms = MAX(last_ms, excluded.last_ms), rank = MIN(rank, excluded.rank)",
            )?;
            for c in &champs_seen {
                pc.execute(params![c.0, c.1, c.2, c.3, c.4, c.5])?;
            }
        }
        // Drop counters, saved with the data they describe.
        let mut totals = self.drops.lock().clone();
        for (d, _) in &drops {
            *totals.entry(*d).or_default() += 1;
        }
        for (d, n) in &totals {
            tx.execute("INSERT INTO meta(key, value) VALUES(?, ?) ON CONFLICT(key) DO UPDATE SET value = excluded.value", params![format!("drop.{}", d.as_str()), n.to_string()])?;
        }
        tx.commit()?;
        *self.drops.lock() = totals;
        for (d, detail) in drops {
            if d != Drop::Duplicate {
                self.log(format!("dropped {} ({detail})", d.as_str()));
            }
        }
        // Enough records to make the dictionary every later record is packed with.
        if w.codec.dictionary_id() == 0 && w.samples.len() >= self.opts.dictionary_after {
            let samples: Vec<&[u8]> = w.samples.iter().map(|s| s.as_slice()).collect();
            let d = self.dicts.create(&w.db, &samples, &format!("first {} records", samples.len()))?;
            w.codec = RecordCodec::new(Some(d))?;
            w.samples.clear();
        }
        Ok(outcomes)
    }

    /// Records the sourcing pool: players from a ladder refreshed at `now_ms`.
    pub fn add_players(&self, platform: bf_core::build::Platform, rank: RankBand, players: &[(String, i64)], now_ms: i64) -> Result<usize> {
        let mut w = self.w.lock();
        let tx = w.db.transaction()?;
        let mut n = 0;
        {
            let mut st = tx.prepare_cached(
                "INSERT INTO players(puuid, platform, rank, lp) VALUES(?, ?, ?, ?) ON CONFLICT DO UPDATE SET rank = excluded.rank, lp = excluded.lp, platform = excluded.platform",
            )?;
            for (puuid, lp) in players.iter().filter(|p| !p.0.is_empty()) {
                n += st.execute(params![puuid, platform_code(Some(platform)), rank_code(Some(rank)), lp])?;
            }
        }
        tx.execute(
            "INSERT INTO meta(key, value) VALUES(?, ?) ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![format!("ladder.{}.{}", platform.as_str(), rank_code(Some(rank))), now_ms.to_string()],
        )?;
        tx.commit()?;
        Ok(n)
    }

    /// When a ladder was last read (None: never).
    pub fn ladder_refreshed(&self, platform: bf_core::build::Platform, rank: RankBand) -> Option<i64> {
        let r = self.r.lock();
        meta_get(&r.db, &format!("ladder.{}.{}", platform.as_str(), rank_code(Some(rank)))).and_then(|v| v.parse().ok())
    }

    /// Players in the pool not checked since `before_ms`, least recently checked first.
    pub fn due_players(&self, platforms: &[bf_core::build::Platform], rank: RankBand, before_ms: i64, limit: usize) -> Result<Vec<(String, bf_core::build::Platform, i64)>> {
        let r = self.r.lock();
        let codes: Vec<String> = platforms.iter().map(|p| platform_code(Some(*p)).to_string()).collect();
        let sql = format!(
            "SELECT puuid, platform, last_checked_ms FROM players WHERE rank = ? AND last_checked_ms < ? AND platform IN ({}) ORDER BY last_checked_ms, lp DESC LIMIT ?",
            if codes.is_empty() { "-1".to_string() } else { codes.join(",") }
        );
        let mut st = r.db.prepare(&sql)?;
        let rows = st.query_map(params![rank_code(Some(rank)), before_ms, limit as i64], |x| Ok((x.get::<_, String>(0)?, x.get::<_, i64>(1)?, x.get::<_, i64>(2)?)))?;
        Ok(rows.filter_map(|x| x.ok()).filter_map(|(p, c, t)| crate::record::platform_from(c as u64).map(|pl| (p, pl, t))).collect())
    }

    pub fn pool_size(&self, rank: RankBand) -> Result<u64> {
        let r = self.r.lock();
        Ok(r.db.query_row("SELECT COUNT(*) FROM players WHERE rank = ?", [rank_code(Some(rank))], |x| x.get::<_, i64>(0))? as u64)
    }

    /// Players seen on a champion-role (rank band of the game they were found in), least recently checked first.
    pub fn players_for(&self, champion: i64, role: Role, rank: RankBand, platforms: &[bf_core::build::Platform], before_ms: i64, limit: usize) -> Result<Vec<(String, bf_core::build::Platform)>> {
        let r = self.r.lock();
        let codes: Vec<String> = platforms.iter().map(|p| platform_code(Some(*p)).to_string()).collect();
        let sql = format!(
            "SELECT c.puuid, c.platform FROM player_champs c LEFT JOIN players p ON p.puuid = c.puuid
             WHERE c.champion = ? AND c.role = ? AND c.rank = ? AND c.platform IN ({}) AND COALESCE(p.last_checked_ms, 0) < ?
             ORDER BY COALESCE(p.last_checked_ms, 0), c.games DESC LIMIT ?",
            if codes.is_empty() { "-1".to_string() } else { codes.join(",") }
        );
        let mut st = r.db.prepare(&sql)?;
        let rows = st.query_map(params![champion, role_code(role), rank_code(Some(rank)), before_ms, limit as i64], |x| Ok((x.get::<_, String>(0)?, x.get::<_, i64>(1)?)))?;
        Ok(rows.filter_map(|x| x.ok()).filter_map(|(p, c)| crate::record::platform_from(c as u64).map(|pl| (p, pl))).collect())
    }

    /// The fetcher checked this player's history now (players from player_champs join the pool here).
    pub fn mark_checked(&self, puuid: &str, platform: bf_core::build::Platform, rank: RankBand, now_ms: i64) -> Result<()> {
        let w = self.w.lock();
        w.db.prepare_cached(
            "INSERT INTO players(puuid, platform, rank, last_checked_ms) VALUES(?, ?, ?, ?) ON CONFLICT DO UPDATE SET last_checked_ms = excluded.last_checked_ms",
        )?
        .execute(params![puuid, platform_code(Some(platform)), rank_code(Some(rank)), now_ms])?;
        Ok(())
    }

    /// Games of a champion-role on a patch in a rank band (None: every band).
    pub fn pair_games(&self, champion: i64, role: Role, patch: Patch, rank: Option<RankBand>) -> Result<u32> {
        let r = self.r.lock();
        let n: i64 = match rank {
            Some(rk) => r.db.query_row("SELECT COALESCE(SUM(games), 0) FROM pairs WHERE champion = ? AND role = ? AND patch = ? AND rank = ?", params![champion, role_code(role), patch_code(patch), rank_code(Some(rk))], |x| x.get(0))?,
            None => r.db.query_row("SELECT COALESCE(SUM(games), 0) FROM pairs WHERE champion = ? AND role = ? AND patch = ?", params![champion, role_code(role), patch_code(patch)], |x| x.get(0))?,
        };
        Ok(n as u32)
    }

    /// Games on a patch in a rank band (each game counts ten player rows in `pairs`).
    pub fn patch_games(&self, patch: Patch, rank: RankBand) -> Result<u32> {
        let r = self.r.lock();
        let n: i64 = r.db.query_row("SELECT COALESCE(SUM(games), 0) FROM pairs WHERE patch = ? AND rank = ?", params![patch_code(patch), rank_code(Some(rank))], |x| x.get(0))?;
        Ok((n / 10) as u32)
    }

    /// The user's own games: (champion, role, patch) of the user in each (from the kept Mine records).
    pub fn my_games(&self) -> Result<Vec<(i64, Role, Patch, i64)>> {
        let mut out = Vec::new();
        let rows: Vec<Vec<u8>> = {
            let r = self.r.lock();
            let mut st = r.db.prepare("SELECT blob FROM matches WHERE rank = ? ORDER BY start_ms")?;
            let v = st.query_map([rank_code(Some(RankBand::Mine))], |x| x.get(0))?.filter_map(|x| x.ok()).collect();
            v
        };
        for b in rows {
            let rec = self.unpack(&b)?;
            if let Some(p) = rec.players.iter().find(|p| p.is_me) {
                if let (Some(role), Some(v)) = (p.role, rec.version) {
                    out.push((p.champion, role, v.patch, rec.start_ms));
                }
            }
        }
        Ok(out)
    }

    /// The newest game start of the user's own stored games (where the next "my games" fetch starts).
    pub fn newest_mine_ms(&self) -> Result<Option<i64>> {
        let r = self.r.lock();
        Ok(r.db.query_row("SELECT MAX(start_ms) FROM matches WHERE rank = ?", [rank_code(Some(RankBand::Mine))], |x| x.get(0))?)
    }

    pub(crate) fn unpack(&self, blob: &[u8]) -> Result<MatchRecord> {
        let mut r = self.r.lock();
        let Reader { db, codec } = &mut *r;
        let snap = self.dicts.snapshot();
        let missing = !snap.contains_key(&bf_core::codec::dictionary_of(blob)) && bf_core::codec::dictionary_of(blob) != 0;
        if missing {
            self.dicts.reload(db);
        }
        let snap = self.dicts.snapshot();
        codec.unpack(blob, &|id| snap.get(&id).cloned())
    }

    /// Runs `f` on the writer connection (clean-up).
    pub(crate) fn with_writer<T>(&self, f: impl FnOnce(&mut Connection) -> Result<T>) -> Result<T> {
        let mut w = self.w.lock();
        let out = f(&mut w.db);
        // Validation and record counts may have changed.
        if let Ok(n) = w.db.query_row("SELECT COUNT(*) FROM validation", [], |r| r.get::<_, i64>(0)) {
            w.validation = n as usize;
        }
        if let Ok(n) = w.db.query_row("SELECT COUNT(*) FROM matches", [], |r| r.get::<_, i64>(0)) {
            w.kept = n as usize;
        }
        let pairs_left: HashSet<i64> = w.db.prepare("SELECT DISTINCT patch FROM pairs").and_then(|mut s| s.query_map([], |r| r.get(0))?.collect()).unwrap_or_default();
        w.pairs.retain(|k, _| pairs_left.contains(&k.2));
        out
    }

    pub(crate) fn with_reader<T>(&self, f: impl FnOnce(&Connection) -> Result<T>) -> Result<T> {
        let r = self.r.lock();
        f(&r.db)
    }
}

fn option_id(tx: &Connection, cache: &mut HashMap<(u8, String), i64>, o: &BuildOption) -> Result<i64> {
    let k = (kind_code(o.kind), o.key.clone());
    if let Some(id) = cache.get(&k) {
        return Ok(*id);
    }
    tx.prepare_cached("INSERT OR IGNORE INTO options(kind, key) VALUES(?, ?)")?.execute(params![k.0, k.1])?;
    let id: i64 = tx.prepare_cached("SELECT id FROM options WHERE kind = ? AND key = ?")?.query_row(params![k.0, k.1], |r| r.get(0))?;
    cache.insert(k, id);
    Ok(id)
}

// ------------------------------------------------------------------------------------------------ DataSource

impl DataSource for Store {
    fn counts(&self, q: &CountQuery) -> Result<Vec<OptionCounts>> {
        let r = self.r.lock();
        let patches: Vec<String> = q.patches.iter().map(|p| patch_code(*p).to_string()).collect();
        let kinds: Vec<String> = q.kinds.iter().map(|k| kind_code(*k).to_string()).collect();
        let mut sql = String::from(
            "SELECT c.patch, c.rank, c.opponent, c.profile, o.kind, o.key, c.games, c.wins, c.expected_wins
             FROM counts c JOIN options o ON o.id = c.option WHERE c.champion = ? AND c.role = ?",
        );
        if !patches.is_empty() {
            sql += &format!(" AND c.patch IN ({})", patches.join(","));
        }
        if !kinds.is_empty() {
            sql += &format!(" AND o.kind IN ({})", kinds.join(","));
        }
        sql += " ORDER BY c.patch DESC, o.kind, c.games DESC";
        let mut st = r.db.prepare(&sql)?;
        let rows = st.query_map(params![q.champion, role_code(q.role)], |x| {
            Ok((x.get::<_, i64>(0)?, x.get::<_, i64>(1)?, x.get::<_, i64>(2)?, x.get::<_, i64>(3)?, x.get::<_, i64>(4)?, x.get::<_, String>(5)?, x.get::<_, i64>(6)?, x.get::<_, i64>(7)?, x.get::<_, f64>(8)?))
        })?;
        let mut out = Vec::new();
        for row in rows {
            let (patch, rank, opp, prof, kind, key, games, wins, exp) = row?;
            let (Some(rank), Some(kind)) = (rank_from(rank as u64), kind_from(kind)) else { continue };
            out.push(OptionCounts {
                patch: patch_from(patch),
                rank,
                context: CountContext { lane_opponent: (opp != 0).then_some(opp), enemy_profile: profile_from(prof) },
                option: BuildOption { kind, key },
                games: games as u32,
                wins: wins as u32,
                expected_wins: exp,
            });
        }
        Ok(out)
    }

    fn volumes(&self, patches: &[Patch]) -> Result<Vec<PairVolume>> {
        let r = self.r.lock();
        let mut sql = String::from("SELECT champion, role, patch, SUM(games), SUM(wins), MAX(newest_ms) FROM pairs");
        if !patches.is_empty() {
            sql += &format!(" WHERE patch IN ({})", patches.iter().map(|p| patch_code(*p).to_string()).collect::<Vec<_>>().join(","));
        }
        sql += " GROUP BY champion, role, patch ORDER BY patch DESC, champion, role";
        let mut st = r.db.prepare(&sql)?;
        let rows = st.query_map([], |x| Ok((x.get::<_, i64>(0)?, x.get::<_, i64>(1)?, x.get::<_, i64>(2)?, x.get::<_, i64>(3)?, x.get::<_, i64>(4)?, x.get::<_, i64>(5)?)))?;
        let mut out = Vec::new();
        for row in rows {
            let (c, role, patch, g, wi, newest) = row?;
            let Some(role) = Role::ALL.get(role as usize).copied() else { continue };
            out.push(PairVolume { champion: c, role, patch: patch_from(patch), games: g as u32, wins: wi as u32, newest_ms: newest });
        }
        Ok(out)
    }

    fn records_after(&self, after_seq: u64, limit: usize) -> Result<Vec<(u64, MatchRecord)>> {
        let rows: Vec<(i64, Vec<u8>)> = {
            let r = self.r.lock();
            let mut st = r.db.prepare_cached("SELECT seq, blob FROM matches WHERE seq > ? ORDER BY seq LIMIT ?")?;
            let v = st.query_map(params![after_seq as i64, limit as i64], |x| Ok((x.get(0)?, x.get(1)?)))?.collect::<rusqlite::Result<Vec<_>>>()?;
            v
        };
        rows.into_iter().map(|(seq, b)| Ok((seq as u64, self.unpack(&b).map_err(|e| anyhow!("record {seq}: {e}"))?))).collect()
    }

    fn validation_records(&self) -> Result<Vec<MatchRecord>> {
        let rows: Vec<Vec<u8>> = {
            let r = self.r.lock();
            let mut st = r.db.prepare("SELECT blob FROM validation ORDER BY match_id")?;
            let v = st.query_map([], |x| x.get(0))?.collect::<rusqlite::Result<Vec<_>>>()?;
            v
        };
        rows.iter().map(|b| self.unpack(b)).collect()
    }

    fn current_patch(&self) -> Result<Option<Patch>> {
        let r = self.r.lock();
        let p: Option<i64> = r.db.query_row("SELECT MAX(patch) FROM pairs", [], |x| x.get(0))?;
        Ok(p.map(patch_from))
    }
}
