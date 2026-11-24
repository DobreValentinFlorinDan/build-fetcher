//! Port of server/model/evalTeam.test.ts: Evaluate builds on every core. The same report to the last digit on any
//! number of threads, while the app is busy (champ select or a game moves the rest onto one thread), and with late-game
//! swaps; the memory reserve the thread plan leaves free.
//!
//! Not ported: 0.22.0's worker-thread and process team internals (shared heap accounting, chunk tables, a worker or a
//! process killed mid-run, IRELIA_EVAL_PROCESSES). 1.x runs the evaluation on a rayon pool in one process: the threads
//! share the games instead of copying them, and a thread can't die on its own (a panic stops the evaluation, which the
//! job reports). The thread plan's numbers for this computer are in crates/server/src/system.rs.

use irelia::engine::exclusivity::build_exclusivity_index;
use irelia::engine::recommend::EngineSettings;
use irelia::model::dataset::{PreparedGame, SwapChoice};
use irelia::model::evaluate::{eval_defaults, evaluate, EvalControl, EvalInput, EvalOptions, EvalReport};
use irelia::model::profiles::{ChampionProfiles, PROFILE_DEFAULTS};
use irelia::sys::memory_reserve;
use irelia::testkit::load_test_static_data;
use irelia::testkit_model::{ap_ad_games, model_inputs, rng, ApAd, SynthGame};
use serde_json::Value;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex, OnceLock};

const GB: f64 = 1024.0 * 1024.0 * 1024.0;

#[test]
fn team_a_reserve_of_memory_left_free_15_percent_at_least_1_5_gb() {
    let total = 16.0 * GB;
    assert_eq!(memory_reserve(total), 0.15 * total);
    assert_eq!(memory_reserve(4.0 * GB), 1.5 * GB, "small computers keep at least 1.5 GB");
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

/// Every comparison the app's evaluation runs, on a small grid: the draft model, patch memory, runes against the
/// draft, the replay.
fn opts() -> EvalOptions {
    EvalOptions { lambdas: vec![0.3, 1.0], bandwidths: vec![0.7, f64::INFINITY], ks: vec![100.0], bootstrap: 100, live: true, max_live_games: 80, min_games: 100, ..eval_defaults(engine()) }
}

fn pool() -> Vec<SynthGame> {
    let ab = ApAd { no_effect: false, random_opponent: true };
    let mut games: Vec<SynthGame> = ap_ad_games(500, 40, 41, ab)
        .into_iter()
        .enumerate()
        .map(|(i, mut g)| {
            g.version = Some(if i % 2 == 1 { "16.18.1.1" } else { "16.19.1.1" }.into());
            g
        })
        .collect();
    games.extend(ap_ad_games(400, 40, 42, ab).into_iter().enumerate().map(|(i, mut g)| {
        g.id = format!("KR_{}", 50_000 + i);
        g.version = Some(format!("16.{}.1.1", 17 - (i % 2)));
        g
    }));
    games
}

struct Run<'a> {
    games: Vec<SynthGame>,
    classic: Option<Vec<String>>,
    opts: EvalOptions,
    threads: usize,
    control: Option<Arc<EvalControl>>,
    progress: Box<dyn Fn(usize) + Sync + 'a>,
    hook: Option<Box<dyn Fn(&mut Vec<PreparedGame>) + 'a>>,
}

fn run(r: Run) -> EvalReport {
    let data = load_test_static_data();
    let (rows, participants, totals) = model_inputs(&r.games);
    let progress = r.progress;
    evaluate(
        EvalInput {
            rows,
            timelines_of: None,
            classic_patches: r.classic,
            participants,
            excl: Arc::new(build_exclusivity_index(&data)),
            profiles: Arc::new(ChampionProfiles::new(&totals, &[], data.clone(), PROFILE_DEFAULTS)),
            data,
            region_patch: Arc::new(|_| None),
            rec_log: vec![],
            starting_item_window_ms: 60_000.0,
            log: Box::new(|_| {}),
            progress: Box::new(move |p| progress(p.step)),
            threads: r.threads,
            base: None,
            salt: None,
            control: r.control,
            prepared_hook: r.hook,
        },
        &r.opts,
    )
    .unwrap()
    .report
}

fn full(threads: usize, control: Option<Arc<EvalControl>>, progress: Box<dyn Fn(usize) + Sync>) -> EvalReport {
    run(Run { games: pool(), classic: Some(vec!["16.19".into(), "16.18".into()]), opts: opts(), threads, control, progress, hook: None })
}

fn comparable(r: &EvalReport) -> Value {
    let mut v = serde_json::to_value(r).unwrap();
    v.as_object_mut().unwrap().remove("evaluatedAt");
    v
}

fn alone() -> &'static Value {
    static ALONE: OnceLock<Value> = OnceLock::new();
    ALONE.get_or_init(|| {
        let r = full(1, None, Box::new(|_| {}));
        assert!(r.memory.is_some() && r.scoring.is_some() && r.stability.is_some(), "every comparison ran");
        comparable(&r)
    })
}

#[test]
fn team_three_threads_report_what_one_does_to_the_last_digit() {
    let shared = full(3, None, Box::new(|_| {}));
    assert_eq!(&comparable(&shared), alone());
}

#[test]
fn team_champ_select_or_a_game_moves_the_rest_onto_one_thread_and_the_report_is_the_same() {
    let control = Arc::new(EvalControl::default());
    let c = control.clone();
    let seen = Arc::new(Mutex::new(Vec::<bool>::new()));
    let s = seen.clone();
    // Champ select begins at the third step and ends at the sixth.
    let shared = full(
        3,
        Some(control),
        Box::new(move |step| {
            if step == 3 {
                c.busy.store(true, Ordering::SeqCst);
            }
            if step == 6 {
                c.busy.store(false, Ordering::SeqCst);
            }
            s.lock().unwrap().push(c.busy.load(Ordering::SeqCst));
        }),
    );
    let seen = seen.lock().unwrap();
    assert!(seen.iter().any(|b| *b) && !seen.last().unwrap(), "busy for a while, then not");
    assert_eq!(&comparable(&shared), alone());
}

#[test]
fn team_late_game_swaps_on_three_threads_report_what_one_does() {
    // Every game decides about its boots late: three threads must go through the swaps' interval together.
    let inject = || {
        |games: &mut Vec<PreparedGame>| {
            let mut r = rng(9);
            for g in games.iter_mut() {
                let item = if r() < (if g.win != 0.0 { 0.7 } else { 0.3 }) { 3065.0 } else { 0.0 };
                g.swaps = vec![SwapChoice { held: 3047.0, item, t: 30.0 * 60_000.0, w: 1.0, x: None, live: [f64::NAN, f64::NAN, f64::NAN, f64::NAN, f64::NAN, f64::NAN, f64::NAN, 30.0], p: None }];
            }
        }
    };
    let o = EvalOptions { live: false, memory: None, scoring: false, tuning: None, ..opts() };
    let games = || ap_ad_games(500, 40, 41, ApAd { no_effect: false, random_opponent: true });
    let one = run(Run { games: games(), classic: None, opts: o.clone(), threads: 1, control: None, progress: Box::new(|_| {}), hook: Some(Box::new(inject())) });
    let s = one.swaps.as_ref().expect("the swaps reached their interval");
    assert!(s.games >= o.min_games as f64);
    let three = run(Run { games: games(), classic: None, opts: o, threads: 3, control: None, progress: Box::new(|_| {}), hook: Some(Box::new(inject())) });
    assert_eq!(three.swaps, one.swaps);
}
