//! A Live Client snapshot (`/liveclientdata/allgamedata`) → the contract's `LiveContext`: the user's champion, role,
//! items, level, CS and gold; every enemy's items; kills, towers, dragons and barons per team. Champions are ids
//! (Data Dragon); no player name leaves this function.
//!
//! Pairs are (the user's team, the enemy team).

use super::live_client::{identify, name_index};
use bf_core::build::{DraftContext, LiveContext, Patch, Role};
use bf_core::ddragon::StaticData;
use serde_json::Value;

/// The Live Client's `championName` is the display name ("Lee Sin", "Kai'Sa"), in the client's language; its
/// `rawChampionName` ("game_character_displayname_LeeSin") carries Data Dragon's id whatever the language.
pub fn champion_id(p: &Value, data: &StaticData) -> Option<i64> {
    let raw = p["rawChampionName"].as_str().and_then(|r| r.strip_prefix("game_character_displayname_"));
    raw.into_iter()
        .chain(p["championName"].as_str())
        .find_map(|n| data.champion_by_key(n).map(|c| c.id).or_else(|| data.champions.values().find(|c| c.name.eq_ignore_ascii_case(n)).map(|c| c.id)))
}

fn items(p: &Value) -> Vec<i64> {
    let mut v: Vec<i64> = p["items"].as_array().into_iter().flatten().filter_map(|i| i["itemID"].as_i64()).filter(|i| *i > 0).collect();
    v.sort_unstable();
    v
}

fn u(v: &Value) -> u64 {
    v.as_f64().map(|x| x.max(0.0) as u64).unwrap_or(0)
}

fn role(p: &Value) -> Option<Role> {
    p["position"].as_str().and_then(Role::from_riot)
}

/// None while the user isn't among the players (spectating, loading). `fallback_role` is champ select's, for games
/// where the Live Client leaves positions empty.
pub fn live_context(raw: &Value, data: &StaticData, fallback_role: Option<Role>) -> Option<LiveContext> {
    let players = raw["allPlayers"].as_array()?;
    let (me_i, opp_i) = identify(raw, None);
    let me = players.get(me_i?)?;
    let my_team = me["team"].clone();
    let mine = |p: &Value| p["team"] == my_team;
    let champion = champion_id(me, data)?;
    let mut allies = vec![];
    let mut enemies = vec![];
    let mut enemy_items = vec![];
    let mut others = vec![];
    let (mut team_kills, mut enemy_kills) = (0u64, 0u64);
    for (i, p) in players.iter().enumerate() {
        let kills = u(&p["scores"]["kills"]);
        if mine(p) {
            team_kills += kills;
        } else {
            enemy_kills += kills;
        }
        if Some(i) == me_i {
            continue;
        }
        let Some(c) = champion_id(p, data) else { continue };
        others.push(bf_core::build::LivePlayer { champion: c, role: role(p), ally: mine(p), level: u(&p["level"]).min(18) as u8, items: items(p) });
        if mine(p) {
            allies.push((c, role(p)));
        } else {
            enemies.push((c, role(p)));
            enemy_items.push((c, items(p)));
        }
    }
    // Objectives from the event feed. A turret's name says whose it was (Turret_T1_… is ORDER's, so CHAOS took it),
    // which also covers turrets minions finish; dragons and barons go to the killer's team.
    let find = name_index(players);
    let my_team_is_order = my_team == "ORDER";
    let (mut towers, mut dragons, mut barons) = ((0u8, 0u8), (0u8, 0u8), (0u8, 0u8));
    let bump = |pair: &mut (u8, u8), ours: bool| {
        if ours {
            pair.0 = pair.0.saturating_add(1);
        } else {
            pair.1 = pair.1.saturating_add(1);
        }
    };
    for e in raw["events"]["Events"].as_array().into_iter().flatten() {
        match e["EventName"].as_str().unwrap_or("") {
            "TurretKilled" => {
                let t = e["TurretKilled"].as_str().unwrap_or("");
                let order_lost = t.contains("_T1_");
                let chaos_lost = t.contains("_T2_");
                if order_lost || chaos_lost {
                    // ORDER's turret down = a CHAOS tower.
                    bump(&mut towers, chaos_lost == my_team_is_order);
                }
            }
            kind @ ("DragonKill" | "BaronKill") => {
                if let Some(k) = find(e["KillerName"].as_str()) {
                    bump(if kind == "DragonKill" { &mut dragons } else { &mut barons }, mine(&players[k]));
                }
            }
            _ => {}
        }
    }
    let opponent = opp_i.map(|i| &players[i]);
    Some(LiveContext {
        draft: DraftContext { champion, role: role(me).or(fallback_role), allies, enemies, patch: Patch::parse(&data.patch) },
        game_time_s: raw["gameData"]["gameTime"].as_f64().unwrap_or(0.0),
        my_items: items(me),
        enemy_items,
        my_level: u(&me["level"]).min(18) as u8,
        my_cs: u(&me["scores"]["creepScore"]).min(u16::MAX as u64) as u16,
        my_gold: raw["activePlayer"]["currentGold"].as_f64().unwrap_or(0.0),
        opponent_level: opponent.map(|o| u(&o["level"]).min(18) as u8),
        opponent_cs: opponent.map(|o| u(&o["scores"]["creepScore"]).min(u16::MAX as u64) as u16),
        team_kills: team_kills.min(u16::MAX as u64) as u16,
        enemy_kills: enemy_kills.min(u16::MAX as u64) as u16,
        towers,
        dragons,
        barons,
        players: others,
    })
}

/// What changes the advice: items anywhere, the user's level, objectives, kills. The game clock alone doesn't (the
/// server asks again every 30 s anyway).
pub fn material(ctx: &LiveContext) -> String {
    format!(
        "{}|{:?}|{:?}|{:?}|{}|{}|{:?}|{:?}|{:?}|{:?}",
        ctx.draft.champion, ctx.draft.role, ctx.my_items, ctx.enemy_items, ctx.my_level, ctx.team_kills + ctx.enemy_kills, ctx.towers, ctx.dragons, ctx.barons, ctx.draft.enemies
    )
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use bf_core::testkit::load_test_static_data;
    use serde_json::json;

    /// A Live Client snapshot as the game serves it at 18:30 (trimmed to the fields the app reads, plus names).
    pub fn snapshot() -> Value {
        let p = |name: &str, champ: &str, raw: &str, team: &str, pos: &str, level: u8, cs: u32, kills: u32, items: &[i64]| {
            json!({
                "championName": champ, "rawChampionName": format!("game_character_displayname_{raw}"),
                "riotId": format!("{name}#EUW"), "riotIdGameName": name, "riotIdTagLine": "EUW", "summonerName": format!("{name}#EUW"),
                "team": team, "position": pos, "level": level, "isBot": false, "isDead": false, "respawnTimer": 0.0,
                "items": items.iter().enumerate().map(|(slot, id)| json!({ "itemID": id, "count": 1, "slot": slot, "displayName": "x", "price": 0 })).collect::<Vec<_>>(),
                "scores": { "kills": kills, "deaths": 1, "assists": 2, "creepScore": cs, "wardScore": 9.5 },
                "summonerSpells": { "summonerSpellOne": { "displayName": "Flash" }, "summonerSpellTwo": { "displayName": "Ignite" } },
            })
        };
        json!({
            "activePlayer": { "riotId": "MeTheUser#EUW", "riotIdGameName": "MeTheUser", "summonerName": "MeTheUser#EUW", "currentGold": 1240.5, "level": 12 },
            "allPlayers": [
                p("MeTheUser", "Ahri", "Ahri", "ORDER", "MIDDLE", 12, 171, 4, &[6655, 3020, 1056, 3340]),
                p("AllyTopName", "Ornn", "Ornn", "ORDER", "TOP", 11, 140, 1, &[3068, 3047]),
                p("AllyJglName", "Lee Sin", "LeeSin", "ORDER", "JUNGLE", 11, 120, 3, &[6692]),
                p("AllyBotName", "Jinx", "Jinx", "ORDER", "BOTTOM", 11, 160, 2, &[3031]),
                p("AllySupName", "Thresh", "Thresh", "ORDER", "UTILITY", 9, 20, 0, &[3190]),
                p("EnemyTopName", "Darius", "Darius", "CHAOS", "TOP", 13, 150, 5, &[3071, 3047]),
                p("EnemyJglName", "Vi", "Vi", "CHAOS", "JUNGLE", 11, 110, 2, &[6692]),
                p("EnemyMidName", "Syndra", "Syndra", "CHAOS", "MIDDLE", 11, 158, 1, &[6655, 3020, 3157]),
                p("EnemyBotName", "Kai'Sa", "Kaisa", "CHAOS", "BOTTOM", 10, 150, 3, &[3124]),
                p("EnemySupName", "Nautilus", "Nautilus", "CHAOS", "UTILITY", 9, 25, 0, &[3190]),
            ],
            "events": { "Events": [
                { "EventID": 0, "EventName": "GameStart", "EventTime": 0.02 },
                { "EventID": 1, "EventName": "ChampionKill", "EventTime": 312.4, "KillerName": "MeTheUser", "VictimName": "EnemyMidName", "Assisters": [] },
                { "EventID": 2, "EventName": "DragonKill", "EventTime": 600.1, "DragonType": "Fire", "KillerName": "AllyJglName", "Assisters": [], "Stolen": "False" },
                { "EventID": 3, "EventName": "TurretKilled", "EventTime": 840.5, "TurretKilled": "Turret_T2_L_03_A", "KillerName": "Minion_T1L0S12N0128", "Assisters": [] },
                { "EventID": 4, "EventName": "TurretKilled", "EventTime": 900.5, "TurretKilled": "Turret_T1_R_03_A", "KillerName": "EnemyBotName", "Assisters": [] },
                { "EventID": 5, "EventName": "TurretKilled", "EventTime": 960.5, "TurretKilled": "Turret_T2_C_05_A", "KillerName": "MeTheUser", "Assisters": [] },
                { "EventID": 6, "EventName": "DragonKill", "EventTime": 1020.0, "DragonType": "Earth", "KillerName": "EnemyJglName", "Assisters": [], "Stolen": "False" },
                { "EventID": 7, "EventName": "HeraldKill", "EventTime": 1080.0, "KillerName": "AllyJglName", "Assisters": [] },
                { "EventID": 8, "EventName": "BaronKill", "EventTime": 1110.0, "KillerName": "EnemyJglName", "Assisters": [] }
            ] },
            "gameData": { "gameMode": "CLASSIC", "gameTime": 1110.7, "mapName": "Map11", "mapNumber": 11, "mapTerrain": "Default" },
        })
    }

    #[test]
    fn live_context_from_a_snapshot() {
        let data = load_test_static_data();
        let ctx = live_context(&snapshot(), &data, None).unwrap();
        assert_eq!(ctx.draft.champion, 103);
        assert_eq!(ctx.draft.role, Some(Role::Middle));
        assert_eq!(ctx.draft.allies.len(), 4);
        assert!(ctx.draft.allies.contains(&(64, Some(Role::Jungle))), "Lee Sin by his display name");
        assert!(ctx.draft.enemies.contains(&(145, Some(Role::Bottom))), "Kai'Sa by her raw name");
        assert_eq!(ctx.my_items, vec![1056, 3020, 3340, 6655]);
        assert_eq!(ctx.enemy_items.iter().find(|e| e.0 == 134).unwrap().1, vec![3020, 3157, 6655]);
        assert_eq!((ctx.my_level, ctx.my_cs, ctx.my_gold), (12, 171, 1240.5));
        assert_eq!((ctx.opponent_level, ctx.opponent_cs), (Some(11), Some(158)), "Syndra, the enemy mid");
        assert_eq!((ctx.team_kills, ctx.enemy_kills), (10, 11));
        assert_eq!(ctx.towers, (2, 1), "one finished by a minion, one by the user; one lost");
        assert_eq!(ctx.dragons, (1, 1));
        assert_eq!(ctx.barons, (0, 1));
        assert!((ctx.game_time_s - 1110.7).abs() < 1e-9);
        let text = serde_json::to_string(&ctx).unwrap();
        for name in ["MeTheUser", "AllyTopName", "EnemyMidName", "EnemyJglName", "#EUW"] {
            assert!(!text.contains(name), "{name} leaked");
        }
    }

    #[test]
    fn from_the_chaos_side_and_without_positions() {
        let data = load_test_static_data();
        let mut raw = snapshot();
        raw["activePlayer"]["riotId"] = json!("EnemyMidName#EUW");
        for p in raw["allPlayers"].as_array_mut().unwrap() {
            p["position"] = json!("");
        }
        let ctx = live_context(&raw, &data, Some(Role::Middle)).unwrap();
        assert_eq!(ctx.draft.champion, 134);
        assert_eq!(ctx.draft.role, Some(Role::Middle), "champ select's role when the game shows none");
        assert_eq!(ctx.towers, (1, 2));
        assert_eq!(ctx.barons, (1, 0));
        assert_eq!(ctx.opponent_cs, None, "no positions: no lane opponent");
        assert_eq!((ctx.team_kills, ctx.enemy_kills), (11, 10));
    }

    #[test]
    fn not_in_the_game() {
        let data = load_test_static_data();
        let mut raw = snapshot();
        raw["activePlayer"] = json!({});
        assert!(live_context(&raw, &data, None).is_none());
        assert!(live_context(&json!({}), &data, None).is_none());
    }
}
