//! From irelia-companion's `server/lcu/liveClient.ts`: the Live Client Data API
//! (https://127.0.0.1:2999/liveclientdata/allgamedata) and the performance tracker (CS, vision, the gold model).
//!
//! The Live Client names every player; names are used here only to find the user and to credit events to a team.
//! Nothing that leaves this module for the dashboard holds a name: champions only.

use bf_core::js;
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};

pub mod gold {
    pub const START: f64 = 500.0;
    pub const PASSIVE_FROM: f64 = 65.0;
    pub const PASSIVE_PER_SEC: f64 = 2.04;
    pub const PER_CS: f64 = 20.0;
    pub const KILL: f64 = 300.0;
    pub const FIRST_BLOOD: f64 = 100.0;
    pub const ASSIST_SHARE: f64 = 0.5;
    pub const STREAK_BONUS: f64 = 100.0;
    pub const DEATH_STREAK_FACTOR: f64 = 0.85;
    pub const MIN_BOUNTY: f64 = 100.0;
    pub const MAX_BOUNTY: f64 = 1000.0;
    pub const TURRET: f64 = 250.0;
    pub const INHIBITOR: f64 = 50.0;
    pub const BARON: f64 = 300.0;
}

const CONTROL_WARD: i64 = 2055;
const TRINKETS: [i64; 3] = [3340, 3363, 3364];
const HISTORY_EVERY_S: f64 = 15.0;
const HISTORY_MAX: usize = 240;

fn clamp(x: f64, lo: f64, hi: f64) -> f64 {
    bf_core::js::js_min(hi, bf_core::js::js_max(lo, x))
}
fn norm(s: Option<&str>) -> String {
    s.unwrap_or("").trim().to_lowercase()
}
fn s<'a>(v: &'a Value, k: &str) -> Option<&'a str> {
    v.get(k).and_then(|x| x.as_str())
}
fn f(v: &Value) -> f64 {
    v.as_f64().unwrap_or(0.0)
}
/// The user (`activePlayer`) and the lane opponent among `allPlayers`: the enemy with `opponent_name`'s champion when
/// given, else the enemy in the same position.
pub fn identify(raw: &Value, opponent_name: Option<Option<&str>>) -> (Option<usize>, Option<usize>) {
    let players = raw["allPlayers"].as_array().map(|a| a.as_slice()).unwrap_or(&[]);
    let ap = &raw["activePlayer"];
    let me_id = s(ap, "riotId").or_else(|| s(ap, "summonerName"));
    let me = players.iter().position(|p| me_id.is_some() && s(p, "riotId").or_else(|| s(p, "summonerName")) == me_id).or_else(|| {
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

pub fn player_key(p: &Value) -> String {
    s(p, "riotId").or_else(|| s(p, "summonerName")).or_else(|| s(p, "championName")).unwrap_or("").to_string()
}

/// Event names may be a Riot ID game name, a full "name#tag", a legacy summoner name or a champion name.
pub fn name_index(players: &[Value]) -> impl Fn(Option<&str>) -> Option<usize> + '_ {
    let mut index: HashMap<String, usize> = HashMap::new();
    for (i, p) in players.iter().enumerate() {
        let riot = s(p, "riotId");
        let summ = s(p, "summonerName");
        for n in [s(p, "riotIdGameName"), riot, riot.map(|r| r.split('#').next().unwrap_or("")), summ, summ.map(|r| r.split('#').next().unwrap_or("")), s(p, "championName")] {
            if let Some(n) = n.filter(|n| !n.is_empty()) {
                index.entry(norm(Some(n))).or_insert(i);
            }
        }
    }
    move |name: Option<&str>| index.get(&norm(name)).copied()
}

/// Gold from kills, assists and objectives per player key, replayed from the event feed; None when the names
/// cannot be resolved (callers then fall back to scoreboard KDA).
pub fn event_income(players: &[Value], events: &[Value]) -> Option<HashMap<String, f64>> {
    let find = name_index(players);
    let mut gold: HashMap<String, f64> = players.iter().map(|p| (player_key(p), 0.0)).collect();
    let add = |gold: &mut HashMap<String, f64>, p: Option<usize>, g: f64| {
        if let Some(i) = p {
            *gold.entry(player_key(&players[i])).or_insert(0.0) += g;
        }
    };
    let mut kill_streak: HashMap<String, f64> = HashMap::new();
    let mut death_streak: HashMap<String, f64> = HashMap::new();
    let (mut kills, mut resolved) = (0.0, 0.0);
    let mut sorted: Vec<&Value> = events.iter().collect();
    sorted.sort_by(|a, b| bf_core::js::js_order(f(&a["EventTime"]) - f(&b["EventTime"])));
    for e in sorted {
        let killer = find(s(e, "KillerName"));
        let assisters: Vec<usize> = e["Assisters"].as_array().map(|a| a.iter().filter_map(|x| find(x.as_str())).collect()).unwrap_or_default();
        match s(e, "EventName").unwrap_or("") {
            "ChampionKill" => {
                kills += 1.0;
                let Some(victim) = find(s(e, "VictimName")) else { continue };
                resolved += 1.0;
                let vk = player_key(&players[victim]);
                let streak = kill_streak.get(&vk).copied().unwrap_or(0.0);
                let deaths = death_streak.get(&vk).copied().unwrap_or(0.0);
                let bounty = clamp(
                    (gold::KILL + gold::STREAK_BONUS * bf_core::js::js_max(0.0, streak - 1.0)) * bf_core::jsmath::pow(gold::DEATH_STREAK_FACTOR, deaths),
                    gold::MIN_BOUNTY,
                    gold::MAX_BOUNTY,
                );
                if let Some(k) = killer {
                    if players[k]["team"] != players[victim]["team"] {
                        add(&mut gold, Some(k), bounty);
                        let kk = player_key(&players[k]);
                        *kill_streak.entry(kk.clone()).or_insert(0.0) += 1.0;
                        death_streak.insert(kk, 0.0);
                    }
                }
                for a in &assisters {
                    add(&mut gold, Some(*a), (bounty * gold::ASSIST_SHARE) / assisters.len() as f64);
                }
                kill_streak.insert(vk.clone(), 0.0);
                death_streak.insert(vk, deaths + 1.0);
            }
            "FirstBlood" => add(&mut gold, find(s(e, "Recipient")), gold::FIRST_BLOOD),
            "TurretKilled" => {
                let team: Vec<usize> = killer.into_iter().chain(assisters.iter().copied()).collect();
                for p in &team {
                    add(&mut gold, Some(*p), gold::TURRET / team.len() as f64);
                }
            }
            "InhibKilled" => {
                for p in killer.into_iter().chain(assisters.iter().copied()) {
                    add(&mut gold, Some(p), gold::INHIBITOR);
                }
            }
            "BaronKill" => {
                let team = killer.map(|k| players[k]["team"].clone()).or_else(|| assisters.first().map(|a| players[*a]["team"].clone()));
                if let Some(team) = team.filter(|t| !t.is_null()) {
                    for (i, p) in players.iter().enumerate() {
                        if p["team"] == team {
                            add(&mut gold, Some(i), gold::BARON);
                        }
                    }
                }
            }
            _ => {}
        }
    }
    if kills > 0.0 && resolved / kills < 0.8 {
        None
    } else {
        Some(gold)
    }
}

fn kda_income(p: &Value) -> f64 {
    f(&p["scores"]["kills"]) * gold::KILL + (f(&p["scores"]["assists"]) * gold::KILL * gold::ASSIST_SHARE) / 2.0
}

pub fn model_income(p: &Value, game_time: f64, from_events: Option<&HashMap<String, f64>>) -> f64 {
    let passive = bf_core::js::js_max(0.0, game_time - gold::PASSIVE_FROM) * gold::PASSIVE_PER_SEC;
    let combat = match from_events {
        Some(m) => m.get(&player_key(p)).copied().unwrap_or(0.0),
        None => kda_income(p),
    };
    gold::START + passive + f(&p["scores"]["creepScore"]) * gold::PER_CS + combat
}

fn items(p: &Value) -> Vec<(i64, f64)> {
    p["items"].as_array().map(|a| a.iter().map(|i| (i["itemID"].as_i64().unwrap_or(0), i["count"].as_f64().unwrap_or(f64::NAN))).collect()).unwrap_or_default()
}
fn count(c: f64) -> f64 {
    bf_core::js::js_max(1.0, if c.is_nan() { 1.0 } else { c })
}

pub fn inventory_value(p: &Value, price_of: &dyn Fn(i64) -> f64) -> f64 {
    items(p).into_iter().fold(0.0, |s, (id, c)| s + price_of(id) * count(c))
}

fn round1(x: f64) -> f64 {
    js::round(x * 10.0) / 10.0
}

/// Turns successive allgamedata snapshots into CS / vision / gold-difference metrics plus a short history.
#[derive(Default)]
pub struct PerformanceTracker {
    history: Vec<Value>,
    last_sample_t: Option<f64>,
    last_game_time: f64,
    cs_seen: HashSet<i64>,
    /// None: not set yet; Some(None): no opponent.
    opponent_name: Option<Option<String>>,
}

impl PerformanceTracker {
    pub fn reset(&mut self) {
        *self = PerformanceTracker::default();
    }

    /// LivePerformance as JSON (None without you in the game). `opponent_name` as identify() takes it.
    pub fn update(&mut self, raw: &Value, price_of: &dyn Fn(i64) -> f64, opponent_name: Option<Option<&str>>) -> Option<Value> {
        let t = f(&raw["gameData"]["gameTime"]);
        if t + 5.0 < self.last_game_time {
            self.reset();
        }
        self.last_game_time = t;
        let players: Vec<Value> = raw["allPlayers"].as_array().cloned().unwrap_or_default();
        let (me_i, opp_i) = identify(raw, opponent_name);
        let me = &players[me_i?];
        let opponent = opp_i.map(|i| &players[i]);
        let against = opponent.and_then(|o| s(o, "championName")).map(String::from);
        if self.opponent_name.is_some() && self.opponent_name.as_ref() != Some(&against) {
            self.history.clear();
            self.last_sample_t = None;
        }
        self.opponent_name = Some(against);
        let minutes = bf_core::js::js_max(t / 60.0, 1.0 / 60.0);
        let events: Vec<Value> = raw["events"]["Events"].as_array().cloned().unwrap_or_default();
        let income = event_income(&players, &events);
        for p in &players {
            let cs = f(&p["scores"]["creepScore"]);
            if cs > 0.0 {
                self.cs_seen.insert(cs as i64);
            }
        }
        let cs_granularity = if self.cs_seen.len() >= 5 && self.cs_seen.iter().all(|cs| cs % 10 == 0) { 10 } else { 1 };
        let base = |p: &Value| -> serde_json::Map<String, Value> {
            let vision = p["scores"]["wardScore"].as_f64().unwrap_or(0.0);
            let it = items(p);
            let mut m = serde_json::Map::new();
            m.insert("championName".into(), p["championName"].clone());
            m.insert("cs".into(), p["scores"]["creepScore"].clone());
            m.insert("csPerMin".into(), js::num(round1(f(&p["scores"]["creepScore"]) / minutes)));
            m.insert("visionScore".into(), js::num(round1(vision)));
            m.insert("visionPerMin".into(), js::num(js::round((vision / minutes) * 100.0) / 100.0));
            m.insert("controlWards".into(), js::num(it.iter().filter(|i| i.0 == CONTROL_WARD).fold(0.0, |s, i| s + count(i.1))));
            m.insert("trinket".into(), it.iter().find(|i| TRINKETS.contains(&i.0)).map(|i| json!(i.0)).unwrap_or(Value::Null));
            m.insert("itemGold".into(), js::num(js::round(inventory_value(p, price_of))));
            m.insert("modelGold".into(), js::num(js::round(model_income(p, t, income.as_ref()))));
            m
        };
        let mut mb = base(me);
        let current_gold = f(&raw["activePlayer"]["currentGold"]).floor();
        let item_gold = f(&mb["itemGold"]);
        let model_gold = f(&mb["modelGold"]);
        let my_net_worth = item_gold + current_gold;
        let ratio = if model_gold > 0.0 { my_net_worth / model_gold } else { 1.0 };
        let calibration = js::round(clamp(1.0 + (ratio - 1.0) * clamp((t - 60.0) / 300.0, 0.0, 1.0), 0.75, 1.15) * 1000.0) / 1000.0;
        mb.insert("unspentGold".into(), js::num(current_gold));
        mb.insert("netWorth".into(), js::num(my_net_worth));
        let me_perf = Value::Object(mb);
        let mut gold_info = json!({ "confidence": "high", "uncertainty": 0, "calibration": js::num(calibration) });
        let opp = opponent.map(|o| {
            let mut ob = base(o);
            let ig = f(&ob["itemGold"]);
            let mg = f(&ob["modelGold"]);
            let net_worth = js::round(bf_core::js::js_max(ig, mg * calibration));
            let unspent = net_worth - ig;
            ob.insert("unspentGold".into(), js::num(unspent));
            ob.insert("netWorth".into(), js::num(net_worth));
            gold_info = json!({
                "confidence": if unspent <= 350.0 { "high" } else if unspent <= 900.0 { "medium" } else { "low" },
                "uncertainty": js::num(js::round((100.0 + 0.04 * mg + 0.5 * unspent) / 10.0) * 10.0),
                "calibration": js::num(calibration),
            });
            Value::Object(ob)
        });
        let diff = opp.as_ref().map(|o| {
            json!({
                "cs": js::num(f(&me_perf["cs"]) - f(&o["cs"])),
                "vision": js::num(round1(f(&me_perf["visionScore"]) - f(&o["visionScore"]))),
                "gold": js::num(f(&me_perf["netWorth"]) - f(&o["netWorth"])),
                "itemGold": js::num(f(&me_perf["itemGold"]) - f(&o["itemGold"])),
            })
        });
        if let Some(d) = &diff {
            if self.last_sample_t.map(|l| t - l >= HISTORY_EVERY_S).unwrap_or(true) {
                self.last_sample_t = Some(t);
                self.history.push(json!({ "t": js::num(js::round(t)), "gold": d["gold"], "cs": d["cs"], "vision": d["vision"] }));
                if self.history.len() > HISTORY_MAX {
                    self.history.remove(0);
                }
            }
        }
        Some(json!({
            "gameTime": raw["gameData"]["gameTime"], "me": me_perf, "opponent": opp, "diff": diff, "gold": gold_info,
            "csGranularity": cs_granularity, "history": self.history.clone(),
        }))
    }
}

/// A running game ends after this many failed polls in a row.
pub const END_AFTER_FAILURES: u32 = 3;
pub const LIVE_PORT: u16 = 2999;

#[cfg(test)]
mod tests {
    //! Port of server/lcu/liveClient.test.ts.
    use super::*;

    fn price_of(id: i64) -> f64 {
        match id {
            3153 => 3200.0,
            1055 => 450.0,
            1036 => 350.0,
            3044 => 1100.0,
            2055 => 75.0,
            _ => 0.0,
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn player(name: &str, champion: &str, team: &str, position: &str, cs: f64, ward: f64, k: f64, d: f64, a: f64, items: &[i64]) -> Value {
        json!({
            "championName": champion, "riotId": format!("{name}#KR1"), "riotIdGameName": name, "summonerName": format!("{name}#KR1"),
            "team": team, "position": position, "level": 9,
            "items": items.iter().enumerate().map(|(slot, id)| json!({ "itemID": id, "count": 1, "slot": slot })).collect::<Vec<_>>(),
            "scores": { "kills": k, "deaths": d, "assists": a, "creepScore": cs, "wardScore": ward },
        })
    }
    fn kill(t: f64, killer: &str, victim: &str, assisters: &[&str]) -> Value {
        json!({ "EventID": t, "EventName": "ChampionKill", "EventTime": t, "KillerName": killer, "VictimName": victim, "Assisters": assisters })
    }
    struct G {
        me_cs: f64,
        opp_cs: f64,
        me_gold: f64,
        events: Option<Vec<Value>>,
        opp_items: Vec<i64>,
    }
    impl Default for G {
        fn default() -> Self {
            G { me_cs: 80.0, opp_cs: 70.0, me_gold: 800.0, events: None, opp_items: vec![1055, 1036, 3044, 3364] }
        }
    }
    fn game(t: f64, o: G) -> Value {
        json!({
            "activePlayer": { "riotId": "Me#KR1", "riotIdGameName": "Me", "currentGold": o.me_gold },
            "allPlayers": [
                player("Me", "Irelia", "ORDER", "TOP", o.me_cs, 12.6, 1.0, 0.0, 0.0, &[3153, 2055, 3340]),
                player("Jg", "LeeSin", "ORDER", "JUNGLE", 50.0, 0.0, 0.0, 0.0, 1.0, &[]),
                player("Opp", "Darius", "CHAOS", "TOP", o.opp_cs, 8.2, 0.0, 1.0, 0.0, &o.opp_items),
                player("Mid", "Ahri", "CHAOS", "MIDDLE", 90.0, 0.0, 0.0, 0.0, 0.0, &[]),
            ],
            "events": { "Events": o.events.unwrap_or(vec![
                json!({ "EventID": 0, "EventName": "GameStart", "EventTime": 0 }),
                kill(300.0, "Me", "Opp", &["Jg"]),
                json!({ "EventID": 1, "EventName": "FirstBlood", "EventTime": 300, "Recipient": "Me" }),
            ]) },
            "gameData": { "gameTime": t },
        })
    }
    fn ps(g: &Value) -> Vec<Value> {
        g["allPlayers"].as_array().unwrap().clone()
    }

    #[test]
    fn kill_events_pay_the_killer() {
        let g = game(600.0, G::default());
        let income = event_income(&ps(&g), g["events"]["Events"].as_array().unwrap()).unwrap();
        assert_eq!(income["Me#KR1"], gold::KILL + gold::FIRST_BLOOD);
        assert_eq!(income["Jg#KR1"], gold::KILL * gold::ASSIST_SHARE);
        assert_eq!(income["Opp#KR1"], 0.0);
    }

    #[test]
    fn repeated_deaths_and_shutdowns() {
        let g = game(900.0, G::default());
        let income = event_income(&ps(&g), &[kill(100.0, "Me", "Opp", &[]), kill(200.0, "Me", "Opp", &[]), kill(300.0, "Opp", "Me", &[])]).unwrap();
        assert_eq!(income["Me#KR1"], gold::KILL + gold::KILL * gold::DEATH_STREAK_FACTOR);
        assert_eq!(income["Opp#KR1"], gold::KILL + gold::STREAK_BONUS);
    }

    #[test]
    fn unresolvable_names_fall_back() {
        let g = game(600.0, G::default());
        assert!(event_income(&ps(&g), &[kill(100.0, "Someone", "Nobody", &[])]).is_none());
    }

    #[test]
    fn tracker_reports_differences() {
        let mut tracker = PerformanceTracker::default();
        let p = tracker.update(&game(600.0, G::default()), &price_of, None).unwrap();
        assert_eq!(p["opponent"]["championName"], "Darius");
        assert_eq!(p["diff"]["cs"], json!(10));
        assert_eq!(p["diff"]["vision"], json!(4.4));
        assert_eq!(p["me"]["controlWards"], json!(1));
        assert_eq!(p["me"]["trinket"], json!(3340));
        assert_eq!(p["opponent"]["trinket"], json!(3364));
        assert_eq!(p["me"]["itemGold"], json!(3275));
        assert_eq!(p["me"]["netWorth"], json!(3275 + 800));
        let passive = (600.0 - gold::PASSIVE_FROM) * gold::PASSIVE_PER_SEC;
        let my_model = js::round(gold::START + passive + 80.0 * gold::PER_CS + gold::KILL + gold::FIRST_BLOOD);
        assert_eq!(f(&p["me"]["modelGold"]), my_model);
        let cal = f(&p["gold"]["calibration"]);
        assert_eq!(cal, js::round(f64::min(1.15, (3275.0 + 800.0) / my_model) * 1000.0) / 1000.0);
        let opp_model = js::round(gold::START + passive + 70.0 * gold::PER_CS);
        assert_eq!(f(&p["opponent"]["netWorth"]), js::round(f64::max(1900.0, opp_model * cal)));
        assert_eq!(f(&p["opponent"]["unspentGold"]), f(&p["opponent"]["netWorth"]) - 1900.0);
        assert_eq!(f(&p["diff"]["gold"]), f(&p["me"]["netWorth"]) - f(&p["opponent"]["netWorth"]));
        assert_eq!(f(&p["diff"]["itemGold"]), 3275.0 - 1900.0);
        assert!(f(&p["gold"]["uncertainty"]) > 0.0);
    }

    #[test]
    fn visible_items_set_a_floor() {
        let p = PerformanceTracker::default().update(&game(600.0, G { opp_items: vec![3153, 3153, 3044], ..Default::default() }), &price_of, None).unwrap();
        assert_eq!(p["opponent"]["netWorth"], json!(7500));
        assert_eq!(p["opponent"]["unspentGold"], json!(0));
        assert_eq!(p["gold"]["confidence"], "high");
    }

    #[test]
    fn history_every_15s_and_resets_on_new_game() {
        let mut tracker = PerformanceTracker::default();
        for t in 600..=660 {
            tracker.update(&game(t as f64, G::default()), &price_of, None);
        }
        let h = tracker.update(&game(661.0, G::default()), &price_of, None).unwrap()["history"].clone();
        let ts: Vec<f64> = h.as_array().unwrap().iter().map(|s| f(&s["t"])).collect();
        assert_eq!(ts, vec![600.0, 615.0, 630.0, 645.0, 660.0]);
        assert_eq!(tracker.update(&game(30.0, G::default()), &price_of, None).unwrap()["history"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn cs_in_steps_of_ten() {
        let mut tracker = PerformanceTracker::default();
        let p = tracker.update(&game(300.0, G { me_cs: 30.0, opp_cs: 20.0, ..Default::default() }), &price_of, None).unwrap();
        assert_eq!(p["csGranularity"], json!(1));
        let p = tracker.update(&game(400.0, G { me_cs: 40.0, opp_cs: 30.0, ..Default::default() }), &price_of, None).unwrap();
        assert_eq!(p["csGranularity"], json!(10));
        let p = tracker.update(&game(450.0, G { me_cs: 43.0, opp_cs: 30.0, ..Default::default() }), &price_of, None).unwrap();
        assert_eq!(p["csGranularity"], json!(1));
    }

    #[test]
    fn another_opponent_restarts_the_history() {
        let mut tracker = PerformanceTracker::default();
        let mut t = 600;
        while t <= 660 {
            tracker.update(&game(t as f64, G::default()), &price_of, None);
            t += 15;
        }
        assert_eq!(tracker.update(&game(675.0, G::default()), &price_of, None).unwrap()["history"].as_array().unwrap().len(), 6);
        let vs = tracker.update(&game(690.0, G::default()), &price_of, Some(Some("Ahri"))).unwrap();
        assert_eq!(vs["opponent"]["championName"], "Ahri");
        assert_eq!(vs["history"].as_array().unwrap().iter().map(|s| f(&s["t"])).collect::<Vec<_>>(), vec![690.0]);
        assert_eq!(tracker.update(&game(705.0, G::default()), &price_of, Some(Some("Ahri"))).unwrap()["history"].as_array().unwrap().len(), 2);
    }
}
