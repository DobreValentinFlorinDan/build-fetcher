//! Port of `server/capture.ts`: what the app keeps from a match beyond the Irelia row (every participant, and for
//! Irelia games a compact timeline). Pure functions of the match-v5 JSON already on disk.

use crate::db::ParticipantRow;
use crate::js;
use serde_json::{json, Map, Value};
use std::collections::BTreeMap;

/// Bump when the capture changes: the backfill re-processes every match stored with an older version.
pub const CAPTURE_VERSION: i64 = 1;
/// A game shorter than this was remade.
pub const REMAKE_MAX_SECONDS: f64 = 300.0;

pub const CHALLENGE_KEYS: [&str; 18] = [
    "damagePerMinute", "damageTakenOnTeamPercentage", "teamDamagePercentage", "effectiveHealAndShielding",
    "enemyChampionImmobilizations", "goldPerMinute", "kda", "soloKills", "takedowns", "laningPhaseGoldExpAdvantage",
    "earlyLaningPhaseGoldExpAdvantage", "maxCsAdvantageOnLaneOpponent", "maxLevelLeadLaneOpponent",
    "laneMinionsFirst10Minutes", "turretPlatesTaken", "visionScorePerMinute", "skillshotsHit", "skillshotsDodged",
];

/// `Number.isFinite(v) ? Math.round(v) : 0` for a JSON field.
pub fn n(v: Option<&Value>) -> i64 {
    match v.and_then(|v| v.as_f64()) {
        Some(x) if x.is_finite() => js::round(x) as i64,
        _ => 0,
    }
}

fn s(v: Option<&Value>) -> String {
    v.and_then(|v| v.as_str()).unwrap_or("").to_string()
}

pub fn participants(m: &Value) -> &[Value] {
    m.get("info").and_then(|i| i.get("participants")).and_then(|p| p.as_array()).map(|a| a.as_slice()).unwrap_or(&[])
}

pub fn participant_rows(m: &Value) -> Vec<ParticipantRow> {
    let match_id = s(m.get("metadata").and_then(|x| x.get("matchId")));
    participants(m)
        .iter()
        .map(|p| {
            let ch = p.get("challenges").filter(|c| js::truthy(c)).map(|c| {
                let mut o = Map::new();
                for k in CHALLENGE_KEYS {
                    if let Some(v) = c.get(k).filter(|v| v.is_number()) {
                        o.insert(k.to_string(), v.clone());
                    }
                }
                o
            });
            let items: Vec<i64> = (0..7).map(|i| n(p.get(format!("item{i}")))).collect();
            ParticipantRow {
                match_id: match_id.clone(),
                participant_id: p.get("participantId").and_then(|v| v.as_i64()).unwrap_or(0),
                puuid: s(p.get("puuid")),
                champion_id: p.get("championId").and_then(|v| v.as_i64()).unwrap_or(0),
                team_id: p.get("teamId").and_then(|v| v.as_i64()).unwrap_or(0),
                position: p.get("teamPosition").and_then(|v| v.as_str()).unwrap_or("").to_string(),
                win: if p.get("win").map(js::truthy).unwrap_or(false) { 1 } else { 0 },
                champ_level: n(p.get("champLevel")),
                kills: n(p.get("kills")),
                deaths: n(p.get("deaths")),
                assists: n(p.get("assists")),
                gold_earned: n(p.get("goldEarned")),
                cs: n(p.get("totalMinionsKilled")) + n(p.get("neutralMinionsKilled")),
                items: js::stringify(&json!(items)),
                dmg_physical: n(p.get("physicalDamageDealtToChampions")),
                dmg_magic: n(p.get("magicDamageDealtToChampions")),
                dmg_true: n(p.get("trueDamageDealtToChampions")),
                dmg_taken: n(p.get("totalDamageTaken")),
                dmg_mitigated: n(p.get("damageSelfMitigated")),
                heal: n(p.get("totalHeal")),
                heal_allies: n(p.get("totalHealsOnTeammates")),
                shield_allies: n(p.get("totalDamageShieldedOnTeammates")),
                cc_time: n(p.get("timeCCingOthers")),
                cc_dealt: n(p.get("totalTimeCCDealt")),
                time_played: n(p.get("timePlayed")),
                challenges: ch.filter(|o| !o.is_empty()).map(|o| js::stringify(&Value::Object(o))),
            }
        })
        .collect()
}

#[derive(Clone, Debug, PartialEq)]
pub struct MatchQuality {
    pub remake: bool,
    pub end_result: Option<String>,
}

pub fn match_quality(m: &Value) -> MatchQuality {
    let remake = participants(m).iter().any(|p| p.get("gameEndedInEarlySurrender") == Some(&Value::Bool(true)))
        || m["info"]["gameDuration"].as_f64().map(|d| d < REMAKE_MAX_SECONDS).unwrap_or(false);
    MatchQuality { remake, end_result: m["info"].get("endOfGameResult").and_then(|v| v.as_str()).map(String::from) }
}

/// Item event kinds in TimelineCapture.items.
pub const BUY: f64 = 0.0;
pub const SELL: f64 = 1.0;
pub const DESTROY: f64 = 2.0;
pub const UNDO: f64 = 3.0;

/// Positions are kept for the first POSITION_FRAMES frames (0:00 … 15:00).
pub const POSITION_FRAMES: usize = 16;

/// The compact per-game timeline (one JSON blob per Irelia game, `timelines`).
///  - frames[pid][i] = [totalGold, currentGold, xp, level, cs] at t[i]
///  - pos[pid][i]    = [x, y] for the first POSITION_FRAMES frames
///  - items          = [t, pid, kind, itemId, afterId?]
///  - kills          = [t, killerId, victimId, assistIds, bounty, shutdownBounty]
///  - objectives     = [t, teamId that took it, kind, subtype]
#[derive(Clone, Debug, Default, PartialEq)]
pub struct TimelineCapture {
    pub interval: f64,
    pub t: Vec<f64>,
    /// By participant id (JS orders integer keys ascending).
    pub frames: BTreeMap<u32, Vec<[f64; 5]>>,
    pub pos: BTreeMap<u32, Vec<[f64; 2]>>,
    /// [t, pid, kind, item, after]; `after` is None for non-undo events (4 elements in JSON).
    pub items: Vec<(f64, f64, f64, f64, Option<f64>)>,
    pub kills: Vec<Kill>,
    pub objectives: Vec<(f64, f64, String, String)>,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Kill {
    pub t: f64,
    pub killer: f64,
    pub victim: f64,
    pub assists: Vec<f64>,
    pub bounty: f64,
    pub shutdown: f64,
}

fn num_or_null(x: f64) -> Value {
    if x.is_nan() {
        Value::Null
    } else {
        js::num(x)
    }
}

/// A JSON value as the TS capture holds it: null and missing (undefined → null in JSON) read as NaN.
fn f(v: Option<&Value>) -> f64 {
    v.and_then(|v| v.as_f64()).unwrap_or(f64::NAN)
}
/// A field read back from stored JSON: null counts as 0 in JS arithmetic.
fn g(v: Option<&Value>) -> f64 {
    v.and_then(|v| v.as_f64()).unwrap_or(0.0)
}

impl TimelineCapture {
    pub fn to_json(&self) -> Value {
        let mut m = Map::new();
        m.insert("v".into(), json!(1));
        m.insert("interval".into(), num_or_null(self.interval));
        m.insert("t".into(), Value::Array(self.t.iter().map(|x| num_or_null(*x)).collect()));
        let mut frames = Map::new();
        for (pid, rows) in &self.frames {
            frames.insert(pid.to_string(), Value::Array(rows.iter().map(|r| Value::Array(r.iter().map(|x| num_or_null(*x)).collect())).collect()));
        }
        m.insert("frames".into(), Value::Object(frames));
        let mut pos = Map::new();
        for (pid, rows) in &self.pos {
            pos.insert(pid.to_string(), Value::Array(rows.iter().map(|r| Value::Array(r.iter().map(|x| num_or_null(*x)).collect())).collect()));
        }
        m.insert("pos".into(), Value::Object(pos));
        m.insert(
            "items".into(),
            Value::Array(
                self.items
                    .iter()
                    .map(|(t, p, k, i, a)| {
                        let mut v = vec![num_or_null(*t), num_or_null(*p), num_or_null(*k), num_or_null(*i)];
                        if let Some(a) = a {
                            v.push(num_or_null(*a));
                        }
                        Value::Array(v)
                    })
                    .collect(),
            ),
        );
        m.insert(
            "kills".into(),
            Value::Array(
                self.kills
                    .iter()
                    .map(|k| {
                        json!([
                            num_or_null(k.t),
                            num_or_null(k.killer),
                            num_or_null(k.victim),
                            Value::Array(k.assists.iter().map(|x| num_or_null(*x)).collect()),
                            num_or_null(k.bounty),
                            num_or_null(k.shutdown)
                        ])
                    })
                    .collect(),
            ),
        );
        m.insert(
            "objectives".into(),
            Value::Array(self.objectives.iter().map(|(t, team, kind, sub)| json!([num_or_null(*t), num_or_null(*team), kind, sub])).collect()),
        );
        Value::Object(m)
    }

    pub fn to_json_string(&self) -> String {
        js::stringify(&self.to_json())
    }

    /// A stored capture (JSON) read back.
    pub fn parse(text: &str) -> Option<TimelineCapture> {
        crate::capture_read::read(text).or_else(|| Self::parse_value(text))
    }

    /// The same through a `serde_json::Value` (the reference `parse` must agree with; also what it falls back to).
    pub fn parse_value(text: &str) -> Option<TimelineCapture> {
        let v: Value = serde_json::from_str(text).ok()?;
        Some(Self::from_json(&v))
    }

    pub fn from_json(v: &Value) -> TimelineCapture {
        let arr = |k: &str| v.get(k).and_then(|x| x.as_array()).map(|a| a.as_slice()).unwrap_or(&[]);
        let mut frames = BTreeMap::new();
        if let Some(o) = v.get("frames").and_then(|x| x.as_object()) {
            for (pid, rows) in o {
                let Ok(p) = pid.parse::<u32>() else { continue };
                let rows: Vec<[f64; 5]> = rows
                    .as_array()
                    .map(|a| a.iter().map(|r| [0, 1, 2, 3, 4].map(|i| g(r.get(i)))).collect())
                    .unwrap_or_default();
                frames.insert(p, rows);
            }
        }
        let mut pos = BTreeMap::new();
        if let Some(o) = v.get("pos").and_then(|x| x.as_object()) {
            for (pid, rows) in o {
                let Ok(p) = pid.parse::<u32>() else { continue };
                let rows: Vec<[f64; 2]> =
                    rows.as_array().map(|a| a.iter().map(|r| [g(r.get(0)), g(r.get(1))]).collect()).unwrap_or_default();
                pos.insert(p, rows);
            }
        }
        TimelineCapture {
            interval: g(v.get("interval")),
            t: arr("t").iter().map(|x| g(Some(x))).collect(),
            frames,
            pos,
            items: arr("items")
                .iter()
                .map(|r| (g(r.get(0)), g(r.get(1)), g(r.get(2)), g(r.get(3)), r.get(4).map(|a| g(Some(a)))))
                .collect(),
            kills: arr("kills")
                .iter()
                .map(|r| Kill {
                    t: g(r.get(0)),
                    killer: g(r.get(1)),
                    victim: g(r.get(2)),
                    assists: r.get(3).and_then(|a| a.as_array()).map(|a| a.iter().map(|x| g(Some(x))).collect()).unwrap_or_default(),
                    bounty: g(r.get(4)),
                    shutdown: g(r.get(5)),
                })
                .collect(),
            objectives: arr("objectives")
                .iter()
                .map(|r| (g(r.get(0)), g(r.get(1)), s(r.get(2)), s(r.get(3))))
                .collect(),
        }
    }
}

fn other_team(team: f64) -> f64 {
    if team == 100.0 {
        200.0
    } else {
        100.0
    }
}

fn item_type(t: &str) -> Option<f64> {
    match t {
        "ITEM_PURCHASED" => Some(BUY),
        "ITEM_SOLD" => Some(SELL),
        "ITEM_DESTROYED" => Some(DESTROY),
        "ITEM_UNDO" => Some(UNDO),
        _ => None,
    }
}

pub fn capture_timeline(timeline: &Value) -> TimelineCapture {
    let info = &timeline["info"];
    let mut cap = TimelineCapture {
        interval: info.get("frameInterval").and_then(|v| v.as_f64()).unwrap_or(60_000.0),
        ..Default::default()
    };
    let frames = info.get("frames").and_then(|f| f.as_array()).map(|a| a.as_slice()).unwrap_or(&[]);
    for (i, frame) in frames.iter().enumerate() {
        cap.t.push(f(frame.get("timestamp")));
        if let Some(pf) = frame.get("participantFrames").and_then(|p| p.as_object()) {
            for (key, fr) in js::entries(pf) {
                let pid_s = match fr.get("participantId").filter(|v| !v.is_null()) {
                    Some(v) => js::num_to_string(js::to_number(v)),
                    None => key.clone(),
                };
                let Ok(pid) = pid_s.parse::<u32>() else { continue };
                let cs = n(fr.get("minionsKilled")) + n(fr.get("jungleMinionsKilled"));
                cap.frames.entry(pid).or_default().push([
                    f(fr.get("totalGold")),
                    f(fr.get("currentGold")),
                    f(fr.get("xp")),
                    f(fr.get("level")),
                    cs as f64,
                ]);
                if i < POSITION_FRAMES {
                    if let Some(p) = fr.get("position").filter(|p| js::truthy(p)) {
                        cap.pos.entry(pid).or_default().push([f(p.get("x")), f(p.get("y"))]);
                    }
                }
            }
        }
        let events = frame.get("events").and_then(|e| e.as_array()).map(|a| a.as_slice()).unwrap_or(&[]);
        for ev in events {
            let ty = ev.get("type").and_then(|t| t.as_str()).unwrap_or("");
            let ts = f(ev.get("timestamp"));
            let pid = ev.get("participantId").and_then(|v| v.as_f64()).unwrap_or(0.0);
            let or0 = |k: &str| ev.get(k).and_then(|v| v.as_f64()).unwrap_or(0.0);
            if let Some(kind) = item_type(ty).filter(|_| pid != 0.0) {
                if kind == UNDO {
                    cap.items.push((ts, pid, kind, or0("beforeId"), Some(or0("afterId"))));
                } else {
                    cap.items.push((ts, pid, kind, or0("itemId"), None));
                }
            } else if ty == "CHAMPION_KILL" {
                cap.kills.push(Kill {
                    t: ts,
                    killer: or0("killerId"),
                    victim: or0("victimId"),
                    assists: ev
                        .get("assistingParticipantIds")
                        .and_then(|a| a.as_array())
                        .map(|a| a.iter().map(|x| x.as_f64().unwrap_or(f64::NAN)).collect())
                        .unwrap_or_default(),
                    bounty: or0("bounty"),
                    shutdown: or0("shutdownBounty"),
                });
            } else if ty == "ELITE_MONSTER_KILL"
                && ev.get("monsterType").map(js::truthy).unwrap_or(false)
                && ev.get("killerTeamId").map(js::truthy).unwrap_or(false)
            {
                let sub = ev.get("monsterSubType").and_then(|v| v.as_str());
                let kind = if sub == Some("ELDER_DRAGON") { "ELDER_DRAGON".to_string() } else { s(ev.get("monsterType")) };
                cap.objectives.push((ts, or0("killerTeamId"), kind, sub.unwrap_or("").to_string()));
            } else if ty == "BUILDING_KILL" && ev.get("teamId").map(js::truthy).unwrap_or(false) {
                let kind = if ev.get("buildingType").and_then(|v| v.as_str()) == Some("INHIBITOR_BUILDING") { "INHIBITOR" } else { "TOWER" };
                let sub = ev.get("towerType").and_then(|v| v.as_str()).or_else(|| ev.get("laneType").and_then(|v| v.as_str())).unwrap_or("");
                cap.objectives.push((ts, other_team(or0("teamId")), kind.into(), sub.into()));
            } else if ty == "TURRET_PLATE_DESTROYED" && ev.get("teamId").map(js::truthy).unwrap_or(false) {
                cap.objectives.push((ts, other_team(or0("teamId")), "PLATE".into(), s(ev.get("laneType"))));
            }
        }
    }
    cap
}

const IDLE_GOLD_PER_MIN: f64 = 140.0;
pub const LEAVER_MIN_IDLE_MS: f64 = 180_000.0;
pub const LEAVER_MIN_IDLE_MAX_LEVEL_MS: f64 = 300_000.0;
const MAX_LEVEL: f64 = 18.0;
const GAME_START_MS: f64 = 120_000.0;

/// Participants who left (or went AFK).
pub fn leavers(cap: &TimelineCapture) -> Vec<i64> {
    let mut out = Vec::new();
    for (pid, frames) in &cap.frames {
        let mut idle = 0.0;
        let mut max_level = false;
        for i in 1..frames.len() {
            let dt = cap.t.get(i).copied().unwrap_or(f64::NAN) - cap.t.get(i - 1).copied().unwrap_or(f64::NAN);
            let [gold, _, xp, level, cs] = frames[i];
            let [gold0, _, xp0, _, cs0] = frames[i - 1];
            let still = (xp == xp0 || level >= MAX_LEVEL) && cs == cs0 && gold - gold0 <= IDLE_GOLD_PER_MIN * (dt / 60_000.0);
            if still && cap.t.get(i - 1).copied().unwrap_or(f64::NAN) >= GAME_START_MS {
                idle += dt;
                max_level = max_level || level >= MAX_LEVEL;
            } else {
                idle = 0.0;
                max_level = false;
            }
            if idle >= if max_level { LEAVER_MIN_IDLE_MAX_LEVEL_MS } else { LEAVER_MIN_IDLE_MS } {
                out.push(*pid as i64);
                break;
            }
        }
    }
    out.sort();
    out
}

const LANE_FROM_MS: f64 = 3.0 * 60_000.0;
const LANE_TO_MS: f64 = 14.0 * 60_000.0;

pub fn median(xs: &[f64]) -> f64 {
    let mut s = xs.to_vec();
    s.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let l = s.len();
    if l % 2 == 1 {
        s[(l - 1) / 2]
    } else {
        (s[l / 2 - 1] + s[l / 2]) / 2.0
    }
}

/// `Math.hypot(a, b)` as V8 computes it for two arguments.
pub fn hypot(a: f64, b: f64) -> f64 {
    crate::jsmath::hypot2(a, b)
}

/// The enemy who actually laned against `pid` (smallest median distance between 3:00 and 14:00).
pub fn timeline_lane_opponent(cap: &TimelineCapture, pid: i64, enemy_ids: &[i64]) -> Option<i64> {
    let mine = cap.pos.get(&(pid as u32))?;
    let frames: Vec<usize> = (0..cap.t.len()).filter(|&i| i < mine.len() && cap.t[i] >= LANE_FROM_MS && cap.t[i] <= LANE_TO_MS).collect();
    if frames.len() < 3 {
        return None;
    }
    let mut best: Option<(i64, f64)> = None;
    for &id in enemy_ids {
        let Some(theirs) = cap.pos.get(&(id as u32)) else { continue };
        let ds: Vec<f64> = frames.iter().filter(|&&i| i < theirs.len()).map(|&i| hypot(mine[i][0] - theirs[i][0], mine[i][1] - theirs[i][1])).collect();
        if ds.len() < 3 {
            continue;
        }
        let d = median(&ds);
        if best.map(|b| d < b.1).unwrap_or(true) {
            best = Some((id, d));
        }
    }
    best.map(|b| b.0)
}

#[derive(Clone, Debug, PartialEq)]
pub struct LaneOpponents {
    pub position: Option<i64>,
    pub timeline: Option<i64>,
    pub effective: Option<i64>,
}

/// Lane opponents by Riot's position label and by the timeline, for the participant `me`.
pub fn lane_opponents(m: &Value, cap: Option<&TimelineCapture>, me: &Value) -> LaneOpponents {
    let team = me.get("teamId");
    let my_pos = me.get("teamPosition").and_then(|v| v.as_str()).unwrap_or("");
    let enemies: Vec<&Value> = participants(m).iter().filter(|p| p.get("teamId") != team).collect();
    let by_position = enemies
        .iter()
        .find(|p| {
            let tp = p.get("teamPosition").and_then(|v| v.as_str()).unwrap_or("");
            !tp.is_empty() && tp == my_pos
        })
        .copied();
    let pid = me.get("participantId").and_then(|v| v.as_i64()).unwrap_or(0);
    let tl_id = cap.and_then(|c| {
        timeline_lane_opponent(c, pid, &enemies.iter().map(|p| p.get("participantId").and_then(|v| v.as_i64()).unwrap_or(0)).collect::<Vec<_>>())
    });
    let by_timeline = tl_id.and_then(|id| enemies.iter().find(|p| p.get("participantId").and_then(|v| v.as_i64()) == Some(id)).copied());
    let champ = |p: Option<&Value>| p.and_then(|p| p.get("championId")).and_then(|v| v.as_i64());
    LaneOpponents {
        position: champ(by_position),
        timeline: champ(by_timeline),
        effective: champ(by_timeline).or(champ(by_position)),
    }
}

/// Item ids in `pid`'s inventory just after time `t`, rebuilt from the item events.
pub fn inventory_at(cap: &TimelineCapture, pid: f64, t: f64) -> Vec<f64> {
    let mut inv: Vec<f64> = Vec::new();
    let remove = |inv: &mut Vec<f64>, id: f64| {
        if let Some(i) = inv.iter().rposition(|x| *x == id) {
            inv.remove(i);
        }
    };
    for &(ts, p, kind, item, after) in &cap.items {
        if p != pid || ts > t {
            continue;
        }
        if kind == BUY {
            inv.push(item);
        } else if kind == SELL || kind == DESTROY {
            remove(&mut inv, item);
        } else if kind == UNDO {
            if item != 0.0 {
                remove(&mut inv, item);
            }
            let a = after.unwrap_or(0.0);
            if a != 0.0 && !a.is_nan() {
                inv.push(a);
            }
        }
    }
    inv
}
