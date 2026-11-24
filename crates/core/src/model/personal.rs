//! Port of `server/model/personal.ts` (B4 personal layer): your item timings against the high-elo median, which
//! options work for you (shrunk toward the high-elo estimate), and a review of each game. Everything stays local.

use super::dataset::{lane_diff, PreparedGame, ScoredGame};
use super::estimate::{estimate_decision, EstimateOptions, Obs, Observation, Query, Z};
use crate::capture::TimelineCapture;
use crate::db::{Db, ParticipantLite, RecLogRow};
use crate::ddragon::StaticData;
use crate::js;
use crate::types::to_lane;
use anyhow::Result;
use indexmap::IndexMap;
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::sync::Arc;

/// Pseudo-games of the high-elo estimate: your own results weigh as much after this many games.
pub const PERSONAL_K: f64 = 30.0;

fn median(xs: &[f64]) -> Option<f64> {
    if xs.is_empty() {
        return None;
    }
    let mut s = xs.to_vec();
    s.sort_by(|a, b| crate::engine::recommend::js_order(a - b));
    let n = s.len();
    Some(if n % 2 == 1 { s[(n - 1) / 2] } else { (s[n / 2 - 1] + s[n / 2]) / 2.0 })
}

type OptionOf = fn(&ScoredGame) -> Option<(String, f64)>;

fn decisions() -> Vec<(&'static str, &'static str, OptionOf)> {
    fn item(s: &ScoredGame, i: usize) -> Option<(String, f64)> {
        s.g.items.get(i).map(|c| (js::num_to_string(c.item), s.item_wpa[i]))
    }
    vec![
        ("keystone", "Keystone", |s| Some((s.g.perks.first().map(|p| p.to_string()).unwrap_or_else(|| "undefined".into()), s.wpa0))),
        ("core1", "First item", |s| item(s, 0)),
        ("core2", "Second item", |s| item(s, 1)),
        ("core3", "Third item", |s| item(s, 2)),
        ("boots", "Boots", |s| match (&s.g.boots, s.boots_wpa) {
            (Some(b), Some(w)) => Some((js::num_to_string(b.item), w)),
            _ => None,
        }),
        ("maxFirst", "Ability maxed first", |s| s.g.max_first.as_ref().filter(|m| !m.is_empty()).map(|m| (m.clone(), s.wpa0))),
        ("summoners", "Summoner spells", |s| Some((s.g.summoners.clone(), s.wpa0))),
    ]
}

/// PersonalReport as JSON.
pub fn personal_report(scored: &[ScoredGame], lane: Option<&str>) -> Value {
    let in_lane = |g: &PreparedGame| lane.map(|l| g.lane == l).unwrap_or(true);
    let mine: Vec<&ScoredGame> = scored.iter().filter(|s| s.g.source == "me" && in_lane(&s.g)).collect();
    let others: Vec<&ScoredGame> = scored.iter().filter(|s| s.g.source != "me" && in_lane(&s.g)).collect();
    let minutes = |games: &[&ScoredGame]| {
        let mut m: IndexMap<i64, Vec<f64>> = IndexMap::new();
        for s in games {
            for c in s.g.items.iter().take(3) {
                m.entry(c.item as i64).or_default().push(c.t / 60_000.0);
            }
        }
        m
    };
    let you = minutes(&mine);
    let them = minutes(&others);
    let mut timings: Vec<(f64, Value)> = you
        .iter()
        .map(|(id, ts)| {
            let he = them.get(id).map(|v| v.as_slice()).unwrap_or(&[]);
            (
                ts.len() as f64,
                json!({ "itemId": id, "you": js::num(median(ts).unwrap()), "games": ts.len(), "highElo": median(he).map(js::num), "highEloGames": he.len() }),
            )
        })
        .collect();
    timings.sort_by(|a, b| crate::engine::recommend::js_order(b.0 - a.0));

    struct Row {
        key: &'static str,
        games: f64,
        v: Value,
    }
    let mut options: Vec<Row> = Vec::new();
    for (key, label, option) in decisions() {
        let obs: Vec<Observation> = others
            .iter()
            .filter_map(|s| {
                option(s).map(|(o, wpa)| Observation {
                    option: Arc::from(o.as_str()),
                    wpa,
                    win: s.g.win,
                    start_win: f64::NAN,
                    w: 1.0,
                    lane: s.g.lane,
                    opponent: None,
                    pilot: Arc::from(s.g.pilot.as_str()),
                    z: Z::None,
                    row: None,
                    age: None,
                })
            })
            .collect();
        let obs = Obs::new(obs);
        let mut lanes: Vec<&'static str> = Vec::new();
        for s in &mine {
            if !lanes.contains(&s.g.lane) {
                lanes.push(s.g.lane);
            }
        }
        let mut high_elo: std::collections::HashMap<String, f64> = std::collections::HashMap::new();
        for l in lanes {
            let q = Query { lane: l, opponent: None, z: vec![], sims: None, memo: None };
            let o = EstimateOptions { h: f64::INFINITY, k: 100.0, min_games: 1.0, min_share: 0.0 };
            for e in estimate_decision(&obs, &q, &o, None, None).options {
                high_elo.insert(format!("{}:{}", l, e.option), e.lane_theta);
            }
        }
        let mut by_option: IndexMap<String, (&'static str, Vec<f64>)> = IndexMap::new();
        for s in &mine {
            let Some((o, wpa)) = option(s) else { continue };
            by_option.entry(format!("{}:{}", s.g.lane, o)).or_insert((s.g.lane, vec![])).1.push(wpa);
        }
        for (k, (l, wpa)) in by_option {
            let n = wpa.len() as f64;
            let m = wpa.iter().fold(0.0, |a, b| a + b) / n;
            let he = high_elo.get(&k).copied().unwrap_or(0.0);
            options.push(Row {
                key,
                games: n,
                v: json!({
                    "key": key, "label": label, "value": k.split(':').nth(1).unwrap_or(""), "lane": l, "games": n as i64,
                    "yourLift": js::num(m), "highEloLift": js::num(he), "blended": js::num((n * m + PERSONAL_K * he) / (n + PERSONAL_K)),
                }),
            });
        }
    }
    options.sort_by(|a, b| a.key.to_lowercase().cmp(&b.key.to_lowercase()).then_with(|| crate::engine::recommend::js_order(b.games - a.games)));
    json!({
        "games": mine.len(),
        "wins": mine.iter().filter(|s| s.g.win != 0.0).count(),
        "timings": timings.into_iter().map(|t| t.1).collect::<Vec<_>>(),
        "options": options.into_iter().map(|o| o.v).collect::<Vec<_>>(),
    })
}

/// A rec_log row belongs to the game whose window contains it: champ select up to 20 min before the game, to its end.
pub fn link_recommendations(db: &Db, now: i64) -> Result<usize> {
    let recs = db.unlinked_recommendations(now - 5 * 60_000)?;
    if recs.is_empty() {
        return Ok(0);
    }
    let games = db.own_games(200)?;
    let mut linked = 0;
    for r in &recs {
        let mut fits: Vec<_> = games
            .iter()
            .filter(|x| r.created_at >= x.created_at - 20 * 60_000 && r.created_at <= x.created_at + x.game_duration * 1000 + 10 * 60_000)
            .collect();
        fits.sort_by_key(|x| (x.created_at - r.created_at).abs());
        let Some(g) = fits.first() else { continue };
        let purchases: Vec<Value> = serde_json::from_str::<Vec<Value>>(&g.purchases).unwrap_or_default();
        let perks: Vec<Value> = serde_json::from_str(&g.perks).unwrap_or_default();
        let bought = json!({ "keystone": perks.first().cloned(), "purchases": purchases.iter().map(|p| p[0].clone()).collect::<Vec<_>>() });
        db.link_recommendation(r.id, &g.match_id, &bought, g.win == 1)?;
        linked += 1;
    }
    Ok(linked)
}

/// One of your games: what the app showed, what you did, how it went (GameReview as JSON), or None.
pub fn review_game(match_id: &str, prepared: &[Arc<PreparedGame>], scored: &[ScoredGame], db: &Db, data: &StaticData, participants: &[ParticipantLite]) -> Result<Option<Value>> {
    let owns = db.own_games(500)?;
    let Some(own) = owns.iter().find(|g| g.match_id == match_id) else { return Ok(None) };
    let game = prepared.iter().find(|g| g.match_id == match_id);
    let lane = to_lane(Some(&own.position));
    let recs: Vec<RecLogRow> = db.recommendations_for(match_id)?;
    let high_elo: Vec<&ScoredGame> = scored.iter().filter(|s| s.g.source != "me" && Some(s.g.lane) == lane).collect();
    let median_minute = |item: f64, slot: usize| {
        median(&high_elo.iter().filter_map(|s| s.g.items.get(slot)).filter(|c| c.item == item).map(|c| c.t / 60_000.0).collect::<Vec<_>>())
    };
    let (mut lane10, mut lane15) = (Value::Null, Value::Null);
    if let Some(tl) = &own.timeline {
        if let Some(cap) = TimelineCapture::parse(tl) {
            let ps: Vec<&ParticipantLite> = participants.iter().filter(|p| p.match_id == match_id).collect();
            let me = own.participant_id.or_else(|| ps.iter().find(|p| p.champion_id == 39).map(|p| p.participant_id));
            let my_team = ps.iter().find(|p| Some(p.participant_id) == me).map(|p| p.team_id);
            let opp = ps.iter().find(|p| Some(p.team_id) != my_team && Some(p.champion_id) == own.opponent_champion_id).map(|p| p.participant_id as f64);
            if let Some(me) = me {
                let d = |m: f64| lane_diff(&cap, me as f64, opp, m).map(|(g, c)| json!({ "gold": js::num(g), "cs": js::num(c) })).unwrap_or(Value::Null);
                lane10 = d(10.0);
                lane15 = d(15.0);
            }
        }
    }
    let shown: Vec<Value> = recs
        .iter()
        .map(|r| json!({ "phase": r.phase, "mode": r.mode, "build": serde_json::from_str::<Value>(&r.shown).unwrap_or(Value::Null) }))
        .collect();
    let items: Vec<(f64, Value)> = game
        .map(|g| g.items.iter().enumerate().map(|(slot, c)| (c.item, json!({ "itemId": js::num(c.item), "minute": js::num(c.t / 60_000.0), "highEloMinute": median_minute(c.item, slot).map(js::num) }))).collect())
        .unwrap_or_default();
    let perks: Vec<Value> = serde_json::from_str(&own.perks).unwrap_or_default();
    let keystone = perks.first().cloned().unwrap_or(Value::Null);
    let draft_shown = shown.iter().find(|s| s["phase"] == "draft").map(|s| &s["build"]);
    let followed = draft_shown.map(|b| {
        let core = b["core"].as_array().map(|a| a.iter().enumerate().filter(|(i, id)| items.get(*i).map(|x| id.as_f64() == Some(x.0)).unwrap_or(false)).count()).unwrap_or(0);
        json!({ "keystone": b["keystone"] == keystone, "core": core })
    });
    let mut names: BTreeMap<i64, String> = BTreeMap::new();
    for (id, _) in &items {
        names.insert(*id as i64, data.item(*id as i64).map(|i| i.name.clone()).unwrap_or_else(|| format!("#{}", js::num_to_string(*id))));
    }
    Ok(Some(json!({
        "matchId": match_id, "created": own.created_at, "lane": lane, "opponentId": own.opponent_champion_id, "win": own.win == 1,
        "kda": [own.kills, own.deaths, own.assists], "minutes": js::num(js::round(own.game_duration as f64 / 60.0)),
        "keystone": keystone, "items": items.iter().map(|x| x.1.clone()).collect::<Vec<_>>(), "boots": game.and_then(|g| g.boots.as_ref()).map(|b| js::num(b.item)),
        "shown": shown, "followed": followed, "lane10": lane10, "lane15": lane15, "itemName": names,
    })))
}
