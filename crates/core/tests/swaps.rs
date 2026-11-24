//! Port of server/model/swaps.test.ts: late-game swaps (0.17.0, docs/SWAPS.md). What a shop visit with every slot full
//! decides, the advice the draft model gives from such decisions, and the Evaluate builds gate on them.

use irelia::capture::{TimelineCapture, BUY, DESTROY, SELL as SOLD, UNDO};
use irelia::engine::exclusivity::build_exclusivity_index;
use irelia::engine::recommend::EngineSettings;
use irelia::model::dataset::{prepare_games, score_games, PrepareOptions, PreparedGame, SwapChoice};
use irelia::model::draft::{advise_swap, build_draft_model, item_query_of, weigh_games, DraftQuery};
use irelia::model::estimate::{EstimateOptions, ESTIMATE_DEFAULTS};
use irelia::model::eval_base::{update_from, BaseHeader};
use irelia::model::evaluate::{eval_defaults, evaluate, EvalBaseData, EvalInput, EvalOptions, EvalReport};
use irelia::model::profiles::{ChampionProfiles, PROFILE_DEFAULTS};
use irelia::model::run_eval::format_eval;
use irelia::model::service::Mark;
use irelia::model::swaps::{swap_decisions, SwapOptions, KEEP, SELL, SWAP_DEFAULTS};
use irelia::model::weights::{weight_defaults, RegionPatchFn};
use irelia::testkit::load_test_static_data;
use irelia::testkit_model::{ap_ad_games, model_inputs, rng, ApAd, SynthGame, AP_TEAM};
use irelia::types::SwapPick;
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

const MIN: f64 = 60_000.0;
const BOOTS: f64 = 3047.0;
const BORK: f64 = 3153.0;
const TRINITY: f64 = 3078.0;
const STERAK: f64 = 3053.0;
const DANCE: f64 = 6333.0;
const CLEAVER: f64 = 3071.0;
const GA: f64 = 3026.0;
const VISAGE: f64 = 3065.0;
const PICKAXE: f64 = 1037.0;

#[test]
fn swaps_a_full_inventorys_shop_visits_from_20_00_what_became_of_each_item_held_an_undo_and_components_taken_into_account() {
    let data = load_test_static_data();
    let at = |minute: f64, kind: f64, item: f64| (minute * MIN, 1.0, kind, item, None);
    let cap = TimelineCapture {
        items: vec![
            at(10.0, BUY, BOOTS),
            at(12.0, BUY, BORK),
            at(15.0, BUY, TRINITY),
            at(18.0, BUY, STERAK),
            at(21.0, BUY, DANCE),
            at(24.0, BUY, CLEAVER),
            (24.5 * MIN, 2.0, BUY, GA, None), // someone else's purchase: not hers
            at(26.0, BUY, 2003.0),
            (26.0 * MIN + 10_000.0, 1.0, DESTROY, 2003.0, None), // a visit with every slot full: everything kept
            at(30.0, SOLD, BOOTS),
            (30.0 * MIN + 5_000.0, 1.0, BUY, PICKAXE, None), // boots sold for a component...
            at(33.0, BUY, GA),
            (33.0 * MIN + 1.0, 1.0, DESTROY, PICKAXE, None), // ...and Guardian Angel finished: not full before
            at(36.0, SOLD, CLEAVER),
            (36.0 * MIN + 3_000.0, 1.0, UNDO, 0.0, Some(CLEAVER)), // sold and taken back: kept
            at(38.0, SOLD, BORK),                                  // sold, nothing finished after
        ],
        ..Default::default()
    };
    let item_of = |id: f64| data.items.get(&(id as i64));
    let d = swap_decisions(&cap, 1.0, &item_of, &SWAP_DEFAULTS);
    let mut points: Vec<f64> = vec![];
    for x in &d {
        if !points.contains(&(x.t / MIN)) {
            points.push(x.t / MIN);
        }
    }
    assert_eq!(points, vec![26.0, 30.0, 36.0, 38.0], "the visits with every slot full, from 20:00");
    assert!(d.iter().all(|x| x.w == 1.0 / 4.0), "the game's points share one game's weight");
    let of = |minute: f64| -> BTreeMap<i64, f64> { d.iter().filter(|x| x.t == minute * MIN).map(|x| (x.held as i64, x.option)).collect() };
    let map = |pairs: &[(f64, f64)]| -> BTreeMap<i64, f64> { pairs.iter().map(|(k, v)| (*k as i64, *v)).collect() };
    assert_eq!(of(26.0), map(&[(BOOTS, KEEP), (BORK, KEEP), (TRINITY, KEEP), (STERAK, KEEP), (DANCE, KEEP), (CLEAVER, KEEP)]));
    assert_eq!(of(30.0), map(&[(BOOTS, GA), (BORK, KEEP), (TRINITY, KEEP), (STERAK, KEEP), (DANCE, KEEP), (CLEAVER, KEEP)]), "boots for the next legendary");
    assert_eq!(of(36.0), map(&[(BORK, KEEP), (TRINITY, KEEP), (STERAK, KEEP), (DANCE, KEEP), (CLEAVER, KEEP), (GA, KEEP)]), "the boots are gone; the sell undone");
    assert_eq!(of(38.0)[&(BORK as i64)], SELL, "nothing finished within 10 minutes");
    let o = SwapOptions { from_minute: 31.0, visit_gap_ms: 30_000.0, replace_within_ms: 10.0 * MIN, full_slots: 6 };
    let mut later: Vec<f64> = vec![];
    for x in swap_decisions(&cap, 1.0, &item_of, &o) {
        if !later.contains(&(x.t / MIN)) {
            later.push(x.t / MIN);
        }
    }
    assert_eq!(later, vec![36.0, 38.0], "from the minute set");
}

/// Synthetic games with one late decision each about their boots: against the AP team (`effect`), swapping them for
/// Spirit Visage goes with winning; against the AD team, and without `effect`, it says nothing.
fn with_swaps(effect: bool, seed: u32) -> impl Fn(&mut Vec<PreparedGame>) {
    let r = Mutex::new(Box::new(rng(seed)) as Box<dyn FnMut() -> f64 + Send>);
    move |games: &mut Vec<PreparedGame>| {
        let mut r = r.lock().unwrap();
        for g in games.iter_mut() {
            let ap = g.enemies.iter().any(|id| AP_TEAM[1..].contains(id));
            let p = if effect && ap {
                if g.win != 0.0 {
                    0.75
                } else {
                    0.25
                }
            } else {
                0.5
            };
            let item = if r() < p { VISAGE } else { KEEP };
            g.swaps = vec![SwapChoice { held: BOOTS, item, t: 30.0 * MIN, w: 1.0, x: None, live: [f64::NAN, f64::NAN, f64::NAN, f64::NAN, f64::NAN, f64::NAN, f64::NAN, 30.0], p: None }];
        }
    }
}

fn synth() -> Vec<SynthGame> {
    ap_ad_games(800, 40, 21, ApAd { no_effect: false, random_opponent: true })
}

#[test]
fn swaps_the_advice_is_the_option_that_did_better_in_drafts_like_this_one_else_keep() {
    let data = load_test_static_data();
    let (rows, participants, totals) = model_inputs(&synth());
    let rows: Vec<Arc<_>> = rows.into_iter().map(Arc::new).collect();
    let profiles = Arc::new(ChampionProfiles::new(&totals, &[], data.clone(), PROFILE_DEFAULTS));
    let mut games = prepare_games(&rows, &participants, &data, &profiles, &PrepareOptions { starting_item_window_ms: 60_000.0 });
    with_swaps(true, 3)(&mut games);
    let games: Vec<Arc<PreparedGame>> = games.into_iter().map(Arc::new).collect();
    let model = build_draft_model(score_games(&games, None, None), None, profiles);
    let none: RegionPatchFn = Arc::new(|_| None);
    let weighted = weigh_games(&model, &data, &none, &weight_defaults());
    let o = EstimateOptions { h: 0.7, k: 100.0, ..ESTIMATE_DEFAULTS };
    let query = |enemies: &[i64]| item_query_of(&model, &DraftQuery { lane: "TOP", opponent: None, enemies: enemies.to_vec(), allies: vec![] }, o.h);
    let ap = advise_swap(&model, &weighted, &query(&AP_TEAM), BOOTS, &o, None).unwrap();
    assert_eq!(ap.pick, SwapPick::Swap { item: VISAGE as i64 });
    let (lift, low, high) = (ap.lift.unwrap(), ap.low.unwrap(), ap.high.unwrap());
    assert!(lift > 0.1 && low < lift && high > lift, "lift {lift} ({low} to {high})");
    assert_eq!(ap.minute, Some(30.0));
    let ad_enemies = games.iter().find(|g| !g.enemies.iter().any(|id| AP_TEAM[1..].contains(id))).unwrap().enemies.clone();
    let ad = advise_swap(&model, &weighted, &query(&ad_enemies), BOOTS, &o, None).unwrap();
    assert!(ad.pick == SwapPick::Keep || ad.lift.unwrap_or(0.0) < lift, "against the AD team, no such lead");
    assert!(advise_swap(&model, &weighted, &query(&AP_TEAM), GA, &o, None).is_none(), "no game held it");
}

fn engine() -> EngineSettings {
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

fn opts() -> EvalOptions {
    EvalOptions { lambdas: vec![1.0], bandwidths: vec![0.7], ks: vec![100.0], bootstrap: 300, live: false, memory: None, scoring: false, tuning: None, ..eval_defaults(engine()) }
}

struct Run {
    report: EvalReport,
    base: Option<EvalBaseData>,
}

fn run(games: &[SynthGame], effect: bool, base: Option<&EvalBaseData>, salt: Option<&str>) -> Run {
    let data = load_test_static_data();
    let (rows, participants, totals) = model_inputs(games);
    let mut o = opts();
    if let Some(b) = base {
        let m = Mark { row: Some(0.0), games: 0.0, patch: None };
        let header = BaseHeader {
            logic: 1.0,
            full_at: 0.0,
            full_mark: m.clone(),
            full_held_out: b.known.len() as f64,
            full_patch: None,
            full_current: 0.0,
            last_at: 0.0,
            last_mark: m,
            known: 0.0,
            patches: vec![],
            regions: vec![],
            key: String::new(),
            portable: None,
            imported_from: None,
        };
        o.update = Some(update_from(b, &header, o.max_grid_games));
    }
    let out = evaluate(
        EvalInput {
            rows,
            timelines_of: None,
            classic_patches: None,
            participants,
            excl: Arc::new(build_exclusivity_index(&data)),
            profiles: Arc::new(ChampionProfiles::new(&totals, &[], data.clone(), PROFILE_DEFAULTS)),
            data,
            region_patch: Arc::new(|_| None),
            rec_log: vec![],
            starting_item_window_ms: 60_000.0,
            log: Box::new(|_| {}),
            progress: Box::new(|_| {}),
            threads: 1,
            base: base.cloned(),
            salt: salt.map(String::from).or_else(|| base.map(|b| b.salt.clone())),
            control: None,
            prepared_hook: Some(Box::new(with_swaps(effect, 5))),
        },
        &o,
    )
    .unwrap();
    Run { report: out.report, base: out.next }
}

#[test]
fn swaps_evaluate_builds_passes_the_advice_when_following_it_does_better_than_keeping_and_fails_it_when_it_says_nothing() {
    let r = run(&synth(), true, None, None).report;
    let s = r.swaps.as_ref().expect("reported");
    assert_eq!(s.situations, 800.0);
    assert_eq!(s.games, 800.0);
    assert!(s.advised > 0.2, "advised to sell in {}", s.advised);
    assert_eq!(s.gate, "passed", "{}", s.gate_reason);
    assert!(format_eval(&r).contains("Late-game swaps: PASSED"));
    let none = run(&synth(), false, None, None).report;
    assert_eq!(none.swaps.as_ref().unwrap().gate, "failed", "{}", none.swaps.as_ref().unwrap().gate_reason);
}

#[test]
fn swaps_a_quick_update_adds_the_new_games_situations_to_its_bases_and_with_none_new_it_is_the_full_evaluations() {
    let all = synth();
    let full = run(&all, true, None, Some("s"));
    let again = run(&all, true, full.base.as_ref(), Some("s"));
    assert_eq!(again.report.swaps, full.report.swaps);
    let some: Vec<SynthGame> = all.iter().enumerate().filter(|(i, _)| i % 5 != 0).map(|(_, g)| g.clone()).collect();
    let first = run(&some, true, None, Some("s"));
    let quick = run(&all, true, first.base.as_ref(), Some("s"));
    let s = quick.report.swaps.unwrap();
    assert_eq!((s.situations, s.games), (800.0, 800.0), "every game's situations once");
}
