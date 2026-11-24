//! Port of server/model/model.test.ts: Stage B models on synthetic games. Champion profiles (B1), the game-state model
//! and its offline/live parity (B2), and the draft-aware recommender (B3), with its weights. Everything runs offline on
//! the Data Dragon snapshot.

use irelia::capture::{capture_timeline, participant_rows};
use irelia::db::{Db, MatchRow};
use irelia::ddragon::StaticData;
use irelia::engine::exclusivity::{build_exclusivity_index, ExclusivityIndex};
use irelia::engine::recommend::{recommend, EngineSettings};
use irelia::model::context::DRAFT_DIMS;
use irelia::model::dataset::{prepare_games, score_games, train_state, PrepareOptions, PreparedGame, STATE_MAX_ROWS};
use irelia::model::draft::{build_draft_model, draft_defaults, recommend_draft, DraftExtra, DraftModel, DraftQuery};
use irelia::model::logistic::{calibration, fit_logistic, score_probabilities, FitOptions};
use irelia::model::profiles::{team_magic_share, ChampionProfiles, PROFILE_DEFAULTS};
use irelia::model::state::{state_features, state_from_live, states_at, train_state_model, win_probability, GameStateInput, TeamState, TimelineGame, STATE_LAMBDA};
use irelia::model::weights::{pilot_cap_factors, pilot_weight, recency_weigher, region_weight, weight_defaults, RegionPatchFn};
use irelia::testkit::{load_test_static_data, make_match, make_timeline, MatchSpec, TimelineSpec, IRELIA};
use irelia::testkit_model::{ap_ad_games, model_inputs, rng, ApAd, SynthGame, AD_TEAM, ALLIES, AP_TEAM};
use irelia::types::BuildRecommendation;
use serde_json::{json, Value};
use std::sync::Arc;

fn settings() -> EngineSettings {
    EngineSettings {
        min_matchup_games: 15.0,
        general_prior_weight: 0.15,
        min_lane_games: 30.0,
        other_lane_weight: 0.15,
        previous_patch_weight: 0.35,
        min_current_patch_games: 40.0,
        starting_item_window_ms: 60_000.0,
    }
}

struct Built {
    m: DraftModel,
    rows: Vec<Arc<irelia::db::IreliaGameRow>>,
}

fn model(data: &Arc<StaticData>, games: &[SynthGame]) -> Built {
    let (rows, participants, totals) = model_inputs(games);
    let rows: Vec<Arc<_>> = rows.into_iter().map(Arc::new).collect();
    let profiles = Arc::new(ChampionProfiles::new(&totals, &[], data.clone(), PROFILE_DEFAULTS));
    let prepared: Vec<Arc<PreparedGame>> =
        prepare_games(&rows, &participants, data, &profiles, &PrepareOptions { starting_item_window_ms: 60_000.0 }).into_iter().map(Arc::new).collect();
    let state = train_state(prepared.iter().map(|g| g.as_ref()), STATE_LAMBDA, STATE_MAX_ROWS).unwrap();
    let scored = score_games(&prepared, state.as_ref(), None);
    Built { m: build_draft_model(scored, state, profiles), rows }
}

fn baseline(data: &Arc<StaticData>, excl: &ExclusivityIndex, rows: &[Arc<irelia::db::IreliaGameRow>]) -> BuildRecommendation {
    recommend(rows, data, excl, None, &settings(), Some("TOP"), &|_| None, None)
}

fn no_region() -> RegionPatchFn {
    Arc::new(|_| None)
}

fn q(opponent: Option<i64>, enemies: &[i64], allies: &[i64]) -> DraftQuery {
    DraftQuery { lane: "TOP", opponent, enemies: enemies.to_vec(), allies: allies.to_vec() }
}

// ------------------------------------------------------------------------------------------- B1 profiles

#[test]
fn profiles_damage_mix_healing_and_class_shrinkage_from_the_participants_table() {
    let data = load_test_static_data();
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open_default(&dir.path().join("profiles.db")).unwrap();
    let games = ap_ad_games(40, 10, 7, ApAd::default());
    for g in model_inputs(&games).0 {
        db.insert_match(&MatchRow {
            id: g.match_id.clone(),
            platform: "KR".into(),
            patch: g.patch.clone(),
            version: g.game_version.clone().unwrap(),
            queue: 420,
            creation: g.created_at,
            duration: g.game_duration,
            has_irelia: true,
        })
        .unwrap();
    }
    // Participants as the capture stores them.
    for s in &games {
        let mut m = make_match(&MatchSpec { champions: Some(ALLIES.iter().chain(s.enemies.iter()).copied().collect()), ..MatchSpec::id(&s.id) });
        for p in m["info"]["participants"].as_array_mut().unwrap() {
            if AP_TEAM.contains(&p["championId"].as_i64().unwrap()) {
                p["magicDamageDealtToChampions"] = json!(25_000);
                p["physicalDamageDealtToChampions"] = json!(1000);
            }
        }
        db.insert_participants(&s.id, &participant_rows(&m)).unwrap();
    }
    let patches = vec![data.patch.clone()];
    let profiles = ChampionProfiles::new(&db.champion_totals(&patches).unwrap(), &db.older_champion_totals(&patches).unwrap(), data.clone(), PROFILE_DEFAULTS);
    let syndra = profiles.get(134, None);
    assert_eq!(syndra.games, 20.0);
    assert!(syndra.magic_share > 0.75, "Syndra deals mostly magic damage ({})", syndra.magic_share);
    assert!(syndra.ranged && !profiles.get(IRELIA, None).ranged);
    // Leona heals her allies in the fixture (4000 per game): the healing goes to ally_heal_per_min.
    assert!(profiles.get(89, None).ally_heal_per_min > 5.0 * profiles.get(134, None).ally_heal_per_min);
    // A mage nobody played takes the mages' average; a champion with few games sits between its data and its class.
    let brand = profiles.get(63, None);
    assert_eq!(brand.games, 0.0);
    assert!(brand.magic_share > 0.5 && brand.magic_share < syndra.magic_share);
    let team = |ids: &[i64]| ids.iter().map(|&id| profiles.get(id, None)).collect::<Vec<_>>();
    assert!(team_magic_share(&team(&AP_TEAM), None) > 0.7);
    assert!(team_magic_share(&team(&AD_TEAM), None) < 0.5);
}

// ------------------------------------------------------------------------------------------- B2 state model

#[test]
fn logistic_regression_recovers_a_known_effect_and_is_calibrated() {
    let mut r = rng(3);
    let mut x: Vec<Vec<f64>> = vec![];
    let mut y: Vec<f64> = vec![];
    for _ in 0..4000 {
        let v = r() * 4.0 - 2.0;
        x.push(vec![1.0, v]);
        y.push(if r() < 1.0 / (1.0 + (-(0.3 + 1.2 * v)).exp()) { 1.0 } else { 0.0 });
    }
    let fit = fit_logistic(&x, &y, &FitOptions { lambda: 0.1, ..Default::default() }).unwrap();
    assert!(fit.converged);
    assert!((fit.beta[1] - 1.2).abs() < 0.15, "slope {}", fit.beta[1]);
    assert!((fit.beta[0] - 0.3).abs() < 0.15, "intercept {}", fit.beta[0]);
    let p: Vec<f64> = x.iter().map(|x| 1.0 / (1.0 + (-(fit.beta[0] + fit.beta[1] * x[1])).exp())).collect();
    let s = score_probabilities(&p, &y);
    assert!(s.brier < 0.22 && s.log_loss < 0.63);
    for b in calibration(&p, &y, 10).iter().filter(|b| b.n > 200.0) {
        assert!((b.predicted - b.observed).abs() < 0.07);
    }
}

fn team(inv: f64) -> TeamState {
    TeamState { inventory: inv, level: 9.0, ..Default::default() }
}

#[test]
fn state_model_more_gold_means_a_higher_win_chance() {
    let mut r = rng(5);
    let mut xs: Vec<Vec<f64>> = vec![];
    let mut wins: Vec<f64> = vec![];
    for _ in 0..3000 {
        let lead = (r() - 0.5) * 8000.0;
        let s = GameStateInput { t: 900.0 + r() * 900.0, us: team(15_000.0 + lead), them: team(15_000.0), me: None, opponent: None };
        xs.push(state_features(&s).to_vec());
        wins.push(if r() < 1.0 / (1.0 + (-lead / 2000.0).exp()) { 1.0 } else { 0.0 });
    }
    let rows: Vec<&[f64]> = xs.iter().map(|x| x.as_slice()).collect();
    let m = train_state_model(&rows, &wins, 300.0, STATE_LAMBDA).unwrap();
    let at = |lead: f64| win_probability(&m.beta, &GameStateInput { t: 1200.0, us: team(15_000.0 + lead), them: team(15_000.0), me: None, opponent: None });
    assert!(at(4000.0) > 0.8 && at(-4000.0) < 0.2 && (at(0.0) - 0.5).abs() < 0.08, "{} {} {}", at(4000.0), at(0.0), at(-4000.0));
}

#[test]
fn the_same_moment_gives_the_same_state_from_a_stored_timeline_and_from_the_live_client() {
    let data = load_test_static_data();
    let m = make_match(&MatchSpec { minutes: Some(20), ..MatchSpec::id("KR_1") });
    let cap = capture_timeline(&make_timeline(&m, &TimelineSpec::default()));
    let price = |id: f64| data.item(id as i64).map(|i| i.gold_total).unwrap_or(0.0);
    let team_of = |p: f64| if p <= 5.0 { 100.0 } else { 200.0 };
    let offline = states_at(&TimelineGame { cap: &cap, irelia: 1.0, opponent: Some(6.0), team_of: &team_of, price_of: &price }, &[13.0 * 60_000.0], None)
        .remove(0);
    // By 13:00 in the fixture: Irelia killed participant 6, team 200 took a dragon, team 100 a top tower; Irelia owns
    // Doran's Blade and a potion.
    assert_eq!(offline.us.kills, 1.0);
    assert_eq!(offline.them.dragons, 1.0);
    assert_eq!(offline.us.towers, 1.0);
    assert_eq!(offline.us.inventory, 500.0);
    assert_eq!(offline.me.as_ref().unwrap().inventory - offline.opponent.as_ref().unwrap().inventory, 500.0);

    let frame = |pid: u32| cap.frames[&pid][13];
    let players: Vec<Value> = m["info"]["participants"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| {
            let pid = p["participantId"].as_u64().unwrap() as u32;
            let puuid = p["puuid"].as_str().unwrap();
            json!({
                "championName": data.champion(p["championId"].as_i64().unwrap()).unwrap().key,
                "riotId": format!("{puuid}#KR"), "riotIdGameName": puuid,
                "team": if p["teamId"] == 100 { "ORDER" } else { "CHAOS" }, "position": p["teamPosition"], "level": frame(pid)[3],
                "items": if pid == 1 { json!([{ "itemID": 1055 }, { "itemID": 2003 }]) } else { json!([]) },
                "scores": { "kills": if pid == 1 { 1 } else { 0 }, "deaths": if pid == 6 { 1 } else { 0 }, "assists": 0, "creepScore": frame(pid)[4] },
            })
        })
        .collect();
    let name = |i: usize| players[i]["riotIdGameName"].clone();
    let raw = json!({
        "activePlayer": { "riotId": players[0]["riotId"], "currentGold": 0 }, "allPlayers": players, "gameData": { "gameTime": 780 },
        "events": { "Events": [
            { "EventID": 1, "EventName": "ChampionKill", "EventTime": 360, "KillerName": name(0), "VictimName": name(5) },
            { "EventID": 2, "EventName": "DragonKill", "EventTime": 540, "KillerName": name(6), "DragonType": "Fire" },
            { "EventID": 3, "EventName": "TurretKilled", "EventTime": 720, "KillerName": name(0), "TurretKilled": "Turret_T2_L_03_A" },
        ] },
    });
    let index = irelia_server::lcu::live_client::name_index(&players);
    let team_of_name = |n: Option<&str>| index(n).map(|i| if players[i]["team"] == "ORDER" { 100.0 } else { 200.0 });
    let live = state_from_live(&raw, &players[0], Some(&players[5]), &price, &team_of_name);
    assert_eq!(state_features(&live), state_features(&offline));
}

// ------------------------------------------------------------------------------------------- B3 draft model

fn prepared(source: &str, tier: Option<&str>, lp: Option<f64>, platform: &str) -> PreparedGame {
    let games = ap_ad_games(1, 1, 7, ApAd::default());
    let data = load_test_static_data();
    let (rows, participants, totals) = model_inputs(&games);
    let rows: Vec<Arc<_>> = rows.into_iter().map(Arc::new).collect();
    let profiles = ChampionProfiles::new(&totals, &[], data.clone(), PROFILE_DEFAULTS);
    let mut g = prepare_games(&rows, &participants, &data, &profiles, &PrepareOptions { starting_item_window_ms: 60_000.0 }).remove(0);
    g.source = source.into();
    g.tier = tier.map(String::from);
    g.lp = lp;
    g.mastery = Some(500_000.0);
    g.platform = platform.into();
    g
}

#[test]
fn weights_pilot_class_and_tier_region_the_5_percent_pilot_cap() {
    let w = weight_defaults();
    let g = |source: &str| prepared(source, Some("CHALLENGER"), Some(1000.0), "KR");
    assert_eq!(pilot_weight(&g("otp"), &w), 1.0);
    assert!((pilot_weight(&g("main"), &w) - 0.8).abs() < 1e-9);
    assert!((pilot_weight(&prepared("harvested", Some("UNKNOWN"), None, "KR"), &w) - 0.3).abs() < 1e-9);
    assert_eq!(pilot_weight(&g("me"), &w), 0.0);
    let euw = prepared("otp", Some("CHALLENGER"), Some(1000.0), "EUW1");
    assert_eq!(region_weight(&euw, &irelia::model::weights::WeightOptions { own_platform: Some("EUW1".into()), ..weight_defaults() }), 1.5);
    let na = prepared("otp", Some("CHALLENGER"), Some(1000.0), "NA1");
    assert_eq!(region_weight(&na, &irelia::model::weights::WeightOptions { regions: Some(vec!["KR".into()]), ..weight_defaults() }), 0.0);

    // One pilot with half of 40 pilots' games is cut to 5 % of the lane.
    let names: Vec<String> = (0..100).map(|i| format!("p{}", i % 40)).collect();
    let mut games: Vec<(&str, &str, f64)> = (0..100).map(|_| ("whale", "TOP", 1.0)).collect();
    games.extend(names.iter().map(|n| (n.as_str(), "TOP", 1.0)));
    let f = pilot_cap_factors(&games, 0.05);
    let total: f64 = games.iter().map(|(p, _, w)| w * f[&format!("TOP:{p}")]).sum();
    assert!((100.0 * f["TOP:whale"] / total - 0.05).abs() < 1e-6);
    // With fewer than 20 pilots the cap rises to an equal share.
    let few = pilot_cap_factors(&[("a", "MIDDLE", 9.0), ("b", "MIDDLE", 1.0)], 0.05);
    assert!((9.0 * few["MIDDLE:a"] - 1.0).abs() < 1e-6);
}

#[test]
fn draft_model_the_item_that_wins_against_the_enemy_damage_type_is_picked_with_the_reason() {
    let data = load_test_static_data();
    let excl = build_exclusivity_index(&data);
    let b = model(&data, &ap_ad_games(800, 40, 7, ApAd::default()));
    assert!(b.m.state.is_some(), "state model trained");
    let base = baseline(&data, &excl, &b.rows);
    let rp = no_region();
    let s = draft_defaults();
    let x = DraftExtra::default();
    let vs_ap = recommend_draft(&b.m, &q(Some(AP_TEAM[0]), &AP_TEAM, &ALLIES[1..]), &data, &excl, &rp, &base, &s, &x);
    let vs_ad = recommend_draft(&b.m, &q(Some(AD_TEAM[0]), &AD_TEAM, &ALLIES[1..]), &data, &excl, &rp, &base, &s, &x);
    let first = |r: &irelia::types::DraftRecommendation| r.picks.iter().find(|p| p.key == "core1").unwrap().clone();
    assert_eq!(first(&vs_ap).value, "3053", "{:?}", first(&vs_ap));
    assert_eq!(first(&vs_ad).value, "3153", "{:?}", first(&vs_ad));
    // Whichever the lane-level pick is, the other draft moved away from it, and says why.
    let moved = [first(&vs_ap), first(&vs_ad)].into_iter().find(|p| p.reason.is_some()).expect("one of the two picks differs from the lane-level pick");
    assert!(moved.reason.unwrap().contains("magic damage"));
    // Each pick names the lane build's choice it replaces, with that choice's lift in this draft.
    let ap1 = first(&vs_ap);
    assert_eq!(ap1.baseline.as_ref().unwrap().value, base.core_build[0].value[0].to_string());
    assert!(ap1.baseline.as_ref().unwrap().lift.is_some());
    let st = ap1.stats.unwrap();
    assert!(st.lift > 0.0 && st.low < st.lift);
    // The assembled build is in the baseline's shape and never holds two exclusive items.
    assert_eq!(vs_ap.build.core_build[0].value[0], 3053);
    let mut all = vs_ap.build.core_build[0].value.clone();
    all.push(vs_ap.build.boots[0].value);
    assert_eq!(excl.conflicts(&all).len(), 0);
    assert_eq!(vs_ap.build.runes.as_ref().unwrap().value.perks.len(), 6);
    assert_eq!(vs_ap.build.skills.as_ref().unwrap().value.path.len(), 18);
    assert!(vs_ap.context.iter().any(|c| c.key == "eMagic" && c.z > 0.5));
    assert_eq!(DRAFT_DIMS.len(), b.m.z_draft.d);
}

#[test]
fn draft_model_too_few_similar_games_keep_the_lane_baseline_flagged() {
    let data = load_test_static_data();
    let excl = build_exclusivity_index(&data);
    let b = model(&data, &ap_ad_games(24, 12, 7, ApAd::default()));
    let base = baseline(&data, &excl, &b.rows);
    let r = recommend_draft(&b.m, &q(None, &AP_TEAM, &[]), &data, &excl, &no_region(), &base, &draft_defaults(), &DraftExtra::default());
    let core1 = r.picks.iter().find(|p| p.key == "core1").unwrap();
    assert!(core1.fallback);
    assert_eq!(core1.value, base.core_build[0].value[0].to_string());
    assert!(!core1.changed);
}

#[test]
fn the_users_own_games_never_feed_the_draft_model() {
    let data = load_test_static_data();
    let excl = build_exclusivity_index(&data);
    let games = ap_ad_games(200, 20, 7, ApAd::default());
    let mut b = model(&data, &games);
    for s in b.m.scored.iter_mut() {
        let mut g = (*s.g).clone();
        g.source = "me".into();
        s.g = Arc::new(g);
    }
    let rows: Vec<Arc<_>> = model_inputs(&games[..50]).0.into_iter().map(Arc::new).collect();
    let base = baseline(&data, &excl, &rows);
    let r = recommend_draft(&b.m, &q(None, &AP_TEAM, &[]), &data, &excl, &no_region(), &base, &draft_defaults(), &DraftExtra::default());
    assert_eq!(r.sample.games, 0.0);
    assert!(r.picks.iter().all(|p| p.stats.is_none() || p.fallback));
}

// ------------------------------------------------------------------------------------------- fine-tuning (0.15.0)

#[test]
fn fine_tuning_pilot_standings_strength_the_fade_within_a_patch_and_the_item_decisions_own_prior() {
    let mut g = prepared("otp", Some("DIAMOND"), None, "KR");
    g.mastery = Some(100_000.0);
    g.created = 0.0;
    g.patch = "16.21".into();
    let w = weight_defaults();
    let w1 = pilot_weight(&g, &w);
    assert!(w1 > 0.0 && w1 < 1.0);
    let with = |s: f64| irelia::model::weights::WeightOptions { pilot_strength: Some(s), ..weight_defaults() };
    assert_eq!(pilot_weight(&g, &with(1.0)), w1, "1: as before");
    assert_eq!(pilot_weight(&g, &with(0.0)), 1.0, "0: every pilot alike");
    assert!((pilot_weight(&g, &with(2.0)) - w1 * w1).abs() < 1e-12);
    let mut me = g.clone();
    me.source = "me".into();
    assert_eq!(pilot_weight(&me, &with(0.0)), 0.0, "your own games never count");

    let day = 86_400_000.0;
    let age = |x: &PreparedGame| Some(if x.patch == "16.21" { 0.0 } else { 1.0 });
    let fade = recency_weigher(30.0 * day, Some(7.0), &age);
    let at = |created: f64, patch: &str| {
        let mut x = g.clone();
        x.created = created;
        x.patch = patch.into();
        x
    };
    assert_eq!(fade(&at(30.0 * day, "16.21")), 1.0);
    assert!((fade(&at(23.0 * day, "16.21")) - 0.5).abs() < 1e-12, "half after 7 days");
    assert_eq!(fade(&at(2.0 * day, "16.20")), 1.0, "older patches: patch memory fades them");
    let zero = |_: &PreparedGame| Some(0.0);
    assert_eq!(recency_weigher(30.0 * day, None, &zero)(&at(0.0, "16.21")), 1.0, "none by default");

    // The item decisions' prior shrinks the item estimates alone.
    let data = load_test_static_data();
    let excl = build_exclusivity_index(&data);
    let b = model(&data, &ap_ad_games(400, 40, 7, ApAd::default()));
    let base = baseline(&data, &excl, &b.rows);
    let est = |item_k: Option<f64>| {
        let mut s = draft_defaults();
        s.item_k = item_k;
        recommend_draft(&b.m, &q(None, &AP_TEAM, &[]), &data, &excl, &no_region(), &base, &s, &DraftExtra { reasons: Some(false), detail: true, ..Default::default() })
            .estimates
            .unwrap()
    };
    let (plain, tight) = (est(None), est(Some(100_000.0)));
    assert_eq!(tight["keystone"], plain["keystone"], "pre-game decisions keep the shared prior");
    let spread = |e: &indexmap::IndexMap<String, f64>| {
        let v: Vec<f64> = e.values().copied().collect();
        v.iter().cloned().fold(f64::MIN, f64::max) - v.iter().cloned().fold(f64::MAX, f64::min)
    };
    assert!(spread(&tight["core1"]) < spread(&plain["core1"]), "a much stronger item prior pulls the item options together");
}

#[test]
fn patch_weights_follow_the_data_dragon_diff_manual_overrides_and_hotfixes() {
    use irelia::model::weights::{PatchWeigher, WeightOptions};
    let data = load_test_static_data();
    let pg = |patch: &str, created: f64| {
        let mut g = prepared("otp", Some("CHALLENGER"), Some(1000.0), "KR");
        g.patch = patch.into();
        g.created = created;
        g
    };
    let fp = data.fingerprints.clone();
    let with_previous = |items: std::collections::HashMap<i64, String>, champion: String| {
        let mut d = (*data).clone();
        d.previous = Some(("16.18.1".into(), irelia::ddragon::Fingerprints { items, champion }));
        Arc::new(d)
    };
    let weigher = |d: Arc<StaticData>, o: &WeightOptions| PatchWeigher::new(d, no_region(), o);
    let w = weight_defaults();
    let same = weigher(with_previous(fp.items.clone(), fp.champion.clone()), &w);
    assert_eq!(same.weigh(&pg("16.19", 0.0), &[]), 1.0);
    assert_eq!(same.weigh(&pg("16.18", 0.0), &[3153]), 0.8);
    assert_eq!(same.weigh(&pg("16.17", 0.0), &[]), 0.0);
    let mut changed_items = fp.items.clone();
    changed_items.insert(3153, "nerfed".into());
    let item_changed = weigher(with_previous(changed_items, fp.champion.clone()), &w);
    assert_eq!(item_changed.weigh(&pg("16.18", 0.0), &[3153]), 0.2);
    assert_eq!(item_changed.weigh(&pg("16.18", 0.0), &[3078]), 0.8);
    let irelia_changed = weigher(with_previous(fp.items.clone(), "old".into()), &w);
    assert_eq!(irelia_changed.weigh(&pg("16.18", 0.0), &[]), 0.2);
    assert_eq!(weigher(data.clone(), &w).weigh(&pg("16.18", 0.0), &[]), 0.35, "no previous files: unknown");
    let opts = WeightOptions {
        patch_weights: vec![("16.18".into(), 0.6)],
        hotfixes: vec![irelia::scope::ManualHotfix { patch: "16.19".into(), date: "2026-09-26T00:00:00Z".into(), note: None }],
        ..weight_defaults()
    };
    let manual = weigher(with_previous(fp.items.clone(), fp.champion.clone()), &opts);
    assert_eq!(manual.weigh(&pg("16.18", 0.0), &[3153]), 0.6);
    assert_eq!(manual.weigh(&pg("16.19", irelia::testkit::date_utc(2026, 8, 25) as f64), &[]), 0.5);
    assert_eq!(manual.weigh(&pg("16.19", irelia::testkit::date_utc(2026, 8, 27) as f64), &[]), 1.0);
}
