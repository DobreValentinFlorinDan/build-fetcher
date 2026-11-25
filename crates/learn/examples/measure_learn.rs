//! Measures the learner on synthetic data (docs/MEASUREMENTS.md, "Learning"):
//!
//!   cargo run --release -p bf-learn --example measure_learn -- [matches] [validation] [records per step] [threads]
//!
//! Prints held-out metrics of the champion-role baseline, step 1 and step 1+2, each update's time and memory, and
//! recommendation latency, and whether the planted effects were recovered.

use bf_core::build::*;
use bf_learn::learner::{baseline_predictions, evaluate};
use bf_learn::metrics::fmt_metrics;
use bf_learn::synthetic::*;
use bf_learn::*;
use std::sync::Arc;
use std::time::Instant;

fn main() -> anyhow::Result<()> {
    let args: Vec<usize> = std::env::args().skip(1).filter_map(|a| a.parse().ok()).collect();
    let matches = args.first().copied().unwrap_or(20_000);
    let validation = args.get(1).copied().unwrap_or(2_000);
    let per_step = args.get(2).copied().unwrap_or(2_000);
    if let Some(t) = args.get(3) {
        rayon::ThreadPoolBuilder::new().num_threads(*t).build_global()?;
    }
    let data = bf_core::testkit::load_test_static_data();
    let t = Instant::now();
    let src = Arc::new(generate(&data, &SyntheticConfig { matches, validation, ..Default::default() }));
    println!("generated {matches} training + {validation} validation matches in {:.1} s", t.elapsed().as_secs_f64());
    let dir = std::env::temp_dir().join(format!("bf-learn-measure-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let mut cfg = LearnConfig::default();
    if let Ok(x) = std::env::var("BF_CAL_SLACK") {
        cfg.guardrail.calibration_slack = x.parse()?;
    }
    let env = |k: &str| std::env::var(k).ok().and_then(|v| v.parse::<f64>().ok());
    if let Some(x) = env("BF_L2G") { cfg.logistic.l2_global = x; }
    if let Some(x) = env("BF_L2P") { cfg.logistic.l2_pair = x; }
    if let Some(x) = env("BF_LR") { cfg.logistic.learning_rate = x; }
    if let Some(x) = env("BF_EPOCHS") { cfg.logistic.epochs = x as usize; }
    if let Some(x) = env("BF_COLD") { cfg.logistic.cold_epochs = x as usize; }
    if let Some(x) = env("BF_NLR") { cfg.net.learning_rate = x; }
    if let Some(x) = env("BF_NL2") { cfg.net.l2 = x; }
    if let Some(x) = env("BF_NEPOCHS") { cfg.net.epochs = x as usize; }
    if std::env::var("BF_NO_NET").is_ok() {
        cfg.train_net = false;
    }
    let learner = Arc::new(Learner::open(&dir, data.clone(), cfg)?);
    let val = learner.samples(&src.validation);
    println!("validation examples: {}", val.len());
    let base = evaluate(&baseline_predictions(src.as_ref(), &val)?, &val);
    println!("baseline (champion-role win rate): {}", fmt_metrics(&base));
    println!("memory before training (synthetic data loaded): rss {:.0} MB, peak {:.0} MB", bf_core::sys::own_rss().unwrap_or(0) as f64 / 1_048_576.0, bf_learn::learner::peak_rss().unwrap_or(0) as f64 / 1_048_576.0);
    println!("threads: {}", rayon::current_num_threads());
    let mut step = 0;
    loop {
        let r = learner.train_step(src.as_ref(), &Budget::records(per_step))?;
        if r.outcome == Outcome::NoData {
            break;
        }
        step += 1;
        println!(
            "update {step}: {:?}, {} records, {} examples (+{} replayed), {} batches, {:.0} ms (train {:.0}, eval {:.0}), rss {:.0} MB, peak {:.0} MB",
            r.outcome,
            r.records,
            r.samples,
            r.replayed,
            r.batches,
            r.elapsed_ms,
            r.train_ms,
            r.eval_ms,
            r.rss_bytes.unwrap_or(0) as f64 / 1_048_576.0,
            r.peak_rss_bytes.unwrap_or(0) as f64 / 1_048_576.0
        );
        for s in &r.steps {
            println!("   {} {}: {}", s.kind, if s.accepted { "accepted" } else { "rejected" }, s.reason.clone().unwrap_or_else(|| fmt_metrics(&s.after)));
        }
    }
    let served = learner.served().expect("a model");
    let p1: Vec<f64> = val.iter().map(|s| served.logistic.predict(&s.situation)).collect();
    let p2: Vec<f64> = val.iter().map(|s| served.predict(&s.situation)).collect();
    println!("served {}", served.version());
    println!("step 1 (logistic):      {}", fmt_metrics(&evaluate(&p1, &val)));
    println!("step 1+2 (shared net):  {}", fmt_metrics(&evaluate(&p2, &val)));
    if std::env::var("BF_RELIABILITY").is_ok() {
        for w in 0..4 {
            let mut bins = [(0.0, 0.0, 0.0); 10];
            for (s, p) in val.iter().zip(&p1) {
                if s.window() == w {
                    let b = ((p * 10.0) as usize).min(9);
                    bins[b].0 += 1.0;
                    bins[b].1 += p;
                    bins[b].2 += s.label();
                }
            }
            println!("  window {w}: {}", bins.iter().filter(|b| b.0 > 0.0).map(|b| format!("{:.2}->{:.2} ({})", b.1 / b.0, b.2 / b.0, b.0)).collect::<Vec<_>>().join("  "));
        }
    }
    println!("checkpoints on disk: {:.1} MB", learner.disk_bytes() as f64 / 1_048_576.0);

    let rec = Recommender::new(src.clone(), Some(learner.clone()), data.clone(), RecommendConfig::default());
    let draft = DraftContext {
        champion: IRELIA,
        role: Some(Role::Top),
        allies: vec![(64, Some(Role::Jungle)), (103, Some(Role::Middle)), (222, Some(Role::Bottom)), (412, Some(Role::Utility))],
        enemies: vec![(DARIUS, Some(Role::Top)), (60, Some(Role::Jungle)), (134, Some(Role::Middle)), (81, Some(Role::Bottom)), (117, Some(Role::Utility))],
        patch: None,
    };
    let t = Instant::now();
    let n = 20;
    let mut b = rec.recommend(&draft)?;
    for _ in 1..n {
        b = rec.recommend(&draft)?;
    }
    println!("recommend (tier {:?}): {:.1} ms each", b.tier, t.elapsed().as_secs_f64() * 1e3 / n as f64);
    for s in &b.sections {
        let top: Vec<String> = s.options.iter().take(3).map(|o| format!("{} (wpa {:+.3}, {} games, {:?}{})", o.label, o.wpa, o.games, o.basis, if o.thin { ", thin" } else { "" })).collect();
        println!("   {:?}: {}", s.kind, top.join(" | "));
    }
    for n in &b.notes {
        println!("   note: {n}");
    }
    let tier_b = Recommender::new(src.clone(), None, data.clone(), RecommendConfig::default()).recommend(&draft)?;
    println!("same draft from counts only (tier B):");
    for s in &tier_b.sections {
        let top: Vec<String> = s.options.iter().take(3).map(|o| format!("{} (wpa {:+.3}, {} games, {:?})", o.label, o.wpa, o.games, o.basis)).collect();
        println!("   {:?}: {}", s.kind, top.join(" | "));
    }
    let magic = DraftContext { enemies: vec![(82, Some(Role::Top)), (60, Some(Role::Jungle)), (134, Some(Role::Middle)), (81, Some(Role::Bottom)), (117, Some(Role::Utility))], ..draft.clone() };
    let bm = rec.recommend(&magic)?;
    println!("against a magic team ({:?}): late items {:?}", bm.enemy_profile, bm.sections.iter().find(|s| s.kind == OptionKind::LateItem).map(|s| s.options.iter().map(|o| format!("{} {:+.3}", o.label, o.wpa)).collect::<Vec<_>>()));
    let thin = DraftContext { champion: IVERN, role: Some(Role::Jungle), ..draft.clone() };
    let t = Instant::now();
    let bt = rec.recommend(&thin)?;
    println!("recommend Ivern (tier {:?}, {} games): {:.1} ms; notes: {:?}", bt.tier, bt.games, t.elapsed().as_secs_f64() * 1e3, bt.notes);
    let live = LiveContext { draft: draft.clone(), game_time_s: 1500.0, my_items: vec![BOTRK, 3053, 3047], enemy_items: vec![(134, vec![6655, 3089]), (117, vec![6617])], my_level: 13, my_cs: 180, opponent_level: Some(12), opponent_cs: Some(170), ..Default::default() };
    let t = Instant::now();
    let a = rec.live(&live)?;
    println!("live: {:.1} ms", t.elapsed().as_secs_f64() * 1e3);
    for o in &a.next_buys {
        println!("   next: {} (wpa {:+.3}, {} games)", o.label, o.wpa, o.games);
    }
    for (o, why) in &a.counters {
        println!("   counter: {} — {why}", o.label);
    }
    let _ = std::fs::remove_dir_all(&dir);
    Ok(())
}
