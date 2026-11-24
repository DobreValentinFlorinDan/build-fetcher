//! Port of server/model/memory.test.ts: patch memory (model/weights). Older patches count, fading per patch, less when
//! Irelia or the item changed since (Data Dragon fingerprints), not at all past the window; off, only today's and the
//! previous patch count, as before. The data check lets today's games overrule older ones that clearly disagree.

use irelia::ddragon::{Fingerprints, StaticData};
use irelia::model::dataset::PreparedGame;
use irelia::model::estimate::{Observation, Z};
use irelia::model::weights::{consistent, patch_age, patch_order, weight_defaults, Memory, PatchAger, PatchWeigher, RegionPatchFn, WeightOptions, MEMORY_PATCHES, PREV_CHANGED, PREV_UNCHANGED, PREV_UNKNOWN};
use irelia::testkit::load_test_static_data;
use irelia::testkit_model::blank_game;
use std::collections::HashMap;
use std::sync::Arc;

const VERSIONS: [&str; 8] = ["16.19.1", "16.18.1", "16.17.1", "16.16.1", "16.15.1", "16.14.1", "16.13.1", "16.12.1"];

fn pg(patch: &str) -> PreparedGame {
    PreparedGame { patch: patch.into(), ..blank_game() }
}

fn versions() -> Option<Vec<String>> {
    Some(VERSIONS.iter().map(|v| v.to_string()).collect())
}

/// Data Dragon as of 16.19 with the fingerprints of the patches before it; `edit` changes one patch's.
fn with_history(edit: impl Fn(&str, &mut Fingerprints)) -> StaticData {
    let data = load_test_static_data();
    let mut history: HashMap<String, Fingerprints> = HashMap::new();
    for v in &VERSIONS[1..] {
        let p = v.split('.').take(2).collect::<Vec<_>>().join(".");
        let mut f = data.fingerprints.clone();
        edit(&p, &mut f);
        history.insert(p, f);
    }
    let mut d = (*data).clone();
    d.versions = versions();
    d.previous = Some(("16.18.1".into(), history["16.18"].clone()));
    d.history = Some(history);
    d
}

fn none() -> RegionPatchFn {
    Arc::new(|_| None)
}

fn weigher(d: StaticData, o: WeightOptions) -> PatchWeigher {
    PatchWeigher::new(Arc::new(d), none(), &o)
}

#[test]
fn patch_ages_count_patches_back_on_the_version_list_across_seasons_too() {
    let mut season = (*load_test_static_data()).clone();
    season.version = "17.1.1".into();
    season.versions = Some(vec!["17.1.1".into(), "16.24.1".into(), "16.23.1".into()]);
    let order = patch_order(&season);
    assert_eq!(order, vec!["17.1", "16.24", "16.23"]);
    assert_eq!(patch_age(&order, "16.23", "17.1"), Some(2.0));
    assert_eq!(patch_age(&order, "17.1", "17.1"), Some(0.0));
    assert_eq!(patch_age(&order, "16.20", "16.22"), Some(2.0), "not on the list: the same season counts by number");
    assert_eq!(patch_age(&order, "15.20", "16.22"), None);
    let mut d = (*load_test_static_data()).clone();
    d.versions = versions();
    let ager = PatchAger::new(Arc::new(d), none());
    let ages: Vec<f64> = ["16.19", "16.18", "16.16", "15.1"].iter().map(|p| ager.age("KR", p)).collect();
    assert_eq!(ages, vec![0.0, 1.0, 3.0, f64::INFINITY]);
}

#[test]
fn without_patch_memory_todays_and_the_previous_patch_only_exactly_as_before() {
    let w = weigher(with_history(|_, _| {}), weight_defaults());
    assert_eq!(w.weigh(&pg("16.19"), &[]), 1.0);
    assert_eq!(w.weigh(&pg("16.18"), &[3153]), PREV_UNCHANGED);
    assert_eq!(w.weigh(&pg("16.17"), &[3153]), 0.0);
    assert_eq!(w.weigh(&pg("16.14"), &[]), 0.0);
}

#[test]
fn with_patch_memory_older_patches_fade_per_patch_count_less_once_something_involved_changed_and_stop_at_the_window() {
    let memory = || WeightOptions { memory: Some(Memory { decay: 0.75, patches: MEMORY_PATCHES }), ..weight_defaults() };
    // Trinity Force (3078) was changed in 16.17: games before it played a different item.
    let changed = || {
        with_history(|patch, f| {
            if patch == "16.16" || patch == "16.15" {
                f.items.insert(3078, "before the change".into());
            }
        })
    };
    let w = weigher(changed(), memory());
    assert_eq!(w.weigh(&pg("16.19"), &[]), 1.0);
    assert_eq!(w.weigh(&pg("16.18"), &[3153]), PREV_UNCHANGED, "the previous patch as without memory");
    assert_eq!(w.weigh(&pg("16.17"), &[3153]), 0.75 * PREV_UNCHANGED);
    assert_eq!(w.weigh(&pg("16.16"), &[3153]), 0.75f64.powi(2) * PREV_UNCHANGED, "BotRK did not change");
    assert_eq!(w.weigh(&pg("16.16"), &[3078]), 0.75f64.powi(2) * PREV_CHANGED, "Trinity Force did");
    assert_eq!(w.weigh(&pg("16.14"), &[3153]), 0.75f64.powi(4) * PREV_UNCHANGED, "the oldest patch in the window");
    assert_eq!(w.weigh(&pg("16.13"), &[3153]), 0.0, "outside the window");

    // An Irelia change demotes every older game; a patch without Data Dragon files is unknown.
    let irelia_changed = weigher(
        with_history(|patch, f| {
            if patch == "16.16" {
                f.champion = "old kit".into();
            }
        }),
        memory(),
    );
    assert_eq!(irelia_changed.weigh(&pg("16.16"), &[3153]), 0.75f64.powi(2) * PREV_CHANGED);
    assert_eq!(irelia_changed.weigh(&pg("16.17"), &[3153]), 0.75 * PREV_UNCHANGED);
    let mut missing = with_history(|_, _| {});
    missing.history.as_mut().unwrap().remove("16.16");
    assert_eq!(weigher(missing, memory()).weigh(&pg("16.16"), &[3153]), 0.75f64.powi(2) * PREV_UNKNOWN);
    // A hand-entered patch weight still wins, and fades with age too.
    let manual = weigher(changed(), WeightOptions { patch_weights: vec![("16.16".into(), 0.5)], ..memory() });
    assert_eq!(manual.weigh(&pg("16.16"), &[3078]), 0.5 * 0.75f64.powi(2));
}

#[test]
fn the_data_check_older_games_of_an_option_todays_games_clearly_contradict_count_less_agreeing_ones_keep_their_weight() {
    let mut obs: Vec<Observation> = vec![];
    let mut add = |option: &str, age: f64, n: usize, wins: usize| {
        for i in 0..n {
            let win = if i < wins { 1.0 } else { 0.0 };
            obs.push(Observation { option: option.into(), age: Some(age), wpa: win - 0.5, win, start_win: f64::NAN, w: 1.0, lane: "TOP", opponent: None, pilot: format!("p{i}").into(), z: Z::None, row: None });
        }
    };
    add("3078", 0.0, 120, 40); // nerfed (hidden): 33 % today...
    add("3078", 2.0, 400, 240); // ...60 % on older patches
    add("3153", 0.0, 120, 66); // 55 % today, 56 % before: agrees
    add("3153", 2.0, 400, 224);
    add("6333", 0.0, 10, 2); // too few games today to judge
    add("6333", 2.0, 400, 280);
    let (out, _) = consistent(obs.clone(), 30.0);
    let weight = |option: &str, age: f64| out.iter().find(|o| &*o.option == option && o.age == Some(age)).unwrap().w;
    assert!(weight("3078", 2.0) < 0.2, "contradicted: {}", weight("3078", 2.0));
    assert_eq!(weight("3078", 0.0), 1.0, "today's games keep their weight");
    assert_eq!(weight("3153", 2.0), 1.0);
    assert_eq!(weight("6333", 2.0), 1.0);
    let agreeing: Vec<Observation> = obs.into_iter().filter(|o| &*o.option == "3153").collect();
    let (back, changed) = consistent(agreeing.clone(), 30.0);
    assert!(!changed, "nothing contradicted: the same observations back");
    assert!(back.iter().zip(&agreeing).all(|(a, b)| a.w == b.w && a.option == b.option && a.age == b.age) && back.len() == agreeing.len());
}
