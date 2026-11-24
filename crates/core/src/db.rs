//! Port of `server/db.ts`: the SQLite database (`<data>/irelia.db`), its schema and in-place migrations, and every
//! query the app makes. The schema, the SQL and the on-disk format are 0.22.0's, unchanged: a data folder of 0.x
//! opens here as it is, and one written here opens in 0.22.0.

use crate::codec::{pack, unpack, Dictionaries, Dictionary};
use crate::db_sql::*;
use crate::util::now_ms;
use anyhow::{anyhow, bail, Result};
use flate2::{read::GzDecoder, write::GzEncoder, Compression};
use rusqlite::functions::FunctionFlags;
use rusqlite::types::{Value as Sql, ValueRef};
use rusqlite::{params, Connection, OptionalExtension, Row, ToSql};
use serde_json::{json, Map, Value};
use std::collections::{HashMap, HashSet};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;

pub const SCHEMA_VERSION: i64 = 6;
/// How long a connection waits for another one's write before "database is locked".
pub const BUSY_TIMEOUT_MS: u64 = 5000;
/// Share of the database participants (and its index) must exceed for the rebuild to be tried.
pub const PARTICIPANTS_SHARE: f64 = 0.25;

const PARTICIPANT_VALUE_COLUMNS: [&str; 23] = [
    "champion_id", "team_id", "position", "win", "champ_level", "kills", "deaths", "assists", "gold_earned", "cs", "items",
    "dmg_physical", "dmg_magic", "dmg_true", "dmg_taken", "dmg_mitigated", "heal", "heal_allies", "shield_allies",
    "cc_time", "cc_dealt", "time_played", "challenges",
];
const PARTICIPANT_COLUMNS: [&str; 26] = [
    "match_id", "participant_id", "puuid", "champion_id", "team_id", "position", "win", "champ_level", "kills", "deaths",
    "assists", "gold_earned", "cs", "items", "dmg_physical", "dmg_magic", "dmg_true", "dmg_taken", "dmg_mitigated",
    "heal", "heal_allies", "shield_allies", "cc_time", "cc_dealt", "time_played", "challenges",
];
pub const GAME_COLUMNS: [&str; 29] = [
    "match_id", "puuid", "patch", "win", "position", "opponent_champion_id", "summoner1", "summoner2", "primary_style",
    "sub_style", "perks", "stat_perks", "skill_order", "purchases", "final_items", "kills", "deaths", "assists",
    "game_duration", "created_at", "participant_id", "position_opponent_id", "timeline_opponent_id", "pilot_source",
    "pilot_tier", "pilot_lp", "pilot_mastery", "pilot_last_play", "pilot_top1",
];
const TOTAL_COLUMNS: [&str; 12] = [
    "games", "wins", "seconds", "dmg_physical", "dmg_magic", "dmg_true", "dmg_taken", "dmg_mitigated", "heal",
    "heal_allies", "shield_allies", "cc_time",
];
/// Matches stored whole in the archive: the tables each is spread over (the match row first).
pub const ARCHIVED_TABLES: [&str; 4] = ["matches", "irelia_games", "participants", "timelines"];

const ADDED_COLUMNS: &[(&str, &[(&str, &str)])] = &[
    (
        "players",
        &[
            ("pilot_class", "TEXT"),
            ("irelia_points", "INTEGER"),
            ("irelia_last_play", "INTEGER"),
            ("irelia_rank", "INTEGER"),
            ("found_by", "TEXT NOT NULL DEFAULT 'ladder'"),
            ("history_matches", "INTEGER NOT NULL DEFAULT 0"),
            ("history_irelia", "INTEGER NOT NULL DEFAULT 0"),
            ("rank_checked", "INTEGER"),
        ],
    ),
    (
        "matches",
        &[
            ("remake", "INTEGER"),
            ("leaver", "INTEGER"),
            ("end_result", "TEXT"),
            ("detail_version", "INTEGER NOT NULL DEFAULT 0"),
        ],
    ),
    ("timelines", &[("data_z", "BLOB")]),
    (
        "irelia_games",
        &[
            ("participant_id", "INTEGER"),
            ("position_opponent_id", "INTEGER"),
            ("timeline_opponent_id", "INTEGER"),
            ("pilot_source", "TEXT"),
            ("pilot_tier", "TEXT"),
            ("pilot_lp", "INTEGER"),
            ("pilot_mastery", "INTEGER"),
            ("pilot_last_play", "INTEGER"),
            ("pilot_top1", "INTEGER"),
        ],
    ),
];

/// SQL predicate on `g` (irelia_games) for the build sources: games stored before v3 count as OTP games.
const SOURCE_FILTER: &str = "COALESCE(g.pilot_source, 'otp') IN (SELECT value FROM json_each(?))";
/// The high-elo build's default sources (scrape.config.json buildSources). 'me' is never one of them.
pub const DEFAULT_BUILD_SOURCES: [&str; 1] = ["otp"];
pub const PILOT_SOURCES: [&str; 4] = ["otp", "main", "harvested", "me"];

fn champion_totals_of(where_: &str) -> String {
    format!(
        "SELECT m.patch, p.champion_id, p.position, COUNT(*) AS games, SUM(p.win) AS wins,
    SUM(m.game_duration) AS seconds, SUM(p.dmg_physical) AS dmg_physical, SUM(p.dmg_magic) AS dmg_magic, SUM(p.dmg_true) AS dmg_true,
    SUM(p.dmg_taken) AS dmg_taken, SUM(p.dmg_mitigated) AS dmg_mitigated, SUM(p.heal) AS heal, SUM(p.heal_allies) AS heal_allies,
    SUM(p.shield_allies) AS shield_allies, SUM(p.cc_time) AS cc_time
  FROM participants p JOIN matches m USING(match_id)
  WHERE {where_} AND COALESCE(m.remake, 0) = 0
  GROUP BY m.patch, p.champion_id, p.position"
    )
}

fn keep_champion_stats_sql(where_: &str) -> String {
    format!(
        "INSERT INTO champion_patch_stats(patch, champion_id, position, {})
  {}
  ON CONFLICT(patch, champion_id, position) DO UPDATE SET {}",
        TOTAL_COLUMNS.join(", "),
        champion_totals_of(where_),
        TOTAL_COLUMNS.iter().map(|c| format!("{c} = {c} + excluded.{c}")).collect::<Vec<_>>().join(", ")
    )
}

// ------------------------------------------------------------------------------------------------ row types

/// One stored Irelia game (irelia_games, plus the matches columns the engine joins).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct IreliaGameRow {
    pub match_id: String,
    pub puuid: String,
    pub patch: String,
    pub win: i64,
    pub position: String,
    pub opponent_champion_id: Option<i64>,
    pub summoner1: i64,
    pub summoner2: i64,
    pub primary_style: i64,
    pub sub_style: i64,
    pub perks: String,
    pub stat_perks: String,
    pub skill_order: String,
    pub purchases: String,
    pub final_items: String,
    pub kills: i64,
    pub deaths: i64,
    pub assists: i64,
    pub game_duration: i64,
    pub created_at: i64,
    pub participant_id: Option<i64>,
    pub position_opponent_id: Option<i64>,
    pub timeline_opponent_id: Option<i64>,
    pub pilot_source: Option<String>,
    pub pilot_tier: Option<String>,
    pub pilot_lp: Option<i64>,
    pub pilot_mastery: Option<i64>,
    pub pilot_last_play: Option<i64>,
    pub pilot_top1: Option<i64>,
    // joined from matches
    pub remake: Option<i64>,
    pub leaver: Option<i64>,
    pub end_result: Option<String>,
    pub game_version: Option<String>,
    // model rows (dataset.ts ModelGameRow)
    pub platform: Option<String>,
    pub timeline: Option<String>,
}

fn col<T: rusqlite::types::FromSql + Default>(r: &Row, name: &str) -> T {
    r.get::<_, T>(name).unwrap_or_default()
}
fn col_opt<T: rusqlite::types::FromSql>(r: &Row, name: &str) -> Option<T> {
    r.get::<_, Option<T>>(name).ok().flatten()
}

impl IreliaGameRow {
    pub fn from_row(r: &Row) -> IreliaGameRow {
        IreliaGameRow {
            match_id: col(r, "match_id"),
            puuid: col(r, "puuid"),
            patch: col(r, "patch"),
            win: col(r, "win"),
            position: col(r, "position"),
            opponent_champion_id: col_opt(r, "opponent_champion_id"),
            summoner1: col(r, "summoner1"),
            summoner2: col(r, "summoner2"),
            primary_style: col(r, "primary_style"),
            sub_style: col(r, "sub_style"),
            perks: col(r, "perks"),
            stat_perks: col(r, "stat_perks"),
            skill_order: col(r, "skill_order"),
            purchases: col(r, "purchases"),
            final_items: col(r, "final_items"),
            kills: col(r, "kills"),
            deaths: col(r, "deaths"),
            assists: col(r, "assists"),
            game_duration: col(r, "game_duration"),
            created_at: col(r, "created_at"),
            participant_id: col_opt(r, "participant_id"),
            position_opponent_id: col_opt(r, "position_opponent_id"),
            timeline_opponent_id: col_opt(r, "timeline_opponent_id"),
            pilot_source: col_opt(r, "pilot_source"),
            pilot_tier: col_opt(r, "pilot_tier"),
            pilot_lp: col_opt(r, "pilot_lp"),
            pilot_mastery: col_opt(r, "pilot_mastery"),
            pilot_last_play: col_opt(r, "pilot_last_play"),
            pilot_top1: col_opt(r, "pilot_top1"),
            remake: col_opt(r, "remake"),
            leaver: col_opt(r, "leaver"),
            end_result: col_opt(r, "end_result"),
            game_version: col_opt(r, "game_version"),
            platform: col_opt(r, "platform"),
            timeline: col_opt(r, "timeline"),
        }
    }

    fn game_values(&self) -> Vec<Sql> {
        use Sql::*;
        let i = |v: i64| Integer(v);
        let oi = |v: Option<i64>| v.map(Integer).unwrap_or(Null);
        let t = |v: &str| Text(v.to_string());
        let ot = |v: &Option<String>| v.clone().map(Text).unwrap_or(Null);
        vec![
            t(&self.match_id), t(&self.puuid), t(&self.patch), i(self.win), t(&self.position), oi(self.opponent_champion_id),
            i(self.summoner1), i(self.summoner2), i(self.primary_style), i(self.sub_style), t(&self.perks),
            t(&self.stat_perks), t(&self.skill_order), t(&self.purchases), t(&self.final_items), i(self.kills),
            i(self.deaths), i(self.assists), i(self.game_duration), i(self.created_at), oi(self.participant_id),
            oi(self.position_opponent_id), oi(self.timeline_opponent_id), ot(&self.pilot_source), ot(&self.pilot_tier),
            oi(self.pilot_lp), oi(self.pilot_mastery), oi(self.pilot_last_play), oi(self.pilot_top1),
        ]
    }
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct PlayerRow {
    pub puuid: String,
    pub platform: String,
    pub irelia_mastery: Option<i64>,
    pub tier: String,
    pub league_points: i64,
    pub is_otp: Option<i64>,
    pub last_checked: Option<i64>,
    pub last_match_scan: Option<i64>,
    pub pilot_class: Option<String>,
    pub irelia_points: Option<i64>,
    pub irelia_last_play: Option<i64>,
    pub irelia_rank: Option<i64>,
    pub found_by: String,
    pub history_matches: i64,
    pub history_irelia: i64,
    pub rank_checked: Option<i64>,
    pub prio: Option<i64>,
}

impl PlayerRow {
    pub fn from_row(r: &Row) -> PlayerRow {
        PlayerRow {
            puuid: col(r, "puuid"),
            platform: col(r, "platform"),
            irelia_mastery: col_opt(r, "irelia_mastery"),
            tier: col(r, "tier"),
            league_points: col(r, "league_points"),
            is_otp: col_opt(r, "is_otp"),
            last_checked: col_opt(r, "last_checked"),
            last_match_scan: col_opt(r, "last_match_scan"),
            pilot_class: col_opt(r, "pilot_class"),
            irelia_points: col_opt(r, "irelia_points"),
            irelia_last_play: col_opt(r, "irelia_last_play"),
            irelia_rank: col_opt(r, "irelia_rank"),
            found_by: col_opt(r, "found_by").unwrap_or_else(|| "ladder".into()),
            history_matches: col(r, "history_matches"),
            history_irelia: col(r, "history_irelia"),
            rank_checked: col_opt(r, "rank_checked"),
            prio: col_opt(r, "prio"),
        }
    }
}

/// A pilot's mastery-based classification (scraper/pilots.ts classifyPilot).
#[derive(Clone, Debug, PartialEq)]
pub struct PilotUpdate {
    pub pilot_class: String,
    pub irelia_points: i64,
    pub irelia_last_play: Option<i64>,
    pub irelia_rank: Option<i64>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct MatchRow {
    pub id: String,
    pub platform: String,
    pub patch: String,
    pub version: String,
    pub queue: i64,
    pub creation: i64,
    pub duration: i64,
    pub has_irelia: bool,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct RecLogRow {
    pub id: i64,
    pub session: String,
    pub phase: String,
    pub created_at: i64,
    pub updated_at: i64,
    pub lane: Option<String>,
    pub opponent_id: Option<i64>,
    pub mode: String,
    pub shown: String,
    pub match_id: Option<String>,
    pub bought: Option<String>,
    pub win: Option<i64>,
}

impl RecLogRow {
    fn from_row(r: &Row) -> RecLogRow {
        RecLogRow {
            id: col(r, "id"),
            session: col(r, "session"),
            phase: col(r, "phase"),
            created_at: col(r, "created_at"),
            updated_at: col(r, "updated_at"),
            lane: col_opt(r, "lane"),
            opponent_id: col_opt(r, "opponent_id"),
            mode: col(r, "mode"),
            shown: col(r, "shown"),
            match_id: col_opt(r, "match_id"),
            bought: col_opt(r, "bought"),
            win: col_opt(r, "win"),
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct QueuedMatch {
    pub match_id: String,
    pub platform: String,
    pub kind: String,
    pub puuid: String,
    pub source: String,
    pub score: f64,
}

impl QueuedMatch {
    fn from_row(r: &Row) -> QueuedMatch {
        QueuedMatch {
            match_id: col(r, "match_id"),
            platform: col(r, "platform"),
            kind: col(r, "kind"),
            puuid: col(r, "puuid"),
            source: col(r, "source"),
            score: col(r, "score"),
        }
    }
}

/// capture.ts ParticipantRow: one player of a stored match.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ParticipantRow {
    pub match_id: String,
    pub participant_id: i64,
    pub puuid: String,
    pub champion_id: i64,
    pub team_id: i64,
    pub position: String,
    pub win: i64,
    pub champ_level: i64,
    pub kills: i64,
    pub deaths: i64,
    pub assists: i64,
    pub gold_earned: i64,
    pub cs: i64,
    pub items: String,
    pub dmg_physical: i64,
    pub dmg_magic: i64,
    pub dmg_true: i64,
    pub dmg_taken: i64,
    pub dmg_mitigated: i64,
    pub heal: i64,
    pub heal_allies: i64,
    pub shield_allies: i64,
    pub cc_time: i64,
    pub cc_dealt: i64,
    pub time_played: i64,
    pub challenges: Option<String>,
}

impl ParticipantRow {
    /// The row as the archive and transfers keep it (column order of the table).
    pub fn to_map(&self) -> Map<String, Value> {
        let mut m = Map::new();
        m.insert("match_id".into(), json!(self.match_id));
        m.insert("participant_id".into(), json!(self.participant_id));
        m.insert("puuid".into(), json!(self.puuid));
        m.insert("champion_id".into(), json!(self.champion_id));
        m.insert("team_id".into(), json!(self.team_id));
        m.insert("position".into(), json!(self.position));
        m.insert("win".into(), json!(self.win));
        m.insert("champ_level".into(), json!(self.champ_level));
        m.insert("kills".into(), json!(self.kills));
        m.insert("deaths".into(), json!(self.deaths));
        m.insert("assists".into(), json!(self.assists));
        m.insert("gold_earned".into(), json!(self.gold_earned));
        m.insert("cs".into(), json!(self.cs));
        m.insert("items".into(), json!(self.items));
        m.insert("dmg_physical".into(), json!(self.dmg_physical));
        m.insert("dmg_magic".into(), json!(self.dmg_magic));
        m.insert("dmg_true".into(), json!(self.dmg_true));
        m.insert("dmg_taken".into(), json!(self.dmg_taken));
        m.insert("dmg_mitigated".into(), json!(self.dmg_mitigated));
        m.insert("heal".into(), json!(self.heal));
        m.insert("heal_allies".into(), json!(self.heal_allies));
        m.insert("shield_allies".into(), json!(self.shield_allies));
        m.insert("cc_time".into(), json!(self.cc_time));
        m.insert("cc_dealt".into(), json!(self.cc_dealt));
        m.insert("time_played".into(), json!(self.time_played));
        m.insert("challenges".into(), self.challenges.clone().map(Value::String).unwrap_or(Value::Null));
        m
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct ParticipantLite {
    pub match_id: String,
    pub participant_id: i64,
    pub champion_id: i64,
    pub team_id: i64,
    pub position: String,
}

impl ParticipantLite {
    fn from_row(r: &Row) -> ParticipantLite {
        ParticipantLite {
            match_id: col(r, "match_id"),
            participant_id: col(r, "participant_id"),
            champion_id: col(r, "champion_id"),
            team_id: col(r, "team_id"),
            position: col(r, "position"),
        }
    }
}

/// Per champion and position, summed stats (model/profiles.ts ChampionTotals).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ChampionTotals {
    pub champion_id: i64,
    pub position: String,
    pub games: f64,
    pub seconds: f64,
    pub dmg_physical: f64,
    pub dmg_magic: f64,
    pub dmg_true: f64,
    pub dmg_taken: f64,
    pub dmg_mitigated: f64,
    pub heal: f64,
    pub heal_allies: f64,
    pub shield_allies: f64,
    pub cc_time: f64,
}

impl ChampionTotals {
    fn from_row(r: &Row) -> ChampionTotals {
        let f = |n: &str| -> f64 {
            match r.get_ref(n) {
                Ok(ValueRef::Integer(i)) => i as f64,
                Ok(ValueRef::Real(x)) => x,
                _ => 0.0,
            }
        };
        ChampionTotals {
            champion_id: col(r, "champion_id"),
            position: col(r, "position"),
            games: f("games"),
            seconds: f("seconds"),
            dmg_physical: f("dmg_physical"),
            dmg_magic: f("dmg_magic"),
            dmg_true: f("dmg_true"),
            dmg_taken: f("dmg_taken"),
            dmg_mitigated: f("dmg_mitigated"),
            heal: f("heal"),
            heal_allies: f("heal_allies"),
            shield_allies: f("shield_allies"),
            cc_time: f("cc_time"),
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct PendingDetail {
    pub match_id: String,
    pub platform: String,
    pub patch: String,
    pub has_irelia: i64,
    pub game_puuid: Option<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Counts {
    pub players: i64,
    pub otp: i64,
    pub mains: i64,
    pub matches: i64,
    pub games: i64,
    pub current: i64,
}

#[derive(Clone, Debug, PartialEq)]
pub struct TableStat {
    pub name: String,
    pub rows: i64,
    pub bytes: Option<i64>,
}

/// One match whole, as the archive and the copies keep it: its rows in each table (the timeline's data as JSON).
pub type MatchRecord = Map<String, Value>;

// --------------------------------------------------------------------------------------- generic row helpers

/// A SQLite value as JS sees it from node:sqlite (INTEGER and REAL → number, TEXT → string, BLOB → bytes).
pub fn sql_to_json(v: ValueRef) -> Value {
    match v {
        ValueRef::Null => Value::Null,
        ValueRef::Integer(i) => json!(i),
        ValueRef::Real(f) => crate::js::num(f),
        ValueRef::Text(t) => Value::String(String::from_utf8_lossy(t).into_owned()),
        // JSON.stringify(Uint8Array) is an object of index keys.
        ValueRef::Blob(b) => {
            let mut m = Map::new();
            for (i, x) in b.iter().enumerate() {
                m.insert(i.to_string(), json!(x));
            }
            Value::Object(m)
        }
    }
}

/// A JSON value bound as node:sqlite binds a JS value (numbers as integers when integral).
pub fn json_to_sql(v: &Value) -> Sql {
    match v {
        Value::Null => Sql::Null,
        Value::Bool(b) => Sql::Integer(*b as i64),
        Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                Sql::Integer(i)
            } else {
                let f = n.as_f64().unwrap_or(0.0);
                if f == f.trunc() && f.abs() < 9.2e18 {
                    Sql::Integer(f as i64)
                } else {
                    Sql::Real(f)
                }
            }
        }
        Value::String(s) => Sql::Text(s.clone()),
        other => Sql::Text(crate::js::stringify(other)),
    }
}

/// Every row of a query as JSON objects (column order).
pub fn rows_as_json(conn: &Connection, sql: &str, p: &[&dyn ToSql]) -> Result<Vec<Map<String, Value>>> {
    let mut st = conn.prepare_cached(sql)?;
    let names: Vec<String> = st.column_names().map(|s| s.to_string()).collect();
    let mut rows = st.query(p)?;
    let mut out = Vec::new();
    while let Some(r) = rows.next()? {
        let mut m = Map::new();
        for (i, n) in names.iter().enumerate() {
            m.insert(n.clone(), sql_to_json(r.get_ref(i)?));
        }
        out.push(m);
    }
    Ok(out)
}

pub fn gzip(bytes: &[u8]) -> Vec<u8> {
    let mut e = GzEncoder::new(Vec::new(), Compression::default());
    e.write_all(bytes).unwrap();
    e.finish().unwrap()
}

pub fn gunzip(bytes: &[u8]) -> Result<Vec<u8>> {
    let mut d = GzDecoder::new(bytes);
    let mut out = Vec::new();
    d.read_to_end(&mut out)?;
    Ok(out)
}

fn js(v: &[&str]) -> String {
    serde_json::to_string(v).unwrap()
}
fn js_owned(v: &[String]) -> String {
    serde_json::to_string(v).unwrap()
}

// ------------------------------------------------------------------------------------------------- the Db

pub struct OpenOptions {
    /// false: threads that start after the main thread opened and migrated it write nothing on open.
    pub migrate: bool,
    pub busy_timeout_ms: u64,
}

impl Default for OpenOptions {
    fn default() -> Self {
        OpenOptions { migrate: true, busy_timeout_ms: BUSY_TIMEOUT_MS }
    }
}

/// One connection to the database (one per thread, as in 0.x).
pub struct Db {
    pub conn: Connection,
    pub path: PathBuf,
    dicts: Arc<Dictionaries>,
    by_ids: bool,
    columns_of: parking_lot::Mutex<HashMap<String, HashSet<String>>>,
}

fn participants_by_ids(conn: &Connection) -> bool {
    conn.query_row("SELECT type FROM sqlite_master WHERE name = 'participants'", [], |r| r.get::<_, String>(0))
        .optional()
        .ok()
        .flatten()
        .as_deref()
        == Some("view")
}

fn add_columns(conn: &Connection) -> Result<()> {
    for (table, cols) in ADDED_COLUMNS {
        let have: HashSet<String> = {
            let mut st = conn.prepare(&format!("PRAGMA table_info({table})"))?;
            let names = st.query_map([], |r| r.get::<_, String>("name"))?.filter_map(|x| x.ok()).collect();
            names
        };
        for (name, decl) in *cols {
            if !have.contains(*name) {
                conn.execute_batch(&format!("ALTER TABLE {table} ADD COLUMN {name} {decl}"))?;
            }
        }
    }
    Ok(())
}

/// Registers timeline_json(data, data_z) on a connection: a timeline as JSON, from either column.
fn register_timeline_json(conn: &Connection, dicts: Arc<Dictionaries>, file: PathBuf) -> Result<()> {
    conn.create_scalar_function("timeline_json", 2, FunctionFlags::SQLITE_UTF8 | FunctionFlags::SQLITE_DETERMINISTIC, move |ctx| {
        match ctx.get_raw(1) {
            ValueRef::Blob(z) => {
                let lookup = |id: u16| -> Option<Arc<Vec<u8>>> {
                    if let Some(d) = dicts.snapshot().get(&id) {
                        return Some(d.clone());
                    }
                    // A dictionary another connection made since: read the table again on a connection of our own.
                    if let Ok(c) = Connection::open_with_flags(&file, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY) {
                        dicts.reload(&c);
                    }
                    dicts.snapshot().get(&id).cloned()
                };
                let raw = unpack(z, &lookup).map_err(|e| rusqlite::Error::UserFunctionError(e.to_string().into()))?;
                Ok(Sql::Text(String::from_utf8(raw).map_err(|e| rusqlite::Error::UserFunctionError(e.into()))?))
            }
            ValueRef::Text(t) => Ok(Sql::Text(String::from_utf8_lossy(t).into_owned())),
            ValueRef::Null => Ok(Sql::Null),
            ValueRef::Integer(i) => Ok(Sql::Integer(i)),
            ValueRef::Real(f) => Ok(Sql::Real(f)),
        }
    })?;
    Ok(())
}

impl Db {
    /// Opens (and creates or upgrades in place) the database.
    pub fn open(file: &Path, o: OpenOptions) -> Result<Db> {
        if let Some(dir) = file.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let conn = Connection::open(file)?;
        conn.busy_timeout(std::time::Duration::from_millis(o.busy_timeout_ms))?;
        if o.migrate {
            conn.execute_batch("PRAGMA auto_vacuum = INCREMENTAL;")?;
        }
        conn.execute_batch(PRAGMAS)?;
        if o.migrate {
            conn.execute_batch("BEGIN IMMEDIATE")?;
            let r = (|| -> Result<()> {
                conn.execute_batch(SCHEMA)?;
                if !participants_by_ids(&conn) {
                    conn.execute_batch(PARTICIPANTS_TABLE)?;
                }
                add_columns(&conn)?;
                conn.execute_batch(MIGRATE_V3)?;
                conn.execute(
                    "INSERT INTO meta(key, value) VALUES('schema_version', ?) ON CONFLICT(key) DO UPDATE SET value = excluded.value",
                    [SCHEMA_VERSION.to_string()],
                )?;
                Ok(())
            })();
            match r {
                Ok(()) => conn.execute_batch("COMMIT")?,
                Err(e) => {
                    let _ = conn.execute_batch("ROLLBACK");
                    return Err(e);
                }
            }
        }
        let dicts = Arc::new(Dictionaries::new(&conn));
        register_timeline_json(&conn, dicts.clone(), file.to_path_buf())?;
        let by_ids = participants_by_ids(&conn);
        Ok(Db { conn, path: file.to_path_buf(), dicts, by_ids, columns_of: Default::default() })
    }

    pub fn open_default(file: &Path) -> Result<Db> {
        Db::open(file, OpenOptions::default())
    }

    /// Runs `f` in one write transaction (BEGIN IMMEDIATE: the write lock first, waiting busy_timeout for it).
    pub fn tx<T>(&self, f: impl FnOnce() -> Result<T>) -> Result<T> {
        self.conn.execute_batch("BEGIN IMMEDIATE")?;
        match f() {
            Ok(v) => {
                self.conn.execute_batch("COMMIT")?;
                Ok(v)
            }
            Err(e) => {
                let _ = self.conn.execute_batch("ROLLBACK");
                Err(e)
            }
        }
    }

    pub fn participants_by_ids(&self) -> bool {
        self.by_ids
    }

    /// A capture compressed with the database's dictionary (made from this capture when there is none yet).
    pub fn pack_timeline(&self, json: &str) -> Result<Vec<u8>> {
        let raw = json.as_bytes();
        let dict = match self.dicts.current() {
            Some(d) => d,
            None => self.dicts.create(&self.conn, &[raw], "a timeline capture of this database")?,
        };
        pack(raw, Some(&dict))
    }

    pub fn unpack_timeline(&self, z: &[u8]) -> Result<String> {
        let raw = unpack(z, &|id| self.dicts.get(&self.conn, id))?;
        Ok(String::from_utf8(raw)?)
    }

    pub fn current_dictionary(&self) -> Option<Dictionary> {
        self.dicts.current()
    }

    // ---- meta
    pub fn get_meta(&self, key: &str) -> Option<String> {
        self.conn.query_row("SELECT value FROM meta WHERE key = ?", [key], |r| r.get(0)).optional().ok().flatten()
    }
    pub fn set_meta(&self, key: &str, value: &str) -> Result<()> {
        self.conn.prepare_cached("INSERT INTO meta(key, value) VALUES(?, ?) ON CONFLICT(key) DO UPDATE SET value = excluded.value")?.execute([key, value])?;
        Ok(())
    }

    // ---- players
    pub fn upsert_player(&self, puuid: &str, platform: &str, tier: &str, lp: i64) -> Result<()> {
        self.conn
            .prepare_cached(
                "
      INSERT INTO players(puuid, platform, tier, league_points, updated_at) VALUES(?, ?, ?, ?, ?)
      ON CONFLICT(puuid) DO UPDATE SET tier = excluded.tier, league_points = excluded.league_points, updated_at = excluded.updated_at",
            )?
            .execute(params![puuid, platform, tier, lp, now_ms()])?;
        Ok(())
    }
    /// A player found outside the ladder (a harvested Irelia, the user's account): added unchecked, never overwritten.
    pub fn add_candidate(&self, puuid: &str, platform: &str, found_by: &str) -> Result<bool> {
        let n = self
            .conn
            .prepare_cached(
                "
      INSERT INTO players(puuid, platform, tier, league_points, updated_at, found_by) VALUES(?, ?, 'UNKNOWN', 0, ?, ?)
      ON CONFLICT(puuid) DO NOTHING",
            )?
            .execute(params![puuid, platform, now_ms(), found_by])?;
        Ok(n > 0)
    }
    pub fn set_rank(&self, puuid: &str, tier: &str, lp: i64) -> Result<()> {
        let now = now_ms();
        self.conn
            .prepare_cached("UPDATE players SET tier = ?, league_points = ?, rank_checked = ?, updated_at = ? WHERE puuid = ?")?
            .execute(params![tier, lp, now, now, puuid])?;
        Ok(())
    }
    pub fn player(&self, puuid: &str) -> Option<PlayerRow> {
        self.conn.query_row("SELECT * FROM players WHERE puuid = ?", [puuid], |r| Ok(PlayerRow::from_row(r))).optional().ok().flatten()
    }
    pub fn set_pilot(&self, puuid: &str, u: &PilotUpdate) -> Result<()> {
        self.conn
            .prepare_cached(
                "UPDATE players SET is_otp = ?, irelia_mastery = ?, last_checked = ?, pilot_class = ?, irelia_points = ?,
      irelia_last_play = ?, irelia_rank = ? WHERE puuid = ?",
            )?
            .execute(params![
                if u.pilot_class == "otp" { 1 } else { 0 },
                if u.irelia_rank == Some(1) { u.irelia_points } else { 0 },
                now_ms(),
                u.pilot_class,
                u.irelia_points,
                u.irelia_last_play,
                u.irelia_rank,
                puuid
            ])?;
        Ok(())
    }
    /// Pilots on `platform` checked since `since` whose last Irelia game is still before it (0.21.2's recheck).
    pub fn forget_activity_checks(&self, platform: &str, since: i64) -> Result<usize> {
        Ok(self
            .conn
            .prepare_cached(
                "UPDATE players SET last_checked = 0 WHERE platform = ? AND pilot_class IN ('otp', 'main')
      AND last_checked >= ? AND (irelia_last_play IS NULL OR irelia_last_play < ?)",
            )?
            .execute(params![platform, since, since])?)
    }
    pub fn due_pilots(&self, platform: &str, classes: &[String], older_than: i64, active_since: i64, limit: i64) -> Result<Vec<PlayerRow>> {
        let mut st = self.conn.prepare_cached(
            "
      SELECT * FROM players WHERE platform = ? AND pilot_class IN (SELECT value FROM json_each(?))
        AND (last_match_scan IS NULL OR last_match_scan < ?) AND (irelia_last_play IS NULL OR irelia_last_play >= ?)
      ORDER BY irelia_last_play IS NULL, irelia_last_play DESC LIMIT ?",
        )?;
        let rows = st.query_map(params![platform, js_owned(classes), older_than, active_since, limit], |r| Ok(PlayerRow::from_row(r)))?;
        Ok(rows.filter_map(|x| x.ok()).collect())
    }
    pub fn set_irelia_activity(&self, puuid: &str, points: i64, last_play: Option<i64>, rank: Option<i64>) -> Result<()> {
        self.conn
            .prepare_cached("UPDATE players SET irelia_points = ?, irelia_last_play = ?, irelia_rank = ? WHERE puuid = ?")?
            .execute(params![points, last_play, rank, puuid])?;
        Ok(())
    }
    pub fn players_missing_activity(&self) -> Result<Vec<PlayerRow>> {
        let mut st = self.conn.prepare_cached("SELECT * FROM players WHERE is_otp = 1 AND irelia_last_play IS NULL")?;
        let rows = st.query_map([], |r| Ok(PlayerRow::from_row(r)))?;
        Ok(rows.filter_map(|x| x.ok()).collect())
    }
    pub fn tag_own_games(&self, puuid: &str) -> Result<usize> {
        Ok(self
            .conn
            .prepare_cached("UPDATE irelia_games SET pilot_source = 'me' WHERE puuid = ? AND COALESCE(pilot_source, '') <> 'me'")?
            .execute([puuid])?)
    }
    pub fn count_history(&self, puuid: &str, matches: i64, irelia: i64) -> Result<()> {
        self.conn
            .prepare_cached("UPDATE players SET history_matches = history_matches + ?, history_irelia = history_irelia + ? WHERE puuid = ?")?
            .execute(params![matches, irelia, puuid])?;
        Ok(())
    }
    pub fn unranked_candidates(&self, platform: &str, limit: i64) -> Result<Vec<PlayerRow>> {
        let mut st = self.conn.prepare_cached(
            "SELECT * FROM players WHERE platform = ? AND tier = 'UNKNOWN' AND rank_checked IS NULL
      AND found_by = 'harvest' AND pilot_class IN ('otp', 'main') LIMIT ?",
        )?;
        let rows = st.query_map(params![platform, limit], |r| Ok(PlayerRow::from_row(r)))?;
        Ok(rows.filter_map(|x| x.ok()).collect())
    }
    pub fn set_scanned(&self, puuid: &str) -> Result<()> {
        self.conn.prepare_cached("UPDATE players SET last_match_scan = ? WHERE puuid = ?")?.execute(params![now_ms(), puuid])?;
        Ok(())
    }

    // ---- matches and games
    pub fn has_match(&self, id: &str) -> bool {
        self.conn.prepare_cached("SELECT 1 AS x FROM matches WHERE match_id = ?").and_then(|mut s| s.exists([id])).unwrap_or(false)
    }
    /// The stored Irelia game's source: None when the match holds none, Some(None) for a NULL source.
    pub fn game_source(&self, id: &str) -> Option<Option<String>> {
        self.conn
            .query_row("SELECT pilot_source FROM irelia_games WHERE match_id = ?", [id], |r| r.get::<_, Option<String>>(0))
            .optional()
            .ok()
            .flatten()
    }
    /// Records a match; returns false when it was already stored. Also keeps game_versions.
    pub fn insert_match(&self, m: &MatchRow) -> Result<bool> {
        let fresh = self
            .conn
            .prepare_cached(
                "
      INSERT OR IGNORE INTO matches(match_id, platform, patch, game_version, queue_id, game_creation, game_duration, has_irelia, fetched_at)
      VALUES(?, ?, ?, ?, ?, ?, ?, ?, ?)",
            )?
            .execute(params![m.id, m.platform, m.patch, m.version, m.queue, m.creation, m.duration, m.has_irelia as i64, now_ms()])?
            > 0;
        if fresh {
            self.upsert_version(&m.platform, &m.version, &m.patch, m.creation)?;
        }
        Ok(fresh)
    }
    fn upsert_version(&self, platform: &str, version: &str, patch: &str, creation: i64) -> Result<()> {
        self.conn
            .prepare_cached(
                "
      INSERT INTO game_versions(platform, game_version, patch, first_seen, last_seen, games) VALUES(?, ?, ?, ?, ?, 1)
      ON CONFLICT(platform, game_version) DO UPDATE SET first_seen = MIN(first_seen, excluded.first_seen),
        last_seen = MAX(last_seen, excluded.last_seen), games = games + 1",
            )?
            .execute(params![platform, version, patch, creation, creation])?;
        Ok(())
    }
    pub fn set_has_irelia(&self, id: &str) -> Result<()> {
        self.conn.prepare_cached("UPDATE matches SET has_irelia = 1 WHERE match_id = ?")?.execute([id])?;
        Ok(())
    }
    pub fn insert_game(&self, row: &IreliaGameRow) -> Result<()> {
        let sql = format!(
            "
      INSERT OR REPLACE INTO irelia_games({}) VALUES({})",
            GAME_COLUMNS.join(", "),
            GAME_COLUMNS.iter().map(|c| format!(":{c}")).collect::<Vec<_>>().join(", ")
        );
        let vals = row.game_values();
        let mut st = self.conn.prepare_cached(&sql)?;
        st.execute(rusqlite::params_from_iter(vals.iter()))?;
        Ok(())
    }
    pub fn set_game_source(&self, id: &str, source: &str) -> Result<()> {
        self.conn.prepare_cached("UPDATE irelia_games SET pilot_source = ? WHERE match_id = ?")?.execute([source, id])?;
        Ok(())
    }

    fn match_ref(&self, match_id: &str) -> Result<i64> {
        Ok(self.conn.prepare_cached("INSERT INTO match_ids(match_id) VALUES(?) ON CONFLICT(match_id) DO UPDATE SET match_id = excluded.match_id RETURNING id")?.query_row([match_id], |r| r.get(0))?)
    }
    fn puuid_ref(&self, puuid: &str) -> Result<i64> {
        Ok(self.conn.prepare_cached("INSERT INTO puuid_ids(puuid) VALUES(?) ON CONFLICT(puuid) DO UPDATE SET puuid = excluded.puuid RETURNING id")?.query_row([puuid], |r| r.get(0))?)
    }

    /// A participants row as the insert takes it (by ids when participants is the view).
    fn insert_participant_map(&self, r: &Map<String, Value>, or: &str) -> Result<()> {
        if !self.by_ids {
            let sql = format!(
                "INSERT {or} INTO participants({}) VALUES({})",
                PARTICIPANT_COLUMNS.join(", "),
                PARTICIPANT_COLUMNS.iter().map(|_| "?").collect::<Vec<_>>().join(", ")
            );
            let vals: Vec<Sql> = PARTICIPANT_COLUMNS.iter().map(|c| r.get(*c).map(json_to_sql).unwrap_or(Sql::Null)).collect();
            self.conn.prepare_cached(&sql)?.execute(rusqlite::params_from_iter(vals.iter()))?;
        } else {
            let match_ref = self.match_ref(r.get("match_id").and_then(|v| v.as_str()).unwrap_or(""))?;
            let puuid_ref = self.puuid_ref(r.get("puuid").and_then(|v| v.as_str()).unwrap_or(""))?;
            let sql = format!(
                "INSERT {or} INTO participant_rows(match_ref, participant_id, puuid_ref, {}) VALUES(?, ?, ?, {})",
                PARTICIPANT_VALUE_COLUMNS.join(", "),
                PARTICIPANT_VALUE_COLUMNS.iter().map(|_| "?").collect::<Vec<_>>().join(", ")
            );
            let mut vals = vec![Sql::Integer(match_ref), r.get("participant_id").map(json_to_sql).unwrap_or(Sql::Null), Sql::Integer(puuid_ref)];
            vals.extend(PARTICIPANT_VALUE_COLUMNS.iter().map(|c| r.get(*c).map(json_to_sql).unwrap_or(Sql::Null)));
            self.conn.prepare_cached(&sql)?.execute(rusqlite::params_from_iter(vals.iter()))?;
        }
        Ok(())
    }

    /// Replaces a match's participants (idempotent).
    pub fn insert_participants(&self, match_id: &str, rows: &[ParticipantRow]) -> Result<()> {
        if self.by_ids {
            self.conn.prepare_cached("DELETE FROM participant_rows WHERE match_ref = (SELECT id FROM match_ids WHERE match_id = ?)")?.execute([match_id])?;
        } else {
            self.conn.prepare_cached("DELETE FROM participants WHERE match_id = ?")?.execute([match_id])?;
        }
        for r in rows {
            self.insert_participant_map(&r.to_map(), "OR REPLACE")?;
        }
        Ok(())
    }

    /// Stores a game's timeline capture (JSON), compressed.
    pub fn insert_timeline(&self, match_id: &str, interval: i64, data: &str) -> Result<()> {
        let z = self.pack_timeline(data)?;
        self.conn
            .prepare_cached("INSERT OR REPLACE INTO timelines(match_id, frame_interval, data, data_z) VALUES(?, ?, '', ?)")?
            .execute(params![match_id, interval, z])?;
        Ok(())
    }
    pub fn set_quality(&self, match_id: &str, remake: bool, end_result: Option<&str>, detail_version: i64) -> Result<()> {
        self.conn
            .prepare_cached("UPDATE matches SET remake = ?, end_result = ?, detail_version = ? WHERE match_id = ?")?
            .execute(params![remake as i64, end_result, detail_version, match_id])?;
        Ok(())
    }
    pub fn set_leaver(&self, match_id: &str, leaver: bool) -> Result<()> {
        self.conn.prepare_cached("UPDATE matches SET leaver = ? WHERE match_id = ?")?.execute(params![leaver as i64, match_id])?;
        Ok(())
    }
    pub fn set_opponents(&self, match_id: &str, participant_id: i64, position: Option<i64>, timeline: Option<i64>, effective: Option<i64>) -> Result<()> {
        self.conn
            .prepare_cached(
                "UPDATE irelia_games SET participant_id = ?, position_opponent_id = ?, timeline_opponent_id = ?,
      opponent_champion_id = ? WHERE match_id = ?",
            )?
            .execute(params![participant_id, position, timeline, effective, match_id])?;
        Ok(())
    }
    /// Pilot metadata for a game stored before v3, from the player's row.
    pub fn fill_pilot_meta(&self, match_id: &str, p: &PlayerRow) -> Result<()> {
        let top1: Option<i64> = if p.irelia_rank == Some(1) || p.is_otp == Some(1) {
            Some(1)
        } else if p.irelia_rank.map(|r| r != 0).unwrap_or(false) {
            Some(0)
        } else {
            None
        };
        self.conn
            .prepare_cached(
                "UPDATE irelia_games SET pilot_tier = ?, pilot_lp = ?, pilot_mastery = ?, pilot_last_play = ?, pilot_top1 = ?
      WHERE match_id = ? AND pilot_tier IS NULL",
            )?
            .execute(params![p.tier, p.league_points, p.irelia_points.or(p.irelia_mastery), p.irelia_last_play, top1, match_id])?;
        Ok(())
    }
    pub fn pending_detail(&self, version: i64, limit: i64) -> Result<Vec<PendingDetail>> {
        let mut st = self.conn.prepare_cached(
            "SELECT m.match_id, m.platform, m.patch, m.has_irelia, g.puuid AS game_puuid
      FROM matches m LEFT JOIN irelia_games g USING(match_id) WHERE m.detail_version < ? ORDER BY m.match_id LIMIT ?",
        )?;
        let rows = st.query_map(params![version, limit], |r| {
            Ok(PendingDetail {
                match_id: r.get(0)?,
                platform: r.get(1)?,
                patch: r.get(2)?,
                has_irelia: r.get(3)?,
                game_puuid: r.get(4)?,
            })
        })?;
        Ok(rows.filter_map(|x| x.ok()).collect())
    }
    pub fn count_pending_detail(&self, version: i64) -> i64 {
        self.conn.query_row("SELECT COUNT(*) AS n FROM matches WHERE detail_version < ?", [version], |r| r.get(0)).unwrap_or(0)
    }
    pub fn mark_detail(&self, match_id: &str, version: i64) -> Result<()> {
        self.conn.prepare_cached("UPDATE matches SET detail_version = ? WHERE match_id = ?")?.execute(params![version, match_id])?;
        Ok(())
    }
    pub fn reset_detail(&self) -> Result<usize> {
        Ok(self.conn.execute("UPDATE matches SET detail_version = 0", [])?)
    }
    pub fn games_for_patches(&self, patches: &[String], sources: &[String]) -> Result<Vec<IreliaGameRow>> {
        let sql = format!(
            "SELECT g.*, m.remake, m.leaver, m.end_result, m.game_version FROM irelia_games g JOIN matches m USING(match_id)
      WHERE g.patch IN (SELECT value FROM json_each(?)) AND {SOURCE_FILTER}"
        );
        let mut st = self.conn.prepare_cached(&sql)?;
        let rows = st.query_map(params![js_owned(patches), js_owned(sources)], |r| Ok(IreliaGameRow::from_row(r)))?;
        Ok(rows.filter_map(|x| x.ok()).collect())
    }
    pub fn counts(&self, current_patch: &str) -> Result<Counts> {
        Ok(self.conn.query_row(
            "SELECT
      (SELECT COUNT(*) FROM players) AS players,
      (SELECT COUNT(*) FROM players WHERE is_otp = 1) AS otp,
      (SELECT COUNT(*) FROM players WHERE pilot_class = 'main') AS mains,
      (SELECT COUNT(*) FROM matches) AS matches,
      (SELECT COUNT(*) FROM irelia_games) AS games,
      (SELECT COUNT(*) FROM irelia_games WHERE patch = ?) AS current",
            [current_patch],
            |r| Ok(Counts { players: r.get(0)?, otp: r.get(1)?, mains: r.get(2)?, matches: r.get(3)?, games: r.get(4)?, current: r.get(5)? }),
        )?)
    }
    /// Stored Irelia games per source, in query order.
    pub fn source_counts(&self) -> Result<Vec<(String, i64)>> {
        let mut st = self.conn.prepare_cached("SELECT COALESCE(pilot_source, 'otp') AS source, COUNT(*) AS n FROM irelia_games GROUP BY 1")?;
        let rows = st.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?;
        Ok(rows.filter_map(|x| x.ok()).collect())
    }
    /// Build-source Irelia games per patch, newest first, split by position.
    pub fn patch_breakdown(&self, sources: &[String]) -> Result<Vec<(String, i64, Vec<(String, i64)>)>> {
        let mut st = self.conn.prepare_cached(&format!(
            "SELECT patch, position, COUNT(*) AS games FROM irelia_games g WHERE {SOURCE_FILTER} GROUP BY patch, position"
        ))?;
        let lanes: Vec<(String, String, i64)> =
            st.query_map([js_owned(sources)], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?.filter_map(|x| x.ok()).collect();
        let mut st = self.conn.prepare_cached(&format!(
            "SELECT patch, COUNT(*) AS games FROM irelia_games g WHERE {SOURCE_FILTER} GROUP BY patch ORDER BY patch DESC"
        ))?;
        let patches: Vec<(String, i64)> = st.query_map([js_owned(sources)], |r| Ok((r.get(0)?, r.get(1)?)))?.filter_map(|x| x.ok()).collect();
        Ok(patches
            .into_iter()
            .map(|(p, g)| {
                let l = lanes.iter().filter(|l| l.0 == p).map(|l| (l.1.clone(), l.2)).collect();
                (p, g, l)
            })
            .collect())
    }
    /// Opponents Irelia faced, optionally only in one position.
    pub fn opponent_counts(&self, patches: &[String], position: Option<&str>, sources: &[String]) -> Result<Vec<(i64, i64)>> {
        let rows: Vec<(i64, i64)> = match position {
            Some(pos) => {
                let mut st = self.conn.prepare_cached(&format!(
                    "SELECT opponent_champion_id AS id, COUNT(*) AS games FROM irelia_games g
      WHERE patch IN (SELECT value FROM json_each(?)) AND position = ? AND opponent_champion_id IS NOT NULL AND {SOURCE_FILTER}
      GROUP BY opponent_champion_id"
                ))?;
                let r = st.query_map(params![js_owned(patches), pos, js_owned(sources)], |r| Ok((r.get(0)?, r.get(1)?)))?.filter_map(|x| x.ok()).collect();
                r
            }
            None => {
                let mut st = self.conn.prepare_cached(&format!(
                    "SELECT opponent_champion_id AS id, COUNT(*) AS games FROM irelia_games g
      WHERE patch IN (SELECT value FROM json_each(?)) AND opponent_champion_id IS NOT NULL AND {SOURCE_FILTER} GROUP BY opponent_champion_id"
                ))?;
                let r = st.query_map(params![js_owned(patches), js_owned(sources)], |r| Ok((r.get(0)?, r.get(1)?)))?.filter_map(|x| x.ok()).collect();
                r
            }
        };
        Ok(rows)
    }
    pub fn game_versions(&self) -> Result<Vec<crate::patches::VersionRow>> {
        let mut st = self.conn.prepare_cached("SELECT platform, game_version, patch, first_seen, last_seen, games FROM game_versions")?;
        let rows = st.query_map([], |r| {
            Ok(crate::patches::VersionRow {
                platform: r.get(0)?,
                game_version: r.get(1)?,
                patch: r.get(2)?,
                first_seen: r.get(3)?,
                last_seen: r.get(4)?,
                games: r.get(5)?,
            })
        })?;
        Ok(rows.filter_map(|x| x.ok()).collect())
    }
    pub fn rebuild_game_versions(&self) -> Result<()> {
        self.conn.execute(
            "INSERT INTO game_versions(platform, game_version, patch, first_seen, last_seen, games)
      SELECT platform, game_version, patch, MIN(game_creation), MAX(game_creation), COUNT(*) FROM matches GROUP BY platform, game_version
      ON CONFLICT(platform, game_version) DO UPDATE SET first_seen = MIN(first_seen, excluded.first_seen),
        last_seen = MAX(last_seen, excluded.last_seen), games = MAX(games, excluded.games)",
            [],
        )?;
        Ok(())
    }

    // ---- old patches: purge and archive
    fn purge_queue(&self, kind: &str) -> Result<()> {
        self.conn.prepare_cached("DELETE FROM match_queue WHERE match_id NOT IN (SELECT match_id FROM matches) AND kind = ?")?.execute([kind])?;
        Ok(())
    }

    /// Deletes matches not on one of the given patches, keeping their per-champion totals first.
    pub fn purge_stale(&self, keep_patches: &[String]) -> Result<usize> {
        let keep = js_owned(keep_patches);
        self.tx(|| {
            self.conn.execute(&keep_champion_stats_sql("m.patch NOT IN (SELECT value FROM json_each(?))"), [&keep])?;
            let removed = self.conn.execute("DELETE FROM matches WHERE patch NOT IN (SELECT value FROM json_each(?))", [&keep])?;
            self.purge_queue("timeline")?;
            Ok(removed)
        })
    }

    fn archive_rows(&self, table: &str, match_id: &str) -> Result<Vec<Map<String, Value>>> {
        let sql = if table == "timelines" {
            "SELECT match_id, frame_interval, timeline_json(data, data_z) AS data FROM timelines WHERE match_id = ?".to_string()
        } else {
            format!("SELECT * FROM {table} WHERE match_id = ?")
        };
        rows_as_json(&self.conn, &sql, &[&match_id])
    }

    /// One stored match whole, in the archive's shape.
    pub fn match_record(&self, match_id: &str) -> Result<MatchRecord> {
        let mut m = Map::new();
        for t in ARCHIVED_TABLES {
            m.insert(t.into(), Value::Array(self.archive_rows(t, match_id)?.into_iter().map(Value::Object).collect()));
        }
        Ok(m)
    }

    /// Archives up to `limit` matches not on one of `keep_patches`. Returns how many it moved (0: none left).
    pub fn archive_batch(&self, keep_patches: &[String], limit: i64) -> Result<usize> {
        let keep = js_owned(keep_patches);
        self.tx(|| {
            let batch: Vec<(String, String)> = {
                let mut st = self.conn.prepare_cached("SELECT match_id, patch FROM matches WHERE patch NOT IN (SELECT value FROM json_each(?)) LIMIT ?")?;
                let r = st.query_map(params![keep, limit], |r| Ok((r.get(0)?, r.get(1)?)))?.filter_map(|x| x.ok()).collect();
                r
            };
            if batch.is_empty() {
                return Ok(0);
            }
            let ids: Vec<String> = batch.iter().map(|b| b.0.clone()).collect();
            self.conn.execute(&keep_champion_stats_sql("m.match_id IN (SELECT value FROM json_each(?))"), [js_owned(&ids)])?;
            let now = now_ms();
            for (match_id, patch) in &batch {
                let rec = self.match_record(match_id)?;
                let data = gzip(crate::js::stringify(&Value::Object(rec)).as_bytes());
                self.conn
                    .prepare_cached("INSERT OR REPLACE INTO archive(match_id, patch, archived_at, data) VALUES(?, ?, ?, ?)")?
                    .execute(params![match_id, patch, now, data])?;
                self.conn.prepare_cached("DELETE FROM matches WHERE match_id = ?")?.execute([match_id])?;
            }
            self.purge_queue("timeline")?;
            Ok(batch.len())
        })
    }

    /// Inserts a row restored from the archive: only the table's own columns.
    fn restore_row(&self, table: &str, row: &Map<String, Value>) -> Result<()> {
        let have = {
            let mut cache = self.columns_of.lock();
            if !cache.contains_key(table) {
                let mut st = self.conn.prepare(&format!("PRAGMA table_info({table})"))?;
                let names: HashSet<String> = st.query_map([], |r| r.get::<_, String>("name"))?.filter_map(|x| x.ok()).collect();
                cache.insert(table.to_string(), names);
            }
            cache[table].clone()
        };
        let cols: Vec<&String> = row.keys().filter(|c| have.contains(*c)).collect();
        let sql = format!(
            "INSERT OR IGNORE INTO {table}({}) VALUES({})",
            cols.iter().map(|c| c.as_str()).collect::<Vec<_>>().join(", "),
            cols.iter().map(|_| "?").collect::<Vec<_>>().join(", ")
        );
        let vals: Vec<Sql> = cols.iter().map(|c| match &row[*c] {
            // data_z made by pack_timeline is passed as bytes (see prepare_restore).
            Value::Object(o) if o.get("$bytes").is_some() => Sql::Blob(
                o["$bytes"].as_array().map(|a| a.iter().map(|x| x.as_u64().unwrap_or(0) as u8).collect()).unwrap_or_default(),
            ),
            v => json_to_sql(v),
        }).collect();
        self.conn.prepare_cached(&sql)?.execute(rusqlite::params_from_iter(vals.iter()))?;
        Ok(())
    }

    /// A record's timelines compressed as every timeline is stored now (before the write lock).
    fn prepare_record(&self, mut rows: Map<String, Value>, only_nonempty: bool) -> Result<Map<String, Value>> {
        if let Some(Value::Array(tls)) = rows.get_mut("timelines") {
            for row in tls.iter_mut() {
                if let Value::Object(o) = row {
                    if let Some(Value::String(d)) = o.get("data").cloned() {
                        if only_nonempty && d.is_empty() {
                            continue;
                        }
                        let z = self.pack_timeline(&d)?;
                        o.insert("data".into(), json!(""));
                        o.insert("data_z".into(), json!({ "$bytes": z }));
                    }
                }
            }
        } else {
            rows.insert("timelines".into(), json!([]));
        }
        Ok(rows)
    }

    fn insert_record_rows(&self, rows: &Map<String, Value>, source: Option<&str>) -> Result<()> {
        for t in ARCHIVED_TABLES {
            if let Some(Value::Array(list)) = rows.get(t) {
                for row in list {
                    let Some(o) = row.as_object() else { continue };
                    if t == "participants" && self.by_ids {
                        self.insert_participant_map(o, "OR IGNORE")?;
                    } else if t == "irelia_games" && source.is_some() {
                        let mut o2 = o.clone();
                        o2.insert("pilot_source".into(), json!(source.unwrap()));
                        self.restore_row(t, &o2)?;
                    } else {
                        self.restore_row(t, o)?;
                    }
                }
            }
        }
        Ok(())
    }

    /// Puts up to `limit` archived matches (of `patches`; None: any) back into the working tables.
    pub fn restore_batch(&self, patches: Option<&[String]>, limit: i64) -> Result<usize> {
        let prepare = |data: &[u8]| -> Result<Map<String, Value>> {
            let rows: Value = serde_json::from_slice(&gunzip(data)?)?;
            self.prepare_record(rows.as_object().cloned().unwrap_or_default(), false)
        };
        let batch: Vec<(String, Vec<u8>)> = {
            let mut st = self.conn.prepare_cached("SELECT match_id, data FROM archive WHERE patch IN (SELECT value FROM json_each(?)) OR ? LIMIT ?")?;
            let p = patches.map(js_owned).unwrap_or_else(|| "[]".into());
            let r = st.query_map(params![p, if patches.is_some() { 0 } else { 1 }, limit], |r| Ok((r.get(0)?, r.get(1)?)))?.filter_map(|x| x.ok()).collect();
            r
        };
        let mut prepared = Vec::new();
        for (id, data) in batch {
            let rows = prepare(&data)?;
            prepared.push((id, data, rows));
        }
        if prepared.is_empty() {
            return Ok(0);
        }
        self.tx(|| {
            let mut restored: Vec<String> = Vec::new();
            for (match_id, data, read) in &prepared {
                let now: Option<Vec<u8>> =
                    self.conn.query_row("SELECT data FROM archive WHERE match_id = ?", [match_id], |r| r.get(0)).optional()?;
                let Some(now) = now else { continue };
                let rows = if now == *data { read.clone() } else { prepare(&now)? };
                self.insert_record_rows(&rows, None)?;
                if !self.has_match(match_id) {
                    bail!("Could not restore {match_id}: nothing was changed");
                }
                self.conn.prepare_cached("DELETE FROM archive WHERE match_id = ?")?.execute([match_id])?;
                restored.push(match_id.clone());
            }
            let sql = format!(
                "UPDATE champion_patch_stats AS s SET {}
      FROM ({}) AS x
      WHERE s.patch = x.patch AND s.champion_id = x.champion_id AND s.position = x.position",
                TOTAL_COLUMNS.iter().map(|c| format!("{c} = s.{c} - x.{c}")).collect::<Vec<_>>().join(", "),
                champion_totals_of("m.match_id IN (SELECT value FROM json_each(?))")
            );
            self.conn.execute(&sql, [js_owned(&restored)])?;
            self.conn.execute("DELETE FROM champion_patch_stats WHERE games <= 0", [])?;
            Ok(restored.len())
        })
    }

    /// Every stored Irelia game's match id, oldest first.
    pub fn irelia_match_ids(&self) -> Result<Vec<String>> {
        let mut st = self.conn.prepare_cached("SELECT match_id FROM irelia_games ORDER BY match_id")?;
        let r = st.query_map([], |r| r.get(0))?.filter_map(|x| x.ok()).collect();
        Ok(r)
    }

    /// Stores matches copied from another database, in one transaction; a match stored here already is skipped.
    pub fn import_records(&self, records: Vec<MatchRecord>, source: Option<&str>) -> Result<usize> {
        let mut ready = Vec::new();
        for r in records {
            ready.push(self.prepare_record(r, true)?);
        }
        self.tx(|| {
            let mut added = 0;
            for rec in &ready {
                let Some(m) = rec.get("matches").and_then(|a| a.get(0)).and_then(|m| m.as_object()) else { continue };
                let Some(mid) = m.get("match_id").and_then(|v| v.as_str()) else { continue };
                if self.has_match(mid) {
                    continue;
                }
                self.insert_record_rows(rec, source)?;
                let s = |k: &str| m.get(k).and_then(|v| v.as_str()).unwrap_or("").to_string();
                let creation = m.get("game_creation").and_then(|v| v.as_i64()).unwrap_or(0);
                self.upsert_version(&s("platform"), &s("game_version"), &s("patch"), creation)?;
                added += 1;
            }
            Ok(added)
        })
    }

    /// For an export copy only: other players' PUUIDs in the matches are blanked (`keep` are kept).
    pub fn strip_other_players(&self, keep: &[String]) -> Result<()> {
        let k = js_owned(keep);
        self.tx(|| {
            if self.by_ids {
                self.conn.execute("INSERT OR IGNORE INTO puuid_ids(puuid) VALUES('')", [])?;
                self.conn.execute(
                    "UPDATE participant_rows SET puuid_ref = (SELECT id FROM puuid_ids WHERE puuid = '')
          WHERE puuid_ref NOT IN (SELECT id FROM puuid_ids WHERE puuid IN (SELECT value FROM json_each(?)))",
                    [&k],
                )?;
                self.conn.execute("DELETE FROM puuid_ids WHERE puuid <> '' AND id NOT IN (SELECT puuid_ref FROM participant_rows)", [])?;
            } else {
                self.conn.execute("UPDATE participants SET puuid = '' WHERE puuid NOT IN (SELECT value FROM json_each(?))", [&k])?;
            }
            let keep_set: HashSet<&String> = keep.iter().collect();
            let rows: Vec<(String, Vec<u8>)> = {
                let mut st = self.conn.prepare("SELECT match_id, data FROM archive")?;
                let r = st.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?.filter_map(|x| x.ok()).collect();
                r
            };
            for (id, data) in rows {
                let mut rec: Value = serde_json::from_slice(&gunzip(&data)?)?;
                if let Some(Value::Array(ps)) = rec.get_mut("participants") {
                    for p in ps.iter_mut() {
                        let pu = p.get("puuid").and_then(|v| v.as_str()).map(String::from);
                        if !pu.as_ref().map(|x| keep_set.contains(x)).unwrap_or(false) {
                            p["puuid"] = json!("");
                        }
                    }
                }
                self.conn.execute("UPDATE archive SET data = ? WHERE match_id = ?", params![gzip(crate::js::stringify(&rec).as_bytes()), id])?;
            }
            self.conn.execute("DELETE FROM match_queue", [])?;
            Ok(())
        })
    }

    /// The stored timelines (JSON) of these matches, by match id.
    pub fn timelines_of(&self, match_ids: &[String]) -> Result<HashMap<String, String>> {
        let mut st = self.conn.prepare_cached("SELECT match_id, timeline_json(data, data_z) AS data FROM timelines WHERE match_id IN (SELECT value FROM json_each(?))")?;
        let r = st.query_map([js_owned(match_ids)], |r| Ok((r.get(0)?, r.get(1)?)))?.filter_map(|x| x.ok()).collect();
        Ok(r)
    }
    pub fn has_archivable(&self, keep_patches: &[String]) -> bool {
        self.conn
            .prepare_cached("SELECT 1 AS x FROM matches WHERE patch NOT IN (SELECT value FROM json_each(?)) LIMIT 1")
            .and_then(|mut s| s.exists([js_owned(keep_patches)]))
            .unwrap_or(false)
    }
    pub fn stale_count(&self, keep_patches: &[String]) -> i64 {
        self.conn
            .query_row("SELECT COUNT(*) AS n FROM matches WHERE patch NOT IN (SELECT value FROM json_each(?))", [js_owned(keep_patches)], |r| r.get(0))
            .unwrap_or(0)
    }
    /// Every known match id's patch, stored or archived.
    pub fn match_patches(&self) -> Result<HashMap<String, String>> {
        let mut st = self.conn.prepare("SELECT match_id, patch FROM matches UNION ALL SELECT match_id, patch FROM archive")?;
        let r = st.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?.filter_map(|x| x.ok()).collect();
        Ok(r)
    }
    /// Archived matches per patch, with their compressed size.
    pub fn archive_summary(&self) -> Result<Vec<(String, i64, i64)>> {
        let mut st = self.conn.prepare("SELECT patch, COUNT(*) AS games, SUM(LENGTH(data)) AS bytes FROM archive GROUP BY patch ORDER BY patch")?;
        let r = st.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get::<_, Option<i64>>(2)?.unwrap_or(0))))?.filter_map(|x| x.ok()).collect();
        Ok(r)
    }

    // ---- match queue
    pub fn enqueue(&self, m: &QueuedMatch) -> Result<()> {
        self.conn
            .prepare_cached(
                "INSERT INTO match_queue(match_id, platform, kind, puuid, source, score, added_at) VALUES(?, ?, ?, ?, ?, ?, ?)
      ON CONFLICT(match_id) DO UPDATE SET score = MAX(score, excluded.score),
        source = CASE WHEN excluded.source = 'me' THEN 'me' ELSE source END,
        puuid = CASE WHEN excluded.source = 'me' THEN excluded.puuid ELSE puuid END",
            )?
            .execute(params![m.match_id, m.platform, m.kind, m.puuid, m.source, m.score, now_ms()])?;
        Ok(())
    }
    pub fn next_queued_batch(&self, platforms: &[String], limit: i64) -> Result<Vec<QueuedMatch>> {
        let mut st = self.conn.prepare_cached(
            "SELECT * FROM match_queue WHERE platform IN (SELECT value FROM json_each(?))
      ORDER BY score DESC, match_id DESC LIMIT ?",
        )?;
        let r = st.query_map(params![js_owned(platforms), limit], |r| Ok(QueuedMatch::from_row(r)))?.filter_map(|x| x.ok()).collect();
        Ok(r)
    }
    /// Players on `platform` whose mastery should be (re)checked, priorities lo..hi, most valuable first.
    pub fn mastery_work(&self, platform: &str, pilot_before: i64, other_before: i64, lo: i64, hi: i64, limit: i64) -> Result<Vec<PlayerRow>> {
        let mut st = self.conn.prepare_cached(
            "SELECT * FROM (SELECT *, CASE
        WHEN is_otp IS NULL AND found_by <> 'ladder' THEN 0
        WHEN pilot_class IN ('otp', 'main') AND (last_checked IS NULL OR last_checked < :pilotBefore) THEN 1
        WHEN is_otp IS NULL OR irelia_points IS NULL THEN 2
        WHEN last_checked < :otherBefore THEN 3
      END AS prio FROM players WHERE platform = :platform)
      WHERE prio BETWEEN :lo AND :hi ORDER BY prio, league_points DESC LIMIT :n",
        )?;
        let r = st
            .query_map(
                rusqlite::named_params! { ":platform": platform, ":pilotBefore": pilot_before, ":otherBefore": other_before, ":lo": lo, ":hi": hi, ":n": limit },
                |r| Ok(PlayerRow::from_row(r)),
            )?
            .filter_map(|x| x.ok())
            .collect();
        Ok(r)
    }
    pub fn next_queued(&self, platforms: &[String]) -> Option<QueuedMatch> {
        self.conn
            .query_row(
                "SELECT * FROM match_queue WHERE platform IN (SELECT value FROM json_each(?)) ORDER BY score DESC, match_id DESC LIMIT 1",
                [js_owned(platforms)],
                |r| Ok(QueuedMatch::from_row(r)),
            )
            .optional()
            .ok()
            .flatten()
    }
    pub fn dequeue(&self, id: &str) -> Result<()> {
        self.conn.prepare_cached("DELETE FROM match_queue WHERE match_id = ?")?.execute([id])?;
        Ok(())
    }
    pub fn drop_older_queued(&self, puuid: &str, platform: &str, older_than_id: i64) -> Result<usize> {
        Ok(self
            .conn
            .prepare_cached(
                "DELETE FROM match_queue WHERE puuid = ? AND kind = 'match' AND platform = ?
      AND CAST(substr(match_id, instr(match_id, '_') + 1) AS INTEGER) < ?",
            )?
            .execute(params![puuid, platform, older_than_id])?)
    }
    /// Queued work per platform and kind.
    pub fn queue_size(&self) -> Result<Vec<(String, String, i64)>> {
        let mut st = self.conn.prepare_cached("SELECT platform, kind, COUNT(*) AS n FROM match_queue GROUP BY platform, kind")?;
        let r = st.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?.filter_map(|x| x.ok()).collect();
        Ok(r)
    }

    // ---- storage
    const TABLE_ROWS: &'static str = "SELECT 'players' AS name, COUNT(*) AS n FROM players UNION ALL SELECT 'matches', COUNT(*) FROM matches
      UNION ALL SELECT 'irelia_games', COUNT(*) FROM irelia_games UNION ALL SELECT 'participants', COUNT(*) FROM participants
      UNION ALL SELECT 'timelines', COUNT(*) FROM timelines UNION ALL SELECT 'game_versions', COUNT(*) FROM game_versions
      UNION ALL SELECT 'champion_patch_stats', COUNT(*) FROM champion_patch_stats UNION ALL SELECT 'match_queue', COUNT(*) FROM match_queue
      UNION ALL SELECT 'archive', COUNT(*) FROM archive";

    /// Row counts per table, and bytes when SQLite's dbstat is available.
    pub fn table_stats(&self) -> Result<Vec<TableStat>> {
        let mut st = self.conn.prepare(Self::TABLE_ROWS)?;
        let rows: Vec<(String, i64)> = st.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?.filter_map(|x| x.ok()).collect();
        let bytes: HashMap<String, i64> = self
            .conn
            .prepare(
                "SELECT COALESCE(i.tbl_name, d.name) AS name, SUM(d.pgsize) AS b FROM dbstat d
          LEFT JOIN sqlite_schema i ON i.name = d.name WHERE d.aggregate = TRUE GROUP BY 1",
            )
            .and_then(|mut st| {
                let r: HashMap<String, i64> = st.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?.filter_map(|x| x.ok()).collect();
                Ok(r)
            })
            .unwrap_or_default();
        Ok(rows.into_iter().map(|(name, n)| TableStat { bytes: bytes.get(&name).copied(), name, rows: n }).collect())
    }
    /// Row counts per table, used to verify a copied database.
    pub fn row_counts(&self) -> Result<Vec<(String, i64)>> {
        let mut st = self.conn.prepare(Self::TABLE_ROWS)?;
        let r = st.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?.filter_map(|x| x.ok()).collect();
        Ok(r)
    }
    /// Consistent, compacted copy of the live database (WAL included) at `file`, which must not exist.
    pub fn vacuum_into(&self, file: &Path) -> Result<()> {
        self.conn.execute("VACUUM INTO ?", [file.to_string_lossy()])?;
        Ok(())
    }

    /// Compresses up to `limit` plain timelines after rowid `after`. Returns the last rowid looked at (None: none
    /// left) and how many were converted.
    pub fn compress_timelines(&self, after: i64, limit: i64) -> Result<(Option<i64>, usize)> {
        let rows: Vec<(i64, String, String)> = {
            let mut st = self.conn.prepare_cached("SELECT rowid AS id, match_id, data FROM timelines WHERE rowid > ? AND data_z IS NULL ORDER BY rowid LIMIT ?")?;
            let r = st.query_map(params![after, limit], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?.filter_map(|x| x.ok()).collect();
            r
        };
        let last: Option<i64> = match rows.last() {
            Some(r) => Some(r.0),
            None => self.conn.query_row(
                "SELECT MAX(rowid) AS last FROM (SELECT rowid FROM timelines WHERE rowid > ? ORDER BY rowid LIMIT ?)",
                params![after, limit],
                |r| r.get(0),
            )?,
        };
        let Some(last) = last else { return Ok((None, 0)) };
        let mut packed = Vec::new();
        for (id, match_id, data) in rows {
            let z = self.pack_timeline(&data)?;
            if self.unpack_timeline(&z)? != data {
                bail!("Timeline {match_id} did not compress back to itself: nothing was changed");
            }
            packed.push((id, data, z));
        }
        let converted = self.tx(|| {
            let mut n = 0;
            for (id, data, z) in &packed {
                n += self.conn.prepare_cached("UPDATE timelines SET data = '', data_z = ? WHERE rowid = ? AND data_z IS NULL AND data = ?")?.execute(params![z, id, data])?;
            }
            Ok(n)
        })?;
        Ok((Some(last), converted))
    }
    pub fn last_timeline_row(&self) -> i64 {
        self.conn.query_row("SELECT COALESCE(MAX(rowid), 0) AS n FROM timelines", [], |r| r.get(0)).unwrap_or(0)
    }
    pub fn plain_timelines_after(&self, after: i64) -> bool {
        self.conn.prepare_cached("SELECT 1 AS x FROM timelines WHERE rowid > ? AND data_z IS NULL LIMIT 1").and_then(|mut s| s.exists([after])).unwrap_or(false)
    }
    /// Gives free pages back to the file system (auto_vacuum = INCREMENTAL).
    pub fn reclaim(&self) -> Result<()> {
        self.conn.execute_batch("PRAGMA incremental_vacuum;")?;
        self.conn.query_row("PRAGMA wal_checkpoint(PASSIVE);", [], |_| Ok(())).ok();
        Ok(())
    }

    /// Participants by integer ids (Compact database). See db.ts normalizeParticipants. Returns what it did (also
    /// recorded in meta participants_layout). Open the database again afterwards.
    pub fn normalize_participants(&self) -> Result<Value> {
        let record = |mut r: Map<String, Value>| -> Result<Value> {
            r.insert("at".into(), json!(now_ms()));
            self.set_meta("participants_layout", &crate::js::stringify(&Value::Object(r.clone())))?;
            r.shift_remove("at");
            Ok(Value::Object(r))
        };
        if self.by_ids {
            return Ok(json!({ "done": false, "why": "already by ids" }));
        }
        let pages = |names: &[&str]| -> Result<i64> {
            Ok(self.conn.query_row(
                "SELECT COALESCE(SUM(pgsize), 0) AS b FROM dbstat WHERE name IN (SELECT value FROM json_each(?))",
                [js(names)],
                |r| r.get(0),
            )?)
        };
        let total: i64 = match self.conn.query_row("SELECT COALESCE(SUM(pgsize), 0) AS b FROM dbstat", [], |r| r.get(0)) {
            Ok(t) => t,
            Err(_) => return Ok(json!({ "done": false, "why": "the database can't be measured here (no dbstat)" })),
        };
        let before = pages(&["participants", "idx_participants_champion"])?;
        let share = if total > 0 { before as f64 / total as f64 } else { 0.0 };
        if share <= PARTICIPANTS_SHARE {
            let mut m = Map::new();
            m.insert("done".into(), json!(false));
            m.insert("share".into(), crate::js::num(share));
            m.insert("before".into(), json!(before));
            m.insert(
                "why".into(),
                json!(format!(
                    "participants are {} % of the database (the rebuild needs over {} %)",
                    crate::js::to_fixed(share * 100.0, 0),
                    crate::js::num_to_string(PARTICIPANTS_SHARE * 100.0)
                )),
            );
            return record(m);
        }
        self.tx(|| {
            self.conn.execute_batch(PARTICIPANTS_BY_IDS)?;
            let count = |sql: &str| -> Result<i64> { Ok(self.conn.query_row(&format!("SELECT COUNT(*) AS n FROM ({sql})"), [], |r| r.get(0))?) };
            let rows = count("SELECT 1 FROM participants")?;
            if count("SELECT 1 FROM participant_rows")? != rows
                || count(&format!("SELECT * FROM participants EXCEPT {PARTICIPANTS_VIEW_SELECT}"))? != 0
                || count(&format!("{PARTICIPANTS_VIEW_SELECT} EXCEPT SELECT * FROM participants"))? != 0
            {
                bail!("Participants by ids did not read back the same rows: nothing was changed");
            }
            let after = pages(&[
                "participant_rows",
                "idx_participant_rows_champion",
                "match_ids",
                "puuid_ids",
                "sqlite_autoindex_match_ids_1",
                "sqlite_autoindex_puuid_ids_1",
            ])?;
            let mut m = Map::new();
            if after >= before {
                self.conn.execute_batch("DROP TABLE participant_rows; DROP TABLE match_ids; DROP TABLE puuid_ids;")?;
                m.insert("done".into(), json!(false));
                m.insert("share".into(), crate::js::num(share));
                m.insert("before".into(), json!(before));
                m.insert("after".into(), json!(after));
                m.insert("why".into(), json!("by ids it would not be smaller here"));
                return record(m);
            }
            self.conn.execute_batch("DROP TABLE participants;")?;
            self.conn.execute_batch(PARTICIPANTS_VIEW)?;
            m.insert("done".into(), json!(true));
            m.insert("share".into(), crate::js::num(share));
            m.insert("before".into(), json!(before));
            m.insert("after".into(), json!(after));
            m.insert("rows".into(), json!(rows));
            record(m)
        })
    }
    pub fn auto_vacuum(&self) -> bool {
        self.conn.query_row("PRAGMA auto_vacuum", [], |r| r.get::<_, i64>(0)).unwrap_or(0) == 2
    }
    pub fn free_bytes(&self) -> i64 {
        let n: i64 = self.conn.query_row("PRAGMA freelist_count", [], |r| r.get(0)).unwrap_or(0);
        let size: i64 = self.conn.query_row("PRAGMA page_size", [], |r| r.get(0)).unwrap_or(0);
        n * size
    }

    // ---- draft-aware model
    pub fn champion_totals(&self, patches: &[String]) -> Result<Vec<ChampionTotals>> {
        let mut st = self.conn.prepare_cached(
            "SELECT p.champion_id, p.position, COUNT(*) AS games, SUM(m.game_duration) AS seconds,
        SUM(p.dmg_physical) AS dmg_physical, SUM(p.dmg_magic) AS dmg_magic, SUM(p.dmg_true) AS dmg_true, SUM(p.dmg_taken) AS dmg_taken,
        SUM(p.dmg_mitigated) AS dmg_mitigated, SUM(p.heal) AS heal, SUM(p.heal_allies) AS heal_allies,
        SUM(p.shield_allies) AS shield_allies, SUM(p.cc_time) AS cc_time
      FROM participants p JOIN matches m USING(match_id)
      WHERE m.patch IN (SELECT value FROM json_each(?)) AND COALESCE(m.remake, 0) = 0 AND COALESCE(m.leaver, 0) = 0
        AND COALESCE(m.end_result, 'GameComplete') = 'GameComplete'
      GROUP BY p.champion_id, p.position",
        )?;
        let r = st.query_map([js_owned(patches)], |r| Ok(ChampionTotals::from_row(r)))?.filter_map(|x| x.ok()).collect();
        Ok(r)
    }
    pub fn older_champion_totals(&self, except_patches: &[String]) -> Result<Vec<ChampionTotals>> {
        let mut st = self.conn.prepare_cached(
            "SELECT champion_id, position, games, seconds, dmg_physical, dmg_magic, dmg_true, dmg_taken,
        dmg_mitigated, heal, heal_allies, shield_allies, cc_time
      FROM champion_patch_stats WHERE patch NOT IN (SELECT value FROM json_each(?))",
        )?;
        let r = st.query_map([js_owned(except_patches)], |r| Ok(ChampionTotals::from_row(r)))?.filter_map(|x| x.ok()).collect();
        Ok(r)
    }

    fn model_games_sql() -> String {
        format!(
            "SELECT g.*, m.platform, m.remake, m.leaver, m.end_result, m.game_version, timeline_json(t.data, t.data_z) AS timeline
      FROM irelia_games g JOIN matches m USING(match_id) LEFT JOIN timelines t USING(match_id)
      WHERE g.patch IN (SELECT value FROM json_each(?)) AND {SOURCE_FILTER}"
        )
    }
    /// Irelia games with platform, quality flags and timeline (None when there is none), for the draft model.
    pub fn model_games(&self, patches: &[String], sources: &[String]) -> Result<Vec<IreliaGameRow>> {
        let mut out = Vec::new();
        self.model_games_each(patches, sources, |g| {
            out.push(g);
            Ok(true)
        })?;
        Ok(out)
    }
    /// The same rows one at a time; `f` returns false to stop.
    pub fn model_games_each(&self, patches: &[String], sources: &[String], mut f: impl FnMut(IreliaGameRow) -> Result<bool>) -> Result<()> {
        let mut st = self.conn.prepare_cached(&Self::model_games_sql())?;
        let mut rows = st.query(params![js_owned(patches), js_owned(sources)])?;
        while let Some(r) = rows.next()? {
            if !f(IreliaGameRow::from_row(r))? {
                break;
            }
        }
        Ok(())
    }
    /// The same rows with each timeline as stored (plain `timeline` or compressed `timeline_z`), same order.
    pub fn model_games_raw_each(&self, patches: &[String], sources: &[String], mut f: impl FnMut(IreliaGameRow, Option<Vec<u8>>) -> Result<bool>) -> Result<()> {
        let sql = format!(
            "SELECT g.*, m.platform, m.remake, m.leaver, m.end_result, m.game_version, t.data AS timeline, t.data_z AS timeline_z
      FROM irelia_games g JOIN matches m USING(match_id) LEFT JOIN timelines t USING(match_id)
      WHERE g.patch IN (SELECT value FROM json_each(?)) AND {SOURCE_FILTER}"
        );
        let mut st = self.conn.prepare_cached(&sql)?;
        let mut rows = st.query(params![js_owned(patches), js_owned(sources)])?;
        while let Some(r) = rows.next()? {
            let z: Option<Vec<u8>> = r.get("timeline_z").ok().flatten();
            if !f(IreliaGameRow::from_row(r), z)? {
                break;
            }
        }
        Ok(())
    }
    pub fn timeline_dicts(&self) -> Vec<(u16, Arc<Vec<u8>>)> {
        self.dicts.all(&self.conn)
    }
    pub fn model_game_count(&self, patches: &[String], sources: &[String]) -> i64 {
        self.conn
            .query_row(
                &format!("SELECT COUNT(*) AS n FROM irelia_games g WHERE g.patch IN (SELECT value FROM json_each(?)) AND {SOURCE_FILTER}"),
                params![js_owned(patches), js_owned(sources)],
                |r| r.get(0),
            )
            .unwrap_or(0)
    }
    pub fn model_participants(&self, patches: &[String], sources: &[String]) -> Result<Vec<ParticipantLite>> {
        let mut st = self.conn.prepare_cached(&format!(
            "SELECT p.match_id, p.participant_id, p.champion_id, p.team_id, p.position
      FROM participants p JOIN irelia_games g USING(match_id)
      WHERE g.patch IN (SELECT value FROM json_each(?)) AND {SOURCE_FILTER}"
        ))?;
        let r = st.query_map(params![js_owned(patches), js_owned(sources)], |r| Ok(ParticipantLite::from_row(r)))?.filter_map(|x| x.ok()).collect();
        Ok(r)
    }
    /// Changes whenever games, timelines or captures change: the draft model rebuilds then.
    pub fn data_version(&self) -> String {
        self.conn
            .query_row(
                "SELECT (SELECT COUNT(*) FROM irelia_games) AS games, (SELECT COALESCE(MAX(created_at), 0) FROM irelia_games) AS last,
      (SELECT COUNT(*) FROM timelines) AS timelines, (SELECT COUNT(*) FROM matches WHERE detail_version > 0) AS captured",
                [],
                |r| Ok(format!("{}:{}:{}:{}", r.get::<_, i64>(0)?, r.get::<_, i64>(1)?, r.get::<_, i64>(2)?, r.get::<_, i64>(3)?)),
            )
            .unwrap_or_default()
    }
    pub fn own_game_count(&self) -> i64 {
        self.conn.query_row("SELECT COUNT(*) AS n FROM irelia_games WHERE pilot_source = 'me'", [], |r| r.get(0)).unwrap_or(0)
    }
    /// The newest stored game's row and the number of games.
    pub fn game_mark(&self) -> (i64, i64) {
        self.conn.query_row("SELECT COALESCE(MAX(rowid), 0) AS row, COUNT(*) AS games FROM irelia_games", [], |r| Ok((r.get(0)?, r.get(1)?))).unwrap_or((0, 0))
    }
    pub fn games_after_row(&self, row: i64) -> i64 {
        self.conn.query_row("SELECT COUNT(*) AS n FROM irelia_games WHERE rowid > ?", [row], |r| r.get(0)).unwrap_or(0)
    }

    // ---- rec_log
    pub fn log_recommendation(&self, session: &str, phase: &str, lane: Option<&str>, opponent_id: Option<i64>, mode: &str, shown: &Value) -> Result<()> {
        let now = now_ms();
        self.conn
            .prepare_cached(
                "INSERT INTO rec_log(session, phase, created_at, updated_at, lane, opponent_id, mode, shown) VALUES(?, ?, ?, ?, ?, ?, ?, ?)
      ON CONFLICT(session) DO UPDATE SET updated_at = excluded.updated_at, lane = excluded.lane, opponent_id = excluded.opponent_id,
        mode = excluded.mode, shown = excluded.shown",
            )?
            .execute(params![session, phase, now, now, lane, opponent_id, mode, crate::js::stringify(shown)])?;
        Ok(())
    }
    pub fn recommendation(&self, session: &str) -> Option<RecLogRow> {
        self.conn.query_row("SELECT * FROM rec_log WHERE session = ?", [session], |r| Ok(RecLogRow::from_row(r))).optional().ok().flatten()
    }
    pub fn unlinked_recommendations(&self, before: i64) -> Result<Vec<RecLogRow>> {
        let mut st = self.conn.prepare_cached("SELECT * FROM rec_log WHERE match_id IS NULL AND created_at < ? ORDER BY created_at")?;
        let r = st.query_map([before], |r| Ok(RecLogRow::from_row(r)))?.filter_map(|x| x.ok()).collect();
        Ok(r)
    }
    pub fn link_recommendation(&self, id: i64, match_id: &str, bought: &Value, win: bool) -> Result<()> {
        self.conn
            .prepare_cached("UPDATE rec_log SET match_id = ?, bought = ?, win = ? WHERE id = ?")?
            .execute(params![match_id, crate::js::stringify(bought), win as i64, id])?;
        Ok(())
    }
    pub fn recommendations_for(&self, match_id: &str) -> Result<Vec<RecLogRow>> {
        let mut st = self.conn.prepare_cached("SELECT * FROM rec_log WHERE match_id = ? ORDER BY phase")?;
        let r = st.query_map([match_id], |r| Ok(RecLogRow::from_row(r)))?.filter_map(|x| x.ok()).collect();
        Ok(r)
    }
    pub fn recent_recommendations(&self, limit: i64) -> Result<Vec<RecLogRow>> {
        let mut st = self.conn.prepare_cached("SELECT * FROM rec_log ORDER BY created_at DESC LIMIT ?")?;
        let r = st.query_map([limit], |r| Ok(RecLogRow::from_row(r)))?.filter_map(|x| x.ok()).collect();
        Ok(r)
    }
    pub fn participants_of(&self, match_id: &str) -> Result<Vec<ParticipantLite>> {
        let mut st = self.conn.prepare_cached("SELECT match_id, participant_id, champion_id, team_id, position FROM participants WHERE match_id = ?")?;
        let r = st.query_map([match_id], |r| Ok(ParticipantLite::from_row(r)))?.filter_map(|x| x.ok()).collect();
        Ok(r)
    }
    /// Your own stored Irelia games, newest first.
    pub fn own_games(&self, limit: i64) -> Result<Vec<IreliaGameRow>> {
        let mut st = self.conn.prepare_cached(
            "SELECT g.*, m.platform, m.remake, m.leaver, m.end_result, m.game_version, timeline_json(t.data, t.data_z) AS timeline
      FROM irelia_games g JOIN matches m USING(match_id) LEFT JOIN timelines t USING(match_id)
      WHERE g.pilot_source = 'me' ORDER BY g.created_at DESC LIMIT ?",
        )?;
        let r = st.query_map([limit], |r| Ok(IreliaGameRow::from_row(r)))?.filter_map(|x| x.ok()).collect();
        Ok(r)
    }
}

/// `anyhow` for row-less errors.
pub fn err(msg: &str) -> anyhow::Error {
    anyhow!(msg.to_string())
}
