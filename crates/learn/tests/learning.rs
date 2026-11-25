//! The learner's contract: guardrails, rollback, determinism, backends and fallback, recommendations on synthetic
//! data with planted effects, live advice.

use bf_core::build::*;
use bf_learn::backend::{Accelerator, Backend, CheckedBackend, CpuBackend, Csr};
use bf_learn::synthetic::*;
use bf_learn::*;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, OnceLock};

fn data() -> Arc<bf_core::ddragon::StaticData> {
    bf_core::testkit::load_test_static_data()
}

fn source(matches: usize, seed: u64) -> SyntheticSource {
    generate(&data(), &SyntheticConfig { matches, validation: 500, seed, ..Default::default() })
}

fn quick() -> LearnConfig {
    LearnConfig { train_net: false, ..Default::default() }
}

/// Model files (name, bytes) in a checkpoint directory, sorted.
fn model_files(dir: &std::path::Path) -> Vec<(String, Vec<u8>)> {
    let mut v: Vec<(String, Vec<u8>)> = std::fs::read_dir(dir)
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.extension().map(|x| x == "bin").unwrap_or(false) && !p.ends_with("contexts.bin"))
        .map(|p| (p.file_name().unwrap().to_string_lossy().into_owned(), std::fs::read(&p).unwrap()))
        .collect();
    v.sort();
    v
}

/// One trained learner shared by the recommendation tests (4,000 matches, both steps).
struct Trained {
    src: Arc<SyntheticSource>,
    learner: Arc<Learner>,
    _dir: tempfile::TempDir,
}

fn trained() -> &'static Trained {
    static T: OnceLock<Trained> = OnceLock::new();
    T.get_or_init(|| {
        let dir = tempfile::tempdir().unwrap();
        let src = Arc::new(generate(&data(), &SyntheticConfig { matches: 4000, validation: 600, seed: 11, ..Default::default() }));
        let learner = Arc::new(Learner::open(dir.path(), data(), LearnConfig::default()).unwrap());
        let r = learner.train_step(src.as_ref(), &Budget::records(4000)).unwrap();
        assert_eq!(r.outcome, Outcome::Accepted, "{:#?}", r.log);
        Trained { src, learner, _dir: dir }
    })
}

fn irelia_vs_darius() -> DraftContext {
    DraftContext {
        champion: IRELIA,
        role: Some(Role::Top),
        allies: vec![(64, Some(Role::Jungle)), (103, Some(Role::Middle)), (222, Some(Role::Bottom)), (412, Some(Role::Utility))],
        enemies: vec![(DARIUS, Some(Role::Top)), (60, Some(Role::Jungle)), (134, Some(Role::Middle)), (81, Some(Role::Bottom)), (117, Some(Role::Utility))],
        patch: None,
    }
}

fn section(b: &BuildRecommendation, k: OptionKind) -> &Section {
    b.sections.iter().find(|s| s.kind == k).unwrap()
}

// ------------------------------------------------------------------------------------------------ guardrails

#[test]
fn guardrail_rejects_a_worse_update_and_keeps_the_checkpoint() {
    let dir = tempfile::tempdir().unwrap();
    let mut src = source(1500, 1);
    // A poisoned batch: the same kind of games with every winner inverted.
    src.append(generate(&data(), &SyntheticConfig { matches: 1500, validation: 0, seed: 2, flip_labels: true, ..Default::default() }), &data());
    let l = Learner::open(dir.path(), data(), quick()).unwrap();
    let r1 = l.train_step(&src, &Budget::records(1500)).unwrap();
    assert_eq!(r1.outcome, Outcome::Accepted, "{:#?}", r1.log);
    let v1 = r1.steps[0].version.clone().unwrap();
    let files = model_files(dir.path());
    let probe = &l.samples(&src.validation)[0].situation;
    let before = l.served().unwrap().predict(probe);

    let r2 = l.train_step(&src, &Budget::records(1500)).unwrap();
    assert_eq!(r2.outcome, Outcome::Rejected);
    let s = &r2.steps[0];
    assert!(!s.accepted && s.reason.as_deref().unwrap().contains("log loss"), "{s:?}");
    assert!(s.after.log_loss > s.before.log_loss);
    // The checkpoint in force is the old one, nothing new on disk, and predictions are unchanged.
    assert_eq!(model_files(dir.path()), files);
    assert_eq!(l.served().unwrap().version(), v1);
    assert_eq!(l.served().unwrap().predict(probe).to_bits(), before.to_bits());
    let st = l.status();
    assert!(st.checkpoints.iter().any(|c| c.version == v1 && c.active));
    assert!(st.log.iter().any(|line| line.contains("rejected, keeping") && line.contains(&v1)), "{:#?}", st.log);
    // Each update logs time and memory.
    assert!(st.log.iter().any(|line| line.contains("ms (train") && line.contains("memory")));
    // The poisoned records were read (the cursor moved): the next step has nothing new.
    assert_eq!(l.train_step(&src, &Budget::records(1500)).unwrap().outcome, Outcome::NoData);
}

#[test]
fn rollback_restores_a_checkpoint_and_survives_a_restart() {
    let dir = tempfile::tempdir().unwrap();
    let src = source(2400, 3);
    // Accept everything here: this test is about rollback, not the guardrail.
    let mut cfg = quick();
    cfg.guardrail.log_loss_slack = 1.0;
    cfg.guardrail.brier_slack = 1.0;
    cfg.guardrail.calibration_slack = 1.0;
    let l = Learner::open(dir.path(), data(), cfg.clone()).unwrap();
    let v1 = l.train_step(&src, &Budget::records(1200)).unwrap().steps[0].version.clone().unwrap();
    let probe = l.samples(&src.validation)[3].situation.clone();
    let p1 = l.served().unwrap().predict(&probe);
    let v2 = l.train_step(&src, &Budget::records(1200)).unwrap().steps[0].version.clone().unwrap();
    assert_ne!(v1, v2);
    assert_ne!(l.served().unwrap().predict(&probe).to_bits(), p1.to_bits());

    l.rollback(&v1).unwrap();
    assert_eq!(l.served().unwrap().version(), v1);
    assert_eq!(l.served().unwrap().predict(&probe).to_bits(), p1.to_bits());
    let st = l.status();
    assert!(st.checkpoints.iter().any(|c| c.version == v1 && c.active));
    assert!(st.checkpoints.iter().any(|c| c.version == v2 && !c.active));
    assert!(st.log[0].contains(&format!("rolled back to {v1}")));
    assert!(l.rollback("v999999").is_err());

    drop(l);
    let again = Learner::open(dir.path(), data(), cfg).unwrap();
    assert_eq!(again.served().unwrap().version(), v1);
    assert_eq!(again.served().unwrap().predict(&probe).to_bits(), p1.to_bits());
}

#[test]
fn training_stops_when_the_user_is_not_idle() {
    let dir = tempfile::tempdir().unwrap();
    let src = source(400, 5);
    let l = Learner::open(dir.path(), data(), quick()).unwrap();
    let busy = Budget { activity: Arc::new(|| Activity::ChampSelect), ..Budget::records(400) };
    assert_eq!(l.train_step(&src, &busy).unwrap().outcome, Outcome::NotIdle);
    // Idle at the start, in game a few mini-batches later.
    let calls = Arc::new(AtomicUsize::new(0));
    let c = calls.clone();
    let flips = Budget { activity: Arc::new(move || if c.fetch_add(1, Ordering::SeqCst) < 5 { Activity::Idle } else { Activity::InGame }), ..Budget::records(400) };
    let r = l.train_step(&src, &flips).unwrap();
    assert_eq!(r.outcome, Outcome::Interrupted);
    assert!(r.batches >= 3 && r.batches < 20, "{}", r.batches);
    assert!(l.served().is_none() && model_files(dir.path()).is_empty());
    // Nothing was lost: the same records train once the user is idle again.
    let r = l.train_step(&src, &Budget::records(400)).unwrap();
    assert_eq!(r.records, 400);
    l.set_learning(false);
    assert_eq!(l.train_step(&src, &Budget::records(400)).unwrap().outcome, Outcome::Disabled);
}

// ------------------------------------------------------------------------------------------------ determinism

#[test]
fn same_data_give_bit_identical_models_whatever_the_thread_count() {
    let src = source(500, 7);
    let mut cfg = LearnConfig::default();
    // Accept so both steps write a checkpoint to compare (the guardrail is tested elsewhere).
    cfg.guardrail.log_loss_slack = 1.0;
    cfg.guardrail.brier_slack = 1.0;
    cfg.guardrail.calibration_slack = 1.0;
    let run = |threads: usize| {
        let dir = tempfile::tempdir().unwrap();
        let pool = rayon::ThreadPoolBuilder::new().num_threads(threads).build().unwrap();
        pool.install(|| {
            let l = Learner::open(dir.path(), data(), cfg.clone()).unwrap();
            for _ in 0..2 {
                l.train_step(&src, &Budget::records(250)).unwrap();
            }
        });
        model_files(dir.path())
    };
    let a = run(1);
    let b = run(4);
    let c = run(4);
    assert!(a.iter().any(|f| f.0.contains("logistic")) && a.iter().any(|f| f.0.contains("shared-nn")), "{:?}", a.iter().map(|f| &f.0).collect::<Vec<_>>());
    assert_eq!(a, b, "1 thread vs 4 threads");
    assert_eq!(b, c, "two runs");
}

// ------------------------------------------------------------------------------------------------ backends

/// A stand-in GPU: the processor's arithmetic with each row's sum in reverse order (as a parallel reduction on a card
/// would differ), optionally wrong after `bad_after` calls, or failing.
struct StandIn {
    calls: usize,
    bad_after: Option<usize>,
    fail: bool,
}

impl Accelerator for StandIn {
    fn name(&self) -> String {
        "gpu: stand-in".into()
    }
    fn logits(&mut self, rows: &Csr, w: &[f64], out: &mut [f64]) -> Option<Result<(), String>> {
        self.calls += 1;
        if self.fail && self.calls > 3 {
            return Some(Err("device lost".into()));
        }
        for r in 0..rows.rows() {
            let mut s = 0.0;
            for k in (rows.ptr[r]..rows.ptr[r + 1]).rev() {
                s += w[rows.idx[k] as usize] * rows.val[k];
            }
            out[r] = s;
        }
        if self.bad_after.map(|n| self.calls > n).unwrap_or(false) {
            out[0] += 1e-3;
        }
        Some(Ok(()))
    }
    fn similarities(&mut self, flat: &[f64], rows: usize, d: usize, q: &[f64], den: f64, out: &mut [f64]) -> Option<Result<(), String>> {
        for (i, o) in out.iter_mut().enumerate().take(rows) {
            *o = bf_learn::backend::row_similarity(flat, d, q, den, i);
        }
        Some(Ok(()))
    }
}

#[test]
fn stand_in_gpu_agrees_with_the_processor_and_falls_back_on_a_mismatch() {
    let src = source(600, 9);
    let mut log = vec![];
    // A device wrong from the start never passes its check.
    assert!(CheckedBackend::new(Box::new(StandIn { calls: 0, bad_after: Some(0), fail: false }), 1, &mut log).is_none());
    assert!(log[0].contains("check failed"), "{log:?}");

    // A correct device: training through it gives the processor's model within the tolerance.
    let gpu = Arc::new(CheckedBackend::new(Box::new(StandIn { calls: 0, bad_after: None, fail: false }), 1, &mut log).unwrap());
    assert_eq!(gpu.name(), "gpu: stand-in");
    let (d1, d2) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    // Accept every update: the comparison is between backends, not against the guardrail.
    let mut cfg = quick();
    cfg.guardrail.log_loss_slack = 1.0;
    cfg.guardrail.brier_slack = 1.0;
    cfg.guardrail.calibration_slack = 1.0;
    let quick = || cfg.clone();
    let on_gpu = Learner::open_with_backend(d1.path(), data(), quick(), gpu.clone()).unwrap();
    let on_cpu = Learner::open_with_backend(d2.path(), data(), quick(), Arc::new(CpuBackend)).unwrap();
    let rg = on_gpu.train_step(&src, &Budget::records(600)).unwrap();
    let rc = on_cpu.train_step(&src, &Budget::records(600)).unwrap();
    assert!(gpu.on_device.load(Ordering::SeqCst) > 10 && !gpu.fell_back());
    assert!((rg.steps[0].after.log_loss - rc.steps[0].after.log_loss).abs() < 1e-6, "{} vs {}", rg.steps[0].after.log_loss, rc.steps[0].after.log_loss);
    let val = on_cpu.samples(&src.validation);
    for s in val.iter().take(200) {
        let (a, b) = (on_gpu.served().unwrap().predict(&s.situation), on_cpu.served().unwrap().predict(&s.situation));
        assert!((a - b).abs() < 1e-6, "{a} vs {b}");
    }

    // A device that goes wrong mid-training: the 1-in-N check catches it and the rest runs on the processor, with the
    // processor's numbers for the call that was caught.
    let flaky = Arc::new(CheckedBackend::new(Box::new(StandIn { calls: 0, bad_after: Some(12), fail: false }), 1, &mut log).unwrap());
    let d3 = tempfile::tempdir().unwrap();
    let l = Learner::open_with_backend(d3.path(), data(), quick(), flaky.clone()).unwrap();
    let r = l.train_step(&src, &Budget::records(600)).unwrap();
    assert!(flaky.fell_back());
    assert!(flaky.name().starts_with("cpu (after gpu: stand-in"));
    assert!(r.log.iter().any(|x| x.contains("verification failed") && x.contains("back to the processor")), "{:#?}", r.log);
    assert!((r.steps[0].after.log_loss - rc.steps[0].after.log_loss).abs() < 1e-6);
    let rows = {
        let mut c = Csr::new();
        c.push_row(&[(0, 1.0), (5, 2.0)]);
        c
    };
    let w: Vec<f64> = (0..8).map(|i| i as f64 * 0.25).collect();
    let mut a = [0.0];
    flaky.logits(&rows, &w, &mut a);
    assert_eq!(a[0].to_bits(), (1.0f64 * 0.0 + 1.25 * 2.0).to_bits());

    // A device that fails: same fallback.
    let failing = CheckedBackend::new(Box::new(StandIn { calls: 0, bad_after: None, fail: false }), 3, &mut log).unwrap();
    drop(failing);
    let lost = Arc::new(CheckedBackend::new(Box::new(StandIn { calls: 0, bad_after: None, fail: false }), 3, &mut log).unwrap());
    let mut flat = vec![0.5; 40];
    flat[3] = f64::NAN;
    let mut o1 = vec![0.0; 10];
    let mut o2 = vec![0.0; 10];
    lost.similarities(&flat, 10, 4, &[0.4, 0.6, f64::NAN, 0.1], 0.5, &mut o1);
    CpuBackend.similarities(&flat, 10, 4, &[0.4, 0.6, f64::NAN, 0.1], 0.5, &mut o2);
    assert_eq!(o1, o2);
    let gone = Arc::new(CheckedBackend::new(Box::new(StandIn { calls: 0, bad_after: None, fail: true }), 1, &mut log).unwrap());
    let d4 = tempfile::tempdir().unwrap();
    let l = Learner::open_with_backend(d4.path(), data(), quick(), gone.clone()).unwrap();
    let r = l.train_step(&src, &Budget::records(600)).unwrap();
    assert!(gone.fell_back() && r.log.iter().any(|x| x.contains("device lost")));
    assert!((r.steps[0].after.log_loss - rc.steps[0].after.log_loss).abs() < 1e-6);
}

// ------------------------------------------------------------------------------------------------ recommendations

#[test]
fn planted_effects_are_recovered() {
    let t = trained();
    let rec = Recommender::new(t.src.clone(), Some(t.learner.clone()), data(), RecommendConfig::default());
    let b = rec.recommend(&irelia_vs_darius()).unwrap();
    assert_eq!(b.tier, Tier::A, "{:?}", b.notes);
    assert_eq!(b.lane_opponent, Some(DARIUS));
    // Conqueror against Darius (planted +0.45 logit), by the model.
    let ks = section(&b, OptionKind::Keystone);
    assert_eq!(ks.options[0].option, BuildOption::keystone(CONQUEROR), "{:#?}", ks.options);
    assert!(ks.options[0].predicted_win.is_some());
    let gap = ks.options[0].wpa - ks.options.iter().find(|o| o.option == BuildOption::keystone(GRASP)).unwrap().wpa;
    assert!(gap > 0.04, "Conqueror − Grasp vs Darius: {gap}");
    assert_eq!(section(&b, OptionKind::RunePage).options[0].option.key.split('/').next().unwrap().split(':').nth(1).unwrap().split(',').next().unwrap(), "8010");
    // Against another top laner the keystones are close (nothing planted there).
    let mut garen = irelia_vs_darius();
    garen.enemies[0] = (86, Some(Role::Top));
    let bg = rec.recommend(&garen).unwrap();
    let kg = section(&bg, OptionKind::Keystone);
    let wpa = |k: i64| kg.options.iter().find(|o| o.option == BuildOption::keystone(k)).unwrap().wpa;
    assert!((wpa(CONQUEROR) - wpa(GRASP)).abs() < gap / 2.0, "vs Garen {} vs {}", wpa(CONQUEROR), wpa(GRASP));
    // Blade of the Ruined King first (planted +0.3), from the counts.
    assert_eq!(section(&b, OptionKind::FirstItem).options[0].option, BuildOption::first_item(BOTRK));
    // Maw of Malmortius against magic teams (planted +0.6, −0.1 otherwise): ranked among the top two late items
    // against a magic team, from the counts at the enemy-profile level, ahead of the late items with nothing planted.
    let counts_only = Recommender::new(t.src.clone(), None, data(), RecommendConfig { per_section: 20, ..Default::default() });
    let d = DraftContext { enemies: vec![(8, None), (876, None), (134, None), (115, None), (117, None)], ..irelia_vs_darius() };
    let bm = counts_only.recommend(&d).unwrap();
    assert_eq!((bm.tier, bm.enemy_profile), (Tier::B, Some(DamageProfile::Magic)));
    let late = &section(&bm, OptionKind::LateItem).options;
    let at = late.iter().position(|o| o.option == BuildOption::late_item(MAW)).unwrap();
    assert!(at < 2 && late[at].wpa > 0.0 && late[at].basis == Basis::Profile, "{:#?}", late);
    let others = late.iter().filter(|o| o.option != BuildOption::late_item(MAW)).map(|o| o.wpa).sum::<f64>() / (late.len() - 1) as f64;
    assert!(late[at].wpa > others + 0.04, "Maw {} vs the others' mean {others}", late[at].wpa);
}

#[test]
fn every_option_carries_counts_labels_and_consistent_sections() {
    let t = trained();
    let rec = Recommender::new(t.src.clone(), Some(t.learner.clone()), data(), RecommendConfig::default());
    let b = rec.recommend(&irelia_vs_darius()).unwrap();
    let excl = bf_core::engine::exclusivity::build_exclusivity_index(&data());
    for s in &b.sections {
        assert!(s.options.len() >= 2, "{:?} has {} options", s.kind, s.options.len());
        for o in &s.options {
            assert!(o.games > 0 && o.weighted_games > 0.0 && o.wins <= o.games, "{o:?}");
            assert!(!o.label.is_empty() && !o.label.contains('#'), "{o:?}");
            assert_eq!(o.option.kind, s.kind);
        }
    }
    assert!(section(&b, OptionKind::Keystone).options.iter().any(|o| o.label == "Conqueror"));
    assert!(section(&b, OptionKind::Summoners).options.iter().any(|o| o.label == "Flash + Teleport" || o.label == "Flash + Ignite"));
    assert!(section(&b, OptionKind::SkillMax).options.iter().any(|o| o.label == "Q > E > W"));
    assert!(section(&b, OptionKind::StartingItems).options.iter().any(|o| o.label == "Doran's Blade + Health Potion"));
    let boots: Vec<i64> = section(&b, OptionKind::Boots).options.iter().flat_map(|o| o.option.item_ids()).collect();
    for k in [OptionKind::FirstItem, OptionKind::CoreItems, OptionKind::LateItem] {
        for o in &section(&b, k).options {
            assert!(o.option.item_ids().iter().all(|i| !boots.contains(i)), "boots in {k:?}");
        }
    }
    let core = section(&b, OptionKind::CoreItems);
    for o in &core.options {
        assert!(excl.conflicts(&o.option.item_ids()).is_empty(), "{}", o.label);
    }
    assert_eq!(core.options[0].option.item_ids()[0], section(&b, OptionKind::FirstItem).options[0].option.item_ids()[0]);
    let top_core = core.options[0].option.item_ids();
    for o in &section(&b, OptionKind::LateItem).options {
        let i = o.option.item_ids()[0];
        assert!(!top_core.contains(&i) && excl.blocker(&top_core, i).is_none(), "{}", o.label);
    }
    let pages = section(&b, OptionKind::RunePage);
    assert!(pages.options[0].label.starts_with(&section(&b, OptionKind::Keystone).options[0].label));
    assert_eq!(b.model, t.learner.served().map(|s| s.version()));
    // The JSON the dashboard gets.
    let j = serde_json::to_value(&b).unwrap();
    assert!(j["sections"][0]["options"][0]["weightedGames"].is_number());
}

#[test]
fn thin_champion_falls_back_and_is_flagged() {
    let t = trained();
    let rec = Recommender::new(t.src.clone(), Some(t.learner.clone()), data(), RecommendConfig::default());
    let d = DraftContext { champion: IVERN, role: Some(Role::Jungle), enemies: irelia_vs_darius().enemies, ..Default::default() };
    let b = rec.recommend(&d).unwrap();
    assert_eq!(b.tier, Tier::B, "too few games for the model");
    assert!(b.games > 0 && b.games < 120, "{}", b.games);
    assert!(b.notes.iter().any(|n| n.starts_with("Thin data")), "{:?}", b.notes);
    let all: Vec<&RankedOption> = b.sections.iter().flat_map(|s| s.options.iter()).collect();
    assert!(!all.is_empty());
    for o in &all {
        assert_ne!(o.basis, Basis::Matchup, "{o:?}");
        if o.thin {
            assert_eq!(o.basis, Basis::Champion);
        }
    }
    assert!(all.iter().filter(|o| o.thin).count() * 2 >= all.len(), "most options are thin");
    // Shrinkage: a thin option's WPA stays near zero, however lucky its few games were.
    for o in all.iter().filter(|o| o.thin) {
        assert!(o.wpa.abs() < 0.12, "{o:?}");
    }
    // A champion with no games at all still gets an answer (an empty one, said so).
    let none = rec.recommend(&DraftContext { champion: 266, role: Some(Role::Top), ..Default::default() }).unwrap();
    assert!(none.notes.iter().any(|n| n.starts_with("No games stored")));
}

#[test]
fn live_advice_gives_several_open_options_with_evidence() {
    let t = trained();
    let rec = Recommender::new(t.src.clone(), Some(t.learner.clone()), data(), RecommendConfig::default());
    let excl = bf_core::engine::exclusivity::build_exclusivity_index(&data());
    let owned = vec![TRINITY, 3074, 3047, 2003];
    let ctx = LiveContext {
        draft: irelia_vs_darius(),
        game_time_s: 1500.0,
        my_items: owned.clone(),
        enemy_items: vec![(134, vec![6655, 3089, 3020]), (117, vec![6617, 3158]), (60, vec![4645])],
        my_level: 14,
        my_cs: 190,
        my_gold: 1400.0,
        opponent_level: Some(13),
        opponent_cs: Some(170),
        team_kills: 12,
        enemy_kills: 9,
        towers: (3, 1),
        dragons: (2, 1),
        barons: (0, 0),
    };
    let a = rec.live(&ctx).unwrap();
    assert!(a.next_buys.len() >= 2);
    assert!(a.predicted_win.is_some());
    for o in a.next_buys.iter().chain(a.counters.iter().map(|c| &c.0)) {
        let id = o.option.item_ids()[0];
        assert!(!owned.contains(&id), "owned {}", o.label);
        assert!(excl.blocker(&owned, id).is_none(), "{} conflicts with {:?}", o.label, excl.blocker(&owned, id));
        assert!(o.option.kind != OptionKind::Boots, "already has boots: {}", o.label);
    }
    for o in &a.next_buys {
        assert!(o.games > 0 && o.predicted_win.is_some());
    }
    // A magic-heavy enemy: magic resist is among the counters, with the reason.
    assert!(a.counters.iter().any(|(o, why)| why.contains("magic") && data().item(o.option.item_ids()[0]).unwrap().magic_resist > 0.0), "{:?}", a.counters);
    assert!(a.notes.iter().any(|n| n.contains("choice is yours")));

    // No data at all for this champion: still several options, flagged.
    // (Aatrox is in none of the synthetic pools.)
    let lone = LiveContext { draft: DraftContext { champion: 266, role: Some(Role::Top), ..Default::default() }, my_items: vec![3071], ..Default::default() };
    let a = rec.live(&lone).unwrap();
    assert!(a.next_buys.len() >= 2 && a.next_buys.iter().all(|o| o.thin && !o.option.item_ids().contains(&3071)), "{:?}", a.next_buys);
}

#[test]
fn checkpoints_report_metrics_and_the_backend() {
    let t = trained();
    let st = t.learner.status();
    assert_eq!(st.backend, "cpu");
    assert!(st.learning);
    let active: Vec<&CheckpointInfo> = st.checkpoints.iter().filter(|c| c.active).collect();
    assert!(!active.is_empty() && active.iter().any(|c| c.kind == "logistic"));
    for c in &active {
        assert_eq!(c.metrics.calibration.len(), 4);
        assert!(c.metrics.log_loss > 0.3 && c.metrics.log_loss < 0.69, "{:?}", c.metrics);
        assert!(c.records_seen == 4000);
    }
    assert!(t.learner.disk_bytes() > 0);
}

// ------------------------------------------------------------------------------------------------ waiting for data

/// The app's budget: the first update waits for 1,500 new games here (2,000 in the app), later ones for 500, with a
/// held-out sample of at least 100 matches.
fn waiting_budget(max_wait: Option<std::time::Duration>) -> Budget {
    Budget { min_records: 500, first_min_records: 1500, max_wait, min_validation: 100, ..Budget::records(2000) }
}

#[test]
fn updates_wait_for_enough_games_and_read_nothing_meanwhile() {
    let dir = tempfile::tempdir().unwrap();
    // 1,000 games so far: the first update waits, and reads nothing.
    let mut src = source(1000, 1);
    let l = Learner::open(dir.path(), data(), quick()).unwrap();
    let r = l.train_step(&src, &waiting_budget(None)).unwrap();
    assert_eq!(r.outcome, Outcome::Waiting);
    let st = l.status();
    assert!(st.checkpoints.is_empty());
    assert_eq!(st.log.first().map(String::as_str), Some("Waiting for 1500 new games before the first update (1000 so far)."), "{:?}", st.log);
    // Waiting again changes nothing either (many polls while fetching trickles in).
    assert_eq!(l.train_step(&src, &waiting_budget(None)).unwrap().outcome, Outcome::Waiting);

    // 500 more arrive: the first update reads all 1,500, none lost to the waits.
    src.append(generate(&data(), &SyntheticConfig { matches: 500, validation: 0, seed: 2, ..Default::default() }), &data());
    let r = l.train_step(&src, &waiting_budget(None)).unwrap();
    assert_eq!(r.records, 1500);
    assert_eq!(r.outcome, Outcome::Accepted, "{:#?}", r.log);
    assert!(!l.status().log.first().unwrap().starts_with("Waiting"), "the waiting line goes once an update ran");

    // A trickle of 200 after it: the next update waits for 500...
    src.append(generate(&data(), &SyntheticConfig { matches: 200, validation: 0, seed: 3, ..Default::default() }), &data());
    assert_eq!(l.train_step(&src, &waiting_budget(Some(std::time::Duration::from_secs(3600)))).unwrap().outcome, Outcome::Waiting);
    assert!(l.status().log[0].contains("before the next update (200 so far)"), "{:?}", l.status().log);
    // ...unless the last update is older than max_wait: then the 200 go in.
    let r = l.train_step(&src, &waiting_budget(Some(std::time::Duration::ZERO))).unwrap();
    assert_eq!(r.records, 200);
    assert!(matches!(r.outcome, Outcome::Accepted | Outcome::Rejected), "{:?}", r.outcome);
}

#[test]
fn updates_wait_for_a_validation_sample() {
    let dir = tempfile::tempdir().unwrap();
    // Plenty of games, but only 50 held out: the guardrail couldn't judge, so nothing is read.
    let src = generate(&data(), &SyntheticConfig { matches: 2000, validation: 50, seed: 4, ..Default::default() });
    let l = Learner::open(dir.path(), data(), quick()).unwrap();
    let r = l.train_step(&src, &waiting_budget(None)).unwrap();
    assert_eq!(r.outcome, Outcome::Waiting);
    assert!(l.status().log[0].starts_with("Waiting for 100 held-out games"), "{:?}", l.status().log);
    // With no minimums (tests, measurements) the same data train at once.
    assert_ne!(l.train_step(&src, &Budget::records(2000)).unwrap().outcome, Outcome::Waiting);
}
