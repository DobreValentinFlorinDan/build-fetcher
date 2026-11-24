//! Port of server/model/service.test.ts: the draft model inside the app (model/service.rs) on a real database. Built
//! from SQL, the lane baseline by default until an evaluation passes, the user's switch winning over both, the user's
//! own games kept out of the model, and the switches (patch memory, runes against the draft, late swaps).
//!
//! The tests share one database and settings folder in order, as in 0.x, so they run as one test. 0.x's check that
//! the kept model drops each purchase's state features is about its memory layout: Rust keeps them (they are small),
//! and drops the state frames as 0.x does. Its "evaluation's reading on threads" part ports with prepareTeam.ts,
//! which 1.0 replaced with rayon inside the build: the build on two threads is checked against one.

use irelia::app_config::update_app_config;
use irelia::db::Db;
use irelia::engine::exclusivity::build_exclusivity_index;
use irelia::engine::recommend::{recommend, EngineSettings};
use irelia::model::buy::{BuyIndex, BuyQuery};
use irelia::model::draft::{recommend_draft, DraftExtra, DraftQuery};
use irelia::model::service::{
    before_window, build_from_db, current_mode, current_swaps, data_due, eval_dir, read_settings, with_switches, ModelEnv, ModelService, BUILD_CANCELLED,
    DATA_REBUILD,
};
use irelia::model::run_eval::model_patches;
use irelia::scope::default_scope;
use irelia::testkit::load_test_static_data;
use irelia::testkit_model::{ap_ad_games, store_games, store_games_as, ApAd, SynthGame, AP_TEAM};
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

fn latest(v: Value) {
    std::fs::create_dir_all(eval_dir()).unwrap();
    let mut o = json!({ "evaluatedAt": irelia::util::now_ms(), "gate": "failed", "bandwidth": 0.7, "k": 100, "lambda": 1, "dataVersion": "" });
    for (k, x) in v.as_object().unwrap() {
        o[k] = x.clone();
    }
    std::fs::write(eval_dir().join("latest.json"), o.to_string()).unwrap();
}

fn set(key: &str, v: Option<&str>) {
    update_app_config(vec![(key, v.map(|s| json!(s)))]);
}

fn renumber(games: Vec<SynthGame>, from: usize) -> Vec<SynthGame> {
    games.into_iter().enumerate().map(|(i, g)| SynthGame { id: format!("KR_{}", from + i), ..g }).collect()
}

fn q(lane: &'static str, opponent: Option<i64>, enemies: &[i64]) -> DraftQuery {
    DraftQuery { lane, opponent, enemies: enemies.to_vec(), allies: vec![] }
}

#[test]
fn service_suite() {
    let root = tempfile::tempdir().unwrap();
    std::env::set_var("IRELIA_CONFIG_DIR", root.path().join("config"));
    let data = load_test_static_data();
    let excl = Arc::new(build_exclusivity_index(&data));
    let db = Db::open_default(&root.path().join("irelia.db")).unwrap();
    let scope = default_scope();
    let env = ModelEnv {
        data: data.clone(),
        excl: excl.clone(),
        scope: scope.clone(),
        region_patch: Arc::new(|_| None),
        own_platform: Some("KR".into()),
        busy: false,
        starting_item_window_ms: 60_000.0,
    };
    let rp = env.region_patch.clone();

    // ---- builds from the database, keeps the baseline until an evaluation passes, and the switch wins
    store_games(&db, &ap_ad_games(300, 30, 7, ApAd::default()), Some(&data.patch));
    let mine: Vec<SynthGame> = renumber(ap_ad_games(4, 1, 9, ApAd::default()), 90_000).into_iter().map(|g| SynthGame { pilot: "ME".into(), ..g }).collect();
    store_games_as(&db, &mine, Some(&data.patch), "me");
    let mut service = ModelService::default();
    let rows: Vec<Arc<_>> = db.games_for_patches(&[data.patch.clone()], &scope.build_sources).unwrap().into_iter().map(Arc::new).collect();
    let baseline = recommend(&rows, &data, &excl, None, &settings(), Some("TOP"), &|_| None, None);
    let top_ap = q("TOP", None, &AP_TEAM);
    assert!(service.draft(&env, &top_ap, None, &baseline, true).is_none(), "before the first build there is only the baseline");
    assert!(service.needs_build(&db, &env));
    service.rebuild(&db, &env, 1);
    assert!(!service.needs_build(&db, &env), "nothing changed since");
    let r = service.draft(&env, &top_ap, None, &baseline, true).expect("a draft-adjusted build");
    assert_eq!(r.sample.games, 300.0, "your own four games are not in it");
    assert_eq!(current_mode(), "baseline");
    let info = service.info(&env).unwrap();
    assert_eq!((info["gate"].clone(), info["games"].clone()), (json!("not evaluated"), json!(300)));
    assert_eq!(service.current().unwrap().prepared.iter().filter(|g| g.source == "me").count(), 4, "the personal layer still sees them");
    // The kept model gives the same draft-adjusted build as a model built afresh.
    let fresh = build_from_db(&db, &data, &scope, 60_000.0, None, &None, None, &rp, 1).unwrap();
    assert_eq!(r, recommend_draft(&fresh.model, &top_ap, &data, &excl, &rp, &baseline, &read_settings(&scope, Some("KR".into()), None), &DraftExtra::default()));
    // The state frames go; the best buy for your gold reads the win probability at each one instead, with the same answers.
    let kept = service.current().unwrap();
    assert!(kept.prepared.iter().all(|g| g.frames.is_none()) && kept.prepared.iter().any(|g| g.p_at.as_ref().map(|p| p.len() > 10).unwrap_or(false)));
    {
        let (rows, participants, _) = {
            let patches = model_patches(&db, &data, 2.0).unwrap();
            let mut sources = scope.draft_sources.clone();
            sources.push("me".into());
            (db.model_games(&patches, &sources).unwrap(), db.model_participants(&patches, &sources).unwrap(), ())
        };
        let rows: Vec<Arc<_>> = rows.into_iter().map(Arc::new).collect();
        let prepared: Vec<Arc<_>> = irelia::model::dataset::prepare_games(&rows, &participants, &data, &kept.profiles, &irelia::model::dataset::PrepareOptions { starting_item_window_ms: 60_000.0 })
            .into_iter()
            .map(Arc::new)
            .collect();
        assert!(prepared.iter().any(|g| g.frames.is_some()), "this one keeps them");
        let state = kept.model.state.clone();
        let with_frames = irelia::model::dataset::score_games(&prepared, state.as_ref(), None);
        for (target, gold) in [(3078, 1300.0), (3053, 900.0), (3153, 2000.0)] {
            let query = BuyQuery { lane: "TOP", opponent: None, target, owned: &[], gold };
            let best = BuyIndex::new(Arc::new(kept.model.scored.clone()), state.clone(), data.clone()).best(&query);
            assert!(best.as_ref().map(|b| !b["items"].as_array().unwrap().is_empty()).unwrap_or(false), "a purchase toward {target}");
            assert_eq!(best, BuyIndex::new(Arc::new(with_frames.clone()), state.clone(), data.clone()).best(&query));
        }
    }
    latest(json!({ "gate": "passed", "bandwidth": 0.5, "k": 30 }));
    service.evaluated();
    assert_eq!(current_mode(), "draft");
    let s = read_settings(&scope, Some("KR".into()), None);
    assert_eq!((s.estimate.h, s.estimate.k), (0.5, 30.0));
    set("buildMode", Some("baseline"));
    assert_eq!(current_mode(), "baseline");
    set("buildMode", None);
    assert_eq!(current_mode(), "draft");

    // ---- patch memory follows the evaluation unless switched, reads the older patches, keeps the state model on two
    store_games(&db, &renumber(ap_ad_games(200, 20, 5, ApAd::default()), 50_000), Some("16.16"));
    assert!(!model_patches(&db, &data, 2.0).unwrap().contains(&"16.16".to_string()), "two patches: not read");
    assert!(model_patches(&db, &data, 6.0).unwrap().contains(&"16.16".to_string()), "within the window");
    let old = before_window(&db, &data, 6.0).unwrap();
    assert_eq!(
        [old("16.19"), old("16.16"), old("16.14"), old("16.13"), old("15.20")],
        [Some(false), Some(false), Some(false), Some(true), None]
    );
    let mut service = ModelService::default();
    latest(json!({ "gate": "passed", "bandwidth": 0.5, "k": 30 }));
    service.rebuild(&db, &env, 1);
    let off = service.info(&env).unwrap();
    assert_eq!(
        [off["memory"]["active"].clone(), off["memory"]["choice"].clone(), off["memory"]["gate"].clone(), off["games"].clone(), off["memory"]["olderGames"].clone()],
        [json!(false), json!("auto"), json!("not evaluated"), json!(300), json!(0)]
    );
    // The switch (saved by the main thread): on at once; the model holds it once rebuilt.
    set("patchMemory", Some("on"));
    service.options_changed();
    assert_eq!(read_settings(&scope, None, None).weights.memory.map(|m| m.patches), Some(6.0));
    let i = service.info(&env).unwrap();
    assert_eq!((i["memory"]["active"].clone(), i["memory"]["inModel"].clone(), service.needs_build(&db, &env)), (json!(true), json!(false), true));
    service.rebuild(&db, &env, 1);
    let on = service.info(&env).unwrap();
    assert_eq!(
        [on["memory"]["active"].clone(), on["memory"]["choice"].clone(), on["games"].clone(), on["memory"]["olderGames"].clone(), on["memory"]["decay"].clone(), on["memory"]["inModel"].clone()],
        [json!(true), json!("on"), json!(500), json!(200), json!(0.75), json!(true)]
    );
    assert_eq!(on["stateModel"]["games"], json!(300), "the game-state model learns from today's and the previous patch only");
    // An evaluation that found it better turns it on by itself, with its fade; Off wins over it.
    latest(json!({ "memory": { "gate": "passed", "decay": 0.9 } }));
    set("patchMemory", None);
    service.evaluated();
    assert_eq!(service.info(&env).unwrap()["memory"]["choice"], json!("auto"));
    assert_eq!(read_settings(&scope, None, None).weights.memory.map(|m| m.decay), Some(0.9));
    set("patchMemory", Some("off"));
    service.options_changed();
    assert!(read_settings(&scope, None, None).weights.memory.is_none());
    set("patchMemory", None);
    service.options_changed();

    // ---- runes against the draft follow the evaluation unless switched, and change the pre-game scores only
    let mut service = ModelService::default();
    latest(json!({ "scoring": { "gate": "failed", "penalty": null } }));
    service.evaluated();
    service.rebuild(&db, &env, 1);
    assert_eq!(service.info(&env).unwrap()["scoring"], json!({ "active": false, "gate": "failed", "choice": "auto", "inModel": false }));
    let overall = service.current().unwrap().model.scored.clone();
    let rates: std::collections::BTreeSet<u64> = overall.iter().map(|x| (x.wpa0 + (1.0 - x.g.win) * 2.0).to_bits()).collect();
    assert_eq!(rates.len(), 2, "one rate for every game: two values (won, lost)");
    latest(json!({ "scoring": { "gate": "passed", "penalty": { "lambda": 100, "opponentLambda": 100 } } }));
    service.evaluated();
    service.rebuild(&db, &env, 1);
    assert_eq!(service.info(&env).unwrap()["scoring"], json!({ "active": true, "gate": "passed", "choice": "auto", "inModel": true }));
    let fair = service.current().unwrap().model.scored.clone();
    let by_id: std::collections::HashMap<&str, &irelia::model::dataset::ScoredGame> = overall.iter().map(|x| (x.g.match_id.as_str(), x)).collect();
    assert!(fair.iter().any(|x| (x.wpa0 - by_id[x.g.match_id.as_str()].wpa0).abs() > 1e-6), "pre-game scores move");
    assert!(fair.iter().all(|x| x.item_wpa == by_id[x.g.match_id.as_str()].item_wpa), "item scores stay");
    // Your switch (saved by the main thread, which also stops following the evaluation): Off wins over the verdict.
    set("pregameScoring", Some("off"));
    set("patchMemory", Some("off"));
    service.options_changed();
    assert_eq!(service.info(&env).unwrap()["scoring"], json!({ "active": false, "gate": "passed", "choice": "off", "inModel": true }));
    set("pregameScoring", None);
    set("patchMemory", None);

    // ---- a switch moved during a build is what the next build is for
    let mut service = ModelService::default();
    service.rebuild(&db, &env, 1);
    let job = service.begin_build(&env);
    set("patchMemory", Some("on"));
    service.options_changed();
    let result = job.run(&db, &env, 1, &|| false);
    service.finish_build(job, result, &env);
    let mid = service.info(&env).unwrap();
    assert_eq!((mid["memory"]["active"].clone(), mid["memory"]["inModel"].clone(), service.needs_build(&db, &env)), (json!(true), json!(false), true), "built without it, so another build is due");
    service.rebuild(&db, &env, 1);
    let done = service.info(&env).unwrap();
    assert_eq!((done["memory"]["active"].clone(), done["memory"]["inModel"].clone()), (json!(true), json!(true)));
    set("patchMemory", None);
    service.options_changed();

    // ---- a cancelled build changes nothing
    let job = service.begin_build(&env);
    let r = job.run(&db, &env, 1, &|| true);
    assert_eq!(r.as_ref().err().map(|e| e.to_string()), Some(BUILD_CANCELLED.to_string()));
    let before = service.current().unwrap().built_at;
    assert!(!service.finish_build(job, r, &env));
    assert_eq!(service.current().unwrap().built_at, before);
    assert!(service.error().is_none());

    // ---- champ-select drafts reuse the weights and observations, with the same answers, and let them go when idle
    let mut service = ModelService::default();
    service.rebuild(&db, &env, 1);
    let empty = recommend(&[], &data, &excl, None, &settings(), Some("TOP"), &|_| None, None);
    let mut drafts = vec![q("TOP", Some(122), &[122, AP_TEAM[1], AP_TEAM[2], AP_TEAM[3], AP_TEAM[4]]), q("TOP", None, &AP_TEAM[..3])];
    drafts.push(DraftQuery { lane: "MIDDLE", opponent: Some(134), enemies: AP_TEAM.to_vec(), allies: vec![] });
    for d in &drafts {
        let kept = service.draft(&env, d, None, &empty, true);
        let model = &service.current().unwrap().model;
        let fresh = recommend_draft(model, d, &data, &excl, &rp, &empty, &read_settings(&scope, Some("KR".into()), None), &DraftExtra::default());
        assert_eq!(kept, Some(fresh));
    }
    assert!(service.keeps_drafts(), "kept between drafts");
    assert!(!service.trim_caches(60_000), "still kept a moment after the last draft");
    assert!(service.trim_caches(0));
    assert!(!service.keeps_drafts(), "let go once idle");
    // Outside champ select: the same answer, nothing kept (and what the last champ select kept goes).
    service.draft(&env, &drafts[0], None, &empty, true);
    let outside = service.draft(&env, &drafts[1], None, &empty, false);
    let model = &service.current().unwrap().model;
    assert_eq!(outside, Some(recommend_draft(model, &drafts[1], &data, &excl, &rp, &empty, &read_settings(&scope, Some("KR".into()), None), &DraftExtra::default())));
    assert!(!service.keeps_drafts());

    // ---- a build on more threads is the same build, in the same order (0.15.0: then helper threads; now rayon, from
    // 4,000 games on)
    {
        let big = Db::open_default(&root.path().join("big.db")).unwrap();
        store_games(&big, &renumber(ap_ad_games(4100, 60, 11, ApAd::default()), 1_000_000), Some(&data.patch));
        let one = build_from_db(&big, &data, &scope, 60_000.0, None, &None, None, &rp, 1).unwrap();
        let many = build_from_db(&big, &data, &scope, 60_000.0, None, &None, None, &rp, 4).unwrap();
        assert_eq!(one.prepared.len(), 4100);
        let bits = |v: &[f64]| v.iter().map(|x| x.to_bits()).collect::<Vec<_>>();
        assert_eq!(format!("{:?}", many.prepared), format!("{:?}", one.prepared));
        assert_eq!(bits(&many.model.z_draft.flat), bits(&one.model.z_draft.flat));
    }

    // ---- late-game swaps (0.17.0) follow the evaluation unless switched, show at once, and make no build due
    let mut service = ModelService::default();
    latest(json!({}));
    service.evaluated();
    service.rebuild(&db, &env, 1);
    assert_eq!(service.info(&env).unwrap()["swaps"], json!({ "active": false, "gate": "not evaluated", "choice": "auto" }));
    latest(json!({ "swaps": { "gate": "failed" } }));
    assert_eq!(service.info(&env).unwrap()["swaps"], json!({ "active": false, "gate": "failed", "choice": "auto" }));
    latest(json!({ "swaps": { "gate": "passed" } }));
    service.evaluated();
    assert!(current_swaps());
    assert_eq!(service.info(&env).unwrap()["swaps"], json!({ "active": true, "gate": "passed", "choice": "auto" }));
    // (needs_build waits 5 minutes after an attempt; the key is what says whether the model is built the same)
    service.rebuild(&db, &env, 1);
    let built_at = service.current().unwrap().built_at;
    let before = service.info(&env).unwrap();
    set("lateSwaps", Some("off"));
    set("patchMemory", Some("off"));
    set("pregameScoring", Some("off"));
    service.options_changed();
    assert_eq!(with_switches(before.clone())["swaps"], json!({ "active": false, "gate": "passed", "choice": "off" }));
    assert_eq!(service.info(&env).unwrap()["swaps"], json!({ "active": false, "gate": "passed", "choice": "off" }));
    set("lateSwaps", Some("on"));
    assert_eq!(with_switches(before)["swaps"], json!({ "active": true, "gate": "passed", "choice": "on" }));
    // Not following, with no choice saved (Follow the evaluation left before 0.17.0): off, as runes against the draft.
    set("lateSwaps", None);
    assert!(!current_swaps());
    assert_eq!(service.info(&env).unwrap()["swaps"]["choice"], json!("off"));
    assert_eq!(service.current().unwrap().built_at, built_at);
    set("patchMemory", None);
    set("pregameScoring", None);
}

#[test]
fn new_data_is_due_for_a_build_at_1_percent_of_the_games_at_least_50_after_half_an_hour_or_after_six_hours() {
    let min = 60_000.0;
    let o = &DATA_REBUILD;
    assert!(!data_due("118000:1:90000:118000", "118000:1:90000:118000", 7.0 * 60.0 * min, o), "nothing changed");
    assert!(!data_due("118000:1:90000:118000", "118080:2:90080:118080", 45.0 * min, o), "80 games: under 1 %");
    assert!(data_due("118000:1:90000:118000", "119180:2:91180:119180", 45.0 * min, o), "1,180 games");
    assert!(!data_due("118000:1:90000:118000", "119180:2:91180:119180", 10.0 * min, o), "but not before half an hour");
    assert!(data_due("118000:1:90000:118000", "118001:2:90001:118001", 6.0 * 60.0 * min, o), "six hours: any change");
    assert!(!data_due("2000:1:1500:2000", "2049:2:1549:2049", 31.0 * min, o), "at least 50 games");
    assert!(data_due("2000:1:1500:2000", "2000:1:1500:2050", 31.0 * min, o), "a backfill of 50 captures counts too");
}
