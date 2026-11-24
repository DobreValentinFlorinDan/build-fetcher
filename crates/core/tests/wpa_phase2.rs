//! 2.2.0, WPA phase 2: the state model's 5-minute sample and the effective window of an item's ΔW (model/window.rs),
//! and the evaluation that decides between each and today's method.

use irelia::capture::{TimelineCapture, BUY, DESTROY, SELL, UNDO};
use irelia::engine::exclusivity::build_exclusivity_index;
use irelia::engine::recommend::EngineSettings;
use irelia::model::dataset::{prepare_games, score_games, score_games_with, state_rows, state_rows_by, PrepareOptions, PreparedGame, StateRows};
use irelia::model::evaluate::{eval_defaults, evaluate, EvalInput, EvalOptions, EvalReport};
use irelia::model::profiles::{ChampionProfiles, PROFILE_DEFAULTS};
use irelia::model::state::{train_state_model, win_probability_of, N_FEATURES};
use irelia::model::window::{ends_of, frame_at_or_before, interval_frames, purchase_lives, Life, StateSample, WindowEnd, SAMPLE_SEED};
use irelia::testkit::load_test_static_data;
use irelia::testkit_model::{ap_ad_games, blank_game, model_inputs, ApAd};
use std::sync::Arc;

const MIN: f64 = 60_000.0;

// ------------------------------------------------------------------------------------------ the 5-minute sample

/// A game with `n` frames, one a minute from 1:00, each row's first feature its frame number.
fn framed(id: &str, n: usize, win: f64) -> PreparedGame {
    let mut fr = Vec::with_capacity(n * N_FEATURES);
    for k in 0..n {
        let mut row = [0f32; N_FEATURES];
        row[0] = 1.0;
        row[1] = k as f32;
        fr.extend_from_slice(&row);
    }
    PreparedGame {
        match_id: id.into(),
        pilot: format!("pilot-{id}"),
        source: "otp".into(),
        win,
        frames: Some(fr),
        frame_t: Some((0..n).map(|k| (k + 1) as f64 * MIN + 37.0).collect()),
        ..blank_game()
    }
}

fn games(n: usize, frames: usize) -> Vec<PreparedGame> {
    (0..n).map(|i| framed(&format!("KR_{i}"), frames, (i % 2) as f64)).collect()
}

/// The frame numbers a sample took from each game.
fn picked(r: &StateRows, per_game: &[usize]) -> Vec<Vec<usize>> {
    let mut out = Vec::new();
    let mut at = 0;
    for n in per_game {
        out.push(r.x[at..at + n].iter().map(|x| x[1] as usize).collect());
        at += n;
    }
    out
}

#[test]
fn the_sample_takes_one_frame_from_each_5_minute_interval() {
    // Frames at 1:00 … 30:00: intervals 0-5 (1:00-4:00), 5-10, …, 25-30, and 30:00 alone: 7.
    let gs = games(30, 30);
    let r = state_rows_by(gs.iter(), 60_000, StateSample::Interval).unwrap();
    assert_eq!(r.x.len(), 30 * 7);
    assert_eq!(r.games, 30);
    for (g, frames) in picked(&r, &[7; 30]).iter().enumerate() {
        let t: Vec<f64> = frames.iter().map(|k| (k + 1) as f64 * MIN + 37.0).collect();
        let bins: Vec<i64> = t.iter().map(|t| (t / (5.0 * MIN)).floor() as i64).collect();
        assert_eq!(bins, vec![0, 1, 2, 3, 4, 5, 6], "game {g}: one frame per interval, in order");
        assert!(r.win[g * 7..(g + 1) * 7].iter().all(|w| *w == (g % 2) as f64), "each row carries its game's result");
    }
    // Uniform within an interval: across games, every frame of the 5:00-9:00 interval is drawn sometimes.
    let mid: std::collections::HashSet<usize> = picked(&r, &[7; 30]).iter().map(|f| f[1]).collect();
    assert_eq!(mid.len(), 5, "{mid:?}");
}

#[test]
fn the_sample_is_the_same_every_time_and_whatever_the_order_of_the_games() {
    let gs = games(25, 34);
    let a = state_rows_by(gs.iter(), 60_000, StateSample::Interval).unwrap();
    let b = state_rows_by(gs.iter(), 60_000, StateSample::Interval).unwrap();
    assert_eq!(a.x, b.x);
    let rev: Vec<PreparedGame> = gs.iter().rev().cloned().collect();
    let c = state_rows_by(rev.iter(), 60_000, StateSample::Interval).unwrap();
    let mut fwd = picked(&a, &[7; 25]);
    fwd.reverse();
    assert_eq!(picked(&c, &[7; 25]), fwd, "a game's frames depend on the game alone");
    // The seed is the game's: the frames drawn from one match are interval_frames's.
    let t = gs[3].frame_t.clone().unwrap();
    assert_eq!(picked(&a, &[7; 25])[3], interval_frames(&t, "KR_3", SAMPLE_SEED));
}

#[test]
fn the_sample_respects_the_cap() {
    let gs = games(40, 30);
    let all = state_rows_by(gs.iter(), 60_000, StateSample::Interval).unwrap();
    assert_eq!(all.x.len(), 280);
    let capped = state_rows_by(gs.iter(), 100, StateSample::Interval).unwrap();
    assert!(capped.x.len() <= 100, "{} rows", capped.x.len());
    assert_eq!(capped.x.len(), 94, "an even stride (3) through the sampled rows");
    assert_eq!(capped.x[1], all.x[3]);
    // Too few games or frames: no rows, as today.
    assert!(state_rows_by(games(19, 30).iter(), 60_000, StateSample::Interval).is_none());
    // Today's sample is unchanged.
    let even = state_rows_by(gs.iter(), 100, StateSample::Even).unwrap();
    let today = state_rows(gs.iter(), 100).unwrap();
    assert_eq!((even.x, even.win, even.games), (today.x, today.win, today.games));
}

// ------------------------------------------------------------------------------------------ the effective window

fn cap(events: &[(f64, f64, f64, f64, Option<f64>)]) -> TimelineCapture {
    TimelineCapture { items: events.to_vec(), ..Default::default() }
}

#[test]
fn a_part_ends_when_it_is_upgraded_and_a_sold_item_when_it_is_sold() {
    // Long Sword (1036) at 3:00, consumed into Black Cleaver (3071) at 12:00; BotRK (3153) sold at 31:00; Steraks (3053)
    // kept; someone else's events ignored.
    let c = cap(&[
        (3.0 * MIN, 1.0, BUY, 1036.0, None),
        (5.0 * MIN, 6.0, BUY, 1036.0, None),
        (9.0 * MIN, 1.0, BUY, 3153.0, None),
        (12.0 * MIN, 1.0, DESTROY, 1036.0, None),
        (12.0 * MIN, 1.0, BUY, 3071.0, None),
        (20.0 * MIN, 1.0, BUY, 3053.0, None),
        (31.0 * MIN, 1.0, SELL, 3153.0, None),
        (31.0 * MIN + 10.0, 6.0, SELL, 1036.0, None),
    ]);
    let lives = purchase_lives(&c, 1.0);
    assert_eq!(
        lives,
        vec![
            Life { item: 1036.0, bought: 3.0 * MIN, end: Some(12.0 * MIN) },
            Life { item: 3153.0, bought: 9.0 * MIN, end: Some(31.0 * MIN) },
            Life { item: 3071.0, bought: 12.0 * MIN, end: None },
            Life { item: 3053.0, bought: 20.0 * MIN, end: None },
        ]
    );
    assert_eq!(ends_of(&lives, &[(3153.0, 9.0 * MIN), (3053.0, 20.0 * MIN), (3071.0, 12.0 * MIN)]), vec![Some(31.0 * MIN), None, None]);
    assert_eq!(ends_of(&lives, &[(9999.0, 9.0 * MIN)]), vec![None], "an item it never bought then: kept");
}

#[test]
fn an_undo_takes_a_purchase_back_and_gives_back_what_it_consumed_or_sold() {
    let c = cap(&[
        (3.0 * MIN, 1.0, BUY, 1036.0, None),
        // Black Cleaver bought (the Long Sword consumed), then undone: the sword is kept after all.
        (12.0 * MIN, 1.0, DESTROY, 1036.0, None),
        (12.0 * MIN + 1.0, 1.0, BUY, 3071.0, None),
        (12.0 * MIN + 5000.0, 1.0, UNDO, 3071.0, Some(0.0)),
        // BotRK sold, the sale undone: kept.
        (14.0 * MIN, 1.0, BUY, 3153.0, None),
        (26.0 * MIN, 1.0, SELL, 3153.0, None),
        (26.0 * MIN + 3000.0, 1.0, UNDO, 0.0, Some(3153.0)),
        // A Dagger bought and undone at once: not a purchase.
        (27.0 * MIN, 1.0, BUY, 1042.0, None),
        (27.0 * MIN + 2000.0, 1.0, UNDO, 1042.0, Some(0.0)),
    ]);
    assert_eq!(
        purchase_lives(&c, 1.0),
        vec![Life { item: 1036.0, bought: 3.0 * MIN, end: None }, Life { item: 3153.0, bought: 14.0 * MIN, end: None }]
    );
}

#[test]
fn the_window_ends_at_the_frame_at_or_just_before_and_an_item_bought_after_the_last_frame_counts() {
    let t: Vec<f64> = (1..=30).map(|m| m as f64 * MIN + 40.0).collect();
    assert_eq!(frame_at_or_before(&t, 12.0 * MIN), Some(10), "11:00 is the frame just before 12:00");
    assert_eq!(frame_at_or_before(&t, 12.0 * MIN + 40.0), Some(11), "at it");
    assert_eq!(frame_at_or_before(&t, 30.0 * MIN), Some(28));
    assert_eq!(frame_at_or_before(&t, 40.0 * MIN), Some(29), "after the last frame: the last");
    assert_eq!(frame_at_or_before(&t, 30_000.0), None, "before the first frame");
    // Items bought after the last frame (30:00): one kept to the end (today's ΔW), one sold at 31:30 (the last frame).
    let c = cap(&[(30.0 * MIN + 50_000.0, 1.0, BUY, 3053.0, None), (30.0 * MIN + 55_000.0, 1.0, BUY, 3153.0, None), (31.5 * MIN, 1.0, SELL, 3153.0, None)]);
    let lives = purchase_lives(&c, 1.0);
    let ends = ends_of(&lives, &[(3053.0, 30.0 * MIN + 50_000.0), (3153.0, 30.0 * MIN + 55_000.0)]);
    assert_eq!(ends, vec![None, Some(31.5 * MIN)]);
    assert_eq!(frame_at_or_before(&t, ends[1].unwrap()), Some(29), "the frame at or just before the sale: the last one");
}

/// A prepared game with two items: one kept, one left at `end_frame` (its features `x_end`).
fn windowed(win: f64, x_before: [f64; N_FEATURES], x_end: Option<[f64; N_FEATURES]>) -> PreparedGame {
    let mut g = blank_game();
    g.win = win;
    let choice = |item: f64, t: f64| irelia::model::dataset::ItemChoice { item, t, x: Some(Box::new(x_before)), live: [0.0; 8] };
    g.items = vec![choice(3153.0, 9.0 * MIN), choice(3053.0, 20.0 * MIN)];
    g.item_ends = vec![Some(WindowEnd { t: 31.0 * MIN, frame: x_end.map(|_| 29), x: x_end.map(Box::new) }), None];
    g.boots = Some(choice(3047.0, 6.0 * MIN));
    g.boots_end = Some(WindowEnd { t: 25.0 * MIN, frame: Some(23), x: x_end.map(Box::new) });
    g
}

#[test]
fn the_delta_of_an_item_is_the_chance_when_it_left_minus_before_and_kept_items_keep_todays() {
    let rows: Vec<Vec<f64>> = (0..400).map(|i| {
        let mut r = vec![0.0; N_FEATURES];
        r[0] = 1.0;
        r[2] = (i % 7) as f64 - 3.0;
        r
    }).collect();
    let wins: Vec<f64> = (0..400).map(|i| if (i % 7) >= 3 { 1.0 } else { 0.0 }).collect();
    let refs: Vec<&[f64]> = rows.iter().map(|r| r.as_slice()).collect();
    let m = train_state_model(&refs, &wins, 40.0, 1.0).unwrap();
    let mut before = [0.0; N_FEATURES];
    before[0] = 1.0;
    let mut after = before;
    after[2] = 2.0;
    let p0 = win_probability_of(&m.beta, &before);
    let p1 = win_probability_of(&m.beta, &after);
    let g = Arc::new(windowed(0.0, before, Some(after)));
    let today = &score_games(&[g.clone()], Some(&m), None)[0];
    let window = &score_games_with(&[g.clone()], Some(&m), None, true)[0];
    assert_eq!(today.item_wpa, vec![0.0 - p0, 0.0 - p0], "today: the result minus the chance before");
    assert_eq!(window.item_wpa, vec![p1 - p0, 0.0 - p0], "sold: the chance then minus before; kept: today's");
    assert_eq!(window.boots_wpa, Some(p1 - p0));
    assert_eq!(score_games_with(&[g.clone()], Some(&m), None, false)[0].item_wpa, today.item_wpa);
    // Left before the first frame: nothing measured, no change.
    let early = Arc::new(windowed(1.0, before, None));
    assert_eq!(score_games_with(&[early], Some(&m), None, true)[0].item_wpa, vec![0.0, 1.0 - p0]);
}

#[test]
fn prepared_games_carry_when_their_items_left_the_inventory() {
    let data = load_test_static_data();
    let gs = ap_ad_games(2, 2, 3, ApAd::default());
    let (mut rows, participants, totals) = model_inputs(&gs);
    // The first game sells its first item at 24:00 (the timeline's frames run to 30:00); the second keeps it.
    let first = gs[0].items[0] as f64;
    let mut c = TimelineCapture::parse(rows[0].timeline.as_deref().unwrap()).unwrap();
    c.items.push((24.0 * MIN + 700.0, 1.0, SELL, first, None));
    c.items.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap());
    rows[0].timeline = Some(c.to_json_string());
    let rows: Vec<Arc<_>> = rows.into_iter().map(Arc::new).collect();
    let profiles = ChampionProfiles::new(&totals, &[], data.clone(), PROFILE_DEFAULTS);
    let p = prepare_games(&rows, &participants, &data, &profiles, &PrepareOptions { starting_item_window_ms: 60_000.0 });
    let end = p[0].item_ends[0].as_ref().expect("sold");
    assert_eq!(end.t, 24.0 * MIN + 700.0);
    let ft = p[0].frame_t.as_ref().unwrap();
    assert_eq!(ft[end.frame.unwrap()], 24.0 * MIN, "the frame at or just before the sale");
    let fr = p[0].frames.as_ref().unwrap();
    let k = end.frame.unwrap();
    let x: Vec<f64> = fr[k * N_FEATURES..(k + 1) * N_FEATURES].iter().map(|v| *v as f64).collect();
    assert!(x.iter().zip(end.x.as_ref().unwrap().iter()).all(|(a, b)| (*a - *b).abs() <= 1e-6 * b.abs().max(1.0)), "that frame's features");
    assert!(p[0].item_ends[1..].iter().all(|e| e.is_none()) && p[0].boots_end.is_none(), "the rest kept to the end");
    assert!(p[1].item_ends.iter().all(|e| e.is_none()));
    // Every purchase has its life: the boots and the items kept, the sold one with its end.
    assert_eq!(p[0].purchase_ends.len(), p[0].purchases.len());
    let sold: Vec<usize> = (0..p[0].purchases.len()).filter(|i| p[0].purchase_ends[*i].is_some()).collect();
    assert_eq!(sold.len(), 1);
    assert_eq!(p[0].purchases[sold[0]].0 as f64, first);
}

// ------------------------------------------------------------------------------------------ the evaluation

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

/// The tests' AP/AD games, a third of them selling their first item at 20:00-27:00.
fn run(o: &EvalOptions, threads: usize) -> (EvalReport, Vec<String>) {
    let data = load_test_static_data();
    let gs = ap_ad_games(400, 20, 21, ApAd { no_effect: false, random_opponent: true });
    let (mut rows, participants, totals) = model_inputs(&gs);
    for (i, row) in rows.iter_mut().enumerate() {
        if i % 3 == 0 {
            let mut c = TimelineCapture::parse(row.timeline.as_deref().unwrap()).unwrap();
            c.items.push(((20 + i % 8) as f64 * MIN + 700.0, 1.0, SELL, gs[i].items[0] as f64, None));
            c.items.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap());
            row.timeline = Some(c.to_json_string());
        }
    }
    let lines = Arc::new(std::sync::Mutex::new(Vec::new()));
    let l2 = lines.clone();
    let mut r = evaluate(
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
            log: Box::new(move |l| l2.lock().unwrap().push(l.to_string())),
            progress: Box::new(|_| {}),
            threads,
            base: None,
            salt: Some("x".into()),
            control: None,
            prepared_hook: None,
        },
        o,
    )
    .unwrap()
    .report;
    r.evaluated_at = 0.0;
    let l = lines.lock().unwrap().clone();
    (r, l)
}

fn small() -> EvalOptions {
    EvalOptions { lambdas: vec![0.3, 3.0], bandwidths: vec![0.7], ks: vec![100.0], bootstrap: 200, live: false, memory: None, scoring: false, tuning: None, min_games: 100, ..eval_defaults(engine()) }
}

#[test]
fn the_evaluation_tests_both_candidates_logs_both_and_is_the_same_on_one_and_three_threads() {
    let on = EvalOptions { state_sample: true, effective_window: true, ..small() };
    let (one, log1) = run(&on, 1);
    let (three, log3) = run(&on, 3);
    assert_eq!(serde_json::to_value(&one).unwrap(), serde_json::to_value(&three).unwrap());
    assert_eq!(log1, log3);
    let s = one.state.as_ref().unwrap().sample.as_ref().expect("both samples measured");
    assert!(s.even.log_loss.is_finite() && s.interval.log_loss.is_finite() && s.interval.ece.is_finite());
    let kept = if s.interval.log_loss < s.even.log_loss { "5min" } else { "even" };
    assert_eq!(s.kept, kept, "the better by held-out log loss");
    assert!(log1.iter().any(|l| l.contains("one frame per 5 minutes") && l.contains("kept")), "{log1:?}");
    let w = one.window.as_ref().expect("both windows measured");
    assert!(w.items > 0.0, "sold first items");
    let better = w.window.delta > w.today.delta && w.window.low > w.today.low;
    assert_eq!(w.kept, if better { "window" } else { "today" });
    assert_eq!(w.gate, if better { "passed" } else { "failed" });
    if better {
        assert_eq!((one.lift.delta, one.lift.low), (w.window.delta, w.window.low), "the report's lift is the kept one's");
    } else {
        assert_eq!((one.lift.delta, one.lift.low), (w.today.delta, w.today.low));
    }
    assert!(log1.iter().any(|l| l.starts_with("effective window:") && l.contains("kept")), "{log1:?}");
}

#[test]
fn with_both_candidates_off_the_report_is_todays() {
    let (off, log_off) = run(&small(), 1);
    assert!(off.window.is_none() && off.state.as_ref().unwrap().sample.is_none());
    // Phase 2 adds nothing to the stored report when off (Phase 1's reliability tables, which always come, have their
    // own "window" names: the report's fields are checked, not its text).
    let json = serde_json::to_value(&off).unwrap();
    assert!(json.get("window").is_none() && json["state"].get("sample").is_none(), "nothing new in the stored report");
    assert!(!log_off.iter().any(|l| l.contains("5 minutes") || l.contains("effective window")));
}

// ------------------------------------------------------------------------------------------ the best buy

const TRINITY: i64 = 3078;
const SHEEN: i64 = 3057;
const PHAGE: i64 = 3044;

/// A game buying `first` at 3:00 (consumed into Trinity Force at 11:30); the chance is 0.4 until 10:00 and `then` from
/// 11:00, the frame just before the upgrade.
fn path_game(n: usize, first: i64, win: f64, then: f64) -> irelia::model::dataset::ScoredGame {
    let mut g = blank_game();
    g.match_id = format!("KR_{n}");
    g.win = win;
    g.opponent = Some(122);
    g.items = vec![irelia::model::dataset::ItemChoice { item: TRINITY as f64, t: 11.5 * MIN, x: None, live: [0.0; 8] }];
    g.purchases = vec![(first, 3.0 * MIN), (TRINITY, 11.5 * MIN)];
    g.purchase_ends = vec![Some(WindowEnd { t: 11.5 * MIN, frame: Some(10), x: None }), None];
    g.p_at = Some((0..30).map(|k| if k < 10 { 0.4 } else { then }).collect());
    irelia::model::dataset::ScoredGame { g: Arc::new(g), wpa0: 0.0, item_wpa: vec![0.0], boots_wpa: None, swap_wpa: vec![], item_w0: vec![0.5], boots_w0: None }
}

#[test]
fn the_best_buy_reads_the_kept_delta_a_part_counting_until_its_upgrade() {
    let data = load_test_static_data();
    // Sheen-first games lost but were ahead when Sheen went into Trinity Force; Phage-first games won from behind.
    let mut games: Vec<_> = (0..40).map(|i| path_game(i, SHEEN, 0.0, 0.7)).collect();
    games.extend((40..75).map(|i| path_game(i, PHAGE, 1.0, 0.1)));
    let state = irelia::model::state::StateModel { version: 1, features: vec![], beta: vec![0.0; N_FEATURES], lambda: 1.0, rows: 0.0, games: 0.0, base: 0.5 };
    let games = Arc::new(games);
    let q = irelia::model::buy::BuyQuery { lane: "TOP", opponent: None, target: TRINITY, owned: &[], gold: 1150.0 };
    let first = |b: Option<serde_json::Value>| b.unwrap()["items"][0].as_i64().unwrap();
    let today = irelia::model::buy::BuyIndex::new(games.clone(), Some(state.clone()), data.clone());
    assert_eq!(first(today.best(&q)), PHAGE, "today: the result minus the chance before (Phage +0.6, Sheen −0.4)");
    let window = irelia::model::buy::BuyIndex::new(games, Some(state), data).with_window(true);
    let b = window.best(&q).unwrap();
    assert_eq!(b["items"][0].as_i64().unwrap(), SHEEN, "the window: the chance at the upgrade minus before (Sheen +0.3, Phage −0.3)");
    assert!((b["lift"].as_f64().unwrap() - 0.3).abs() < 1e-12, "{}", b["lift"]);
}
