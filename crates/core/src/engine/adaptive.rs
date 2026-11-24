//! Port of `server/engine/adaptive.ts`: the live game (Live Client Data API JSON): who you are and who you face,
//! threats and counter items, and who is ahead.

use super::exclusivity::{is_purchasable_item, ExclusivityIndex};
use crate::ddragon::StaticData;
use crate::model::profiles::{team_magic_share, ChampionProfiles};
use serde_json::{json, Value};

fn s<'a>(v: &'a Value, k: &str) -> Option<&'a str> {
    v.get(k).and_then(|x| x.as_str())
}

/// You and your lane opponent (indexes into allPlayers). `opponent_name`: None → the enemy in your position;
/// Some(None) → none; Some(Some(name)) → the enemy playing that champion.
pub fn identify(raw: &Value, opponent_name: Option<Option<&str>>) -> (Option<usize>, Option<usize>) {
    let players = raw["allPlayers"].as_array().map(|a| a.as_slice()).unwrap_or(&[]);
    let ap = &raw["activePlayer"];
    let me_id = s(ap, "riotId").or_else(|| s(ap, "summonerName"));
    let me = players
        .iter()
        .position(|p| s(p, "riotId").or_else(|| s(p, "summonerName")) == me_id)
        .or_else(|| {
            let gn = s(ap, "riotIdGameName").filter(|g| !g.is_empty())?;
            players.iter().position(|p| s(p, "riotIdGameName") == Some(gn))
        });
    let opponent = me.and_then(|m| {
        let my = &players[m];
        match opponent_name {
            Some(name) => players.iter().position(|p| p["team"] != my["team"] && name.is_some() && s(p, "championName") == name),
            None => players.iter().position(|p| {
                p["team"] != my["team"] && s(p, "position").map(|x| !x.is_empty()).unwrap_or(false) && p["position"] == my["position"]
            }),
        }
    });
    (me, opponent)
}

const HEALERS: [&str; 23] = [
    "Aatrox", "Vladimir", "Warwick", "Soraka", "Yuumi", "Sylas", "Fiddlesticks", "DrMundo", "Briar", "Olaf", "Swain", "Illaoi", "Sona", "Nami",
    "Seraphine", "Renata", "Darius", "Volibear", "Trundle", "Gwen", "Kayn", "Nasus", "Yone",
];

fn counters(t: &str) -> (&'static [i64], &'static str) {
    match t {
        "ap" => (&[3156, 3091, 6665, 3065, 2504], "Enemy damage is mostly magic"),
        "ad" => (&[6333, 3143, 3026, 3742], "Enemy damage is mostly physical"),
        "heal" => (&[3123, 6609, 3033], "Enemy team relies on healing"),
        "tank" => (&[3153, 3071, 3036], "Enemy front line is tanky"),
        _ => (&[3026, 6333], "Lane opponent is ahead - buy survivability"),
    }
}
const AP_BOOTS: i64 = 3111;
const AD_BOOTS: i64 = 3047;

/// LiveGameState without the performance block, as JSON.
pub fn analyse_live_game(raw: &Value, data: &StaticData, excl: &ExclusivityIndex, profiles: Option<&ChampionProfiles>, opponent_name: Option<Option<&str>>) -> Value {
    let (self_i, opp_i) = identify(raw, opponent_name);
    let all = raw["allPlayers"].as_array().cloned().unwrap_or_default();
    let players: Vec<Value> = all
        .iter()
        .enumerate()
        .map(|(i, p)| {
            let champ = s(p, "championName").and_then(|n| data.champion_by_key(n));
            json!({
                "championName": p["championName"], "championId": champ.map(|c| c.id), "team": p["team"], "position": p["position"],
                "level": p["level"], "items": p["items"].as_array().map(|a| a.iter().map(|i| i["itemID"].clone()).collect::<Vec<_>>()).unwrap_or_default(),
                "kills": p["scores"]["kills"], "deaths": p["scores"]["deaths"], "assists": p["scores"]["assists"], "creepScore": p["scores"]["creepScore"],
                "isMe": Some(i) == self_i,
            })
        })
        .collect();
    let me = self_i.map(|i| &players[i]);
    let enemies: Vec<&Value> = match me {
        Some(m) => players.iter().filter(|p| p["team"] != m["team"]).collect(),
        None => vec![],
    };
    let lane_opponent = opp_i.map(|i| players[i].clone());
    let (mut ap, mut ad, mut tanks, mut healers) = (0.0, 0.0, 0.0, 0.0);
    let ap_share;
    if let Some(pr) = profiles.filter(|p| p.calibrated()) {
        let ps: Vec<_> = enemies.iter().filter_map(|e| e["championId"].as_i64()).filter(|id| *id != 0).map(|id| pr.get(id, None)).collect();
        ap_share = team_magic_share(&ps, None);
        tanks = ps.iter().filter(|p| pr.is_tank(p)).count() as f64;
        healers = ps.iter().filter(|p| pr.is_healer(p)).count() as f64;
    } else {
        for e in &enemies {
            let Some(c) = e["championId"].as_i64().and_then(|id| data.champion(id)) else { continue };
            ap += c.info.magic;
            ad += c.info.attack;
            if c.tags.first().map(|t| t == "Tank").unwrap_or(false) {
                tanks += 1.0;
            }
            if HEALERS.contains(&c.key.as_str()) {
                healers += 1.0;
            }
        }
        ap_share = if ap + ad > 0.0 { ap / (ap + ad) } else { 0.5 };
    }
    let mut threats: Vec<&str> = Vec::new();
    if ap_share >= 0.55 {
        threats.push("ap");
    }
    if ap_share <= 0.4 {
        threats.push("ad");
    }
    if healers >= 2.0 {
        threats.push("heal");
    }
    if tanks >= 2.0 {
        threats.push("tank");
    }
    if let (Some(m), Some(o)) = (me, &lane_opponent) {
        if o["kills"].as_f64().unwrap_or(0.0) - m["kills"].as_f64().unwrap_or(0.0) >= 3.0 {
            threats.push("fed");
        }
    }
    let owned: Vec<i64> = me.map(|m| m["items"].as_array().unwrap().iter().filter_map(|x| x.as_i64()).filter(|id| *id > 0).collect()).unwrap_or_default();
    let mut suggestions: Vec<Value> = Vec::new();
    let mut offered: Vec<i64> = Vec::new();
    let consider = |ids: &[i64], reason: &str, suggestions: &mut Vec<Value>, offered: &mut Vec<i64>| {
        let mut first_blocked: Option<serde_json::Map<String, Value>> = None;
        for id in ids {
            if !is_purchasable_item(data.item(*id)) || offered.contains(id) {
                continue;
            }
            let mut all: Vec<i64> = owned.clone();
            all.extend(offered.iter());
            let block = excl.blocker(&all, *id);
            if block.as_deref() == Some("Duplicate") {
                return;
            }
            if let Some(b) = block {
                if first_blocked.is_none() {
                    let mut m = serde_json::Map::new();
                    m.insert("itemId".into(), json!(id));
                    m.insert("reason".into(), json!(reason));
                    m.insert("blockedBy".into(), json!(b));
                    first_blocked = Some(m);
                }
                continue;
            }
            let v = match first_blocked.take() {
                Some(mut m) => {
                    m.insert("replacement".into(), json!(id));
                    Value::Object(m)
                }
                None => json!({ "itemId": id, "reason": reason }),
            };
            suggestions.push(v);
            offered.push(*id);
            return;
        }
        if let Some(m) = first_blocked {
            suggestions.push(Value::Object(m));
        }
    };
    for t in &threats {
        let (items, reason) = counters(t);
        consider(items, reason, &mut suggestions, &mut offered);
    }
    if threats.contains(&"ap") {
        consider(&[AP_BOOTS], "Magic damage + crowd control: tenacity boots", &mut suggestions, &mut offered);
    } else if threats.contains(&"ad") {
        consider(&[AD_BOOTS], "Physical damage: armor boots", &mut suggestions, &mut offered);
    }
    json!({
        "active": true,
        "gameTime": raw["gameData"]["gameTime"],
        "myGold": raw["activePlayer"]["currentGold"],
        "players": players,
        "laneOpponent": lane_opponent,
        "threat": { "apShare": crate::js::num(ap_share), "tanks": tanks as i64, "healers": healers as i64 },
        "suggestions": suggestions,
        "summary": me.map(|m| live_summary(&players, m, data, profiles)),
    })
}

/// Who is ahead, from what the scoreboard shows.
pub fn live_summary(players: &[Value], me: &Value, data: &StaticData, profiles: Option<&ChampionProfiles>) -> Value {
    let value = |p: &Value| -> f64 {
        p["items"].as_array().map(|a| a.iter().map(|id| data.item(id.as_i64().unwrap_or(0)).map(|i| i.gold_total).unwrap_or(0.0)).fold(0.0, |s, x| s + x)).unwrap_or(0.0)
    };
    let us: Vec<&Value> = players.iter().filter(|p| p["team"] == me["team"]).collect();
    let them: Vec<&Value> = players.iter().filter(|p| p["team"] != me["team"]).collect();
    let sum = |ps: &[&Value], f: &dyn Fn(&Value) -> f64| ps.iter().fold(0.0, |s, p| s + f(p));
    let fed = |team: &[&Value]| -> Option<Value> {
        let mut sorted: Vec<&&Value> = team.iter().collect();
        sorted.sort_by(|a, b| crate::engine::recommend::js_order(value(b) - value(a)));
        let top = sorted.first()?;
        let avg = sum(team, &value) / (team.len().max(1)) as f64;
        (top["championId"].is_number() && avg > 0.0 && value(top) >= 1.25 * avg).then(|| (**top).clone())
    };
    let is_ap = |id: i64| -> bool {
        if let Some(pr) = profiles.filter(|p| p.calibrated()) {
            return pr.get(id, None).magic_share > 0.5;
        }
        data.champion(id).map(|c| c.info.magic > c.info.attack).unwrap_or(false)
    };
    let enemy = fed(&them);
    let ally = fed(&us);
    json!({
        "teamItems": crate::js::num(sum(&us, &value) - sum(&them, &value)),
        "kills": crate::js::num(sum(&us, &|p| p["kills"].as_f64().unwrap_or(0.0)) - sum(&them, &|p| p["kills"].as_f64().unwrap_or(0.0))),
        "fedEnemy": enemy.map(|e| json!({ "championId": e["championId"], "items": crate::js::num(value(&e)), "ap": is_ap(e["championId"].as_i64().unwrap_or(0)) })),
        "fedAlly": ally.map(|a| json!({ "championId": a["championId"], "items": crate::js::num(value(&a)), "isMe": a["isMe"] })),
    })
}

/// Champion id of a live player by champion name.
pub fn champion_of(data: &StaticData, p: &Value) -> Option<i64> {
    s(p, "championName").and_then(|n| data.champion_by_key(n)).map(|c| c.id)
}
