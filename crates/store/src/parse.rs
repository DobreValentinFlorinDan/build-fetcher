//! match-v5 answers (match + optional timeline) → a validated `MatchRecord`, or the reason it was dropped.
//!
//! Data-quality rules, and which match-v5 fields they read:
//!
//! | reason | rule |
//! |---|---|
//! | `queue` | `info.queueId` ≠ 420 (ranked solo/duo) or `info.mapId` ≠ 11 (Summoner's Rift) |
//! | `remake` | game shorter than 300 s, or any participant's `gameEndedInEarlySurrender` |
//! | `leaver` | any participant's `teamEarlySurrendered` (the 15-minute vote, nearly always over an AFK); a participant whose `timePlayed` is under 80% of the game; a participant who looks AFK from the end-of-game numbers alone (under 170 gold/min, under 1.5 CS/min and at most one kill + assist); with a timeline: a participant missing from it, or idle for 3 minutes or more after 2:00 (no XP and no CS, under 140 gold/min of growth: passive income only; 5 minutes when at level 18) |
//! | `roles` | `teamPosition` missing, unknown or repeated within a team; with a timeline: a player who spent under half of minutes 2-10 (base time aside) in their role's area (the top, middle or bottom lane, or the jungle for JUNGLE) |
//! | `malformed` | no match id, version, duration or start time; not 10 participants in two teams of 5; one winner expected; a participant without champion, team, win flag or a full rune page |
//!
//! A malformed or partial timeline (fewer frames than the game's minutes, unknown participants) doesn't drop the
//! game: the game is kept without it and the timeline is counted under `timeline`.
//!
//! `gameDuration` is in seconds when `gameEndTimestamp` is present and in milliseconds before patch 11.20 (Riot's
//! documented change); the start is `gameStartTimestamp`, else `gameCreation`.

use bf_core::build::{Frame, GameVersion, MatchRecord, Platform, PlayerRecord, RankBand, Role, TeamEvent, TeamEventKind};
use serde_json::Value;

pub const QUEUE_RANKED_SOLO: i64 = 420;
pub const MAP_SUMMONERS_RIFT: i64 = 11;
pub const REMAKE_MAX_S: u32 = 300;

/// Why an answer or a game was not used. The strings are what `FetchStatus::dropped` and the log show.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Drop {
    Queue,
    Remake,
    Leaver,
    Roles,
    Malformed,
    Duplicate,
    /// The timeline only (the game was kept without it).
    Timeline,
}

impl Drop {
    pub const ALL: [Drop; 7] = [Drop::Queue, Drop::Remake, Drop::Leaver, Drop::Roles, Drop::Malformed, Drop::Duplicate, Drop::Timeline];
    pub fn as_str(self) -> &'static str {
        match self {
            Drop::Queue => "queue",
            Drop::Remake => "remake",
            Drop::Leaver => "leaver",
            Drop::Roles => "roles",
            Drop::Malformed => "malformed",
            Drop::Duplicate => "duplicate",
            Drop::Timeline => "timeline",
        }
    }
    pub fn parse(s: &str) -> Option<Drop> {
        Drop::ALL.into_iter().find(|d| d.as_str() == s)
    }
}

/// A drop with a short detail for the log ("participant 4 idle from 12:00").
#[derive(Clone, Debug, PartialEq)]
pub struct Rejected {
    pub reason: Drop,
    pub detail: String,
}

fn reject<T>(reason: Drop, detail: impl Into<String>) -> Result<T, Rejected> {
    Err(Rejected { reason, detail: detail.into() })
}

/// A parsed game: the record (players in team 100 then 200, each in Role::ALL order), the puuid of each record
/// player, and the timeline's own problem when it was set aside.
#[derive(Clone, Debug)]
pub struct Parsed {
    pub record: MatchRecord,
    pub puuids: Vec<String>,
    pub timeline_dropped: Option<Rejected>,
}

fn int(v: &Value) -> Option<i64> {
    v.as_i64().or_else(|| v.as_f64().filter(|f| f.is_finite()).map(|f| f as i64))
}
fn u32of(v: &Value) -> u32 {
    int(v).unwrap_or(0).clamp(0, u32::MAX as i64) as u32
}
fn u16of(v: &Value) -> u16 {
    int(v).unwrap_or(0).clamp(0, u16::MAX as i64) as u16
}

/// The match id, if the answer has one (for logging drops of answers that fail early).
pub fn match_id_of(m: &Value) -> Option<&str> {
    m["metadata"]["matchId"].as_str().filter(|s| !s.is_empty())
}

pub fn parse(m: &Value, timeline: Option<&Value>, rank: RankBand, me: Option<&str>) -> Result<Parsed, Rejected> {
    let info = &m["info"];
    if !info.is_object() {
        return reject(Drop::Malformed, "no info");
    }
    let Some(match_id) = match_id_of(m) else { return reject(Drop::Malformed, "no match id") };
    let Some(queue) = int(&info["queueId"]) else { return reject(Drop::Malformed, "no queueId") };
    if queue != QUEUE_RANKED_SOLO {
        return reject(Drop::Queue, format!("queue {queue}"));
    }
    let Some(map) = int(&info["mapId"]) else { return reject(Drop::Malformed, "no mapId") };
    if map != MAP_SUMMONERS_RIFT {
        return reject(Drop::Queue, format!("map {map}"));
    }
    let Some(version) = info["gameVersion"].as_str().and_then(GameVersion::parse) else { return reject(Drop::Malformed, "no gameVersion") };
    let platform = info["platformId"].as_str().and_then(Platform::parse).or_else(|| match_id.split('_').next().and_then(Platform::parse));
    let Some(raw_duration) = int(&info["gameDuration"]).filter(|d| *d >= 0) else { return reject(Drop::Malformed, "no gameDuration") };
    let duration_s = if info["gameEndTimestamp"].is_null() { raw_duration / 1000 } else { raw_duration } as u32;
    let Some(start_ms) = int(&info["gameStartTimestamp"]).or_else(|| int(&info["gameCreation"])).filter(|t| *t > 0) else {
        return reject(Drop::Malformed, "no start time");
    };
    let Some(ps) = info["participants"].as_array().filter(|a| a.len() == 10) else { return reject(Drop::Malformed, "not 10 participants") };

    if duration_s < REMAKE_MAX_S || ps.iter().any(|p| p["gameEndedInEarlySurrender"].as_bool() == Some(true)) {
        return reject(Drop::Remake, format!("{duration_s} s"));
    }

    // Participants: shape first (malformed beats every judgement on content).
    let mut players: Vec<(PlayerRecord, String, i64)> = Vec::with_capacity(10);
    for (k, p) in ps.iter().enumerate() {
        let champion = int(&p["championId"]).filter(|c| *c > 0);
        let team = int(&p["teamId"]).filter(|t| *t == 100 || *t == 200);
        let win = p["win"].as_bool();
        let (Some(champion), Some(team), Some(win)) = (champion, team, win) else { return reject(Drop::Malformed, format!("participant {} incomplete", k + 1)) };
        let styles = p["perks"]["styles"].as_array();
        let primary = styles.and_then(|s| s.iter().find(|x| x["description"] == "primaryStyle").or_else(|| s.first()));
        let sub = styles.and_then(|s| s.iter().find(|x| x["description"] == "subStyle").or_else(|| s.get(1)));
        let perks_of = |s: Option<&Value>| -> Vec<i64> { s.and_then(|s| s["selections"].as_array()).map(|a| a.iter().filter_map(|x| int(&x["perk"])).collect()).unwrap_or_default() };
        let (pp, sp) = (perks_of(primary), perks_of(sub));
        let sh = &p["perks"]["statPerks"];
        let shards = [int(&sh["offense"]), int(&sh["flex"]), int(&sh["defense"])];
        if pp.len() != 4 || sp.len() != 2 || shards.iter().any(|s| s.is_none()) {
            return reject(Drop::Malformed, format!("participant {} rune page incomplete", k + 1));
        }
        let pid = int(&p["participantId"]).unwrap_or(k as i64 + 1);
        let puuid = p["puuid"].as_str().unwrap_or("").to_string();
        let mut final_items = [0i64; 7];
        for (j, slot) in final_items.iter_mut().enumerate() {
            *slot = int(&p[format!("item{j}")]).unwrap_or(0).max(0);
        }
        let rec = PlayerRecord {
            champion,
            role: p["teamPosition"].as_str().and_then(Role::from_riot),
            team: team as u16,
            win,
            is_me: me.map(|me| !me.is_empty() && me == puuid).unwrap_or(false),
            summoners: [int(&p["summoner1Id"]).unwrap_or(0), int(&p["summoner2Id"]).unwrap_or(0)],
            rune_style: int(&primary.unwrap()["style"]).unwrap_or(0),
            perks: [pp[0], pp[1], pp[2], pp[3]],
            rune_sub_style: int(&sub.unwrap()["style"]).unwrap_or(0),
            sub_perks: [sp[0], sp[1]],
            shards: [shards[0].unwrap(), shards[1].unwrap(), shards[2].unwrap()],
            final_items,
            kills: u16of(&p["kills"]),
            deaths: u16of(&p["deaths"]),
            assists: u16of(&p["assists"]),
            gold_earned: u32of(&p["goldEarned"]),
            cs: u16of(&p["totalMinionsKilled"]).saturating_add(u16of(&p["neutralMinionsKilled"])),
            champ_level: int(&p["champLevel"]).unwrap_or(0).clamp(0, 30) as u8,
            physical_damage_to_champions: u32of(&p["physicalDamageDealtToChampions"]),
            magic_damage_to_champions: u32of(&p["magicDamageDealtToChampions"]),
            true_damage_to_champions: u32of(&p["trueDamageDealtToChampions"]),
            purchases: vec![],
            skill_order: vec![],
        };
        players.push((rec, puuid, pid));
    }
    let team_size = |t: u16| players.iter().filter(|p| p.0.team == t).count();
    if team_size(100) != 5 || team_size(200) != 5 {
        return reject(Drop::Malformed, "teams are not 5 and 5");
    }
    let win100 = players.iter().filter(|p| p.0.team == 100).all(|p| p.0.win);
    let win200 = players.iter().filter(|p| p.0.team == 200).all(|p| p.0.win);
    let any100 = players.iter().any(|p| p.0.team == 100 && p.0.win);
    let any200 = players.iter().any(|p| p.0.team == 200 && p.0.win);
    if !((win100 && !any200) || (win200 && !any100)) {
        return reject(Drop::Malformed, "not exactly one winning team");
    }

    // Leavers, from the end-of-game numbers.
    let minutes = duration_s as f64 / 60.0;
    for (k, p) in ps.iter().enumerate() {
        if p["teamEarlySurrendered"].as_bool() == Some(true) {
            return reject(Drop::Leaver, "team early surrender");
        }
        if let Some(tp) = int(&p["timePlayed"]) {
            if (tp as f64) < 0.8 * duration_s as f64 {
                return reject(Drop::Leaver, format!("participant {} played {tp} of {duration_s} s", k + 1));
            }
        }
        let r = &players[k].0;
        if (r.gold_earned as f64) < 170.0 * minutes && (r.cs as f64) < 1.5 * minutes && r.kills + r.assists <= 1 {
            return reject(Drop::Leaver, format!("participant {} looks AFK ({} gold, {} CS)", k + 1, r.gold_earned, r.cs));
        }
    }

    // Roles: complete and unique per team.
    for t in [100u16, 200] {
        let mut seen = [false; 5];
        for (r, _, _) in players.iter().filter(|p| p.0.team == t) {
            match r.role {
                Some(role) if !seen[role.index()] => seen[role.index()] = true,
                Some(role) => return reject(Drop::Roles, format!("two {} on team {t}", role.as_riot())),
                None => return reject(Drop::Roles, format!("a player without teamPosition on team {t}")),
            }
        }
    }
    players.sort_by_key(|p| (p.0.team, p.0.role.map(|r| r.index()).unwrap_or(9)));

    let mut record = MatchRecord {
        match_id: match_id.to_string(),
        platform,
        version: Some(version),
        rank: Some(if players.iter().any(|p| p.0.is_me) { RankBand::Mine } else { rank }),
        start_ms,
        duration_s,
        players: players.iter().map(|p| p.0.clone()).collect(),
        frames: vec![],
        events: vec![],
    };
    let pids: Vec<i64> = players.iter().map(|p| p.2).collect();
    let puuids: Vec<String> = players.iter().map(|p| p.1.clone()).collect();
    let mut timeline_dropped = None;
    if let Some(tl) = timeline {
        match parse_timeline(tl, &pids, &puuids, duration_s) {
            Ok(t) => {
                // A timeline that shows a leaver or wrong roles drops the game; one that is merely broken doesn't.
                check_leavers(&t.frames)?;
                check_roles(&record, &t.positions)?;
                for (k, p) in record.players.iter_mut().enumerate() {
                    p.purchases = t.purchases[k].clone();
                    p.skill_order = t.skills[k].clone();
                }
                record.frames = t.frames;
                record.events = t.events;
            }
            Err(e) if e.reason == Drop::Timeline => timeline_dropped = Some(e),
            Err(e) => return Err(e),
        }
    }
    Ok(Parsed { record, puuids, timeline_dropped })
}

// ------------------------------------------------------------------------------------------------ timeline

struct Timeline {
    /// Per record player.
    frames: Vec<Vec<Frame>>,
    positions: Vec<Vec<(u16, f64, f64)>>,
    purchases: Vec<Vec<(i64, u32)>>,
    skills: Vec<Vec<u8>>,
    events: Vec<TeamEvent>,
}

fn parse_timeline(tl: &Value, pids: &[i64], puuids: &[String], duration_s: u32) -> Result<Timeline, Rejected> {
    let Some(frames) = tl["info"]["frames"].as_array() else { return reject(Drop::Timeline, "no frames") };
    // The timeline's participant ids must be the match's (same puuids when both are given).
    if let Some(tp) = tl["info"]["participants"].as_array() {
        for (k, pid) in pids.iter().enumerate() {
            let entry = tp.iter().find(|x| int(&x["participantId"]) == Some(*pid));
            match entry.and_then(|e| e["puuid"].as_str()) {
                Some(pu) if !puuids[k].is_empty() && pu != puuids[k] => return reject(Drop::Timeline, "participants don't match the game"),
                _ => {}
            }
            if entry.is_none() && !tp.is_empty() {
                return reject(Drop::Leaver, format!("participant {pid} not in the timeline"));
            }
        }
    }
    let needed = (duration_s / 60).saturating_sub(1) as usize;
    if frames.len() < needed {
        return reject(Drop::Timeline, format!("{} frames for a {duration_s} s game", frames.len()));
    }
    let index_of = |pid: i64| pids.iter().position(|p| *p == pid);
    let n = pids.len();
    let mut out = Timeline { frames: vec![vec![]; n], positions: vec![vec![]; n], purchases: vec![vec![]; n], skills: vec![vec![]; n], events: vec![] };
    let mut last_minute: Option<u16> = None;
    for f in frames {
        let ts = int(&f["timestamp"]).unwrap_or(-1);
        if ts < 0 {
            return reject(Drop::Timeline, "frame without timestamp");
        }
        let minute = (ts / 60_000) as u16;
        // The last frame (at the game's end) repeats a minute: keep the first of each minute.
        if last_minute != Some(minute) {
            last_minute = Some(minute);
            let pf = &f["participantFrames"];
            for (k, pid) in pids.iter().enumerate() {
                let x = &pf[pid.to_string()];
                if !x.is_object() {
                    return reject(Drop::Leaver, format!("participant {pid} missing from the frame at {minute}:00"));
                }
                out.frames[k].push(Frame {
                    minute,
                    total_gold: u32of(&x["totalGold"]),
                    cs: u16of(&x["minionsKilled"]).saturating_add(u16of(&x["jungleMinionsKilled"])),
                    level: int(&x["level"]).unwrap_or(0).clamp(0, 30) as u8,
                    xp: u32of(&x["xp"]),
                });
                if let (Some(px), Some(py)) = (x["position"]["x"].as_f64(), x["position"]["y"].as_f64()) {
                    out.positions[k].push((minute, px, py));
                }
            }
        }
        for ev in f["events"].as_array().map(|a| a.as_slice()).unwrap_or(&[]) {
            let t = int(&ev["timestamp"]).unwrap_or(0).max(0);
            let sec = (t / 1000) as u32;
            let who = int(&ev["participantId"]).and_then(index_of);
            match ev["type"].as_str().unwrap_or("") {
                "ITEM_PURCHASED" => {
                    if let (Some(k), Some(item)) = (who, int(&ev["itemId"]).filter(|i| *i > 0)) {
                        out.purchases[k].push((item, sec));
                    }
                }
                "ITEM_SOLD" => {
                    if let (Some(k), Some(item)) = (who, int(&ev["itemId"]).filter(|i| *i > 0)) {
                        out.purchases[k].push((-item, sec));
                    }
                }
                "ITEM_UNDO" => {
                    if let Some(k) = who {
                        let before = int(&ev["beforeId"]).unwrap_or(0);
                        let after = int(&ev["afterId"]).unwrap_or(0);
                        // Undoing a purchase removes it; undoing a sale (before 0, after the item) removes the sale.
                        let target = if before > 0 { before } else { -after };
                        if target != 0 {
                            if let Some(i) = out.purchases[k].iter().rposition(|p| p.0 == target) {
                                out.purchases[k].remove(i);
                            }
                        }
                    }
                }
                "SKILL_LEVEL_UP" => {
                    if let (Some(k), Some(slot)) = (who, int(&ev["skillSlot"]).filter(|s| (1..=4).contains(s))) {
                        if ev["levelUpType"].as_str().map(|t| t == "NORMAL").unwrap_or(true) {
                            out.skills[k].push(slot as u8);
                        }
                    }
                }
                "CHAMPION_KILL" => {
                    let team_of = |pid: i64| index_of(pid).map(|k| if k < 5 { 100u16 } else { 200 });
                    let team = int(&ev["killerId"]).and_then(team_of).or_else(|| int(&ev["victimId"]).and_then(team_of).map(|t| 300 - t));
                    if let Some(team) = team {
                        out.events.push(TeamEvent { second: sec, team, kind: TeamEventKind::Kill });
                    }
                }
                "ELITE_MONSTER_KILL" => {
                    let team = int(&ev["killerTeamId"]).filter(|t| *t == 100 || *t == 200);
                    let kind = match ev["monsterType"].as_str().unwrap_or("") {
                        "DRAGON" => Some(TeamEventKind::Dragon),
                        "BARON_NASHOR" => Some(TeamEventKind::Baron),
                        "RIFTHERALD" => Some(TeamEventKind::Herald),
                        "HORDE" => Some(TeamEventKind::Grubs),
                        "ATAKHAN" => Some(TeamEventKind::Atakhan),
                        _ => None,
                    };
                    if let (Some(team), Some(kind)) = (team, kind) {
                        out.events.push(TeamEvent { second: sec, team: team as u16, kind });
                    }
                }
                "BUILDING_KILL" => {
                    // teamId is the team that lost the building.
                    let lost = int(&ev["teamId"]).filter(|t| *t == 100 || *t == 200);
                    let kind = match ev["buildingType"].as_str().unwrap_or("") {
                        "TOWER_BUILDING" => Some(TeamEventKind::Tower),
                        "INHIBITOR_BUILDING" => Some(TeamEventKind::Inhibitor),
                        _ => None,
                    };
                    if let (Some(lost), Some(kind)) = (lost, kind) {
                        out.events.push(TeamEvent { second: sec, team: (300 - lost) as u16, kind });
                    }
                }
                _ => {}
            }
        }
    }
    out.events.sort_by_key(|e| e.second);
    Ok(out)
}

const IDLE_GOLD_PER_MIN: f64 = 140.0;
const IDLE_MIN: u16 = 3;
const IDLE_MIN_AT_MAX_LEVEL: u16 = 5;

/// A player idle for IDLE_MIN minutes in a row after 2:00 left or went AFK.
fn check_leavers(frames: &[Vec<Frame>]) -> Result<(), Rejected> {
    for (k, fs) in frames.iter().enumerate() {
        let mut idle = 0u16;
        let mut maxed = false;
        for w in fs.windows(2) {
            let (a, b) = (w[0], w[1]);
            let dt = b.minute.saturating_sub(a.minute).max(1);
            let still = (b.xp == a.xp || b.level >= 18) && b.cs == a.cs && (b.total_gold as f64 - a.total_gold as f64) <= IDLE_GOLD_PER_MIN * dt as f64;
            if still && a.minute >= 2 {
                idle += dt;
                maxed |= b.level >= 18;
            } else {
                idle = 0;
                maxed = false;
            }
            if idle >= if maxed { IDLE_MIN_AT_MAX_LEVEL } else { IDLE_MIN } {
                return reject(Drop::Leaver, format!("player {} idle from {}:00", k + 1, b.minute - idle));
            }
        }
    }
    Ok(())
}

/// Where a position is on Summoner's Rift (0-14800 on both axes, blue base bottom-left).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Area {
    Base,
    Top,
    Middle,
    Bottom,
    Jungle,
}

pub fn area(x: f64, y: f64) -> Area {
    if (x < 2500.0 && y < 2500.0) || (x > 12300.0 && y > 12300.0) {
        Area::Base
    } else if (x < 2500.0 && y > 4000.0) || (y > 12300.0 && x < 10800.0) {
        Area::Top
    } else if (y < 2500.0 && x > 4000.0) || (x > 12300.0 && y < 10800.0) {
        Area::Bottom
    } else if (x - y).abs() < 1800.0 && (2500.0..=12300.0).contains(&x) && (2500.0..=12300.0).contains(&y) {
        Area::Middle
    } else {
        Area::Jungle
    }
}

fn expected_area(role: Role) -> Area {
    match role {
        Role::Top => Area::Top,
        Role::Jungle => Area::Jungle,
        Role::Middle => Area::Middle,
        Role::Bottom | Role::Utility => Area::Bottom,
    }
}

/// Catches a `teamPosition` that is plainly wrong (a role swap), from where each player spent minutes 2-10 (base time
/// aside): a top, mid or bot laner must be in their own lane at least half the time; a jungler or a support must not
/// spend more than half of it in one lane that isn't theirs. Junglers gank and clear camps by the lanes, and supports
/// ward the river and roam, so asking them to stay "in their area" dropped most games with a timeline.
fn check_roles(r: &MatchRecord, positions: &[Vec<(u16, f64, f64)>]) -> Result<(), Rejected> {
    for (k, p) in r.players.iter().enumerate() {
        let Some(role) = p.role else { continue };
        let want = expected_area(role);
        let (mut considered, mut in_area) = (0u32, [0u32; 5]);
        for (minute, x, y) in positions.get(k).map(|v| v.as_slice()).unwrap_or(&[]) {
            if !(2..=10).contains(minute) {
                continue;
            }
            let a = area(*x, *y);
            if a == Area::Base {
                continue;
            }
            considered += 1;
            in_area[a as usize] += 1;
        }
        // Too few points to judge (a player in base all along is caught as a leaver): no verdict.
        if considered < 3 {
            continue;
        }
        let wrong = match role {
            Role::Top | Role::Middle | Role::Bottom => in_area[want as usize] * 2 < considered,
            Role::Jungle | Role::Utility => [Area::Top, Area::Middle, Area::Bottom].into_iter().filter(|a| *a != want).any(|a| in_area[a as usize] * 2 > considered),
        };
        if wrong {
            return reject(Drop::Roles, format!("{} on team {} spent {} of {considered} early minutes in their area", role.as_riot(), p.team, in_area[want as usize]));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use bf_core::testkit::{make_match, make_timeline, MatchSpec, TimelineSpec};
    use serde_json::json;

    fn ok(m: &Value, tl: Option<&Value>) -> Parsed {
        parse(m, tl, RankBand::MasterPlus, None).unwrap()
    }
    fn why(m: &Value, tl: Option<&Value>) -> Drop {
        parse(m, tl, RankBand::MasterPlus, None).unwrap_err().reason
    }

    #[test]
    fn a_clean_game_with_timeline() {
        let m = make_match(&MatchSpec::id("EUW1_1"));
        let spec = TimelineSpec {
            items: vec![(9, "ITEM_PURCHASED".into(), 3078, None), (10, "ITEM_PURCHASED".into(), 3047, None), (11, "ITEM_PURCHASED".into(), 1036, None), (11, "ITEM_UNDO".into(), 1036, Some(0))],
            ..Default::default()
        };
        let tl = make_timeline(&m, &spec);
        let p = parse(&m, Some(&tl), RankBand::Diamond, Some("EUW1_1-p3")).unwrap();
        let r = &p.record;
        assert_eq!(r.match_id, "EUW1_1");
        assert_eq!(r.platform, Some(Platform::Euw1));
        assert_eq!(r.version.unwrap().build, 712);
        assert_eq!(r.rank, Some(RankBand::Mine), "the user's game is in the Mine band");
        assert_eq!(r.duration_s, 1800);
        assert_eq!(r.players.len(), 10);
        assert_eq!(r.players[0].champion, 39);
        assert_eq!(r.players[0].role, Some(Role::Top));
        assert!(r.players[2].is_me);
        assert_eq!(r.players[0].purchases, vec![(1055, 10), (2003, 11), (3078, 540), (3047, 600)]);
        assert_eq!(r.players[0].skill_order[..6], [1, 3, 2, 1, 1, 4]);
        assert_eq!(r.frames.len(), 10);
        assert_eq!(r.frames[0].len(), 31);
        assert_eq!(r.frames[0][10].total_gold, 500 + 3800);
        assert!(r.events.iter().any(|e| e.kind == TeamEventKind::Dragon && e.team == 200));
        assert!(r.events.iter().any(|e| e.kind == TeamEventKind::Tower && e.team == 100));
        assert!(p.timeline_dropped.is_none());
        assert_eq!(p.puuids[2], "EUW1_1-p3");
        assert_eq!(r.players[0].cs, 210);
    }

    #[test]
    fn queue_and_map() {
        assert_eq!(why(&make_match(&MatchSpec { queue: Some(440), ..MatchSpec::id("EUW1_1") }), None), Drop::Queue);
        let mut m = make_match(&MatchSpec::id("EUW1_1"));
        m["info"]["mapId"] = json!(12);
        assert_eq!(why(&m, None), Drop::Queue);
    }

    #[test]
    fn remakes() {
        assert_eq!(why(&make_match(&MatchSpec { minutes: Some(4), ..MatchSpec::id("EUW1_1") }), None), Drop::Remake);
        assert_eq!(why(&make_match(&MatchSpec { remake_vote: Some(true), ..MatchSpec::id("EUW1_1") }), None), Drop::Remake);
        // Pre-11.20 answers give the duration in ms (no gameEndTimestamp).
        let mut m = make_match(&MatchSpec::id("EUW1_1"));
        m["info"].as_object_mut().unwrap().remove("gameEndTimestamp");
        m["info"]["gameDuration"] = json!(1_800_000);
        assert_eq!(ok(&m, None).record.duration_s, 1800);
        m["info"]["gameDuration"] = json!(200_000);
        assert_eq!(why(&m, None), Drop::Remake);
    }

    #[test]
    fn leavers() {
        let mut m = make_match(&MatchSpec::id("EUW1_1"));
        m["info"]["participants"][4]["teamEarlySurrendered"] = json!(true);
        assert_eq!(why(&m, None), Drop::Leaver);
        let mut m = make_match(&MatchSpec::id("EUW1_1"));
        m["info"]["participants"][7]["timePlayed"] = json!(600);
        assert_eq!(why(&m, None), Drop::Leaver);
        let mut m = make_match(&MatchSpec::id("EUW1_1"));
        for (k, v) in [("goldEarned", 4000), ("totalMinionsKilled", 0), ("neutralMinionsKilled", 0), ("kills", 0), ("assists", 1)] {
            m["info"]["participants"][3][k] = json!(v);
        }
        assert_eq!(why(&m, None), Drop::Leaver);
        // From the timeline: idle from minute 12.
        let m = make_match(&MatchSpec::id("EUW1_1"));
        let tl = make_timeline(&m, &TimelineSpec { leaver: Some((8, 12)), ..Default::default() });
        assert_eq!(ok(&m, None).record.players.len(), 10);
        assert_eq!(why(&m, Some(&tl)), Drop::Leaver);
        // Missing from the timeline's frames.
        let mut tl = make_timeline(&m, &TimelineSpec::default());
        tl["info"]["frames"][5]["participantFrames"].as_object_mut().unwrap().remove("6");
        assert_eq!(why(&m, Some(&tl)), Drop::Leaver);
    }

    #[test]
    fn roles() {
        let mut pos: Vec<String> = ["TOP", "JUNGLE", "MIDDLE", "BOTTOM", "UTILITY", "TOP", "JUNGLE", "MIDDLE", "BOTTOM", "UTILITY"].iter().map(|s| s.to_string()).collect();
        pos[1] = "TOP".into();
        assert_eq!(why(&make_match(&MatchSpec { positions: Some(pos.clone()), ..MatchSpec::id("EUW1_1") }), None), Drop::Roles);
        pos[1] = "".into();
        assert_eq!(why(&make_match(&MatchSpec { positions: Some(pos), ..MatchSpec::id("EUW1_1") }), None), Drop::Roles);
        // The timeline puts the "jungler" in mid lane all early game.
        let m = make_match(&MatchSpec::id("EUW1_1"));
        let tl = make_timeline(&m, &TimelineSpec { spots: vec![(2, (7400.0, 7400.0))], ..Default::default() });
        assert_eq!(why(&m, Some(&tl)), Drop::Roles);
        // A support on the mid lane all early game is a swap too.
        let tl = make_timeline(&m, &TimelineSpec { spots: vec![(5, (7400.0, 7400.0))], ..Default::default() });
        assert_eq!(why(&m, Some(&tl)), Drop::Roles);
        // A jungler who clears camps and ganks (gromp counts as top, raptors as mid, then bot) and a support who wards
        // the river and roams mid: neither spends most of the early game in a lane that isn't theirs, so the game stays.
        let r = ok(&m, None).record;
        let lane = |p: &PlayerRecord| -> (f64, f64) {
            match p.role.unwrap() {
                Role::Top => (1500.0, 9000.0),
                Role::Jungle => (4000.0, 8000.0),
                Role::Middle => (7400.0, 7400.0),
                Role::Bottom | Role::Utility => (9000.0, 1500.0),
            }
        };
        let mut pos: Vec<Vec<(u16, f64, f64)>> = r.players.iter().map(|p| (2..=10).map(|m| (m, lane(p).0, lane(p).1)).collect()).collect();
        let jungler = r.players.iter().position(|p| p.role == Some(Role::Jungle)).unwrap();
        let support = r.players.iter().position(|p| p.role == Some(Role::Utility)).unwrap();
        pos[jungler] = [(2100.0, 8400.0), (4000.0, 8000.0), (7000.0, 5400.0), (7400.0, 7400.0), (1500.0, 9000.0), (4000.0, 6500.0), (9000.0, 1500.0), (7800.0, 4000.0), (2100.0, 8400.0)].iter().zip(2u16..).map(|(xy, m)| (m, xy.0, xy.1)).collect();
        pos[support] = [(9000.0, 1500.0), (10200.0, 3000.0), (9500.0, 4500.0), (7400.0, 7400.0), (9000.0, 1500.0), (6000.0, 6500.0), (9000.0, 1500.0), (10200.0, 3000.0), (9000.0, 1500.0)].iter().zip(2u16..).map(|(xy, m)| (m, xy.0, xy.1)).collect();
        assert!(check_roles(&r, &pos).is_ok());
        // The same mid laner roaming that much is still dropped: laners must be in lane half the time.
        let mid = r.players.iter().position(|p| p.role == Some(Role::Middle)).unwrap();
        pos[mid] = pos[support].clone();
        assert_eq!(check_roles(&r, &pos).unwrap_err().reason, Drop::Roles);
        assert_eq!(area(1500.0, 12500.0), Area::Top);
        assert_eq!(area(4000.0, 8000.0), Area::Jungle);
        assert_eq!(area(7400.0, 7400.0), Area::Middle);
        assert_eq!(area(12500.0, 1500.0), Area::Bottom);
        assert_eq!(area(400.0, 400.0), Area::Base);
    }

    #[test]
    fn malformed_answers() {
        let base = make_match(&MatchSpec::id("EUW1_1"));
        let cases: Vec<Box<dyn Fn(&mut Value)>> = vec![
            Box::new(|m| m["metadata"]["matchId"] = Value::Null),
            Box::new(|m| m["info"]["gameVersion"] = Value::Null),
            Box::new(|m| m["info"]["gameDuration"] = Value::Null),
            Box::new(|m| {
                m["info"]["participants"].as_array_mut().unwrap().pop();
            }),
            Box::new(|m| m["info"]["participants"][3]["championId"] = Value::Null),
            Box::new(|m| m["info"]["participants"][6]["perks"]["styles"] = json!([])),
            Box::new(|m| m["info"]["participants"][6]["win"] = json!(true)),
            Box::new(|m| m["info"]["participants"][6]["teamId"] = json!(100)),
            Box::new(|m| m["info"]["mapId"] = Value::Null),
            Box::new(|m| *m = json!({ "status": { "status_code": 404 } })),
        ];
        for (i, f) in cases.iter().enumerate() {
            let mut m = base.clone();
            f(&mut m);
            assert_eq!(why(&m, None), Drop::Malformed, "case {i}");
        }
    }

    #[test]
    fn a_partial_timeline_is_set_aside_not_the_game() {
        let m = make_match(&MatchSpec::id("EUW1_1"));
        let mut tl = make_timeline(&m, &TimelineSpec::default());
        tl["info"]["frames"].as_array_mut().unwrap().truncate(10);
        let p = ok(&m, Some(&tl));
        assert_eq!(p.timeline_dropped.unwrap().reason, Drop::Timeline);
        assert!(!p.record.has_timeline());
        let p = ok(&m, Some(&json!({ "info": {} })));
        assert!(p.timeline_dropped.is_some());
    }

    #[test]
    fn synthetic_games_pass_and_defects_are_caught() {
        let data = bf_core::testkit::load_test_static_data();
        let s = bf_core::testkit::Synth::new(bf_core::testkit::SynthOptions { timeline_share: 0.5, noise_share: 0.3, ..Default::default() }, data);
        let (mut clean, mut caught) = (0, 0);
        for i in 0..300 {
            let g = s.game(i);
            let r = parse(&g.m, g.timeline.as_ref(), RankBand::MasterPlus, None);
            match g.defect {
                None => {
                    let p = r.unwrap_or_else(|e| panic!("game {i} dropped: {e:?}"));
                    assert!(p.timeline_dropped.is_none());
                    assert_eq!(p.record.has_timeline(), g.timeline.is_some());
                    clean += 1;
                }
                Some(d) => {
                    assert_eq!(r.unwrap_err().reason.as_str(), d, "game {i}");
                    caught += 1;
                }
            }
        }
        assert!(clean > 150 && caught > 50, "{clean} clean, {caught} caught");
    }
}
