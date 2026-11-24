//! Port of `server/test/modelFixtures.ts`: synthetic Irelia games for the draft-model tests. AP or AD enemy teams,
//! chosen items and results drawn from a seeded generator, turned into the rows the model reads from the database.

use crate::capture::{capture_timeline, participant_rows, CAPTURE_VERSION};
use crate::db::{ChampionTotals, Db, IreliaGameRow, MatchRow, ParticipantLite};
use crate::scraper::ingest::capture_game;
use crate::testkit::{make_match, make_timeline, MatchSpec, TimelineSpec, IRELIA};
use serde_json::{json, Value};

/// Deterministic random numbers in [0, 1) (mulberry32), the same sequence as the TS fixture's `rng`.
pub fn rng(seed: u32) -> impl FnMut() -> f64 {
    let mut a = seed;
    move || {
        a = a.wrapping_add(0x6d2b_79f5);
        let mut t = a;
        t = (t ^ (t >> 15)).wrapping_mul(t | 1);
        t ^= t.wrapping_add((t ^ (t >> 7)).wrapping_mul(t | 61));
        (t ^ (t >> 14)) as f64 / 4_294_967_296.0
    }
}

/// Five ranged mages and five ranged marksmen: the two enemy teams differ only in their damage type.
pub const AP_TEAM: [i64; 5] = [134, 1, 103, 99, 61];
pub const AD_TEAM: [i64; 5] = [51, 22, 110, 222, 21];
/// Irelia's allies in every game.
pub const ALLIES: [i64; 5] = [IRELIA, 121, 86, 202, 89];
/// Top laners with the fixture's default damage (mostly physical): opponents that say nothing about the enemy team.
pub const NEUTRAL_TOPS: [i64; 5] = [122, 157, 54, 516, 24];

#[derive(Clone, Debug, Default)]
pub struct SynthGame {
    pub id: String,
    pub pilot: String,
    pub enemies: Vec<i64>,
    /// Completed items in order, and boots.
    pub items: Vec<i64>,
    pub boots: Option<i64>,
    pub win: bool,
    pub minutes: Option<i64>,
    /// 'TOP' (default) or 'MIDDLE'.
    pub position: Option<&'static str>,
    pub creation: Option<i64>,
    pub version: Option<String>,
    /// Irelia's summoner spells (default Flash and Teleport).
    pub summoners: Option<(i64, i64)>,
    /// The pilot's ladder standing when the game was stored.
    pub tier: Option<String>,
    pub lp: Option<i64>,
}

/// Damage to champions per champion: mages deal magic, marksmen physical, everyone else the fixture default.
fn damage(champion_id: i64) -> Option<(i64, i64)> {
    if AP_TEAM.contains(&champion_id) {
        Some((1000, 25_000))
    } else if AD_TEAM.contains(&champion_id) {
        Some((22_000, 800))
    } else {
        None
    }
}

/// `synthMatch`: the match and its timeline.
pub fn synth_match(s: &SynthGame) -> (Value, Value) {
    let champions: Vec<i64> = ALLIES.iter().chain(s.enemies.iter()).copied().collect();
    let positions = (s.position == Some("MIDDLE")).then(|| {
        ["MIDDLE", "JUNGLE", "TOP", "BOTTOM", "UTILITY", "MIDDLE", "JUNGLE", "TOP", "BOTTOM", "UTILITY"].iter().map(|p| p.to_string()).collect()
    });
    let mut m = make_match(&MatchSpec {
        id: s.id.clone(),
        champions: Some(champions.clone()),
        positions,
        winner: Some(if s.win { 100 } else { 200 }),
        minutes: Some(s.minutes.unwrap_or(30)),
        creation: s.creation,
        version: s.version.clone(),
        puuids: Some((0..champions.len()).map(|i| if i == 0 { s.pilot.clone() } else { format!("{}-p{}", s.id, i + 1) }).collect()),
        ..Default::default()
    });
    for p in m["info"]["participants"].as_array_mut().unwrap() {
        if let Some((physical, magic)) = damage(p["championId"].as_i64().unwrap()) {
            p["physicalDamageDealtToChampions"] = json!(physical);
            p["magicDamageDealtToChampions"] = json!(magic);
        }
    }
    if let Some((a, b)) = s.summoners {
        m["info"]["participants"][0]["summoner1Id"] = json!(a);
        m["info"]["participants"][0]["summoner2Id"] = json!(b);
    }
    let minute_of = [10, 16, 22, 26, 28, 29];
    let mut items: Vec<(i64, String, i64, Option<i64>)> = vec![];
    if let Some(b) = s.boots {
        items.push((6, "ITEM_PURCHASED".into(), b, None));
    }
    for (k, id) in s.items.iter().enumerate() {
        items.push((minute_of[k], "ITEM_PURCHASED".into(), *id, None));
    }
    let timeline = make_timeline(&m, &TimelineSpec { items, ..Default::default() });
    (m, timeline)
}

/// What db.model_games / model_participants / champion_totals would return for these games (platform KR): `modelInputs`.
pub fn model_inputs(games: &[SynthGame]) -> (Vec<IreliaGameRow>, Vec<ParticipantLite>, Vec<ChampionTotals>) {
    let mut rows = vec![];
    let mut participants = vec![];
    let mut totals: Vec<ChampionTotals> = vec![];
    for s in games {
        let (m, timeline) = synth_match(s);
        let me = m["info"]["participants"][0].clone();
        let cap = capture_timeline(&timeline);
        let mut row = capture_game(&m, &timeline, &me, "otp", None).row;
        row.platform = Some(s.id.split('_').next().unwrap().to_string());
        row.remake = Some(0);
        row.leaver = Some(0);
        row.end_result = Some("GameComplete".into());
        row.game_version = m["info"]["gameVersion"].as_str().map(String::from);
        row.timeline = Some(cap.to_json_string());
        if let Some(t) = &s.tier {
            row.pilot_tier = Some(t.clone());
            row.pilot_lp = s.lp;
        }
        rows.push(row);
        let duration = m["info"]["gameDuration"].as_f64().unwrap();
        for p in participant_rows(&m) {
            participants.push(ParticipantLite {
                match_id: p.match_id.clone(),
                participant_id: p.participant_id,
                champion_id: p.champion_id,
                team_id: p.team_id,
                position: p.position.clone(),
            });
            let i = match totals.iter().position(|t| t.champion_id == p.champion_id && t.position == p.position) {
                Some(i) => i,
                None => {
                    totals.push(ChampionTotals { champion_id: p.champion_id, position: p.position.clone(), ..Default::default() });
                    totals.len() - 1
                }
            };
            let t = &mut totals[i];
            t.games += 1.0;
            t.seconds += duration;
            t.dmg_physical += p.dmg_physical as f64;
            t.dmg_magic += p.dmg_magic as f64;
            t.dmg_true += p.dmg_true as f64;
            t.dmg_taken += p.dmg_taken as f64;
            t.dmg_mitigated += p.dmg_mitigated as f64;
            t.heal += p.heal as f64;
            t.heal_allies += p.heal_allies as f64;
            t.shield_allies += p.shield_allies as f64;
            t.cc_time += p.cc_time as f64;
        }
    }
    (rows, participants, totals)
}

#[derive(Clone, Copy, Debug, Default)]
pub struct ApAd {
    /// false: both items win 55 % everywhere.
    pub no_effect: bool,
    /// A neutral top laner in lane, so the matchup says nothing about the comp.
    pub random_opponent: bool,
}

/// `apAdGames`: `n` top games of `pilots` pilots, half against the AP team, half against the AD team. The first item is
/// Sterak's Gage (3053) or Blade of the Ruined King (3153) at random; Sterak's wins 70 % against AP and 40 % against
/// AD, BotRK the reverse, so both win 55 % overall and only the draft tells them apart.
pub fn ap_ad_games(n: usize, pilots: usize, seed: u32, o: ApAd) -> Vec<SynthGame> {
    let mut r = rng(seed);
    (0..n)
        .map(|i| {
            let ap = i % 2 == 0;
            let sterak = r() < 0.5;
            let p = if o.no_effect {
                0.55
            } else if ap == sterak {
                0.7
            } else {
                0.4
            };
            let team = if ap { AP_TEAM } else { AD_TEAM };
            let enemies = if o.random_opponent {
                let first = NEUTRAL_TOPS[(r() * NEUTRAL_TOPS.len() as f64).floor() as usize];
                std::iter::once(first).chain(team[1..].iter().copied()).collect()
            } else {
                team.to_vec()
            };
            SynthGame {
                id: format!("KR_{}", 10_000 + i),
                pilot: format!("pilot{}", i % pilots),
                enemies,
                items: vec![if sterak { 3053 } else { 3153 }, 3078, 6333],
                boots: Some(3047),
                win: r() < p,
                ..Default::default()
            }
        })
        .collect()
}

/// `pilotSkillGames`: `n` top games where the pilot's strength decides a summoner spell (0.9.0's scoring against the
/// expected win rate): strong pilots (Challenger, 1,200 LP: 62 %) take Ignite in 85 % of their games, the others
/// (Master, 50 LP: 42 %) in 15 %; Teleport otherwise. Ignite itself costs 8 points.
pub fn pilot_skill_games(n: usize, pilots: usize, seed: u32) -> Vec<SynthGame> {
    let mut r = rng(seed);
    (0..n)
        .map(|i| {
            let pilot = i % pilots;
            let strong = pilot % 2 == 0;
            let ignite = r() < if strong { 0.85 } else { 0.15 };
            let p = if strong { 0.62 } else { 0.42 } - if ignite { 0.08 } else { 0.0 };
            let team = if i % 2 == 0 { AP_TEAM } else { AD_TEAM };
            let first = NEUTRAL_TOPS[(r() * NEUTRAL_TOPS.len() as f64).floor() as usize];
            let enemies = std::iter::once(first).chain(team[1..].iter().copied()).collect();
            let first_item = if r() < 0.5 { 3053 } else { 3153 };
            SynthGame {
                id: format!("KR_{}", 20_000 + i),
                pilot: format!("pilot{pilot}"),
                enemies,
                items: vec![first_item, 3078, 6333],
                boots: Some(3047),
                win: r() < p,
                summoners: Some((4, if ignite { 14 } else { 12 })),
                tier: Some(if strong { "CHALLENGER" } else { "MASTER" }.into()),
                lp: Some(if strong { 1200 } else { 50 }),
                ..Default::default()
            }
        })
        .collect()
}

/// Stores synthetic games the way fetching does (match, Irelia row, players, timeline, quality), in one transaction.
/// `patch` overrides each game's version with `<patch>.1.1`.
pub fn store_games(db: &Db, games: &[SynthGame], patch: Option<&str>) {
    store_games_as(db, games, patch, "otp")
}

/// `store_games` with the games' source ('me' for your own).
pub fn store_games_as(db: &Db, games: &[SynthGame], patch: Option<&str>, source: &str) {
    db.tx(|| {
        for g in games {
            let mut s = g.clone();
            if let Some(p) = patch {
                s.version = Some(format!("{p}.1.1"));
            }
            let (m, timeline) = synth_match(&s);
            let me = m["info"]["participants"][0].clone();
            let cap = capture_game(&m, &timeline, &me, source, None);
            let version = m["info"]["gameVersion"].as_str().unwrap().to_string();
            let patch = version.split('.').take(2).collect::<Vec<_>>().join(".");
            db.insert_match(&MatchRow {
                id: s.id.clone(),
                platform: s.id.split('_').next().unwrap().into(),
                patch,
                version,
                queue: 420,
                creation: m["info"]["gameCreation"].as_i64().unwrap(),
                duration: m["info"]["gameDuration"].as_i64().unwrap(),
                has_irelia: true,
            })?;
            db.insert_game(&cap.row)?;
            db.insert_participants(&s.id, &participant_rows(&m))?;
            db.insert_timeline(&s.id, cap.timeline.interval as i64, &cap.timeline.to_json_string())?;
            db.set_quality(&s.id, false, Some("GameComplete"), CAPTURE_VERSION)?;
        }
        Ok(())
    })
    .expect("store synthetic games");
}

#[cfg(test)]
mod tests {
    #[test]
    fn mulberry32_matches_the_ts_fixture() {
        // rng(7) in Node: 0.011704753153026104, 0.06195825757458806, 0.97690763277933
        let mut r = super::rng(7);
        let xs: Vec<f64> = (0..3).map(|_| r()).collect();
        assert_eq!(xs, [0.011704753153026104, 0.06195825757458806, 0.97690763277933]);
    }
}

/// A prepared game with nothing in it (KR, patch 16.19, a 30-minute top win): tests fill in what they need.
pub fn blank_game() -> crate::model::dataset::PreparedGame {
    crate::model::dataset::PreparedGame {
        match_id: "KR_1".into(),
        platform: "KR".into(),
        patch: "16.19".into(),
        created: 0.0,
        pilot: "p".into(),
        source: "otp".into(),
        tier: None,
        lp: None,
        mastery: None,
        lane: "TOP",
        dg: None,
        ix: 0,
        win: 1.0,
        opponent: None,
        enemies: vec![],
        allies: vec![],
        draft: [0.0; 12],
        primary: 0,
        sub: 0,
        perks: vec![],
        shards: vec![],
        summoners: String::new(),
        start: String::new(),
        max_first: None,
        early: None,
        items: vec![],
        boots: None,
        swaps: vec![],
        frames: None,
        frame_t: None,
        p_at: None,
        purchases: vec![],
        item_ends: vec![],
        boots_end: None,
        purchase_ends: vec![],
        kda: [0.0; 3],
        duration: 1800.0,
        lane10: None,
        lane15: None,
    }
}
