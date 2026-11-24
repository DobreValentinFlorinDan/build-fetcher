//! Port of `server/model/state.ts`: the game-state model (B2): the chance Irelia's team wins from a moment of the
//! game, from what the Live Client also shows. One feature function serves the stored timelines and the live game.

use super::logistic::{fit_logistic, predict_logistic, FitOptions};
use crate::capture::{TimelineCapture, BUY, DESTROY, SELL, UNDO};
use anyhow::Result;
use indexmap::IndexMap;
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct TeamState {
    pub inventory: f64,
    pub kills: f64,
    pub cs: f64,
    pub level: f64,
    pub towers: f64,
    pub inhibitors: f64,
    pub dragons: f64,
    pub elders: f64,
    pub barons: f64,
    pub grubs_heralds: f64,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct LaneState {
    pub inventory: f64,
    pub level: f64,
    pub cs: f64,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct GameStateInput {
    pub t: f64,
    pub us: TeamState,
    pub them: TeamState,
    pub me: Option<LaneState>,
    pub opponent: Option<LaneState>,
}

pub const STATE_DIFFS: [&str; 13] = [
    "team items (k gold)", "kills", "team CS (×10)", "average level", "towers", "inhibitors", "dragons", "elder dragons",
    "barons", "grubs and heralds", "lane items (k gold)", "lane level", "lane CS (×10)",
];

pub fn state_features_names() -> Vec<String> {
    let mut v = vec!["intercept".to_string(), "time".to_string()];
    v.extend(STATE_DIFFS.iter().map(|s| s.to_string()));
    v.extend(STATE_DIFFS.iter().map(|d| format!("{d} × time")));
    v
}

pub const N_FEATURES: usize = 28;

/// Time as a model input: 1 at 30:00, held from 40:00 on.
fn tau(t: f64) -> f64 {
    super::logistic::js_min(t, 2400.0) / 1800.0
}

pub fn state_features(s: &GameStateInput) -> [f64; N_FEATURES] {
    let lane = match (s.me, s.opponent) {
        (Some(m), Some(o)) => Some((m, o)),
        _ => None,
    };
    let d = [
        (s.us.inventory - s.them.inventory) / 1000.0,
        s.us.kills - s.them.kills,
        (s.us.cs - s.them.cs) / 10.0,
        s.us.level - s.them.level,
        s.us.towers - s.them.towers,
        s.us.inhibitors - s.them.inhibitors,
        s.us.dragons - s.them.dragons,
        s.us.elders - s.them.elders,
        s.us.barons - s.them.barons,
        s.us.grubs_heralds - s.them.grubs_heralds,
        lane.map(|(m, o)| (m.inventory - o.inventory) / 1000.0).unwrap_or(0.0),
        lane.map(|(m, o)| m.level - o.level).unwrap_or(0.0),
        lane.map(|(m, o)| (m.cs - o.cs) / 10.0).unwrap_or(0.0),
    ];
    let time = tau(s.t);
    let mut out = [0.0; N_FEATURES];
    out[0] = 1.0;
    out[1] = time;
    for i in 0..13 {
        out[2 + i] = d[i];
        out[15 + i] = d[i] * time;
    }
    out
}

/// A stored game seen from Irelia.
pub struct TimelineGame<'a> {
    pub cap: &'a TimelineCapture,
    pub irelia: f64,
    pub opponent: Option<f64>,
    pub team_of: &'a dyn Fn(f64) -> f64,
    pub price_of: &'a dyn Fn(f64) -> f64,
}

fn bump(c: &mut TeamState, kind: &str) {
    match kind {
        "TOWER" => c.towers += 1.0,
        "INHIBITOR" => c.inhibitors += 1.0,
        "DRAGON" => c.dragons += 1.0,
        "ELDER_DRAGON" => c.elders += 1.0,
        "BARON_NASHOR" => c.barons += 1.0,
        "RIFTHERALD" | "HORDE" => c.grubs_heralds += 1.0,
        _ => {}
    }
}

/// Every participant's inventory at a moment (insertion order of first item event, as the TS Map).
pub type Inventories = IndexMap<i64, Vec<f64>>;

/// The game state at each of `times` (ms, any order), replaying the timeline once. `seen` is called at each of
/// `times` with every participant's inventory then.
pub fn states_at(g: &TimelineGame, times: &[f64], mut seen: Option<&mut dyn FnMut(usize, &Inventories)>) -> Vec<GameStateInput> {
    let cap = g.cap;
    let mut order: Vec<(f64, usize)> = times.iter().enumerate().map(|(i, t)| (*t, i)).collect();
    order.sort_by(|a, b| super::super::engine::recommend::js_order(a.0 - b.0));
    let empty = GameStateInput { t: 0.0, us: TeamState::default(), them: TeamState::default(), me: None, opponent: None };
    let mut out = vec![empty; times.len()];
    let mut inv: Inventories = IndexMap::new();
    let mut c100 = TeamState::default();
    let mut c200 = TeamState::default();
    let (mut e, mut k, mut o, mut f) = (0usize, 0usize, 0usize, 0usize);
    let my_team = (g.team_of)(g.irelia);
    let people: Vec<(i64, &Vec<[f64; 5]>, bool)> =
        cap.frames.iter().map(|(pid, frames)| (*pid as i64, frames, (g.team_of)(*pid as f64) == my_team)).collect();
    let mut values: std::collections::HashMap<i64, f64> = std::collections::HashMap::new();
    for &(t, i) in &order {
        while e < cap.items.len() && cap.items[e].0 <= t {
            let (_, pid, kind, item, after) = cap.items[e];
            let pid = pid as i64;
            let list = inv.entry(pid).or_default();
            if kind == BUY {
                list.push(item);
            } else if kind == SELL || kind == DESTROY {
                if let Some(at) = list.iter().rposition(|x| *x == item) {
                    list.remove(at);
                }
            } else if kind == UNDO {
                if item != 0.0 && !item.is_nan() {
                    if let Some(at) = list.iter().rposition(|x| *x == item) {
                        list.remove(at);
                    }
                }
                if let Some(a) = after {
                    if a != 0.0 && !a.is_nan() {
                        list.push(a);
                    }
                }
            }
            values.remove(&pid);
            e += 1;
        }
        while k < cap.kills.len() && cap.kills[k].t <= t {
            let killer = cap.kills[k].killer;
            if killer != 0.0 && !killer.is_nan() {
                let team = (g.team_of)(killer);
                if team == 100.0 {
                    c100.kills += 1.0;
                } else if team == 200.0 {
                    c200.kills += 1.0;
                }
            }
            k += 1;
        }
        while o < cap.objectives.len() && cap.objectives[o].0 <= t {
            let (_, team, ref kind, _) = cap.objectives[o];
            if team == 100.0 {
                bump(&mut c100, kind);
            } else if team == 200.0 {
                bump(&mut c200, kind);
            }
            o += 1;
        }
        while f + 1 < cap.t.len() && cap.t[f + 1] <= t {
            f += 1;
        }
        let (mut us, mut them) = if my_team == 100.0 { (c100, c200) } else { (c200, c100) };
        let mut value = |pid: i64| -> f64 {
            if let Some(v) = values.get(&pid) {
                return *v;
            }
            let mut v = 0.0;
            if let Some(list) = inv.get(&pid) {
                for id in list {
                    v += (g.price_of)(*id);
                }
            }
            values.insert(pid, v);
            v
        };
        let mut n_us = 0.0;
        let mut n_them = 0.0;
        for (pid, frames, ours) in &people {
            let Some(fr) = frames.get(f) else { continue };
            let side = if *ours { &mut us } else { &mut them };
            side.inventory += value(*pid);
            side.cs += fr[4];
            side.level += fr[3];
            if *ours {
                n_us += 1.0;
            } else {
                n_them += 1.0;
            }
        }
        if n_us != 0.0 {
            us.level /= n_us;
        }
        if n_them != 0.0 {
            them.level /= n_them;
        }
        let mut lane = |pid: Option<f64>| -> Option<LaneState> {
            let pid = pid?;
            let fr = cap.frames.get(&(pid as u32))?.get(f)?;
            Some(LaneState { inventory: value(pid as i64), level: fr[3], cs: fr[4] })
        };
        let me = lane(Some(g.irelia));
        let opp = lane(g.opponent);
        out[i] = GameStateInput { t: t / 1000.0, us, them, me, opponent: opp };
        if let Some(cb) = seen.as_mut() {
            cb(i, &inv);
        }
    }
    out
}

/// Training rows of one game: its state at every frame from 1:00 on.
pub fn frame_states(g: &TimelineGame) -> Vec<GameStateInput> {
    let times: Vec<f64> = g.cap.t.iter().copied().filter(|t| *t >= 60_000.0).collect();
    states_at(g, &times, None)
}

// --------------------------------------------------------------------------------------------- live game

fn live_team(t: &str) -> Option<f64> {
    match t {
        "ORDER" => Some(100.0),
        "CHAOS" => Some(200.0),
        _ => None,
    }
}

/// "Turret_T2_C_05_A" / "Barracks_T1_R1": the team that owned the building (T1 = ORDER).
fn building_owner(name: Option<&str>) -> Option<f64> {
    let n = name.unwrap_or("");
    let i = n.find("_T")?;
    let rest = &n[i + 2..];
    let mut ch = rest.chars();
    let d = ch.next()?;
    if (d == '1' || d == '2') && ch.next() == Some('_') {
        Some(if d == '1' { 100.0 } else { 200.0 })
    } else {
        // /_T([12])_/ finds the first match anywhere: look further.
        building_owner(Some(&n[i + 2..]))
    }
}

/// The live game's state from the Live Client Data API (`raw` is allgamedata, `me`/`opponent` allPlayers entries).
pub fn state_from_live(
    raw: &Value,
    me: &Value,
    opponent: Option<&Value>,
    price_of: &dyn Fn(f64) -> f64,
    team_of_name: &dyn Fn(Option<&str>) -> Option<f64>,
) -> GameStateInput {
    let my_team = live_team(me["team"].as_str().unwrap_or("")).unwrap_or(f64::NAN);
    let mut t100 = TeamState::default();
    let mut t200 = TeamState::default();
    let value = |p: &Value| -> f64 {
        let mut s = 0.0;
        for it in p["items"].as_array().map(|a| a.as_slice()).unwrap_or(&[]) {
            let count = it["count"].as_f64().unwrap_or(1.0);
            s += price_of(it["itemID"].as_f64().unwrap_or(f64::NAN)) * super::logistic::js_max(1.0, count);
        }
        s
    };
    let mut sizes = [0.0f64; 2];
    for p in raw["allPlayers"].as_array().map(|a| a.as_slice()).unwrap_or(&[]) {
        let Some(team) = live_team(p["team"].as_str().unwrap_or("")) else { continue };
        let ts = if team == 100.0 { &mut t100 } else { &mut t200 };
        ts.inventory += value(p);
        ts.kills += p["scores"]["kills"].as_f64().unwrap_or(0.0);
        ts.cs += p["scores"]["creepScore"].as_f64().unwrap_or(0.0);
        ts.level += p["level"].as_f64().unwrap_or(0.0);
        sizes[if team == 100.0 { 0 } else { 1 }] += 1.0;
    }
    t100.level /= if sizes[0] != 0.0 { sizes[0] } else { 1.0 };
    t200.level /= if sizes[1] != 0.0 { sizes[1] } else { 1.0 };
    for ev in raw["events"]["Events"].as_array().map(|a| a.as_slice()).unwrap_or(&[]) {
        let name = ev["EventName"].as_str().unwrap_or("");
        let killer = ev["KillerName"].as_str();
        let (team, field): (Option<f64>, Option<&str>) = match name {
            "DragonKill" => (team_of_name(killer), Some(if ev["DragonType"].as_str() == Some("Elder") { "ELDER_DRAGON" } else { "DRAGON" })),
            "BaronKill" => (team_of_name(killer), Some("BARON_NASHOR")),
            "HeraldKill" | "HordeKill" => (team_of_name(killer), Some("HORDE")),
            "TurretKilled" => {
                let owner = building_owner(ev["TurretKilled"].as_str());
                (owner.map(|o| 300.0 - o).or_else(|| team_of_name(killer)), Some("TOWER"))
            }
            "InhibKilled" => {
                let owner = building_owner(ev["InhibKilled"].as_str());
                (owner.map(|o| 300.0 - o).or_else(|| team_of_name(killer)), Some("INHIBITOR"))
            }
            _ => (None, None),
        };
        if let (Some(team), Some(field)) = (team.filter(|t| *t != 0.0), field) {
            if team == 100.0 {
                bump(&mut t100, field);
            } else if team == 200.0 {
                bump(&mut t200, field);
            }
        }
    }
    let lane = |p: Option<&Value>| -> Option<LaneState> {
        let p = p?;
        Some(LaneState { inventory: value(p), level: p["level"].as_f64().unwrap_or(0.0), cs: p["scores"]["creepScore"].as_f64().unwrap_or(0.0) })
    };
    let (us, them) = if my_team == 100.0 { (t100, t200) } else { (t200, t100) };
    GameStateInput { t: raw["gameData"]["gameTime"].as_f64().unwrap_or(0.0), us, them, me: lane(Some(me)), opponent: lane(opponent) }
}

// ----------------------------------------------------------------------------------------------- the model

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct StateModel {
    pub version: u32,
    pub features: Vec<String>,
    pub beta: Vec<f64>,
    pub lambda: f64,
    pub rows: f64,
    pub games: f64,
    pub base: f64,
}

pub const STATE_LAMBDA: f64 = 1.0;

/// Fits the state model on `rows` (one per game frame). Every row of a game carries that game's result.
pub fn train_state_model(x: &[&[f64]], win: &[f64], games: f64, lambda: f64) -> Result<StateModel> {
    let rows: Vec<&[f64]> = x.to_vec();
    let fit = fit_logistic(&rows, win, &FitOptions { lambda, ..Default::default() })?;
    let mut m = StateModel { version: 1, features: state_features_names(), beta: fit.beta, lambda, rows: x.len() as f64, games, base: 0.0 };
    m.base = win_probability(&m.beta, &GameStateInput { t: 0.0, us: TeamState::default(), them: TeamState::default(), me: None, opponent: None });
    Ok(m)
}

pub fn win_probability(beta: &[f64], s: &GameStateInput) -> f64 {
    predict_logistic(beta, &state_features(s))
}

pub fn win_probability_of(beta: &[f64], x: &[f64]) -> f64 {
    predict_logistic(beta, x)
}
