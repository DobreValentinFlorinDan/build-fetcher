//! Port of `server/engine/parseMatch.ts`: a match + timeline into the raw row stored in irelia_games.
//! Riot's DTOs stay JSON (serde_json::Value) as in TS: only the fields the app reads are looked at.

use crate::capture::participants;
use crate::db::IreliaGameRow;
use crate::ddragon::to_patch;
use crate::js;
use serde_json::{json, Value};

const SKILL_KEYS: [&str; 5] = ["", "Q", "W", "E", "R"];

pub fn find_irelia<'a>(m: &'a Value, puuid: &str, champion_id: i64) -> Option<&'a Value> {
    participants(m).iter().find(|p| p["puuid"].as_str() == Some(puuid) && p["championId"].as_i64() == Some(champion_id))
}

fn i(v: &Value) -> i64 {
    v.as_i64().unwrap_or_else(|| v.as_f64().map(|f| f as i64).unwrap_or(0))
}

/// Turns a match + timeline into the raw row stored in irelia_games.
pub fn extract_irelia_game(m: &Value, timeline: &Value, me: &Value) -> IreliaGameRow {
    let pid = timeline["info"]["participants"]
        .as_array()
        .and_then(|a| a.iter().find(|p| p["puuid"] == me["puuid"]))
        .and_then(|p| p["participantId"].as_i64())
        .unwrap_or_else(|| i(&me["participantId"]));
    let my_pos = me["teamPosition"].as_str().unwrap_or("");
    let opponent = participants(m).iter().find(|p| {
        p["teamId"] != me["teamId"] && p["teamPosition"].as_str().map(|t| !t.is_empty() && t == my_pos).unwrap_or(false)
    });
    let mut skills = String::new();
    let mut purchases: Vec<(i64, i64)> = Vec::new();
    for frame in timeline["info"]["frames"].as_array().map(|a| a.as_slice()).unwrap_or(&[]) {
        for ev in frame["events"].as_array().map(|a| a.as_slice()).unwrap_or(&[]) {
            if ev["participantId"].as_i64() != Some(pid) {
                continue;
            }
            let ty = ev["type"].as_str().unwrap_or("");
            if ty == "SKILL_LEVEL_UP" && !ev["skillSlot"].is_null() && ev["levelUpType"].as_str() == Some("NORMAL") {
                if let Some(k) = ev["skillSlot"].as_i64().and_then(|s| SKILL_KEYS.get(s as usize)) {
                    skills.push_str(k);
                }
            } else if ty == "ITEM_PURCHASED" && js::truthy(&ev["itemId"]) {
                purchases.push((i(&ev["itemId"]), i(&ev["timestamp"])));
            } else if ty == "ITEM_UNDO" && js::truthy(&ev["beforeId"]) {
                let before = i(&ev["beforeId"]);
                if let Some(k) = purchases.iter().rposition(|p| p.0 == before) {
                    purchases.remove(k);
                }
            }
        }
    }
    let styles = me["perks"]["styles"].as_array().cloned().unwrap_or_default();
    let primary = styles.first().cloned().unwrap_or(Value::Null);
    let sub = styles.get(1).cloned().unwrap_or(Value::Null);
    let perks: Vec<Value> = primary["selections"]
        .as_array()
        .into_iter()
        .flatten()
        .chain(sub["selections"].as_array().into_iter().flatten())
        .map(|s| s["perk"].clone())
        .collect();
    let sp = &me["perks"]["statPerks"];
    let items: Vec<Value> = (0..7).map(|k| me[format!("item{k}")].clone()).collect();
    IreliaGameRow {
        match_id: m["metadata"]["matchId"].as_str().unwrap_or("").into(),
        puuid: me["puuid"].as_str().unwrap_or("").into(),
        patch: to_patch(m["info"]["gameVersion"].as_str().unwrap_or("")),
        win: if js::truthy(&me["win"]) { 1 } else { 0 },
        position: if my_pos.is_empty() { "UNKNOWN".into() } else { my_pos.into() },
        opponent_champion_id: opponent.and_then(|p| p["championId"].as_i64()),
        summoner1: i(&me["summoner1Id"]),
        summoner2: i(&me["summoner2Id"]),
        primary_style: i(&primary["style"]),
        sub_style: i(&sub["style"]),
        perks: js::stringify(&Value::Array(perks)),
        stat_perks: js::stringify(&json!([sp["offense"], sp["flex"], sp["defense"]])),
        skill_order: skills,
        purchases: js::stringify(&json!(purchases.iter().map(|p| json!([p.0, p.1])).collect::<Vec<_>>())),
        final_items: js::stringify(&Value::Array(items)),
        kills: i(&me["kills"]),
        deaths: i(&me["deaths"]),
        assists: i(&me["assists"]),
        game_duration: i(&m["info"]["gameDuration"]),
        created_at: i(&m["info"]["gameCreation"]),
        ..Default::default()
    }
}
