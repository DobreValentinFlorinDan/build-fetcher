//! The LCU's champ-select session (`/lol-champ-select/v1/session`) → champions and roles, nothing else.
//!
//! Ranked champ select hides other players' names and Riot requires apps not to reveal them, so this parser reads
//! only the fields it names below (`localPlayerCellId`, and per cell `cellId`, `championId`, `championPickIntent`,
//! `assignedPosition`; per action `actorCellId`, `type`, `completed`). Summoner names, Riot IDs, puuids and summoner
//! ids are never read, so they can't reach the dashboard, the log or the learner. A test feeds a session full of
//! names and checks none comes out.

use bf_core::build::{DraftContext, Patch, Role};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashSet;

/// One cell of the draft as the dashboard shows it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Pick {
    /// 0 while nothing is hovered or picked.
    pub champion: i64,
    pub role: Option<Role>,
    /// The pick is locked in (not just hovered).
    pub locked: bool,
    pub ally: bool,
    pub me: bool,
}

/// The draft so far.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ChampSelect {
    /// The recommendation's input (the user's champion is 0 until they hover or pick one).
    pub draft: DraftContext,
    pub picks: Vec<Pick>,
    pub my_pick_locked: bool,
}

fn int(v: &Value, k: &str) -> i64 {
    v.get(k).and_then(Value::as_i64).unwrap_or(0)
}

fn role(v: &Value) -> Option<Role> {
    v.get("assignedPosition").and_then(Value::as_str).and_then(Role::from_riot)
}

/// A locked pick, else the hovered one (`championPickIntent`).
fn champion(v: &Value) -> i64 {
    match int(v, "championId") {
        0 => int(v, "championPickIntent"),
        c => c,
    }
}

pub fn parse_session(s: &Value, patch: Option<Patch>) -> ChampSelect {
    let locked: HashSet<i64> = s
        .get("actions")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_array)
        .flatten()
        .filter(|a| a.get("type").and_then(Value::as_str) == Some("pick") && a.get("completed").and_then(Value::as_bool) == Some(true))
        .filter_map(|a| a.get("actorCellId").and_then(Value::as_i64))
        .collect();
    let me_cell = s.get("localPlayerCellId").and_then(Value::as_i64);
    let cells = |k: &str| s.get(k).and_then(Value::as_array).cloned().unwrap_or_default();
    let mut picks = vec![];
    for (ally, team) in [(true, cells("myTeam")), (false, cells("theirTeam"))] {
        for c in &team {
            let cell = c.get("cellId").and_then(Value::as_i64);
            picks.push(Pick {
                champion: champion(c),
                role: role(c),
                locked: cell.is_some_and(|x| locked.contains(&x)),
                ally,
                me: ally && cell.is_some() && cell == me_cell,
            });
        }
    }
    let me = picks.iter().find(|p| p.me);
    let others = |ally: bool| picks.iter().filter(|p| p.ally == ally && !p.me && p.champion != 0).map(|p| (p.champion, p.role)).collect::<Vec<_>>();
    ChampSelect {
        draft: DraftContext {
            champion: me.map(|p| p.champion).unwrap_or(0),
            role: me.and_then(|p| p.role),
            allies: others(true),
            enemies: others(false),
            patch,
        },
        my_pick_locked: me.is_some_and(|p| p.locked),
        picks,
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use serde_json::json;

    /// A ranked solo/duo session as the client sends it (shape of patch 16.x), with every identifying field filled in.
    pub fn ranked_session() -> Value {
        let cell = |id: i64, champ: i64, intent: i64, pos: &str, name: &str, team: i64| {
            json!({
                "assignedPosition": pos, "cellId": id, "championId": champ, "championPickIntent": intent,
                "entitledFeatureType": "NONE", "gameName": format!("{name}"), "tagLine": format!("T{id}"),
                "internalName": format!("{name}internal"), "isHumanoid": false, "nameVisibilityType": "HIDDEN",
                "obfuscatedPuuid": format!("obf-puuid-{name}"), "obfuscatedSummonerId": 900 + id,
                "playerType": "PLAYER", "puuid": format!("puuid-{name}-0000-1111"), "selectedSkinId": champ * 1000,
                "spell1Id": 4, "spell2Id": 14, "summonerId": 7_000_000 + id, "team": team, "wardSkinId": -1,
                "summonerName": format!("{name}#EUNE"),
            })
        };
        json!({
            "actions": [
                [{ "actorCellId": 0, "championId": 238, "completed": true, "id": 1, "isAllyAction": true, "isInProgress": false, "pickTurn": 0, "type": "ban" }],
                [
                    { "actorCellId": 1, "championId": 64, "completed": true, "id": 11, "isAllyAction": true, "type": "pick" },
                    { "actorCellId": 5, "championId": 122, "completed": true, "id": 12, "isAllyAction": false, "type": "pick" }
                ],
                [{ "actorCellId": 2, "championId": 103, "completed": false, "id": 13, "isAllyAction": true, "isInProgress": true, "type": "pick" }]
            ],
            "allowBattleBoost": false, "allowDuplicatePicks": false, "allowLockedEvents": false, "allowRerolling": false,
            "bans": { "myTeamBans": [238], "theirTeamBans": [], "numBans": 10 },
            "benchChampions": [], "benchEnabled": false, "boostableSkinCount": 1,
            "chatDetails": { "mucJwtDto": { "channelClaim": "", "domain": "champ-select", "jwt": "jwt-secret", "targetRegion": "eu1" }, "multiUserChatId": "c1-abc", "multiUserChatPassword": "pw" },
            "counter": 7, "gameId": 3_712_345_678_i64, "hasSimultaneousBans": true, "hasSimultaneousPicks": false,
            "isCustomGame": false, "isSpectating": false, "localPlayerCellId": 2,
            "myTeam": [
                cell(0, 0, 0, "top", "AllyTopPlayer", 1),
                cell(1, 64, 0, "jungle", "AllyJunglePlayer", 1),
                cell(2, 0, 103, "middle", "MyOwnName", 1),
                cell(3, 0, 222, "bottom", "AllyBotPlayer", 1),
                cell(4, 0, 0, "utility", "AllySupportPlayer", 1),
            ],
            "theirTeam": [
                cell(5, 122, 0, "", "EnemyOnePlayer", 2),
                cell(6, 0, 0, "", "EnemyTwoPlayer", 2),
                cell(7, 0, 0, "", "EnemyThreePlayer", 2),
                cell(8, 0, 0, "", "EnemyFourPlayer", 2),
                cell(9, 0, 0, "", "EnemyFivePlayer", 2),
            ],
            "pickOrderSwaps": [], "recoveryCounter": 0, "rerollsRemaining": 0, "skipChampionSelect": false,
            "timer": { "adjustedTimeLeftInPhase": 25000, "internalNowInEpochMs": 1_764_000_000_000_i64, "isInfinite": false, "phase": "BAN_PICK", "totalTimeInPhase": 30000 },
            "trades": []
        })
    }

    /// Every identifying string in `ranked_session`.
    pub fn identifying_strings() -> Vec<String> {
        let mut v = vec![];
        for name in ["AllyTopPlayer", "AllyJunglePlayer", "MyOwnName", "AllyBotPlayer", "AllySupportPlayer", "EnemyOnePlayer", "EnemyTwoPlayer", "EnemyThreePlayer", "EnemyFourPlayer", "EnemyFivePlayer"] {
            v.push(name.to_string());
            v.push(format!("puuid-{name}"));
            v.push(format!("obf-puuid-{name}"));
        }
        for id in 0..10 {
            v.push((7_000_000 + id).to_string());
        }
        v.extend(["EUNE", "jwt-secret", "c1-abc", "summonerName", "gameName", "tagLine", "puuid", "summonerId", "internalName"].map(String::from));
        v
    }

    #[test]
    fn champions_and_roles_from_a_ranked_session() {
        let cs = parse_session(&ranked_session(), Some(Patch { major: 16, minor: 19 }));
        assert_eq!(cs.draft.champion, 103, "the hovered champion until it is locked");
        assert_eq!(cs.draft.role, Some(Role::Middle));
        assert_eq!(cs.draft.allies, vec![(64, Some(Role::Jungle)), (222, Some(Role::Bottom))]);
        assert_eq!(cs.draft.enemies, vec![(122, None)], "enemy positions are hidden in ranked");
        assert!(!cs.my_pick_locked);
        assert_eq!(cs.picks.len(), 10);
        assert!(cs.picks.iter().find(|p| p.champion == 64).unwrap().locked);
        assert!(cs.picks.iter().find(|p| p.champion == 122).unwrap().locked);
        assert!(!cs.picks.iter().find(|p| p.champion == 222).unwrap().locked, "hovered only");
    }

    #[test]
    fn no_name_puuid_or_summoner_id_comes_out() {
        let cs = parse_session(&ranked_session(), None);
        let text = serde_json::to_string(&cs).unwrap() + &serde_json::to_string(&cs.draft).unwrap();
        for s in identifying_strings() {
            assert!(!text.contains(&s), "{s:?} leaked into {text}");
        }
    }

    #[test]
    fn blind_pick_without_positions_leaves_the_role_to_the_build_source() {
        let s = json!({ "localPlayerCellId": 0, "myTeam": [{ "cellId": 0, "championId": 266, "assignedPosition": "" }], "theirTeam": [], "actions": [] });
        let cs = parse_session(&s, None);
        assert_eq!((cs.draft.champion, cs.draft.role), (266, None));
        let empty = parse_session(&json!({}), None);
        assert_eq!(empty.draft.champion, 0);
    }
}
