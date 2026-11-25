//! Continuous learning on synthetic data (1.2.0): the first model from a batch, then games arriving in small groups,
//! learnt as they come and checked every `every` games. Prints, per check, the outcome and the held-out log loss of
//! the model in force, the time per absorbed game and per check, and memory; and the same games learnt the 1.1 way
//! (batches of 500) for comparison.
//!
//! cargo run --release -p bf-learn --example measure_continuous -- [first] [later] [group] [every] [armor 0/1] [min games] [lr scale] [epochs]
use bf_core::build::Activity;
use bf_learn::synthetic::*;
use bf_learn::*;
use std::sync::Arc;
use std::time::Instant;

fn main() {
    let a: Vec<String> = std::env::args().collect();
    let arg = |i: usize, d: usize| a.get(i).and_then(|x| x.parse().ok()).unwrap_or(d);
    let (first, later, group, every, armor) = (arg(1, 4000), arg(2, 6000), arg(3, 1), arg(4, 150), arg(5, 1) == 1);
    let data = bf_core::testkit::load_test_static_data();
    let all = generate(&data, &SyntheticConfig { matches: first + later, validation: 1500, seed: 5, armor_effect: armor, ..Default::default() });
    let f = |i: usize, d: f64| a.get(i).and_then(|x| x.parse().ok()).unwrap_or(d);
    let config = LearnConfig { continuous: ContinuousOptions { min_games: arg(6, 1), lr_scale: f(7, 0.1), epochs: arg(8, 1) }, ..LearnConfig::default() };
    println!("{:?}", config.continuous);
    let logloss = |l: &Learner| l.status().checkpoints.iter().find(|c| c.active && c.kind == "logistic").map(|c| c.metrics.log_loss).unwrap_or(f64::NAN);

    let modes: Vec<&str> = if std::env::var("ONLY_CONT").is_ok() { vec!["continuous"] } else { vec!["continuous", "batches of 500"] };
    for mode in modes {
        let dir = tempfile::tempdir().unwrap();
        let l = Learner::open(dir.path(), data.clone(), config.clone()).unwrap();
        // The source grows as games "arrive".
        let mut src = SyntheticSource { records: all.records[..first].to_vec(), validation: all.validation.clone(), counts: all.counts.clone(), volumes: all.volumes.clone(), current: all.current };
        let r = l.train_step(&src, &Budget::records(first)).unwrap();
        println!("[{mode}] first model: {:?}, log loss {:.5}, {:.1} s (train {:.1} s)\n  {}", r.outcome, logloss(&l), r.elapsed_ms / 1e3, r.train_ms / 1e3, r.log[0]);
        let policy = CheckPolicy { every, max_wait: None, ..Default::default() };
        let idle: Arc<dyn Fn() -> Activity + Send + Sync> = Arc::new(|| Activity::Idle);
        let (mut absorb_ms, mut absorbed, mut checks, mut accepted, mut check_ms) = (0.0, 0usize, 0, 0, 0.0);
        let t0 = Instant::now();
        let mut at = first;
        while at < first + later {
            let n = group.min(first + later - at);
            src.records.extend_from_slice(&all.records[at..at + n]);
            at += n;
            if mode == "continuous" {
                let t = Instant::now();
                let ab = l.absorb(&src, 1000, None, idle.clone()).unwrap();
                absorb_ms += t.elapsed().as_secs_f64() * 1e3;
                absorbed += ab.records;
                let t = Instant::now();
                if let Some(r) = l.check(&src, &policy).unwrap() {
                    check_ms += t.elapsed().as_secs_f64() * 1e3;
                    checks += 1;
                    if r.outcome == Outcome::Accepted {
                        accepted += 1;
                    }
                    if std::env::var("WHY").is_ok() {
                        println!("    {}", r.log[0]);
                    }
                    if checks % 5 == 0 || at >= first + later {
                        println!("  after {:>5} games: {checks} checks, {accepted} accepted; log loss in force {:.5}", at - first, logloss(&l));
                    }
                }
            } else if (at - first) % 500 == 0 || at >= first + later {
                let r = l.train_step(&src, &Budget::records(1000)).unwrap();
                checks += 1;
                if r.outcome == Outcome::Accepted {
                    accepted += 1;
                }
                println!("  after {:>5} games: {:?}; log loss in force {:.5}", at - first, r.outcome, logloss(&l));
            }
        }
        let rss = bf_core::sys::own_rss().map(|b| b as f64 / 1_048_576.0).unwrap_or(f64::NAN);
        println!(
            "[{mode}] {later} games in {:.1} s: {checks} checks, {accepted} accepted; final log loss {:.5}; absorb {:.2} ms per game, check {:.0} ms each; memory {rss:.0} MB",
            t0.elapsed().as_secs_f64(),
            logloss(&l),
            if absorbed > 0 { absorb_ms / absorbed as f64 } else { 0.0 },
            if checks > 0 && mode == "continuous" { check_ms / checks as f64 } else { 0.0 }
        );
        let files: Vec<(String, u64)> = std::fs::read_dir(dir.path()).unwrap().filter_map(|e| e.ok()).map(|e| (e.file_name().to_string_lossy().into_owned(), e.metadata().unwrap().len())).collect();
        let largest = files.iter().filter(|f| f.0.ends_with(".bin") && f.0 != "contexts.bin").map(|f| f.1).max().unwrap_or(0);
        println!("[{mode}] checkpoint folder {:.1} MB in {} files; largest model file {:.1} MB", files.iter().map(|f| f.1).sum::<u64>() as f64 / 1e6, files.len(), largest as f64 / 1e6);
    }
}
