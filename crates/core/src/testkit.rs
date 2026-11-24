//! Port of `server/test/fixtures.ts`: synthetic match-v5 matches and timelines in Riot's shapes, for the tests.
//! Ten players: participants 1-5 are team 100 and 6-10 team 200, each in TOP, JUNGLE, MIDDLE, BOTTOM, UTILITY order.

use serde_json::{json, Map, Value};

pub const IRELIA: i64 = 39;
pub const POSITIONS: [&str; 5] = ["TOP", "JUNGLE", "MIDDLE", "BOTTOM", "UTILITY"];
/// Irelia top vs Darius by default; Syndra is the enemy mid.
pub const DEFAULT_CHAMPIONS: [i64; 10] = [IRELIA, 64, 103, 222, 412, 122, 121, 134, 21, 89];

fn lane_spot(pos: &str) -> Option<(f64, f64)> {
    Some(match pos {
        "TOP" => (1500.0, 12500.0),
        "JUNGLE" => (4000.0, 8000.0),
        "MIDDLE" => (7400.0, 7400.0),
        "BOTTOM" => (12500.0, 1500.0),
        "UTILITY" => (12000.0, 2000.0),
        _ => return None,
    })
}

/// Date.UTC(year, monthIndex, day).
pub fn date_utc(y: i32, m0: u32, d: u32) -> i64 {
    chrono::NaiveDate::from_ymd_opt(y, m0 + 1, d).unwrap().and_hms_opt(0, 0, 0).unwrap().and_utc().timestamp_millis()
}

#[derive(Clone, Debug, Default)]
pub struct MatchSpec {
    pub id: String,
    pub version: Option<String>,
    pub creation: Option<i64>,
    pub minutes: Option<i64>,
    pub champions: Option<Vec<i64>>,
    pub puuids: Option<Vec<String>>,
    pub positions: Option<Vec<String>>,
    pub winner: Option<i64>,
    pub remake_vote: Option<bool>,
    pub queue: Option<i64>,
}

impl MatchSpec {
    pub fn id(id: &str) -> MatchSpec {
        MatchSpec { id: id.into(), ..Default::default() }
    }
}

pub fn make_match(s: &MatchSpec) -> Value {
    let champions = s.champions.clone().unwrap_or_else(|| DEFAULT_CHAMPIONS.to_vec());
    let puuids = s.puuids.clone().unwrap_or_else(|| (0..champions.len()).map(|i| format!("{}-p{}", s.id, i + 1)).collect());
    let positions =
        s.positions.clone().unwrap_or_else(|| POSITIONS.iter().chain(POSITIONS.iter()).map(|p| p.to_string()).collect());
    let minutes = s.minutes.unwrap_or(30);
    let winner = s.winner.unwrap_or(100);
    let participants: Vec<Value> = champions
        .iter()
        .enumerate()
        .map(|(i, &champion_id)| {
            let team_id = if i < 5 { 100 } else { 200 };
            json!({
                "puuid": puuids[i], "participantId": i + 1, "championId": champion_id, "teamId": team_id,
                "teamPosition": positions[i], "win": team_id == winner,
                "summoner1Id": 4, "summoner2Id": 12, "kills": 3, "deaths": 2, "assists": 5,
                "item0": 3153, "item1": 3078, "item2": 3047, "item3": 0, "item4": 0, "item5": 0, "item6": 3340,
                "perks": {
                    "statPerks": { "offense": 5005, "flex": 5008, "defense": 5011 },
                    "styles": [
                        { "description": "primaryStyle", "style": 8000, "selections": [{ "perk": 8010 }, { "perk": 9111 }, { "perk": 9104 }, { "perk": 8299 }] },
                        { "description": "subStyle", "style": 8400, "selections": [{ "perk": 8444 }, { "perk": 8242 }] },
                    ],
                },
                "champLevel": 16, "goldEarned": 12_000, "totalMinionsKilled": 200, "neutralMinionsKilled": 10,
                "physicalDamageDealtToChampions": if champion_id == 134 { 1000 } else { 15_000 },
                "magicDamageDealtToChampions": if champion_id == 134 { 25_000 } else { 2000 },
                "trueDamageDealtToChampions": 1500, "totalDamageTaken": 20_000, "damageSelfMitigated": 12_000,
                "totalHeal": 3000, "totalHealsOnTeammates": if champion_id == 89 { 4000 } else { 0 }, "totalDamageShieldedOnTeammates": 0,
                "timeCCingOthers": 20, "totalTimeCCDealt": 90, "timePlayed": minutes * 60,
                "gameEndedInEarlySurrender": s.remake_vote.unwrap_or(false), "gameEndedInSurrender": false,
                "challenges": { "damagePerMinute": 600, "kda": 4, "soloKills": 1, "laningPhaseGoldExpAdvantage": 1, "unrelatedCosmetic": 3 },
            })
        })
        .collect();
    json!({
        "metadata": { "matchId": s.id, "participants": puuids },
        "info": {
            "gameVersion": s.version.clone().unwrap_or_else(|| "16.19.712.4471".into()),
            "queueId": s.queue.unwrap_or(420),
            "gameCreation": s.creation.unwrap_or_else(|| date_utc(2026, 8, 25)),
            "gameDuration": minutes * 60, "endOfGameResult": "GameComplete",
            "platformId": s.id.split('_').next().unwrap_or(""),
            "participants": participants,
        },
    })
}

#[derive(Clone, Debug, Default)]
pub struct TimelineSpec {
    /// Participant who stops playing from this minute on.
    pub leaver: Option<(i64, i64)>,
    /// Lane spots by participant, overriding the role's (a lane swap).
    pub spots: Vec<(i64, (f64, f64))>,
    /// Irelia's skill-ups ("QEWQQRQEQERQEEWWRW").
    pub skills: Option<String>,
    /// Item events for Irelia: (minute, type, itemId, afterId).
    pub items: Vec<(i64, String, i64, Option<i64>)>,
}

pub fn make_timeline(m: &Value, spec: &TimelineSpec) -> Value {
    let minutes = crate::js::round(m["info"]["gameDuration"].as_f64().unwrap() / 60.0) as i64;
    let ps = m["info"]["participants"].as_array().unwrap();
    let irelia = ps.iter().find(|p| p["championId"] == IRELIA).map(|p| p["participantId"].as_i64().unwrap()).unwrap_or(1);
    let mut frames = Vec::new();
    for mm in 0..=minutes {
        let mut pf = Map::new();
        for p in ps {
            let pid = p["participantId"].as_i64().unwrap();
            let left = spec.leaver.map(|(lp, from)| pid == lp && mm > from).unwrap_or(false);
            let active = if left { spec.leaver.unwrap().1 } else { mm };
            let idle = if left { mm - spec.leaver.unwrap().1 } else { 0 };
            let pos = p["teamPosition"].as_str().unwrap_or("");
            let spot = spec.spots.iter().find(|s| s.0 == pid).map(|s| s.1).or_else(|| lane_spot(pos)).unwrap_or((7000.0, 7000.0));
            let side = if p["teamId"] == 100 { -150.0 } else { 150.0 };
            pf.insert(
                pid.to_string(),
                json!({
                    "participantId": pid,
                    "totalGold": 500 + active * 380 + idle * 122, "currentGold": 200, "xp": active * 600,
                    "level": (1 + active / 2).min(18),
                    "minionsKilled": active * 7, "jungleMinionsKilled": if pos == "JUNGLE" { active * 5 } else { 0 },
                    "position": { "x": crate::js::num(spot.0 + side + ((mm % 3) * 40) as f64), "y": crate::js::num(spot.1 + side) },
                }),
            );
        }
        let mut events: Vec<Value> = Vec::new();
        if mm == 0 {
            events.push(json!({ "type": "ITEM_PURCHASED", "timestamp": 10_000, "participantId": irelia, "itemId": 1055 }));
            events.push(json!({ "type": "ITEM_PURCHASED", "timestamp": 11_000, "participantId": irelia, "itemId": 2003 }));
        }
        for (min, ty, item, after) in &spec.items {
            if *min == mm {
                events.push(if ty == "ITEM_UNDO" {
                    json!({ "type": ty, "timestamp": mm * 60_000 + 500, "participantId": irelia, "beforeId": item, "afterId": after.unwrap_or(0) })
                } else {
                    json!({ "type": ty, "timestamp": mm * 60_000 + 500, "participantId": irelia, "itemId": item })
                });
            }
        }
        let skills = spec.skills.clone().unwrap_or_else(|| "QEWQQRQEQERQEEWWRW".into());
        let sk: Vec<char> = skills.chars().collect();
        if mm >= 1 && mm as usize <= sk.len() {
            let slot = "QWER".find(sk[mm as usize - 1]).map(|x| x as i64).unwrap_or(-1) + 1;
            events.push(json!({ "type": "SKILL_LEVEL_UP", "timestamp": mm * 60_000 + 100, "participantId": irelia, "skillSlot": slot, "levelUpType": "NORMAL" }));
        }
        if mm == 6 {
            events.push(json!({ "type": "CHAMPION_KILL", "timestamp": 6 * 60_000 + 200, "killerId": irelia, "victimId": 6, "assistingParticipantIds": [2], "bounty": 300, "shutdownBounty": 0 }));
        }
        if mm == 9 {
            events.push(json!({ "type": "ELITE_MONSTER_KILL", "timestamp": 9 * 60_000, "killerId": 7, "killerTeamId": 200, "monsterType": "DRAGON", "monsterSubType": "FIRE_DRAGON" }));
        }
        if mm == 12 {
            events.push(json!({ "type": "TURRET_PLATE_DESTROYED", "timestamp": 12 * 60_000, "teamId": 200, "laneType": "TOP_LANE" }));
            events.push(json!({ "type": "BUILDING_KILL", "timestamp": 12 * 60_000 + 300, "killerId": irelia, "teamId": 200, "buildingType": "TOWER_BUILDING", "towerType": "OUTER_TURRET", "laneType": "TOP_LANE" }));
        }
        frames.push(json!({ "timestamp": mm * 60_000, "participantFrames": pf, "events": events }));
    }
    json!({
        "info": {
            "frameInterval": 60_000,
            "participants": ps.iter().map(|p| json!({ "participantId": p["participantId"], "puuid": p["puuid"] })).collect::<Vec<_>>(),
            "frames": frames,
        },
    })
}

/// The tests' Data Dragon: the trimmed snapshot committed in fixtures/ddragon-snapshot.json.
pub fn load_test_static_data() -> std::sync::Arc<crate::ddragon::StaticData> {
    use std::sync::OnceLock;
    static CACHED: OnceLock<std::sync::Arc<crate::ddragon::StaticData>> = OnceLock::new();
    CACHED
        .get_or_init(|| {
            let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/ddragon-snapshot.json");
            let snap: Value = serde_json::from_str(&std::fs::read_to_string(path).expect("ddragon snapshot")).unwrap();
            let raw = &snap["raw"];
            let r = crate::ddragon::RawStaticData {
                item: raw["item"].clone(),
                champion: raw["champion"].clone(),
                runes_reforged: raw["runesReforged"].clone(),
                summoner: raw["summoner"].clone(),
            };
            std::sync::Arc::new(
                crate::ddragon::build_static_data(snap["version"].as_str().unwrap(), snap["previousPatch"].as_str().unwrap(), &r).unwrap(),
            )
        })
        .clone()
}

// ------------------------------------------------------------------------------------------- FakeRiot

/// An in-memory Riot API (server/test/fixtures.ts FakeRiot): records every call.
#[derive(Default)]
pub struct FakeRiot {
    pub calls: parking_lot::Mutex<Vec<(String, Vec<Value>)>>,
    pub ladders: parking_lot::Mutex<std::collections::HashMap<String, Vec<crate::riot::client::LeagueEntry>>>,
    pub masteries: parking_lot::Mutex<std::collections::HashMap<String, Vec<crate::riot::client::MasteryDto>>>,
    pub ranks: parking_lot::Mutex<std::collections::HashMap<String, Vec<crate::riot::client::RankedEntry>>>,
    pub histories: parking_lot::Mutex<std::collections::HashMap<String, Vec<(String, i64)>>>,
    pub matches: parking_lot::Mutex<std::collections::HashMap<String, Value>>,
    pub timelines: parking_lot::Mutex<std::collections::HashMap<String, Value>>,
    pub regions: parking_lot::Mutex<std::collections::HashMap<String, String>>,
    pub accounts: parking_lot::Mutex<std::collections::HashMap<String, String>>,
}

impl FakeRiot {
    fn log(&self, method: &str, args: Vec<Value>) {
        self.calls.lock().push((method.into(), args));
    }
    pub fn count(&self, method: &str) -> usize {
        self.calls.lock().iter().filter(|c| c.0 == method).count()
    }
    /// Adds a match (and its timeline) to the store and to every player's history.
    pub fn add(&self, m: Value, timeline: Option<Value>) -> Value {
        let id = m["metadata"]["matchId"].as_str().unwrap().to_string();
        self.matches.lock().insert(id.clone(), m.clone());
        if let Some(t) = timeline {
            self.timelines.lock().insert(id.clone(), t);
        }
        for p in m["info"]["participants"].as_array().unwrap() {
            let key = format!("{}:{}", p["puuid"].as_str().unwrap(), m["info"]["queueId"]);
            self.histories.lock().entry(key).or_default().push((id.clone(), m["info"]["gameCreation"].as_i64().unwrap()));
        }
        m
    }
    /// Adds a match with the standard timeline.
    pub fn add_with_timeline(&self, m: Value) -> Value {
        let t = make_timeline(&m, &TimelineSpec::default());
        self.add(m, Some(t))
    }
    fn sorted(&self, puuid: &str, queue: i64) -> Vec<(String, i64)> {
        let mut v = self.histories.lock().get(&format!("{puuid}:{queue}")).cloned().unwrap_or_default();
        v.sort_by(|a, b| b.1.cmp(&a.1));
        v
    }
}

impl crate::riot::client::RiotApi for FakeRiot {
    async fn league_entries(&self, platform: &str, tier: &str, _d: &[String], _m: i64, _s: &crate::riot::rate_limiter::Signal) -> anyhow::Result<Vec<crate::riot::client::LeagueEntry>> {
        self.log("leagueEntries", vec![json!(platform), json!(tier)]);
        Ok(self.ladders.lock().get(&format!("{platform}:{tier}")).cloned().unwrap_or_default())
    }
    async fn top_masteries(&self, platform: &str, puuid: &str, count: i64, _s: &crate::riot::rate_limiter::Signal, _since: Option<i64>) -> anyhow::Result<Vec<crate::riot::client::MasteryDto>> {
        self.log("topMasteries", vec![json!(platform), json!(puuid), json!(count)]);
        Ok(self.masteries.lock().get(puuid).cloned().unwrap_or_default().into_iter().take(count as usize).collect())
    }
    async fn champion_mastery(&self, platform: &str, puuid: &str, champion_id: i64, _s: &crate::riot::rate_limiter::Signal, _since: Option<i64>) -> anyhow::Result<Option<crate::riot::client::MasteryDto>> {
        self.log("championMastery", vec![json!(platform), json!(puuid), json!(champion_id)]);
        Ok(self.masteries.lock().get(puuid).and_then(|m| m.iter().find(|x| x.champion_id == champion_id).cloned()))
    }
    async fn ranked_entries(&self, platform: &str, puuid: &str, _s: &crate::riot::rate_limiter::Signal) -> anyhow::Result<Vec<crate::riot::client::RankedEntry>> {
        self.log("rankedEntries", vec![json!(platform), json!(puuid)]);
        Ok(self.ranks.lock().get(puuid).cloned().unwrap_or_default())
    }
    async fn match_ids(&self, platform: &str, puuid: &str, queue: i64, count: i64, _s: &crate::riot::rate_limiter::Signal) -> anyhow::Result<Vec<String>> {
        self.log("matchIds", vec![json!(platform), json!(puuid), json!(queue), json!(count)]);
        Ok(self.sorted(puuid, queue).into_iter().take(count as usize).map(|h| h.0).collect())
    }
    async fn match_ids_page(&self, platform: &str, puuid: &str, q: &crate::riot::client::MatchIdQuery, _s: &crate::riot::rate_limiter::Signal) -> anyhow::Result<Vec<String>> {
        self.log("matchIdsPage", vec![json!(platform), json!(puuid), json!({ "queue": q.queue, "startTime": q.start_time, "start": q.start, "count": q.count, "fresh": q.fresh })]);
        let since = q.start_time.map(|s| s * 1000).unwrap_or(i64::MIN);
        Ok(self.sorted(puuid, q.queue).into_iter().filter(|h| h.1 >= since).skip(q.start as usize).take(q.count as usize).map(|h| h.0).collect())
    }
    async fn get_match(&self, platform: &str, id: &str, _s: &crate::riot::rate_limiter::Signal) -> anyhow::Result<Option<Value>> {
        self.log("match", vec![json!(platform), json!(id)]);
        Ok(self.matches.lock().get(id).cloned())
    }
    async fn timeline(&self, platform: &str, id: &str, _s: &crate::riot::rate_limiter::Signal) -> anyhow::Result<Option<Value>> {
        self.log("timeline", vec![json!(platform), json!(id)]);
        Ok(self.timelines.lock().get(id).cloned())
    }
    async fn account_region(&self, puuid: &str, _s: &crate::riot::rate_limiter::Signal) -> anyhow::Result<Option<String>> {
        self.log("accountRegion", vec![json!(puuid)]);
        Ok(self.regions.lock().get(puuid).cloned())
    }
    async fn account_by_riot_id(&self, game_name: &str, tag_line: &str, host: &str, _s: &crate::riot::rate_limiter::Signal) -> anyhow::Result<Option<String>> {
        self.log("accountByRiotId", vec![json!(game_name), json!(tag_line), json!(host)]);
        Ok(self.accounts.lock().get(&format!("{game_name}#{tag_line}").to_lowercase()).cloned())
    }
}
