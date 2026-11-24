//! 2.2.0 (WPA phase 2): a full evaluation of synthetic games, written to a folder, so two builds' reports can be
//! compared byte for byte (the candidates off must give main's report) and their numbers side by side (on).
//!
//! Ignored by default (it takes minutes). Run with
//!   EVAL_SNAPSHOT_DIR=/some/dir EVAL_SNAPSHOT_THREADS=1 EVAL_CANDIDATES=off \
//!     cargo test -p irelia --test wpa_eval_snapshot -- --ignored --nocapture
//! Two data sets: `plain` (the tests' AP/AD games) and `rich` (the same games with a CS lead that follows the result,
//! so the game-state model has something to learn, and some first items and boots sold late, so the effective window
//! has something to change).

use irelia::capture::{TimelineCapture, SELL};
use irelia::engine::exclusivity::build_exclusivity_index;
use irelia::engine::recommend::EngineSettings;
use irelia::model::evaluate::{eval_defaults, evaluate, EvalInput, EvalOptions};
use irelia::model::profiles::{ChampionProfiles, PROFILE_DEFAULTS};
use irelia::model::run_eval::{format_eval, format_timings};
use irelia::testkit::load_test_static_data;
use irelia::testkit_model::{ap_ad_games, model_inputs, rng, ApAd};
use std::sync::{Arc, Mutex};

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

/// The candidates of 2.2.0 on or off (main has none: off).
fn candidates(o: &mut EvalOptions, on: bool) {
    o.state_sample = on;
    o.effective_window = on;
}

/// The `rich` data set: a CS lead for Irelia's team that drifts with the result, and late sales.
fn enrich(rows: &mut [irelia::db::IreliaGameRow]) {
    for (i, row) in rows.iter_mut().enumerate() {
        let mut r = rng(7919 + i as u32);
        let mut cap = TimelineCapture::parse(row.timeline.as_deref().unwrap()).unwrap();
        let sign = if row.win != 0 { 1.0 } else { -1.0 };
        let mut lead = 0.0f64;
        for k in 0..cap.t.len() {
            lead += 1.5 * sign + (r() - 0.5) * 12.0;
            for pid in 1..=5u32 {
                if let Some(fr) = cap.frames.get_mut(&pid).and_then(|f| f.get_mut(k)) {
                    fr[4] = (fr[4] + lead / 5.0).round().max(0.0);
                }
            }
        }
        let first: Option<f64> = cap.items.iter().find(|e| e.1 == 1.0 && e.2 == 0.0 && (e.3 == 3053.0 || e.3 == 3153.0)).map(|e| e.3);
        if let (Some(item), true) = (first, r() < 0.4) {
            let minute = 18.0 + (r() * 11.0).floor();
            cap.items.push((minute * 60_000.0 + 700.0, 1.0, SELL, item, None));
        }
        if r() < 0.15 {
            cap.items.push((25.0 * 60_000.0 + 800.0, 1.0, SELL, 3047.0, None));
        }
        cap.items.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap());
        row.timeline = Some(cap.to_json_string());
    }
}

#[test]
#[ignore]
fn full_evaluation_snapshot() {
    let dir = std::env::var("EVAL_SNAPSHOT_DIR").expect("EVAL_SNAPSHOT_DIR");
    let threads: usize = std::env::var("EVAL_SNAPSHOT_THREADS").ok().and_then(|t| t.parse().ok()).unwrap_or(1);
    let on = std::env::var("EVAL_CANDIDATES").map(|v| v == "on").unwrap_or(false);
    let sets: Vec<String> = std::env::var("EVAL_SNAPSHOT_SETS").unwrap_or_else(|_| "plain,rich".into()).split(',').map(String::from).collect();
    std::fs::create_dir_all(&dir).unwrap();
    for set in sets {
        let data = load_test_static_data();
        let n: usize = std::env::var("EVAL_SNAPSHOT_GAMES").ok().and_then(|t| t.parse().ok()).unwrap_or(800);
        let games = ap_ad_games(n, 40, 21, ApAd { no_effect: false, random_opponent: true });
        let (mut rows, participants, totals) = model_inputs(&games);
        if set == "rich" {
            enrich(&mut rows);
        }
        let mut o = eval_defaults(engine());
        if on {
            candidates(&mut o, true);
        }
        let lines: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(vec![]));
        let l2 = lines.clone();
        let t0 = std::time::Instant::now();
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
                log: Box::new(move |l| l2.lock().unwrap().push(l.to_string())),
                progress: Box::new(|_| {}),
                threads,
                base: None,
                salt: Some("snapshot".into()),
                control: None,
                prepared_hook: None,
            },
            &o,
        )
        .unwrap();
        let ms = t0.elapsed().as_secs_f64() * 1000.0;
        let mut report = out.report.clone();
        report.evaluated_at = 0.0;
        let tag = format!("{set}-{}-t{threads}{}", if on { "on" } else { "off" }, if n == 800 { String::new() } else { format!("-g{n}") });
        std::fs::write(format!("{dir}/{tag}.json"), serde_json::to_string_pretty(&report).unwrap()).unwrap();
        std::fs::write(format!("{dir}/{tag}.txt"), format_eval(&report)).unwrap();
        std::fs::write(format!("{dir}/{tag}.log"), lines.lock().unwrap().join("\n")).unwrap();
        std::fs::write(format!("{dir}/{tag}.time"), format!("{ms:.0} ms\n{}\n", format_timings(&out.timings))).unwrap();
        eprintln!("{tag}: {ms:.0} ms, gate {}, lift {} ({}..{})", report.gate, report.lift.delta, report.lift.low, report.lift.high);
    }
}
