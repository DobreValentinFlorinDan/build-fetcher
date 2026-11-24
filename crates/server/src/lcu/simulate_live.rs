//! Port of `server/lcu/simulateLive.ts`: synthetic Live Client data for POST /api/dev/simulate-live (Irelia vs Darius
//! top, or vs Syndra mid; one game minute per 6 real seconds; {positions: false} leaves positions empty).

use serde_json::{json, Value};

type Seat = (&'static str, &'static str, &'static str, &'static str);

fn roster(lane: &str) -> [Seat; 10] {
    if lane == "MIDDLE" {
        [
            ("You", "Irelia", "ORDER", "MIDDLE"), ("Ally Top", "Ornn", "ORDER", "TOP"), ("Ally Jungle", "LeeSin", "ORDER", "JUNGLE"),
            ("Ally Bot", "Jinx", "ORDER", "BOTTOM"), ("Ally Support", "Thresh", "ORDER", "UTILITY"),
            ("Enemy Top", "Darius", "CHAOS", "TOP"), ("Enemy Jungle", "Vi", "CHAOS", "JUNGLE"), ("Enemy Mid", "Syndra", "CHAOS", "MIDDLE"),
            ("Enemy Bot", "Kaisa", "CHAOS", "BOTTOM"), ("Enemy Support", "Nautilus", "CHAOS", "UTILITY"),
        ]
    } else {
        [
            ("You", "Irelia", "ORDER", "TOP"), ("Ally Jungle", "LeeSin", "ORDER", "JUNGLE"), ("Ally Mid", "Ahri", "ORDER", "MIDDLE"),
            ("Ally Bot", "Jinx", "ORDER", "BOTTOM"), ("Ally Support", "Thresh", "ORDER", "UTILITY"),
            ("Enemy Top", "Darius", "CHAOS", "TOP"), ("Enemy Jungle", "Vi", "CHAOS", "JUNGLE"), ("Enemy Mid", "Syndra", "CHAOS", "MIDDLE"),
            ("Enemy Bot", "Kaisa", "CHAOS", "BOTTOM"), ("Enemy Support", "Nautilus", "CHAOS", "UTILITY"),
        ]
    }
}

const ME_ITEMS: &[(f64, &[i64])] = &[
    (0.0, &[3340]), (20.0, &[1055, 2003, 3340]), (300.0, &[1055, 1036, 1001, 2055, 3340]), (540.0, &[3153, 1055, 1001, 2055, 3340]),
    (780.0, &[3153, 3047, 1055, 3044, 3340]), (1080.0, &[3153, 3047, 3078, 3364]), (1440.0, &[3153, 3174, 3078, 3053, 3364]),
];
const OPP_TOP: &[(f64, &[i64])] = &[
    (0.0, &[1054, 2003, 3340]), (330.0, &[1054, 3044, 3340]), (600.0, &[1054, 3044, 1001, 1036, 3340]),
    (840.0, &[3071, 1054, 3047, 3340]), (1200.0, &[3071, 3047, 3044, 1036, 3364]), (1500.0, &[3071, 3047, 3053, 3364]),
];
const OPP_MID: &[(f64, &[i64])] = &[
    (0.0, &[1056, 2003, 3340]), (330.0, &[1056, 3802, 3340]), (600.0, &[1056, 3802, 1001, 3340]),
    (840.0, &[6655, 1056, 3020, 3340]), (1200.0, &[6655, 3020, 3089, 3364]), (1500.0, &[6655, 3020, 3089, 3157, 3364]),
];

fn at(sched: &[(f64, &'static [i64])], t: f64) -> &'static [i64] {
    sched.iter().rev().find(|(s, _)| t >= *s).map(|x| x.1).unwrap_or(&[])
}

/// The simulated game: call `tick()` once per second.
pub struct SimulatedGame {
    pub t: f64,
    lane: &'static str,
    positions: bool,
    events: Vec<Value>,
    game_seconds_per_tick: f64,
}

impl SimulatedGame {
    pub fn new(start_at: f64, lane: &'static str, positions: bool) -> SimulatedGame {
        SimulatedGame { t: start_at, lane, positions, events: vec![json!({ "EventID": 0, "EventName": "GameStart", "EventTime": 0 })], game_seconds_per_tick: 10.0 }
    }

    fn has(&self, time: f64) -> bool {
        self.events.iter().any(|e| e["EventTime"].as_f64() == Some(time))
    }
    fn kill(&mut self, time: f64, killer: &str, victim: &str, assisters: &[&str]) {
        let id = self.events.len();
        self.events.push(json!({ "EventID": id, "EventName": "ChampionKill", "EventTime": time, "KillerName": killer, "VictimName": victim, "Assisters": assisters }));
    }

    /// The next snapshot (allgamedata as JSON).
    pub fn tick(&mut self) -> Value {
        let t = self.t;
        let opponent = if self.lane == "MIDDLE" { "Enemy Mid" } else { "Enemy Top" };
        if t >= 390.0 && !self.has(390.0) {
            self.kill(390.0, "You", opponent, &["Ally Jungle"]);
            let id = self.events.len();
            self.events.push(json!({ "EventID": id, "EventName": "FirstBlood", "EventTime": 390, "Recipient": "You" }));
        }
        if t >= 700.0 && !self.has(700.0) {
            self.kill(700.0, "Enemy Jungle", "You", &[opponent]);
        }
        if t >= 1020.0 && !self.has(1020.0) {
            self.kill(1020.0, "You", opponent, &[]);
        }
        let min = t / 60.0;
        let kills = |e: &Value, key: &str, name: &str| e["EventName"] == "ChampionKill" && e[key] == name;
        let players: Vec<Value> = roster(self.lane)
            .iter()
            .map(|(name, champion, team, position)| {
                let me = *name == "You";
                let opp = *name == opponent;
                let cs_rate = if *position == "UTILITY" { 1.2 } else if me { 8.1 } else if opp { 7.2 } else { 6.5 };
                let items: &[i64] = if me { at(ME_ITEMS, t) } else if opp { at(if self.lane == "MIDDLE" { OPP_MID } else { OPP_TOP }, t) } else { &[] };
                json!({
                    "championName": champion, "riotId": format!("{name}#SIM"), "riotIdGameName": name, "summonerName": format!("{name}#SIM"),
                    "team": team, "position": if self.positions { *position } else { "" },
                    "level": f64::min(18.0, 1.0 + (min / 1.4).floor()),
                    "items": items.iter().enumerate().map(|(slot, id)| json!({ "itemID": id, "count": 1, "slot": slot })).collect::<Vec<_>>(),
                    "scores": {
                        "kills": self.events.iter().filter(|e| kills(e, "KillerName", name)).count(),
                        "deaths": self.events.iter().filter(|e| kills(e, "VictimName", name)).count(),
                        "assists": self.events.iter().filter(|e| e["EventName"] == "ChampionKill" && e["Assisters"].as_array().map(|a| a.iter().any(|x| x == name)).unwrap_or(false)).count(),
                        "creepScore": f64::max(0.0, ((min - 1.5) * cs_rate).floor()),
                        "wardScore": irelia::js::num(irelia::js::round(min * if me { 0.75 } else { 0.6 } * 10.0) / 10.0),
                    },
                })
            })
            .collect();
        let raw = json!({
            "activePlayer": { "riotId": "You#SIM", "riotIdGameName": "You", "summonerName": "You#SIM", "currentGold": if t < 20.0 { 500.0 } else { 80.0 + ((t * 9.0) % 700.0) } },
            "allPlayers": players,
            "events": { "Events": self.events.clone() },
            "gameData": { "gameTime": t, "gameMode": "CLASSIC" },
        });
        self.t += self.game_seconds_per_tick;
        raw
    }
}
