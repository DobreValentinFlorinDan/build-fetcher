//! The three parts together, as `wiring.rs` connects them: synthetic match-v5 games go through the real store
//! (filters, dedupe, counts, records), the real learner trains on the store under its guardrails, the real
//! recommender answers from both, and clean-up runs after training. Then the real key handles against a stand-in
//! Irelia Companion folder. Offline; no key, no Riot.

use bf_core::build::{Activity, BuildSource, DataSource, DraftContext, LiveContext, Role, Tier};
use bf_core::testkit::{load_test_static_data, Synth, SynthOptions};
use bf_learn::{Budget, LearnConfig, Learner, Outcome as LearnOutcome, RecommendConfig, Recommender};
use bf_server::handles::{KeyHandle, KeyOffer, StoreHandle};
use bf_server::wire_store::{RealKeys, RealStore};
use bf_store::{Ingest, LearnWindow, OtherParts, Outcome, Store, StoreOptions};
use std::collections::HashSet;
use std::sync::Arc;

const IRELIA: i64 = 39;

#[test]
fn store_learner_and_recommender_end_to_end() {
    let dir = tempfile::tempdir().unwrap();
    let data = load_test_static_data();
    let store = Arc::new(Store::open(dir.path(), data.clone(), StoreOptions::default()).unwrap());
    store.set_tier_a(&HashSet::from([IRELIA])).unwrap();

    // 4,000 games, half with a timeline, Irelia mid in a third of them, some defective.
    let synth = Synth::new(
        SynthOptions { timeline_share: 0.5, featured: Some((IRELIA, "MIDDLE".into())), featured_share: 0.33, noise_share: 0.03, games_per_version: 2_000, ..Default::default() },
        data.clone(),
    );
    let t = std::time::Instant::now();
    let mut outcomes = Vec::new();
    for chunk in (0..4_000u64).collect::<Vec<_>>().chunks(500) {
        let games: Vec<_> = chunk.iter().map(|i| synth.game(*i)).collect();
        let items: Vec<Ingest> = games.iter().map(|g| Ingest { m: &g.m, timeline: g.timeline.as_ref(), rank: bf_core::build::RankBand::MasterPlus, me: None }).collect();
        outcomes.extend(store.ingest(&items).unwrap());
    }
    // The same games again (found from another player or region): all duplicates.
    let again: Vec<_> = (0..50u64).map(|i| synth.game(i)).collect();
    let items: Vec<Ingest> = again.iter().map(|g| Ingest { m: &g.m, timeline: g.timeline.as_ref(), rank: bf_core::build::RankBand::MasterPlus, me: None }).collect();
    assert!(store.ingest(&items).unwrap().iter().all(|o| matches!(o, Outcome::Duplicate | Outcome::Dropped(_))));
    let dropped = outcomes.iter().filter(|o| matches!(o, Outcome::Dropped(_))).count();
    let kept = outcomes.iter().filter(|o| matches!(o, Outcome::Counted { kept: true, .. })).count();
    eprintln!("ingested 4000 in {:?}: {kept} records kept, {dropped} dropped", t.elapsed());
    assert!(dropped > 0 && dropped < 400, "{dropped}");
    assert!(!store.validation_records().unwrap().is_empty());

    // The learner trains on the store until it has seen every record.
    let ck = dir.path().join("checkpoints");
    let learner = Arc::new(Learner::open(&ck, data.clone(), LearnConfig::default()).unwrap());
    let mut accepted = 0;
    for _ in 0..10 {
        let r = learner.train_step(&*store, &Budget::records(2_000)).unwrap();
        eprintln!("train step: {:?}, {} records, {} samples, {} validation samples, {:.1} s", r.outcome, r.records, r.samples, r.validation_samples, r.elapsed_ms / 1000.0);
        for s in &r.steps { eprintln!("  {} accepted={} before={:?} after={:?} reason={:?}", s.kind, s.accepted, s.before, s.after, s.reason); }
        match r.outcome {
            LearnOutcome::Accepted => accepted += 1,
            LearnOutcome::NoData => break,
            _ => {}
        }
    }
    assert!(accepted >= 1, "no update beat the baseline");
    let status = learner.status();
    assert!(status.checkpoints.iter().any(|c| c.active));

    // Not idle: no training starts.
    let busy = Budget { activity: Arc::new(|| Activity::ChampSelect), ..Budget::records(2_000) };
    assert_eq!(learner.train_step(&*store, &busy).unwrap().outcome, LearnOutcome::NotIdle);

    // Recommendations: Irelia mid (tier A, the model) and the thinnest pair the store has (counts, flagged).
    let source: Arc<dyn DataSource> = store.clone();
    let rec = Recommender::new(source, Some(learner.clone()), data.clone(), RecommendConfig::default());
    let irelia = rec.recommend(&DraftContext { champion: IRELIA, role: Some(Role::Middle), ..Default::default() }).unwrap();
    assert_eq!(irelia.tier, Tier::A);
    assert!(irelia.sections.iter().filter(|s| !s.options.is_empty()).count() >= 5, "{:?}", irelia.sections.iter().map(|s| (s.kind, s.options.len())).collect::<Vec<_>>());
    for s in &irelia.sections {
        for o in &s.options {
            assert!(o.wins <= o.games && (0.0..=1.0).contains(&o.win_rate), "{o:?}");
        }
    }
    let vols = store.volumes(&[]).unwrap();
    let thin = vols.iter().filter(|v| v.games > 0 && v.champion != IRELIA).min_by_key(|v| v.games).unwrap();
    let r = rec.recommend(&DraftContext { champion: thin.champion, role: Some(thin.role), ..Default::default() }).unwrap();
    assert_eq!(r.tier, Tier::B);
    let flagged = r.sections.iter().flat_map(|s| &s.options).filter(|o| o.thin).count();
    eprintln!("thinnest pair {} {:?}: {} games, {flagged} options flagged thin, notes {:?}", thin.champion, thin.role, thin.games, r.notes);
    assert!(flagged > 0 || !r.notes.is_empty());

    // In game: several options, none already owned.
    let live = rec
        .live(&LiveContext { draft: DraftContext { champion: IRELIA, role: Some(Role::Middle), ..Default::default() }, game_time_s: 900.0, my_items: vec![3153], ..Default::default() })
        .unwrap();
    assert!(live.next_buys.len() >= 2);
    assert!(live.next_buys.iter().all(|o| !o.option.item_ids().contains(&3153)));

    // Clean-up after the training cycle: validation kept, bytes reported, budget held.
    let other = OtherParts::standard(dir.path(), None);
    let before_val = store.validation_records().unwrap().len();
    let c = store.cleanup(&LearnWindow::default(), 2 * 1024 * 1024 * 1024, &other).unwrap();
    eprintln!("cleanup: {} -> {} bytes ({} freed) in {} ms", c.before_bytes, c.after_bytes, c.freed_bytes, c.millis);
    assert_eq!(store.validation_records().unwrap().len(), before_val);
    assert!(!c.over_budget);
    let st = store.storage_status(2 * 1024 * 1024 * 1024, &other).unwrap();
    assert!(st.used_bytes > 0 && st.parts.iter().any(|(p, b)| p == "checkpoints" && *b > 0), "{:?}", st.parts);
}

#[tokio::test]
async fn real_key_handles_import_from_irelia_without_touching_it() {
    let home = tempfile::tempdir().unwrap();
    let settings_file = home.path().join("build-fetcher").join("settings.json");
    let irelia = home.path().join("irelia-companion");
    std::fs::create_dir_all(&irelia).unwrap();
    let irelia_file = irelia.join("config.json");
    let original = "{\n  \"riotApiKey\": \"RGAPI-00000000-irelia\",\n  \"dataDir\": \"D:/IreliaData\",\n  \"overlay\": true\n}\n";
    std::fs::write(&irelia_file, original).unwrap();
    let mtime = std::fs::metadata(&irelia_file).unwrap().modified().unwrap();

    let store = RealStore::new(settings_file.clone(), home.path().join("data"), home.path().join("logs"));
    let keys = RealKeys::new(settings_file.clone(), irelia.clone(), store.clone());
    assert_eq!(keys.irelia_offer(), KeyOffer::Available);
    assert_eq!(keys.key_masked(), "");
    // Without a key and before the first run, fetching waits and says why.
    assert!(store.fetch_status().paused.is_some());

    keys.import_irelia_key().unwrap();
    assert_eq!(keys.key_masked(), "RGAPI-…elia");
    // Irelia Companion's file: same bytes, same time.
    assert_eq!(std::fs::read_to_string(&irelia_file).unwrap(), original);
    assert_eq!(std::fs::metadata(&irelia_file).unwrap().modified().unwrap(), mtime);
    // Only the key came over.
    let ours = std::fs::read_to_string(&settings_file).unwrap();
    assert!(!ours.contains("IreliaData") && !ours.contains("overlay"), "{ours}");
    // The key never shows in what the dashboard gets.
    let shown = serde_json::to_string(&(store.fetch_status(), store.settings(), keys.key_masked(), keys.irelia_offer())).unwrap();
    assert!(!shown.contains("00000000-irelia"), "{shown}");

    // The timeline share goes through the real store to the settings file and back (1.2.1: it was missing here).
    assert_eq!(store.settings().timeline_sample, bf_store::settings::DEFAULT_TIMELINE_SAMPLE);
    let after = store.update_settings(&bf_server::handles::SettingsPatch { timeline_sample: Some(0.5), ..Default::default() }).unwrap();
    assert_eq!(after.timeline_sample, 0.5);
    assert_eq!(store.settings().timeline_sample, 0.5);
    assert_eq!(bf_store::settings::load(&settings_file).0.timeline_sample, 0.5);

    // A pasted key replaces it; Irelia's file still untouched.
    keys.set_key("RGAPI-00000000-0000-0000-0000-00000000test").unwrap();
    assert_eq!(keys.key_masked(), "RGAPI-…test");
    assert_eq!(std::fs::read_to_string(&irelia_file).unwrap(), original);

    // An unreadable Irelia file is reported without its content.
    std::fs::write(&irelia_file, "{ not json RGAPI-00000000-irelia").unwrap();
    match keys.irelia_offer() {
        KeyOffer::Unreadable { reason } => assert!(!reason.contains("00000000-irelia"), "{reason}"),
        o => panic!("{o:?}"),
    }
}
