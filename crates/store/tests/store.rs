//! The store end to end: ingestion, dedupe, filters, the DataSource view, the baseline hook and clean-up.

use bf_core::build::{CountQuery, DataSource, MatchRecord, OptionKind, Patch, RankBand, Role};
use bf_core::testkit::{load_test_static_data, make_match, make_timeline, MatchSpec, Synth, SynthOptions, TimelineSpec};
use bf_store::cleanup::OtherParts;
use bf_store::{Baseline, Drop, Ingest, LearnWindow, Outcome, Store, StoreOptions};
use std::collections::HashSet;
use std::sync::Arc;

fn open(dir: &std::path::Path) -> Store {
    Store::open(dir, load_test_static_data(), StoreOptions { dictionary_after: 16, ..Default::default() }).unwrap()
}

fn ingest_synth(store: &Store, s: &Synth, range: std::ops::Range<u64>) -> Vec<Outcome> {
    let games: Vec<_> = range.map(|i| s.game(i)).collect();
    let items: Vec<Ingest> = games.iter().map(|g| Ingest { m: &g.m, timeline: g.timeline.as_ref(), rank: RankBand::MasterPlus, me: None }).collect();
    store.ingest(&items).unwrap()
}

#[test]
fn ingests_counts_for_all_ten_players_and_keeps_only_tier_a_and_timeline_records() {
    let dir = tempfile::tempdir().unwrap();
    let store = open(dir.path());
    store.set_tier_a(&HashSet::from([39])).unwrap();
    // Irelia mid in a third of the games; a fifth of games with a timeline.
    let s = Synth::new(SynthOptions { timeline_share: 0.2, featured: Some((39, "MIDDLE".into())), featured_share: 0.3, ..Default::default() }, load_test_static_data());
    let out = ingest_synth(&store, &s, 0..600);
    let counted = out.iter().filter(|o| matches!(o, Outcome::Counted { .. })).count();
    let kept = out.iter().filter(|o| matches!(o, Outcome::Counted { kept: true, .. })).count();
    let validation = out.iter().filter(|o| **o == Outcome::Validation).count();
    assert_eq!(counted + validation, 600);
    assert!(validation > 0 && validation < 30, "{validation}");
    // Every kept record has Irelia or a timeline; every Irelia or timeline game is kept.
    let records = store.records_after(0, 10_000).unwrap();
    assert_eq!(records.len(), kept);
    for (_, r) in &records {
        assert!(r.has_timeline() || r.players.iter().any(|p| p.champion == 39), "{}", r.match_id);
    }
    let seqs: Vec<u64> = records.iter().map(|x| x.0).collect();
    assert!(seqs.windows(2).all(|w| w[0] < w[1]));
    assert_eq!(store.records_after(seqs[5], 3).unwrap().iter().map(|x| x.0).collect::<Vec<_>>(), seqs[6..9].to_vec());
    // Validation records are never in records_after.
    let v = store.validation_records().unwrap();
    assert_eq!(v.len(), validation);
    let ids: HashSet<&str> = records.iter().map(|r| r.1.match_id.as_str()).collect();
    assert!(v.iter().all(|r| r.has_timeline() && !ids.contains(r.match_id.as_str())));

    // Counts: every champion-role of every counted game, "any" context = games.
    let patch = store.current_patch().unwrap().unwrap();
    assert_eq!(patch, Patch { major: 16, minor: 18 });
    let vols = store.volumes(&[patch]).unwrap();
    assert_eq!(vols.iter().map(|v| v.games).sum::<u32>() as usize, counted * 10);
    let irelia = vols.iter().find(|v| v.champion == 39 && v.role == Role::Middle).unwrap();
    let c = store.counts(&CountQuery { champion: 39, role: Role::Middle, patches: vec![patch], kinds: vec![OptionKind::Keystone] }).unwrap();
    let any: Vec<_> = c.iter().filter(|x| x.context.lane_opponent.is_none() && x.context.enemy_profile.is_none()).collect();
    assert_eq!(any.iter().map(|x| x.games).sum::<u32>(), irelia.games, "one keystone per game");
    assert_eq!(any.iter().map(|x| x.wins).sum::<u32>(), irelia.wins);
    let by_profile: u32 = c.iter().filter(|x| x.context.enemy_profile.is_some()).map(|x| x.games).sum();
    let by_opponent: u32 = c.iter().filter(|x| x.context.lane_opponent.is_some()).map(|x| x.games).sum();
    assert_eq!((by_profile, by_opponent), (irelia.games, irelia.games));
    // Item options only from timeline games; every kind shows up for a popular pair.
    let all = store.counts(&CountQuery { champion: 39, role: Role::Middle, patches: vec![], kinds: vec![] }).unwrap();
    let kinds: HashSet<OptionKind> = all.iter().map(|x| x.option.kind).collect();
    assert_eq!(kinds.len(), OptionKind::ALL.len(), "{kinds:?}");
    let first: u32 = all.iter().filter(|x| x.option.kind == OptionKind::FirstItem && x.context == Default::default()).map(|x| x.games).sum();
    assert!(first > 0 && first < irelia.games);
    // Baseline: the champion-role's own rate, so expected wins track wins over all options.
    let exp: f64 = any.iter().map(|x| x.expected_wins).sum();
    assert!((exp - irelia.wins as f64).abs() < 0.25 * irelia.games as f64, "{exp} vs {}", irelia.wins);
}

#[test]
fn the_same_match_from_two_players_or_regions_counts_once() {
    let dir = tempfile::tempdir().unwrap();
    let store = open(dir.path());
    let m = make_match(&MatchSpec::id("EUW1_42"));
    let tl = make_timeline(&m, &TimelineSpec::default());
    // In one batch: from a Master player, then from a Diamond one.
    let out = store
        .ingest(&[Ingest { m: &m, timeline: None, rank: RankBand::MasterPlus, me: None }, Ingest { m: &m, timeline: Some(&tl), rank: RankBand::Diamond, me: None }])
        .unwrap();
    assert!(matches!(out[0], Outcome::Counted { .. }));
    assert_eq!(out[1], Outcome::Duplicate);
    // Later, found again through the user's history on another platform's list.
    let out = store.ingest(&[Ingest { m: &m, timeline: Some(&tl), rank: RankBand::MasterPlus, me: Some("EUW1_42-p1") }]).unwrap();
    assert_eq!(out, vec![Outcome::Duplicate]);
    assert!(store.is_seen("EUW1_42").unwrap());
    let vols = store.volumes(&[]).unwrap();
    assert_eq!(vols.iter().map(|v| v.games).sum::<u32>(), 10);
    assert_eq!(store.drops(), vec![("duplicate".to_string(), 2)]);
}

#[test]
fn every_filter_counts_its_drops_and_the_counts_survive_a_restart() {
    let dir = tempfile::tempdir().unwrap();
    let s = Synth::new(SynthOptions { noise_share: 0.4, timeline_share: 0.3, ..Default::default() }, load_test_static_data());
    let mut expected = std::collections::BTreeMap::<String, u64>::new();
    for i in 0..300 {
        if let Some(d) = s.game(i).defect {
            *expected.entry(d.to_string()).or_default() += 1;
        }
    }
    {
        let store = open(dir.path());
        let out = ingest_synth(&store, &s, 0..300);
        let dropped = out.iter().filter(|o| matches!(o, Outcome::Dropped(_))).count() as u64;
        assert_eq!(dropped, expected.values().sum::<u64>());
        let got: std::collections::BTreeMap<String, u64> = store.drops().into_iter().collect();
        assert_eq!(got, expected);
        assert!(store.log_lines().iter().any(|l| l.contains("dropped leaver")));
        // Dropped ids are remembered: not fetched or counted again.
        let again = ingest_synth(&store, &s, 0..300);
        assert!(again.iter().all(|o| *o == Outcome::Duplicate));
    }
    let store = open(dir.path());
    let got: std::collections::BTreeMap<String, u64> = store.drops().into_iter().collect();
    assert_eq!(got.get("duplicate"), Some(&300));
    assert_eq!(got.get("remake"), expected.get("remake"));
    assert!(Drop::ALL.iter().all(|d| Drop::parse(d.as_str()) == Some(*d)));
}

#[test]
fn users_games_are_mine_and_kept_and_model_baselines_plug_in() {
    struct Half;
    impl Baseline for Half {
        fn win_chance(&self, _: &MatchRecord, _: usize) -> Option<f64> {
            Some(0.5)
        }
    }
    let dir = tempfile::tempdir().unwrap();
    let store = open(dir.path());
    store.set_baseline(Some(Arc::new(Half)));
    let m = make_match(&MatchSpec::id("EUN1_7"));
    store.ingest(&[Ingest { m: &m, timeline: None, rank: RankBand::MasterPlus, me: Some("EUN1_7-p1") }]).unwrap();
    let recs = store.records_after(0, 10).unwrap();
    assert_eq!(recs.len(), 1, "the user's games are kept even without a timeline");
    assert_eq!(recs[0].1.rank, Some(RankBand::Mine));
    assert!(recs[0].1.players[0].is_me);
    let c = store.counts(&CountQuery { champion: 39, role: Role::Top, patches: vec![], kinds: vec![OptionKind::Summoners] }).unwrap();
    assert_eq!(c.len(), 3, "any, matchup, profile");
    assert!(c.iter().all(|x| x.rank == RankBand::Mine && x.games == 1 && x.expected_wins == 0.5));
    assert_eq!(c.iter().find(|x| x.context.lane_opponent.is_some()).unwrap().context.lane_opponent, Some(122));
    assert_eq!(store.my_games().unwrap(), vec![(39, Role::Top, Patch { major: 16, minor: 19 }, recs[0].1.start_ms)]);
}

#[test]
fn records_read_back_after_reopen_with_the_dictionary() {
    let dir = tempfile::tempdir().unwrap();
    let s = Synth::new(SynthOptions { timeline_share: 1.0, ..Default::default() }, load_test_static_data());
    let first;
    {
        let store = open(dir.path());
        ingest_synth(&store, &s, 0..10);
        ingest_synth(&store, &s, 10..60);
        first = store.records_after(0, 1000).unwrap();
        assert!(first.len() > 40);
    }
    let store = open(dir.path());
    assert_eq!(store.records_after(0, 1000).unwrap(), first);
}

#[test]
fn cleanup_keeps_counts_validation_and_the_window_and_honours_the_budget() {
    let dir = tempfile::tempdir().unwrap();
    let store = open(dir.path());
    let versions: Vec<String> = ["16.16.1.1", "16.17.1.1", "16.18.1.1", "16.19.1.1"].iter().map(|s| s.to_string()).collect();
    let s = Synth::new(SynthOptions { timeline_share: 0.5, versions, games_per_version: 150, ..Default::default() }, load_test_static_data());
    for k in 0..6 {
        ingest_synth(&store, &s, k * 100..(k + 1) * 100);
    }
    let p = |minor| Patch { major: 16, minor };
    let validation = store.validation_records().unwrap();
    assert!(!validation.is_empty());
    let counts_before = store.counts(&CountQuery { champion: 39, role: Role::Top, patches: vec![], kinds: vec![] }).unwrap().len()
        + store.volumes(&[]).unwrap().len();
    let other = OtherParts::standard(dir.path(), None);
    let st = store.storage_status(u64::MAX, &other).unwrap();
    assert_eq!(st.patches, vec![p(19), p(18), p(17), p(16)]);
    assert!(st.parts.iter().any(|(n, b)| n == "records" && *b > 0));
    assert_eq!(st.used_bytes, st.parts.iter().map(|x| x.1).sum::<u64>());

    // The learner trains on 16.18-16.19: older records go, counts and validation stay.
    let rep = store.cleanup(&LearnWindow { patches: vec![p(19), p(18)] }, u64::MAX, &other).unwrap();
    assert!(rep.records_deleted > 0);
    assert!(rep.freed_bytes > 0, "{rep:?}");
    assert!(!rep.over_budget);
    let left = store.records_after(0, 100_000).unwrap();
    assert!(left.iter().all(|r| r.1.version.unwrap().patch >= p(18)));
    assert_eq!(store.validation_records().unwrap(), validation);
    let counts_after = store.counts(&CountQuery { champion: 39, role: Role::Top, patches: vec![], kinds: vec![] }).unwrap().len() + store.volumes(&[]).unwrap().len();
    assert_eq!(counts_after, counts_before);
    assert_eq!(store.storage_status(u64::MAX, &other).unwrap().last_cleanup_freed_bytes, Some(rep.freed_bytes));

    // A budget below what's there: oldest patches go until under it; the newest patch always stays.
    let used = store.storage_status(u64::MAX, &other).unwrap().used_bytes;
    let rep = store.cleanup(&LearnWindow::default(), used * 3 / 4, &other).unwrap();
    assert!(!rep.patches_trimmed.is_empty());
    assert_eq!(rep.patches_trimmed[0], p(16), "oldest first");
    assert!(rep.after_bytes <= used * 3 / 4 || rep.over_budget);
    assert_eq!(rep.after_bytes, store.storage_status(u64::MAX, &other).unwrap().used_bytes);
    assert_eq!(store.validation_records().unwrap(), validation, "validation survives trimming");
    let rep = store.cleanup(&LearnWindow::default(), 1, &other).unwrap();
    assert!(rep.over_budget);
    assert_eq!(store.current_patch().unwrap(), Some(p(19)));
    assert_eq!(store.storage_status(u64::MAX, &other).unwrap().patches, vec![p(19)]);
}
