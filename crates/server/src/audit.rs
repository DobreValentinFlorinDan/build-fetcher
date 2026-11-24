//! Port of `server/audit.ts`: the data audit. Strictly read-only (the database opened read-only, cache entries only
//! read, no Riot API call): what the stored data covers and what sourcing would add, on this data folder.

use irelia::capture::{capture_timeline, lane_opponents, leavers, match_quality, participant_rows};
use irelia::db::IreliaGameRow;
use irelia::ddragon::{build_static_data, compare_patch, to_patch, RawStaticData, StaticData};
use irelia::engine::exclusivity::build_exclusivity_index;
use irelia::engine::recommend::{recommend, EngineSettings};
use irelia::patches::{region_patch, Fallback, RegionPatch, VersionRow};
use irelia::riot::disk_cache::CacheStore;
use irelia::riot::routing::region_of;
use rusqlite::{Connection, OpenFlags};
use serde_json::{json, Map, Value};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::Arc;

const DAY: f64 = 24.0 * 3_600_000.0;
pub const DEV_KEY_CALLS_PER_DAY: f64 = 90.0 * 30.0 * 24.0;
const HARVEST_POSITIONS: [&str; 2] = ["TOP", "MIDDLE"];
pub const AUDIT_STEPS: [&str; 6] = ["Reading players", "Reading matches and games", "Listing the API cache", "Reading cached matches", "Reading cached timelines", "Measuring the build impact"];

fn median(xs: &[f64]) -> f64 {
    if xs.is_empty() {
        return 0.0;
    }
    let mut s = xs.to_vec();
    s.sort_by(|a, b| a.partial_cmp(b).unwrap());
    s[s.len() / 2]
}
fn quantile(xs: &[f64], q: f64) -> f64 {
    if xs.is_empty() {
        return 0.0;
    }
    let mut s = xs.to_vec();
    s.sort_by(|a, b| a.partial_cmp(b).unwrap());
    s[((s.len() as f64 * q).floor() as usize).min(s.len() - 1)]
}
fn share(a: f64, b: f64) -> f64 {
    if b != 0.0 {
        a / b
    } else {
        0.0
    }
}
fn round(x: f64, d: i32) -> f64 {
    let p = 10f64.powi(d);
    irelia::js::round(x * p) / p
}
fn n(x: f64) -> Value {
    irelia::js::num(x)
}

enum Cached {
    File(PathBuf),
    Entry(String, String, String),
}

fn list_cache(root: &Path, kind: &str, store: Option<&CacheStore>) -> indexmap::IndexMap<String, Cached> {
    let mut out = indexmap::IndexMap::new();
    if let Ok(rd) = std::fs::read_dir(root.join(kind)) {
        for scope in rd.flatten() {
            if let Ok(files) = std::fs::read_dir(scope.path()) {
                for f in files.flatten() {
                    let name = f.file_name().to_string_lossy().to_string();
                    if let Some(key) = name.strip_suffix(".json.gz") {
                        out.insert(format!("{}/{}", scope.file_name().to_string_lossy(), key), Cached::File(f.path()));
                    }
                }
            }
        }
    }
    if let Some(s) = store {
        if let Ok(mut st) = s.db.prepare("SELECT scope, key FROM entries WHERE kind = ?") {
            let rows: Vec<(String, String)> = st.query_map([kind], |r| Ok((r.get(0)?, r.get(1)?))).map(|it| it.filter_map(|x| x.ok()).collect()).unwrap_or_default();
            for (scope, key) in rows {
                out.insert(format!("{scope}/{key}"), Cached::Entry(kind.into(), scope, key));
            }
        }
    }
    out
}

fn read_cached(c: &Cached, store: Option<&CacheStore>) -> Option<Value> {
    match c {
        Cached::Entry(kind, scope, key) => store?.read(kind, scope, key).ok().flatten().and_then(|(_, raw)| serde_json::from_slice(&raw).ok()),
        Cached::File(p) => {
            let b = std::fs::read(p).ok()?;
            let mut s = String::new();
            flate2::read::GzDecoder::new(&b[..]).read_to_string(&mut s).ok()?;
            serde_json::from_str(&s).ok()
        }
    }
}

fn row_bytes(m: &Map<String, Value>) -> f64 {
    m.values().fold(12.0, |n, v| {
        n + match v {
            Value::String(s) => s.encode_utf16().count() as f64 + 2.0,
            Value::Null => 1.0,
            _ => 5.0,
        }
    })
}

fn load_cached_static(dir: &Path, version: &str, previous_patch: &str) -> Option<StaticData> {
    let read = |f: &str| -> Option<Value> { serde_json::from_str(&std::fs::read_to_string(dir.join(f)).ok()?).ok() };
    let raw = RawStaticData { item: read("item.json")?, champion: read("champion.json")?, runes_reforged: read("runesReforged.json")?, summoner: read("summoner.json")?, champ: read("Irelia.json")? };
    build_static_data(version, previous_patch, &raw, None).ok()
}

/// Runs the audit for the job thread: the report saved under the settings' audits folder; (text, file) as JSON.
pub fn run_audit_job(data_dir: &Path, progress: &dyn Fn(Value), stop: &dyn Fn() -> bool) -> anyhow::Result<Value> {
    let report = audit_data(data_dir, irelia::util::now_ms() as f64, 39, &|step, part| {
        if stop() {
            std::panic::panic_any(irelia::model::evaluate::EvalStopped);
        }
        let mut p = json!({ "step": step, "steps": AUDIT_STEPS.len(), "label": AUDIT_STEPS[step - 1] });
        if let Some(x) = part {
            p["part"] = json!(x);
        }
        progress(p);
    })?;
    let text = format_audit(&report);
    let dir = irelia::app_config::app_config_dir().join("audits");
    std::fs::create_dir_all(&dir)?;
    let stamp = chrono::Utc::now().format("%Y-%m-%d-%H-%M").to_string();
    let file = dir.join(format!("audit-{stamp}"));
    std::fs::write(file.with_extension("json"), format!("{}\n", serde_json::to_string_pretty(&report)?))?;
    let txt = PathBuf::from(format!("{}.txt", file.to_string_lossy()));
    std::fs::write(&txt, format!("{text}\n"))?;
    Ok(json!({ "text": text, "file": txt.to_string_lossy() }))
}

pub fn audit_data(data_dir: &Path, now: f64, champion_id: i64, progress: &dyn Fn(usize, Option<f64>)) -> anyhow::Result<Value> {
    let db_file = data_dir.join("irelia.db");
    if !db_file.exists() {
        anyhow::bail!("No irelia.db in {}", data_dir.display());
    }
    let db = Connection::open_with_flags(&db_file, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    let columns = |table: &str| -> HashSet<String> {
        db.prepare(&format!("PRAGMA table_info({table})")).and_then(|mut st| st.query_map([], |r| r.get::<_, String>(1)).map(|it| it.filter_map(|x| x.ok()).collect())).unwrap_or_default()
    };
    let player_cols = columns("players");
    let game_cols = columns("irelia_games");
    let schema_version: f64 = db.query_row("SELECT value FROM meta WHERE key = 'schema_version'", [], |r| r.get::<_, String>(0)).ok().and_then(|v| v.parse().ok()).unwrap_or(2.0);

    let mut versions: Vec<String> = std::fs::read_dir(data_dir.join("ddragon")).map(|rd| rd.flatten().map(|e| e.file_name().to_string_lossy().to_string()).collect()).unwrap_or_default();
    versions.sort_by(|a, b| irelia::engine::recommend::js_order(compare_patch(b, a)));
    let dd_patch = versions.first().map(|v| to_patch(v));
    let dd_prev = versions.iter().map(|v| to_patch(v)).find(|p| Some(p) != dd_patch.as_ref());

    progress(1, None);
    let has_class = player_cols.contains("pilot_class");
    let by_pt: Vec<Value> = {
        let sql = format!(
            "SELECT platform, tier, COUNT(*) AS players, SUM(is_otp IS NOT NULL) AS checked, COALESCE(SUM(is_otp = 1), 0) AS otps,
        {} AS mains FROM players GROUP BY platform, tier ORDER BY platform, players DESC",
            if has_class { "COALESCE(SUM(pilot_class = 'main'), 0)" } else { "0" }
        );
        let mut st = db.prepare(&sql)?;
        let rows = st.query_map([], |r| {
            Ok(json!({ "platform": r.get::<_, String>(0)?, "tier": r.get::<_, String>(1)?, "players": r.get::<_, i64>(2)?, "checked": r.get::<_, Option<i64>>(3)?.unwrap_or(0), "otps": r.get::<_, i64>(4)?, "mains": r.get::<_, i64>(5)? }))
        })?;
        rows.filter_map(|x| x.ok()).collect()
    };
    let pilot_set: indexmap::IndexMap<String, String> = {
        let sql = format!("SELECT puuid, platform FROM players WHERE is_otp = 1 {}", if has_class { "OR pilot_class IN ('otp', 'main')" } else { "" });
        let mut st = db.prepare(&sql)?;
        let rows = st.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?;
        rows.filter_map(|x| x.ok()).collect()
    };

    progress(2, None);
    struct MatchR {
        id: String,
        platform: String,
        patch: String,
        version: String,
        creation: f64,
    }
    let match_rows: Vec<MatchR> = {
        let mut st = db.prepare("SELECT match_id, platform, patch, game_version, game_creation FROM matches")?;
        let rows = st.query_map([], |r| Ok(MatchR { id: r.get(0)?, platform: r.get(1)?, patch: r.get(2)?, version: r.get(3)?, creation: r.get::<_, i64>(4)? as f64 }))?;
        rows.filter_map(|x| x.ok()).collect()
    };
    struct GameR {
        id: String,
        puuid: String,
        patch: String,
        position: String,
        opponent: Option<i64>,
        source: String,
    }
    let games: Vec<GameR> = {
        let sql = format!(
            "SELECT match_id, puuid, patch, position, opponent_champion_id, {} AS source FROM irelia_games",
            if game_cols.contains("pilot_source") { "COALESCE(pilot_source, 'otp')" } else { "'otp'" }
        );
        let mut st = db.prepare(&sql)?;
        let rows = st.query_map([], |r| Ok(GameR { id: r.get(0)?, puuid: r.get(1)?, patch: r.get(2)?, position: r.get(3)?, opponent: r.get(4)?, source: r.get(5)? }))?;
        rows.filter_map(|x| x.ok()).collect()
    };
    let game_by_id: HashMap<&str, &GameR> = games.iter().map(|g| (g.id.as_str(), g)).collect();
    let mut version_map: indexmap::IndexMap<String, VersionRow> = indexmap::IndexMap::new();
    for r in &match_rows {
        let v = version_map.entry(format!("{}|{}", r.platform, r.version)).or_insert(VersionRow {
            platform: r.platform.clone(),
            game_version: r.version.clone(),
            patch: r.patch.clone(),
            first_seen: r.creation as i64,
            last_seen: r.creation as i64,
            games: 0,
        });
        v.first_seen = v.first_seen.min(r.creation as i64);
        v.last_seen = v.last_seen.max(r.creation as i64);
        v.games += 1;
    }
    let version_rows: Vec<VersionRow> = version_map.into_values().collect();
    let fallback = Fallback { patch: dd_patch.clone().unwrap_or_else(|| "0.0".into()), previous_patch: dd_prev.clone().unwrap_or_else(|| "0.0".into()) };
    let mut platforms: Vec<String> = match_rows.iter().map(|r| r.platform.clone()).collect::<HashSet<_>>().into_iter().collect();
    platforms.sort();
    let regions: HashMap<String, RegionPatch> = platforms.iter().map(|p| (p.clone(), region_patch(p, &version_rows, &fallback, now as i64))).collect();

    let mut by_patch: indexmap::IndexMap<String, [f64; 5]> = indexmap::IndexMap::new();
    for r in &match_rows {
        let e = by_patch.entry(r.patch.clone()).or_insert([0.0; 5]);
        e[0] += 1.0;
        if let Some(g) = game_by_id.get(r.id.as_str()) {
            e[1] += 1.0;
            match g.position.as_str() {
                "TOP" => e[2] += 1.0,
                "MIDDLE" => e[3] += 1.0,
                _ => e[4] += 1.0,
            }
        }
    }
    let by_platform: Vec<Value> = platforms
        .iter()
        .map(|p| {
            let cur = &regions[p].patch;
            let ms: Vec<&MatchR> = match_rows.iter().filter(|r| &r.platform == p).collect();
            json!({
                "platform": p, "matches": ms.len(), "ireliaGames": ms.iter().filter(|r| game_by_id.contains_key(r.id.as_str())).count(), "currentPatch": cur,
                "currentPatchMatches": ms.iter().filter(|r| &r.patch == cur).count(),
                "currentPatchIrelia": ms.iter().filter(|r| &r.patch == cur && game_by_id.contains_key(r.id.as_str())).count(),
            })
        })
        .collect();

    let build_games: Vec<&GameR> = games.iter().filter(|g| g.source != "me").collect();
    let mut per_pilot: indexmap::IndexMap<&str, f64> = indexmap::IndexMap::new();
    for g in &build_games {
        *per_pilot.entry(g.puuid.as_str()).or_insert(0.0) += 1.0;
    }
    let counts: Vec<f64> = per_pilot.values().copied().collect();
    let top_share = |gs: &[&&GameR]| -> (usize, f64) {
        let mut m: HashMap<&str, f64> = HashMap::new();
        for g in gs {
            *m.entry(g.puuid.as_str()).or_insert(0.0) += 1.0;
        }
        let mut v: Vec<f64> = m.values().copied().collect();
        v.sort_by(|a, b| b.partial_cmp(a).unwrap());
        (m.len(), share(v.iter().take(10).sum(), gs.len() as f64))
    };
    let lanes = ["TOP", "MIDDLE"];
    let mut by_lane = Map::new();
    for l in lanes {
        let gs: Vec<&&GameR> = build_games.iter().filter(|g| g.position == l).collect();
        let (pilots, t) = top_share(&gs);
        by_lane.insert(l.into(), json!({ "games": gs.len(), "pilots": pilots, "top10Share": n(t) }));
    }
    let mut by_source = Map::new();
    for g in &games {
        let e = by_source.entry(g.source.clone()).or_insert(json!(0));
        *e = json!(e.as_i64().unwrap() + 1);
    }
    let mut matchups = Map::new();
    for l in lanes {
        let gs: Vec<&&GameR> = build_games.iter().filter(|g| g.position == l && regions.get(g.id.split('_').next().unwrap_or("")).map(|r| r.patch == g.patch).unwrap_or(false)).collect();
        let mut m: indexmap::IndexMap<i64, f64> = indexmap::IndexMap::new();
        for g in &gs {
            if let Some(o) = g.opponent.filter(|o| *o != 0) {
                *m.entry(o).or_insert(0.0) += 1.0;
            }
        }
        let in15: f64 = m.values().filter(|n| **n >= 15.0).sum();
        matchups.insert(l.into(), json!({ "games": gs.len(), "opponents": m.len(), "gamesInMatchupsWith15": in15 as i64, "share15": n(round(share(in15, gs.len() as f64), 3)) }));
    }

    progress(3, None);
    let cache_root = data_dir.join("cache");
    let store = CacheStore::open_default(&cache_root).ok();
    let match_files = list_cache(&cache_root, "match", store.as_ref());
    let timeline_files = list_cache(&cache_root, "timeline", store.as_ref());
    let mastery_files = list_cache(&cache_root, "mastery", store.as_ref());
    let in_db: HashSet<String> = match_rows.iter().map(|r| format!("{}/{}", r.platform, r.id)).collect();
    let cache = json!({
        "matchFiles": match_files.len(), "timelineFiles": timeline_files.len(), "masteryFiles": mastery_files.len(),
        "matchesWithoutFile": match_rows.iter().filter(|r| !match_files.contains_key(&format!("{}/{}", r.platform, r.id))).count(),
        "gamesWithoutTimeline": games.iter().filter(|g| !timeline_files.contains_key(&format!("{}/{}", g.id.split('_').next().unwrap_or(""), g.id))).count(),
        "matchFilesNotInDb": match_files.keys().filter(|k| !in_db.contains(*k)).count(),
        "timelineFilesNotInDb": timeline_files.keys().filter(|k| !game_by_id.contains_key(k.split('/').nth(1).unwrap_or(""))).count(),
    });

    let mut remake_of: HashMap<String, bool> = HashMap::new();
    let mut leaver_of: HashMap<String, bool> = HashMap::new();
    let mut timeline_opp_of: HashMap<String, Option<i64>> = HashMap::new();
    let (mut matches_read, mut remakes, mut early, mut short, mut irelia_remakes, mut irelia_leavers, mut timelines_read) = (0, 0, 0, 0, 0, 0, 0);
    let mut end_results: indexmap::IndexMap<String, i64> = indexmap::IndexMap::new();
    let (mut h_other, mut h_current, mut h_current_ok) = (0, 0, 0);
    let mut h_by_lane: indexmap::IndexMap<String, i64> = indexmap::IndexMap::new();
    let mut harvest_players: HashSet<String> = HashSet::new();
    let mut pilot_matches: indexmap::IndexMap<String, Vec<f64>> = indexmap::IndexMap::new();
    let mut pilot_irelia: HashMap<String, f64> = HashMap::new();
    let (mut row_sum, mut row_count) = (0.0, 0.0);
    let rows: Vec<&MatchR> = match_rows.iter().filter(|r| match_files.contains_key(&format!("{}/{}", r.platform, r.id))).collect();
    progress(4, Some(0.0));
    for (k, r) in rows.iter().enumerate() {
        let m = read_cached(&match_files[&format!("{}/{}", r.platform, r.id)], store.as_ref());
        if (k + 1) % 500 == 0 {
            progress(4, Some((k + 1) as f64 / rows.len() as f64));
        }
        let Some(m) = m.filter(|m| m["info"]["participants"].is_array()) else { continue };
        matches_read += 1;
        let q = match_quality(&m);
        if q.remake {
            remakes += 1;
        }
        let ps = m["info"]["participants"].as_array().unwrap();
        if ps.iter().any(|p| p["gameEndedInEarlySurrender"] == true) {
            early += 1;
        }
        if m["info"]["gameDuration"].as_f64().unwrap_or(0.0) < 300.0 {
            short += 1;
        }
        *end_results.entry(q.end_result.clone().unwrap_or_else(|| "(not sent)".into())).or_insert(0) += 1;
        let current = regions.get(&r.platform).map(|x| x.patch == r.patch).unwrap_or(false);
        for row in participant_rows(&m) {
            row_sum += row_bytes(&row.to_map());
            row_count += 1.0;
        }
        let irelia = ps.iter().find(|p| p["championId"].as_i64() == Some(champion_id));
        let stored = game_by_id.get(r.id.as_str());
        if stored.is_some() {
            remake_of.insert(r.id.clone(), q.remake);
            if q.remake {
                irelia_remakes += 1;
            }
        }
        if let Some(ir) = irelia {
            let pu = ir["puuid"].as_str().unwrap_or("");
            let pos = ir["teamPosition"].as_str().unwrap_or("");
            if stored.is_none() && !pilot_set.contains_key(pu) && HARVEST_POSITIONS.contains(&pos) {
                h_other += 1;
                *h_by_lane.entry(pos.into()).or_insert(0) += 1;
                harvest_players.insert(pu.into());
                if current {
                    h_current += 1;
                    if !q.remake {
                        h_current_ok += 1;
                    }
                }
            }
        }
        if current {
            for p in ps {
                let pu = p["puuid"].as_str().unwrap_or("");
                if !pilot_set.contains_key(pu) {
                    continue;
                }
                pilot_matches.entry(pu.into()).or_default().push(m["info"]["gameCreation"].as_f64().unwrap_or(0.0));
                if stored.map(|s| s.puuid == pu).unwrap_or(false) {
                    *pilot_irelia.entry(pu.into()).or_insert(0.0) += 1.0;
                }
            }
        }
    }

    progress(5, None);
    let (mut lo_with, mut lo_compared, mut lo_disagree, mut lo_no_pos) = (0, 0, 0, 0);
    let (mut blob_sum, mut blobs) = (0.0, 0.0);
    let with_tl: Vec<&GameR> = games
        .iter()
        .filter(|g| {
            let p = g.id.split('_').next().unwrap_or("");
            timeline_files.contains_key(&format!("{p}/{}", g.id)) && match_files.contains_key(&format!("{p}/{}", g.id))
        })
        .collect();
    for (k, g) in with_tl.iter().enumerate() {
        let p = g.id.split('_').next().unwrap_or("");
        let m = read_cached(&match_files[&format!("{p}/{}", g.id)], store.as_ref());
        let t = read_cached(&timeline_files[&format!("{p}/{}", g.id)], store.as_ref());
        if (k + 1) % 200 == 0 {
            progress(5, Some((k + 1) as f64 / with_tl.len() as f64));
        }
        let (Some(m), Some(t)) = (m, t.filter(|t| t["info"]["frames"].is_array())) else { continue };
        timelines_read += 1;
        let cap = capture_timeline(&t);
        blob_sum += cap.to_json_string().encode_utf16().count() as f64;
        blobs += 1.0;
        let left = !leavers(&cap).is_empty();
        leaver_of.insert(g.id.clone(), left);
        if left {
            irelia_leavers += 1;
        }
        let ps = m["info"]["participants"].as_array().cloned().unwrap_or_default();
        let Some(me) = ps.iter().find(|x| x["puuid"].as_str() == Some(g.puuid.as_str()) && x["championId"].as_i64() == Some(champion_id)) else { continue };
        lo_with += 1;
        let opp = lane_opponents(&m, Some(&cap), me);
        timeline_opp_of.insert(g.id.clone(), opp.timeline);
        if opp.position.is_none() {
            lo_no_pos += 1;
        }
        if let (Some(a), Some(b)) = (opp.position, opp.timeline) {
            lo_compared += 1;
            if a != b {
                lo_disagree += 1;
            }
        }
    }

    let (mut active_known, mut active) = (0, 0);
    for (puuid, platform) in &pilot_set {
        let top_key = mastery_files.keys().find(|k| k.starts_with(&format!("{platform}/{puuid}-top")) && k[format!("{platform}/{puuid}-top").len()..].chars().all(|c| c.is_ascii_digit()));
        let file = top_key.and_then(|k| mastery_files.get(k)).or_else(|| mastery_files.get(&format!("{platform}/{puuid}")));
        let Some(file) = file else { continue };
        let Some(top) = read_cached(file, store.as_ref()) else { continue };
        let Some(e) = top.as_array().and_then(|a| a.iter().find(|m| m["championId"].as_i64() == Some(champion_id))) else { continue };
        let Some(last) = e["lastPlayTime"].as_f64().filter(|x| *x != 0.0) else { continue };
        active_known += 1;
        if last >= regions.get(platform).map(|r| r.start as f64).unwrap_or(now - 14.0 * DAY) {
            active += 1;
        }
    }

    let builds: Vec<Value> = platforms
        .iter()
        .flat_map(|p| {
            let mut ps: Vec<String> = version_rows.iter().filter(|v| &v.platform == p).map(|v| v.patch.clone()).collect::<HashSet<_>>().into_iter().collect();
            ps.sort_by(|a, b| irelia::engine::recommend::js_order(compare_patch(b, a)));
            ps.truncate(2);
            ps.into_iter()
                .map(|patch| {
                    let mut vs: Vec<&VersionRow> = version_rows.iter().filter(|v| &v.platform == p && v.patch == patch).collect();
                    vs.sort_by_key(|v| v.first_seen);
                    json!({ "platform": p, "patch": patch, "firstGame": vs.first().map(|v| v.first_seen).unwrap_or(0),
                        "builds": vs.iter().map(|v| json!({ "gameVersion": v.game_version, "firstSeen": v.first_seen, "games": v.games })).collect::<Vec<_>>() })
                })
                .collect::<Vec<_>>()
        })
        .collect();

    let rates: Vec<f64> = pilot_matches
        .values()
        .filter(|ts| ts.len() >= 5)
        .map(|ts| {
            let (mx, mn) = (ts.iter().cloned().fold(f64::MIN, f64::max), ts.iter().cloned().fold(f64::MAX, f64::min));
            ts.len() as f64 / f64::max(1.0, (mx - mn) / DAY)
        })
        .collect();
    let pilot_share = share(pilot_irelia.values().sum(), pilot_matches.values().map(|t| t.len() as f64).sum());
    let elapsed = platforms.iter().map(|p| (now - regions.get(p).map(|r| r.start as f64).unwrap_or(now)) / DAY).fold(0.0, f64::max);
    let current_matches: f64 = by_platform.iter().map(|p| p["currentPatchMatches"].as_f64().unwrap_or(0.0)).sum();
    let regional_hosts = platforms.iter().map(|p| region_of(p)).collect::<HashSet<_>>().len().max(1);
    let y = json!({
        "pilots": pilot_set.len(), "activeShare": if active_known > 0 { n(round(active as f64 / active_known as f64, 3)) } else { Value::Null },
        "activeKnown": active_known, "matchesPerPilotDay": n(round(median(&rates), 2)), "ireliaShareOfPilotMatches": n(round(pilot_share, 3)),
        "patchDaysElapsed": n(round(f64::min(14.0, elapsed), 1)), "patchDays": 14, "historyIdsNow": 20,
        "harvestPerMatch": n(round(share(h_current_ok as f64, current_matches), 3)),
        "callsPerIreliaGameNow": n(round(share((pilot_set.len() + match_rows.len() + games.len()) as f64, games.len() as f64), 2)),
        "regionalHosts": regional_hosts,
    });
    let avg_row = if row_count > 0.0 { row_sum / row_count } else { 0.0 };
    let avg_blob = if blobs > 0.0 { blob_sum / blobs } else { 0.0 };
    let mpg = share(match_rows.len() as f64, games.len() as f64);
    let disk = json!({
        "avgParticipantRowBytes": n(irelia::js::round(avg_row)), "avgTimelineBytes": n(irelia::js::round(avg_blob)), "matchesPerIreliaGame": n(round(mpg, 2)),
        "per1000Games": {
            "participantsMB": n(round((1000.0 * mpg * 10.0 * avg_row) / 1e6, 1)),
            "timelinesMB": n(round((1000.0 * avg_blob) / 1e6, 1)),
            "totalMB": n(round((1000.0 * (mpg * 10.0 * avg_row + avg_blob)) / 1e6, 1)),
        },
    });

    progress(6, None);
    let static_data = versions.first().and_then(|v| load_cached_static(&data_dir.join("ddragon").join(v), v, dd_prev.as_deref().or(dd_patch.as_deref()).unwrap_or("")));
    let build_impact = match static_data {
        Some(sd) => {
            let sql = format!(
                "SELECT g.*, m.game_version FROM irelia_games g JOIN matches m USING(match_id) {}",
                if game_cols.contains("pilot_source") { "WHERE COALESCE(g.pilot_source, 'otp') = 'otp'" } else { "" }
            );
            let has_pos_opp = game_cols.contains("position_opponent_id");
            let mut st = db.prepare(&sql)?;
            let rows: Vec<IreliaGameRow> = st
                .query_map([], |r| Ok(IreliaGameRow::from_row(r)))?
                .filter_map(|x| x.ok())
                .map(|mut g| {
                    if has_pos_opp {
                        g.opponent_champion_id = g.position_opponent_id.or(g.opponent_champion_id);
                    }
                    g
                })
                .collect();
            let regions_pp: HashMap<String, (String, String)> = regions.iter().map(|(k, r)| (k.clone(), (r.patch.clone(), r.previous_patch.clone()))).collect();
            Value::Array(build_impact_of(rows, &sd, &remake_of, &leaver_of, &timeline_opp_of, &regions_pp))
        }
        None => Value::Null,
    };

    let total_players: i64 = by_pt.iter().map(|r| r["players"].as_i64().unwrap_or(0)).sum();
    let checked: i64 = by_pt.iter().map(|r| r["checked"].as_i64().unwrap_or(0)).sum();
    let mut by_patch_v: Vec<(String, [f64; 5])> = by_patch.into_iter().collect();
    by_patch_v.sort_by(|a, b| irelia::engine::recommend::js_order(compare_patch(&b.0, &a.0)));
    let current_games = games.iter().filter(|g| regions.get(g.id.split('_').next().unwrap_or("")).map(|r| r.patch == g.patch).unwrap_or(false)).count();
    let projection = project_yield(&y, current_games as f64, Some(total_players as f64));
    Ok(json!({
        "generatedAt": chrono::DateTime::from_timestamp_millis(now as i64).unwrap_or_default().to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
        "dataDir": data_dir.to_string_lossy(), "schemaVersion": n(schema_version), "dataDragon": { "patch": dd_patch, "previousPatch": dd_prev },
        "players": { "total": total_players, "checked": checked, "unchecked": total_players - checked, "byPlatformTier": by_pt },
        "matches": {
            "total": match_rows.len(),
            "byPatch": by_patch_v.iter().map(|(p, e)| json!({ "patch": p, "matches": e[0] as i64, "ireliaGames": e[1] as i64, "top": e[2] as i64, "mid": e[3] as i64, "other": e[4] as i64 })).collect::<Vec<_>>(),
            "byPlatform": by_platform,
        },
        "ireliaShare": { "matches": match_rows.len(), "ireliaGames": games.len(), "share": n(round(share(games.len() as f64, match_rows.len() as f64), 3)) },
        "pilots": {
            "distinct": per_pilot.len(),
            "gamesPerPilot": { "min": n(if counts.is_empty() { 0.0 } else { counts.iter().cloned().fold(f64::INFINITY, f64::min) }), "median": n(median(&counts)), "p90": n(quantile(&counts, 0.9)), "max": n(counts.iter().cloned().fold(0.0, f64::max)) },
            "top10Share": n(round(top_share(&build_games.iter().collect::<Vec<_>>()).1, 3)), "byLane": by_lane, "bySource": by_source,
        },
        "matchups": matchups,
        "quality": { "matchesRead": matches_read, "remakes": remakes, "earlySurrenderFlag": early, "shorterThan5Min": short, "endResults": end_results, "ireliaRemakes": irelia_remakes, "ireliaLeavers": irelia_leavers, "timelinesRead": timelines_read },
        "laneOpponent": { "gamesWithTimeline": lo_with, "compared": lo_compared, "disagree": lo_disagree, "share": n(round(share(lo_disagree as f64, lo_compared as f64), 3)), "noPositionOpponent": lo_no_pos },
        "builds": builds, "cache": cache,
        "harvest": { "cachedMatchesWithOtherIrelia": h_other, "currentPatch": h_current, "currentPatchNotRemake": h_current_ok, "players": harvest_players.len(), "byLane": h_by_lane },
        "disk": disk, "yield": y, "buildImpact": build_impact, "projection": projection,
    }))
}

/// The yield model (docs/BUILD-SOURCING.md §Yield model).
pub fn project_yield(y: &Value, current_games: f64, ladder_players: Option<f64>) -> Vec<Value> {
    let f = |k: &str| y[k].as_f64().unwrap_or(0.0);
    let p = f("pilots");
    let a = y["activeShare"].as_f64().unwrap_or(0.6);
    let s = if f("ireliaShareOfPilotMatches") != 0.0 { f("ireliaShareOfPilotMatches") } else { 0.5 };
    let h = f("harvestPerMatch");
    let m = (if f("matchesPerPilotDay") != 0.0 { f("matchesPerPilotDay") } else { 6.0 }) * f("patchDays");
    let nn = ladder_players.unwrap_or(p * 200.0);
    let b = DEV_KEY_CALLS_PER_DAY;
    let pages = f64::max(1.0, (m / 100.0).ceil());
    #[derive(Clone, Copy)]
    struct X {
        g: f64,
        m: f64,
        l: f64,
        c: f64,
        hosts: f64,
        parallel: bool,
        budget: Option<f64>,
    }
    let mut rows = vec![];
    let mut push = |option: &str, x: X, note: String| {
        let regional = x.m + x.g + x.l;
        let budget = x.budget.unwrap_or(b) * x.hosts;
        let days = if x.parallel { f64::max(regional, x.c) / budget } else { (regional + x.c) / budget };
        rows.push(json!({
            "option": option, "gamesPerPatch": n(irelia::js::round(x.g)), "callsPerPatch": n(irelia::js::round(regional + x.c)), "daysAtDevKey": n(round(days, 2)),
            "note": format!("{note}; {} regional calls per game", irelia::js::num_to_string(round(if x.g != 0.0 { regional / x.g } else { 0.0 }, 2))),
        }));
    };
    let c0 = (nn * f("patchDays")) / 3.0;
    let c1 = (p * f("patchDays")) / 3.0 + (nn - p);
    let hist = f("historyIdsNow");
    let now = X { g: p * a * hist.min(m) * s, m: p * a * hist.min(m) + p * (1.0 - a), l: p, c: c0, hosts: 1.0, parallel: false, budget: None };
    push("as now (20 newest ids)", now, format!("measured: {} games on the current patch so far", irelia::js::num_to_string(current_games)));
    let pag = X { g: p * a * m * s, m: p * a * m + p * (1.0 - a), l: p * a * pages + p * (1.0 - a), ..now };
    push("+ whole-patch pagination", pag, format!("{} matches per active pilot per patch", irelia::js::num_to_string(irelia::js::round(m))));
    let act = X { m: p * a * m, l: p * a * pages, c: c1, ..pag };
    push("+ lastPlayTime filter", act, format!("skips {} inactive pilots; others rechecked every 14 days", irelia::js::num_to_string(irelia::js::round(p * (1.0 - a)))));
    let har = X { g: act.g + act.m * h, ..act };
    push("+ harvesting other Irelias", har, format!("{} harvestable per match, one timeline call each", irelia::js::num_to_string(h)));
    let lanes = X { parallel: true, ..har };
    push("+ per-host queues", lanes, "match-v5 never waits for mastery checks".into());
    let k = 3.0 / f64::max(1.0, f("regionalHosts"));
    let regions = X { g: lanes.g * k, m: lanes.m * k, l: lanes.l * k, c: lanes.c * k, hosts: 3.0, ..lanes };
    push("+ EUW1 and NA1", regions, "as many pilots per region (assumed); three regional and platform hosts".into());
    let tiers = X { g: regions.g * 2.0, m: regions.m * 2.0, l: regions.l * 2.0, c: regions.c * 2.0, ..regions };
    push("+ Master / Diamond I tiers", tiers, "twice the pilots and ladder (assumed; the audit counts pilots per tier)".into());
    push("+ production key", X { budget: Some(30_000.0 * 6.0 * 24.0 * 0.9), ..tiers }, "budget ×66; a personal key keeps dev limits but never expires".into());
    rows
}

fn engine() -> EngineSettings {
    EngineSettings { min_matchup_games: 15.0, general_prior_weight: 0.15, min_lane_games: 30.0, other_lane_weight: 0.15, previous_patch_weight: 0.35, min_current_patch_games: 40.0, starting_item_window_ms: 60_000.0 }
}

/// The lane builds of the stored OTP games under v0.3.0's rules, then with each data-quality fix added in turn.
pub fn build_impact_of(
    rows: Vec<IreliaGameRow>,
    data: &StaticData,
    remake_of: &HashMap<String, bool>,
    leaver_of: &HashMap<String, bool>,
    timeline_opp_of: &HashMap<String, Option<i64>>,
    regions: &HashMap<String, (String, String)>,
) -> Vec<Value> {
    let excl = build_exclusivity_index(data);
    let v030: Vec<Arc<IreliaGameRow>> = rows.iter().map(|r| Arc::new(IreliaGameRow { remake: Some(0), leaver: Some(0), game_duration: r.game_duration.max(300), ..r.clone() })).collect();
    let remakes: Vec<IreliaGameRow> = rows.iter().map(|r| IreliaGameRow { remake: Some(remake_of.get(&r.match_id).copied().unwrap_or(false) as i64), leaver: Some(0), ..r.clone() }).collect();
    let leavers: Vec<IreliaGameRow> = remakes.iter().map(|r| IreliaGameRow { leaver: Some(leaver_of.get(&r.match_id).copied().unwrap_or(false) as i64), ..r.clone() }).collect();
    let opponents: Vec<IreliaGameRow> = leavers
        .iter()
        .map(|r| IreliaGameRow { opponent_champion_id: timeline_opp_of.get(&r.match_id).copied().flatten().or(r.opponent_champion_id), ..r.clone() })
        .collect();
    let arc = |v: Vec<IreliaGameRow>| v.into_iter().map(Arc::new).collect::<Vec<_>>();
    let variants: Vec<(&str, Vec<Arc<IreliaGameRow>>, bool)> = vec![
        ("v0.3.0", v030, false),
        ("+ remakes", arc(remakes), false),
        ("+ leavers", arc(leavers), false),
        ("+ timeline opponent", arc(opponents.clone()), false),
        ("+ patch per region", arc(opponents), true),
    ];
    let mut out = vec![];
    for lane in ["TOP", "MIDDLE"] {
        for (variant, rs, per_region) in &variants {
            let rp = |p: &str| if *per_region { regions.get(p).cloned() } else { None };
            let b = recommend(rs, data, &excl, None, &engine(), Some(lane), &rp, None);
            let mut m: indexmap::IndexMap<i64, i64> = indexmap::IndexMap::new();
            for r in rs.iter() {
                if r.position == lane {
                    if let Some(o) = r.opponent_champion_id.filter(|o| *o != 0) {
                        *m.entry(o).or_insert(0) += 1;
                    }
                }
            }
            let mut top: Vec<(i64, i64)> = m.into_iter().collect();
            top.sort_by(|a, c| c.1.cmp(&a.1));
            let mut discarded = Map::new();
            discarded.insert("stale".into(), n(b.validation.stale_discarded));
            for (k, v) in &b.validation.invalid_reasons {
                discarded.insert(k.clone(), n(*v));
            }
            out.push(json!({
                "lane": lane, "variant": variant, "laneGames": n(b.sample.lane_games), "used": n(b.validation.fresh_games + b.validation.previous_patch_games),
                "discarded": discarded, "keystone": b.runes.as_ref().and_then(|r| r.value.perks.first().copied()),
                "core": b.core_build.first().map(|c| c.value.clone()).unwrap_or_default(), "boots": b.boots.first().map(|x| x.value),
                "skills": b.skills.as_ref().map(|s| s.value.priority.iter().map(|x| x.to_string()).collect::<Vec<_>>().join(">")).unwrap_or_default(),
                "winRate": b.win_rate.map(n), "topMatchups": top.iter().take(3).map(|(id, g)| json!({ "id": id, "games": g })).collect::<Vec<_>>(),
            }));
        }
    }
    out
}

fn pct(x: Option<f64>) -> String {
    match x {
        None => "—".into(),
        Some(x) => format!("{}%", irelia::js::to_fixed(x * 100.0, 1)),
    }
}

/// A compact plain-text summary.
pub fn format_audit(r: &Value) -> String {
    let s = |v: &Value| match v {
        Value::String(x) => x.clone(),
        Value::Null => "?".into(),
        other => irelia::js::num_to_string(other.as_f64().unwrap_or(f64::NAN)),
    };
    let f = |v: &Value| v.as_f64().unwrap_or(0.0);
    let loc = |v: &Value| irelia::js::thousands(f(v));
    let mut lines: Vec<String> = vec![];
    let t = |lines: &mut Vec<String>, title: &str| {
        lines.push(String::new());
        lines.push(title.into());
        lines.push("-".repeat(title.chars().count()));
    };
    lines.push(format!("Irelia Companion data audit · {} · schema v{} · Data Dragon {}", s(&r["dataDir"]), s(&r["schemaVersion"]), s(&r["dataDragon"]["patch"])));
    t(&mut lines, "Players");
    lines.push(format!("{} players: {} checked, {} unchecked", loc(&r["players"]["total"]), loc(&r["players"]["checked"]), loc(&r["players"]["unchecked"])));
    for p in r["players"]["byPlatformTier"].as_array().unwrap_or(&vec![]) {
        lines.push(format!("  {:<5} {:<12} {:>7} players  {:>7} checked  {:>4} OTPs  {:>4} mains", s(&p["platform"]), s(&p["tier"]), s(&p["players"]), s(&p["checked"]), s(&p["otps"]), s(&p["mains"])));
    }
    t(&mut lines, "Matches and Irelia games");
    lines.push(format!("{} matches, {} Irelia games ({} of fetched matches)", loc(&r["matches"]["total"]), loc(&r["ireliaShare"]["ireliaGames"]), pct(r["ireliaShare"]["share"].as_f64())));
    for p in r["matches"]["byPatch"].as_array().unwrap_or(&vec![]) {
        lines.push(format!("  {:<6} {:>6} matches {:>5} Irelia: {} top / {} mid / {} other", s(&p["patch"]), s(&p["matches"]), s(&p["ireliaGames"]), s(&p["top"]), s(&p["mid"]), s(&p["other"])));
    }
    for p in r["matches"]["byPlatform"].as_array().unwrap_or(&vec![]) {
        lines.push(format!("  {:<5} current {}: {} matches, {} Irelia games", s(&p["platform"]), s(&p["currentPatch"]), s(&p["currentPatchMatches"]), s(&p["currentPatchIrelia"])));
    }
    t(&mut lines, "Pilots and matchups");
    let gp = &r["pilots"]["gamesPerPilot"];
    lines.push(format!("{} pilots · games per pilot: median {}, p90 {}, max {} · top-10 pilots {}", s(&r["pilots"]["distinct"]), s(&gp["median"]), s(&gp["p90"]), s(&gp["max"]), pct(r["pilots"]["top10Share"].as_f64())));
    let src: Vec<String> = r["pilots"]["bySource"].as_object().map(|o| o.iter().map(|(k, v)| format!("{k} {}", s(v))).collect()).unwrap_or_default();
    lines.push(format!("  by source: {}", if src.is_empty() { "—".into() } else { src.join(", ") }));
    if let Some(m) = r["matchups"].as_object() {
        for (lane, x) in m {
            lines.push(format!(
                "  {:<6} {} games, {} opponents, {} in matchups with 15+ games, top-10 pilots {}",
                lane, s(&x["games"]), s(&x["opponents"]), pct(x["share15"].as_f64()), pct(Some(r["pilots"]["byLane"][lane]["top10Share"].as_f64().unwrap_or(0.0)))
            ));
        }
    }
    t(&mut lines, "Data quality");
    let q = &r["quality"];
    lines.push(format!("{} matches read: {} remakes ({} by the early-surrender flag, {} under 5 min)", s(&q["matchesRead"]), s(&q["remakes"]), s(&q["earlySurrenderFlag"]), s(&q["shorterThan5Min"])));
    lines.push(format!("  end results: {}", q["endResults"].as_object().map(|o| o.iter().map(|(k, v)| format!("{k} {}", s(v))).collect::<Vec<_>>().join(", ")).unwrap_or_default()));
    lines.push(format!("  stored Irelia games: {} remakes, {} with a leaver (of {} timelines)", s(&q["ireliaRemakes"]), s(&q["ireliaLeavers"]), s(&q["timelinesRead"])));
    let lo = &r["laneOpponent"];
    lines.push(format!("  lane opponent: teamPosition and timeline differ in {} of {} games ({}); {} without a position opponent", s(&lo["disagree"]), s(&lo["compared"]), pct(lo["share"].as_f64()), s(&lo["noPositionOpponent"])));
    t(&mut lines, "Patches and builds");
    for b in r["builds"].as_array().unwrap_or(&vec![]) {
        let first = chrono::DateTime::from_timestamp_millis(b["firstGame"].as_i64().unwrap_or(0)).unwrap_or_default().format("%Y-%m-%dT%H:%M").to_string();
        let bs: Vec<String> = b["builds"].as_array().unwrap_or(&vec![]).iter().map(|v| format!("{} ({})", s(&v["gameVersion"]), s(&v["games"]))).collect();
        lines.push(format!("  {:<5} {:<6} first game {} · {}", s(&b["platform"]), s(&b["patch"]), first, bs.join(", ")));
    }
    t(&mut lines, "Cache coverage");
    let c = &r["cache"];
    lines.push(format!("{} match files, {} timeline files, {} mastery files", s(&c["matchFiles"]), s(&c["timelineFiles"]), s(&c["masteryFiles"])));
    lines.push(format!("  in the database but not on disk: {} matches, {} Irelia timelines", s(&c["matchesWithoutFile"]), s(&c["gamesWithoutTimeline"])));
    lines.push(format!("  on disk but not in the database: {} matches, {} timelines", s(&c["matchFilesNotInDb"]), s(&c["timelineFilesNotInDb"])));
    t(&mut lines, "Harvesting");
    let h = &r["harvest"];
    lines.push(format!(
        "{} cached matches have another player's top/mid Irelia ({} players); {} on the current patch, not remade → that many games for one timeline call each",
        s(&h["cachedMatchesWithOtherIrelia"]), s(&h["players"]), s(&h["currentPatchNotRemake"])
    ));
    t(&mut lines, "Disk");
    let d = &r["disk"];
    lines.push(format!(
        "participant row ≈ {} B, timeline ≈ {} KB, {} matches per Irelia game → per 1,000 games ≈ {} MB ({} participants + {} timelines)",
        s(&d["avgParticipantRowBytes"]), irelia::js::to_fixed(f(&d["avgTimelineBytes"]) / 1024.0, 1), s(&d["matchesPerIreliaGame"]),
        s(&d["per1000Games"]["totalMB"]), s(&d["per1000Games"]["participantsMB"]), s(&d["per1000Games"]["timelinesMB"])
    ));
    t(&mut lines, "Yield model");
    let y = &r["yield"];
    lines.push(format!(
        "pilots {}, active {} (of {} with cached mastery), {} matches/pilot/day, Irelia share {}, patch day {}/{}, harvest {}/match, {} calls/game now",
        s(&y["pilots"]), pct(y["activeShare"].as_f64()), s(&y["activeKnown"]), s(&y["matchesPerPilotDay"]), pct(y["ireliaShareOfPilotMatches"].as_f64()),
        s(&y["patchDaysElapsed"]), s(&y["patchDays"]), s(&y["harvestPerMatch"]), s(&y["callsPerIreliaGameNow"])
    ));
    for p in r["projection"].as_array().unwrap_or(&vec![]) {
        lines.push(format!("  {:<30} {:>7} games/patch {:>8} calls  {:>6} days at a dev key · {}", s(&p["option"]), s(&p["gamesPerPatch"]), s(&p["callsPerPatch"]), s(&p["daysAtDevKey"]), s(&p["note"])));
    }
    t(&mut lines, "Lane builds: v0.3.0, then each data-quality fix");
    if r["buildImpact"].is_null() {
        lines.push("(no Data Dragon files cached in this folder: start the app once to cache them)".into());
    }
    for b in r["buildImpact"].as_array().unwrap_or(&vec![]) {
        let gone: Vec<String> = b["discarded"].as_object().map(|o| o.iter().filter(|(_, v)| f(v) != 0.0).map(|(k, v)| format!("{k} {}", s(v))).collect()).unwrap_or_default();
        let core: Vec<String> = b["core"].as_array().unwrap_or(&vec![]).iter().map(&s).collect();
        let vs: Vec<String> = b["topMatchups"].as_array().unwrap_or(&vec![]).iter().map(|m| format!("{}×{}", s(&m["id"]), s(&m["games"]))).collect();
        lines.push(format!(
            "  {:<6} {:<20} {:>5} used{} · keystone {} · core {} · boots {} · {} · WR {} · vs {}",
            s(&b["lane"]), s(&b["variant"]), s(&b["used"]),
            if gone.is_empty() { String::new() } else { format!(" (dropped: {})", gone.join(", ")) },
            if b["keystone"].is_null() { "—".into() } else { s(&b["keystone"]) },
            if core.is_empty() { "—".into() } else { core.join(" ") },
            if b["boots"].is_null() { "—".into() } else { s(&b["boots"]) },
            if b["skills"].as_str().unwrap_or("").is_empty() { "—".into() } else { s(&b["skills"]) },
            pct(b["winRate"].as_f64()), vs.join(" ")
        ));
    }
    lines.join("\n")
}

#[allow(dead_code)]
fn _unused(_: BTreeMap<i64, i64>) {}
