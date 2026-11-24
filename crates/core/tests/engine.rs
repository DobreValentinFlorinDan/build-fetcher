//! Port of server/engine/engine.test.ts.
use irelia::db::IreliaGameRow;
use irelia::engine::exclusivity::*;
use irelia::engine::recommend::*;
use irelia::testkit::load_test_static_data;
use irelia::types::BuildRecommendation;
use std::sync::Arc;

fn settings() -> EngineSettings {
    EngineSettings { min_matchup_games: 15.0, general_prior_weight: 0.15, min_lane_games: 30.0, other_lane_weight: 0.15, previous_patch_weight: 0.35, min_current_patch_games: 40.0, starting_item_window_ms: 60_000.0 }
}

#[derive(Default, Clone)]
struct Over {
    items: Option<Vec<i64>>,
    boots: Option<i64>,
    patch: Option<String>,
    position: Option<&'static str>,
    opponent: Option<i64>,
    summoner2: Option<i64>,
    perks: Option<&'static str>,
}

fn game(i: i64, o: Over) -> IreliaGameRow {
    let data = load_test_static_data();
    let items = o.items.unwrap_or(vec![3153, 3078, 3053, 6333]);
    let boots = o.boots.unwrap_or(3047);
    let mut purchases: Vec<(i64, i64)> = vec![(1055, 1000), (2003, 1000), (boots, 400_000)];
    purchases.extend(items.iter().enumerate().map(|(k, id)| (*id, 600_000 + k as i64 * 300_000)));
    IreliaGameRow {
        match_id: format!("T_{i}"), puuid: "p".into(), patch: o.patch.unwrap_or(data.patch.clone()), win: if i % 3 == 0 { 0 } else { 1 },
        position: o.position.unwrap_or("TOP").into(), opponent_champion_id: Some(o.opponent.unwrap_or(122)),
        summoner1: 4, summoner2: o.summoner2.unwrap_or(12), primary_style: 8000, sub_style: 8400,
        perks: o.perks.unwrap_or("[8010,9111,9104,8299,8444,8242]").into(), stat_perks: "[5005,5008,5011]".into(),
        skill_order: "QEWQQRQEQERQEEWW".into(),
        purchases: format!("[{}]", purchases.iter().map(|(a, b)| format!("[{a},{b}]")).collect::<Vec<_>>().join(",")),
        final_items: "[]".into(), kills: 5, deaths: 3, assists: 4, game_duration: 1800, created_at: 0, ..Default::default()
    }
}

fn rec(rows: &[IreliaGameRow], enemy: Option<i64>, lane: Option<&str>) -> BuildRecommendation {
    let data = load_test_static_data();
    let excl = build_exclusivity_index(&data);
    let rows: Vec<Arc<IreliaGameRow>> = rows.iter().cloned().map(Arc::new).collect();
    recommend(&rows, &data, &excl, enemy, &settings(), lane, &|_| None, None)
}

const TOP_CORE: [i64; 3] = [3153, 3078, 3053];
const MID_CORE: [i64; 3] = [3153, 6333, 3071];
fn top(i: i64, o: Over) -> IreliaGameRow {
    game(i, Over { position: Some("TOP"), items: Some(o.items.clone().unwrap_or([&TOP_CORE[..], &[6333]].concat())), boots: Some(o.boots.unwrap_or(3047)), summoner2: Some(12), ..o })
}
fn mid(i: i64, o: Over) -> IreliaGameRow {
    game(1000 + i, Over { position: Some("MIDDLE"), items: Some([&MID_CORE[..], &[3053]].concat()), boots: Some(3111), summoner2: Some(14), opponent: Some(o.opponent.unwrap_or(157)), ..o })
}
fn many(n: i64, f: impl Fn(i64) -> IreliaGameRow) -> Vec<IreliaGameRow> {
    (0..n).map(f).collect()
}

#[test]
fn opportunity_is_removed() {
    let data = load_test_static_data();
    assert!(!is_purchasable_item(data.items.get(&6701)));
}

#[test]
fn sunfire_and_hollow_radiance_are_exclusive() {
    let data = load_test_static_data();
    let excl = build_exclusivity_index(&data);
    assert!(excl.blocker(&[3068], 6664).unwrap_or_default().contains("Immolate"));
    assert_eq!(excl.conflicts(&[3068, 6664, 3053]).len(), 1);
    assert!(excl.blocker(&[3053], 3156).unwrap_or_default().contains("Lifeline"));
    assert!(excl.blocker(&[3078], 6662).unwrap_or_default().contains("Spellblade"));
    assert!(excl.blocker(&[3047], 3111).unwrap_or_default().contains("Boots"));
    assert_eq!(excl.blocker(&[3153, 3078], 3071), None);
}

#[test]
fn matchup_recommendation_discards_stale_and_removed() {
    let mut rows = many(30, |i| game(i, Over::default()));
    rows.push(game(100, Over { patch: Some("15.1".into()), ..Default::default() }));
    rows.push(game(101, Over { items: Some(vec![6701, 3078, 3053]), ..Default::default() }));
    let r = rec(&rows, Some(122), None);
    assert_eq!(r.core_build[0].value, vec![3153, 3078, 3053]);
    assert_eq!(r.boots[0].value, 3047);
    assert_eq!(r.validation.stale_discarded, 1.0);
    assert_eq!(r.validation.invalid_discarded, 1.0);
    assert_eq!(r.skills.as_ref().unwrap().value.priority, vec!["Q", "E", "W"]);
    assert_eq!(r.skills.as_ref().unwrap().value.path.len(), 18);
    assert_eq!(r.runes.as_ref().unwrap().value.perks, vec![8010, 9111, 9104, 8299, 8444, 8242]);
}

#[test]
fn situational_never_conflicts_with_core() {
    let rows = many(30, |i| game(i, Over { items: Some(if i % 3 == 0 { vec![3153, 3078, 6333, 3156] } else { vec![3153, 3078, 3053, 6333] }), ..Default::default() }));
    let r = rec(&rows, None, None);
    assert!(r.situational.slot4.iter().all(|o| o.value != 3156));
    assert!(r.validation.conflicts_rejected.iter().any(|c| c.contains("Lifeline")));
}

#[test]
fn lanes_are_separated() {
    let rows = [many(40, |i| top(i, Over::default())), many(40, |i| mid(i, Over::default()))].concat();
    let t = rec(&rows, None, Some("TOP"));
    assert_eq!(t.lane.as_deref(), Some("TOP"));
    assert_eq!(t.core_build.iter().map(|o| o.value.clone()).collect::<Vec<_>>(), vec![TOP_CORE.to_vec()]);
    assert_eq!(t.boots[0].value, 3047);
    assert_eq!(t.summoners.as_ref().unwrap().value, vec![4, 12]);
    assert_eq!((t.sample.lane_games, t.sample.other_lane_games, t.sample.used_lane_fallback), (40.0, 0.0, false));
    let m = rec(&rows, None, Some("MIDDLE"));
    assert_eq!(m.core_build.iter().map(|o| o.value.clone()).collect::<Vec<_>>(), vec![MID_CORE.to_vec()]);
    assert_eq!(m.boots[0].value, 3111);
    assert_eq!(m.summoners.as_ref().unwrap().value, vec![4, 14]);
    assert_eq!(m.sample.total_games, 80.0);
}

#[test]
fn no_lane_mixes_every_position() {
    let rows = [many(40, |i| top(i, Over::default())), many(20, |i| mid(i, Over::default()))].concat();
    let l = rec(&rows, None, None);
    assert_eq!(l.lane, None);
    assert_eq!((l.sample.lane_games, l.sample.total_games, l.sample.other_lane_games), (60.0, 60.0, 0.0));
    assert!(!l.sample.used_lane_fallback);
    assert_eq!(l.core_build.iter().find(|o| o.value == MID_CORE.to_vec()).unwrap().games, 20.0);
}

#[test]
fn matchups_only_count_same_lane() {
    let rows = [many(40, |i| top(i, Over::default())), many(5, |i| top(100 + i, Over { opponent: Some(157), ..Default::default() })), many(40, |i| mid(i, Over::default()))].concat();
    let t = rec(&rows, Some(157), Some("TOP"));
    assert_eq!(t.sample.matchup_games, 5.0);
    assert!(t.sample.used_fallback);
    let m = rec(&rows, Some(157), Some("MIDDLE"));
    assert_eq!(m.sample.matchup_games, 40.0);
    assert!(!m.sample.used_fallback);
}

#[test]
fn thin_lane_blends_other_lane() {
    let rows = [many(60, |i| top(i, Over::default())), many(8, |i| mid(i, Over::default()))].concat();
    let m = rec(&rows, None, Some("MIDDLE"));
    assert!(m.sample.used_lane_fallback);
    assert_eq!((m.sample.lane_games, m.sample.other_lane_games), (8.0, 60.0));
    let opt = |c: &[i64]| m.core_build.iter().find(|o| o.value == c.to_vec()).cloned();
    assert_eq!(opt(&MID_CORE).unwrap().games, 8.0);
    assert_eq!((opt(&TOP_CORE).unwrap().games, opt(&TOP_CORE).unwrap().raw_games), (60.0 * 0.15, 60.0));
    assert_eq!(m.win_rate, Some(6.0 / 8.0));
    let enough = rec(&[many(60, |i| top(i, Over::default())), many(30, |i| mid(i, Over::default()))].concat(), None, Some("MIDDLE"));
    assert_eq!((enough.sample.used_lane_fallback, enough.sample.other_lane_games), (false, 0.0));
    assert_eq!(enough.core_build.iter().map(|o| o.value.clone()).collect::<Vec<_>>(), vec![MID_CORE.to_vec()]);
}

#[test]
fn empty_lane_from_other_lane() {
    let m = rec(&many(40, |i| top(i, Over::default())), Some(122), Some("MIDDLE"));
    assert_eq!(m.core_build[0].value, TOP_CORE.to_vec());
    assert_eq!((m.sample.lane_games, m.sample.matchup_games, m.sample.other_lane_games), (0.0, 0.0, 40.0));
    assert_eq!(m.win_rate, None);
    assert!(m.sample.used_fallback);
}

#[test]
fn jungle_bot_support_unknown_never_feed() {
    let odd = vec![3153, 3071, 3742];
    let mut odd4 = odd.clone();
    odd4.push(6333);
    let rows = [
        many(10, |i| top(i, Over::default())),
        many(30, |i| game(500 + i, Over { position: Some("JUNGLE"), items: Some(odd4.clone()), ..Default::default() })),
        many(30, |i| game(600 + i, Over { position: Some("UNKNOWN"), items: Some(odd4.clone()), ..Default::default() })),
    ].concat();
    let t = rec(&rows, None, Some("TOP"));
    assert!(t.sample.used_lane_fallback);
    assert_eq!(t.sample.other_lane_games, 0.0);
    assert!(t.core_build.iter().all(|o| o.value != odd));
}

#[test]
fn validation_report_covers_usable_games() {
    let broken = Over { perks: Some("[]"), ..Default::default() };
    let rows = [
        many(40, |i| top(i, Over::default())), vec![top(40, broken.clone()), top(41, Over { patch: Some("1.1".into()), ..Default::default() })],
        many(8, |i| mid(i, Over::default())), vec![mid(8, broken.clone())],
        many(5, |i| game(500 + i, Over { position: Some("JUNGLE"), ..Default::default() })), vec![game(505, Over { position: Some("JUNGLE"), ..broken.clone() })],
    ].concat();
    let report = |r: &BuildRecommendation| (r.validation.fresh_games, r.validation.stale_discarded, r.validation.invalid_discarded, r.validation.invalid_reasons.get("rune no longer exists").copied());
    let t = rec(&rows, None, Some("TOP"));
    assert_eq!(report(&t), (40.0, 1.0, 1.0, Some(1.0)));
    assert_eq!(t.sample.total_games, 53.0);
    let m = rec(&rows, None, Some("MIDDLE"));
    assert!(m.sample.used_lane_fallback);
    assert_eq!(report(&m), (48.0, 1.0, 2.0, Some(2.0)));
    assert_eq!(report(&rec(&rows, None, None)), (53.0, 1.0, 3.0, Some(3.0)));
}

#[test]
fn previous_patch_admission_counts_requested_lane() {
    let data = load_test_static_data();
    assert_ne!(data.previous_patch, data.patch);
    let prev = data.previous_patch.clone();
    let rows = [many(50, |i| top(i, Over::default())), many(10, |i| mid(i, Over::default())), many(20, |i| mid(200 + i, Over { patch: Some(prev.clone()), ..Default::default() }))].concat();
    let t = rec(&rows, None, Some("TOP"));
    assert_eq!(t.sample.patches, vec![data.patch.clone()]);
    let m = rec(&rows, None, Some("MIDDLE"));
    assert_eq!(m.sample.patches, vec![data.patch.clone(), data.previous_patch.clone()]);
    assert_eq!(m.sample.lane_games, 30.0);
    assert_eq!(m.validation.previous_patch_games, 20.0);
}

#[test]
fn boots_go_where_games_finish_them() {
    assert_eq!(rec(&many(40, |i| top(i, Over::default())), None, Some("TOP")).boots_after, Some(Some(0.0)));
    let late = |i: i64| {
        let mut g = top(i, Over::default());
        let p = irelia::engine::validator::parse_purchases(&g.purchases);
        g.purchases = format!("[{}]", p.iter().map(|(id, t)| format!("[{id},{}]", if *id == 3047 { 1_200_000.0 } else { *t })).collect::<Vec<_>>().join(","));
        g
    };
    let r = rec(&[many(30, late), many(10, |i| top(100 + i, Over::default()))].concat(), None, Some("TOP"));
    assert_eq!(r.boots_after, Some(Some(2.0)));
}
