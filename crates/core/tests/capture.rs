//! Port of server/capture.test.ts.
use irelia::capture::*;
use irelia::testkit::*;
use serde_json::json;

#[test]
fn every_participant_becomes_a_row() {
    let rows = participant_rows(&make_match(&MatchSpec::id("KR_1")));
    assert_eq!(rows.len(), 10);
    let syndra = rows.iter().find(|r| r.champion_id == 134).unwrap();
    assert_eq!((syndra.participant_id, syndra.team_id, syndra.position.as_str()), (8, 200, "MIDDLE"));
    assert_eq!((syndra.dmg_physical, syndra.dmg_magic, syndra.dmg_true), (1000, 25_000, 1500));
    assert_eq!(syndra.cs, 210);
    assert_eq!(syndra.items, "[3153,3078,3047,0,0,0,3340]");
    assert_eq!(rows.iter().find(|r| r.champion_id == 89).unwrap().heal_allies, 4000);
    let ch: serde_json::Value = serde_json::from_str(rows[0].challenges.as_ref().unwrap()).unwrap();
    assert_eq!(ch["damagePerMinute"], 600);
    assert!(ch.get("unrelatedCosmetic").is_none());
}

#[test]
fn remakes() {
    assert!(!match_quality(&make_match(&MatchSpec::id("KR_1"))).remake);
    assert!(match_quality(&make_match(&MatchSpec { remake_vote: Some(true), minutes: Some(4), ..MatchSpec::id("KR_2") })).remake);
    let mut short = make_match(&MatchSpec::id("KR_3"));
    short["info"]["gameDuration"] = json!(REMAKE_MAX_SECONDS - 1.0);
    assert!(match_quality(&short).remake);
    assert_eq!(match_quality(&make_match(&MatchSpec::id("KR_4"))).end_result.as_deref(), Some("GameComplete"));
}

#[test]
fn leavers_xp_and_gold_stop() {
    let m = make_match(&MatchSpec { minutes: Some(30), ..MatchSpec::id("KR_1") });
    assert!(leavers(&capture_timeline(&make_timeline(&m, &TimelineSpec::default()))).is_empty());
    assert_eq!(leavers(&capture_timeline(&make_timeline(&m, &TimelineSpec { leaver: Some((4, 18)), ..Default::default() }))), vec![4]);
    assert!(leavers(&capture_timeline(&make_timeline(&m, &TimelineSpec { leaver: Some((4, 28)), ..Default::default() }))).is_empty());
}

#[test]
fn leavers_at_level_18() {
    let frames = |idle_from: i64, minutes: i64| -> Vec<[f64; 5]> {
        (0..=minutes)
            .map(|m| {
                let active = m.min(idle_from);
                [(500 + active * 400 + (m - idle_from).max(0) * 122) as f64, 0.0, 18_360.0, 18.0, (300 + active * 6) as f64]
            })
            .collect()
    };
    let cap = |idle_from: i64| {
        let minutes = 45;
        let mut c = TimelineCapture { interval: 60_000.0, t: (0..=minutes).map(|m| (m * 60_000) as f64).collect(), ..Default::default() };
        c.frames.insert(1, frames(idle_from, minutes));
        c
    };
    assert!(leavers(&cap(41)).is_empty());
    assert_eq!(leavers(&cap(38)), vec![1]);
    let mut farming = cap(41);
    let f: Vec<[f64; 5]> = farming.frames[&1].iter().enumerate().map(|(m, r)| [r[0], r[1], 9000.0, 11.0, r[4] + m as f64]).collect();
    farming.frames.insert(1, f);
    assert!(leavers(&farming).is_empty());
}

#[test]
fn lane_opponent_from_positions() {
    let m = make_match(&MatchSpec::id("KR_1"));
    let irelia = &m["info"]["participants"][0];
    let normal = lane_opponents(&m, Some(&capture_timeline(&make_timeline(&m, &TimelineSpec::default()))), irelia);
    assert_eq!(normal, LaneOpponents { position: Some(122), timeline: Some(122), effective: Some(122) });
    let swapped = make_timeline(&m, &TimelineSpec { spots: vec![(6, (7400.0, 7400.0)), (8, (1500.0, 12500.0))], ..Default::default() });
    assert_eq!(lane_opponents(&m, Some(&capture_timeline(&swapped)), irelia), LaneOpponents { position: Some(122), timeline: Some(134), effective: Some(134) });
    assert_eq!(lane_opponents(&m, None, irelia), LaneOpponents { position: Some(122), timeline: None, effective: Some(122) });
}

#[test]
fn compact_timeline() {
    let m = make_match(&MatchSpec { minutes: Some(25), ..MatchSpec::id("KR_1") });
    let cap = capture_timeline(&make_timeline(&m, &TimelineSpec::default()));
    assert_eq!(cap.t.len(), 26);
    assert_eq!(cap.frames.len(), 10);
    assert_eq!(cap.frames[&1][10], [(500 + 10 * 380) as f64, 200.0, 6000.0, 6.0, 70.0]);
    assert_eq!(cap.pos[&1].len(), 16);
    let k = &cap.kills[0];
    assert_eq!((k.t, k.killer, k.victim, k.assists.clone(), k.bounty, k.shutdown), ((6 * 60_000 + 200) as f64, 1.0, 6.0, vec![2.0], 300.0, 0.0));
    let o: Vec<(f64, &str)> = cap.objectives.iter().map(|o| (o.1, o.2.as_str())).collect();
    assert_eq!(o, vec![(200.0, "DRAGON"), (100.0, "PLATE"), (100.0, "TOWER")]);
    let json = cap.to_json_string();
    assert!(json.len() < 40_000);
    assert_eq!(TimelineCapture::parse(&json).unwrap(), cap, "round-trips through its JSON");
    assert!(json.starts_with(r#"{"v":1,"interval":60000,"t":[0,60000"#));
}

#[test]
fn inventory_replay() {
    let m = make_match(&MatchSpec::id("KR_1"));
    let items = vec![
        (3, "ITEM_PURCHASED", 1036, None), (4, "ITEM_PURCHASED", 3057, None), (5, "ITEM_UNDO", 3057, Some(0)),
        (8, "ITEM_DESTROYED", 1036, None), (8, "ITEM_PURCHASED", 3078, None), (10, "ITEM_SOLD", 1055, None), (11, "ITEM_UNDO", 0, Some(1055)),
    ];
    let cap = capture_timeline(&make_timeline(&m, &TimelineSpec { items: items.into_iter().map(|(a, b, c, d)| (a, b.to_string(), c, d)).collect(), ..Default::default() }));
    assert!(cap.items.iter().any(|i| i.2 == UNDO));
    let inv = |t: i64| { let mut v: Vec<i64> = inventory_at(&cap, 1.0, t as f64).iter().map(|x| *x as i64).collect(); v.sort(); v };
    assert_eq!(inv(4 * 60_000 + 600), vec![1036, 1055, 2003, 3057]);
    assert_eq!(inv(5 * 60_000 + 600), vec![1036, 1055, 2003]);
    assert_eq!(inv(9 * 60_000), vec![1055, 2003, 3078]);
    assert_eq!(inv(10 * 60_000 + 600), vec![2003, 3078]);
    assert_eq!(inv(11 * 60_000 + 600), vec![1055, 2003, 3078]);
    assert_eq!(m["info"]["participants"][0]["championId"], IRELIA);
}

/// The direct reader behind `TimelineCapture::parse` and the `serde_json::Value` path give the same capture: for a
/// real capture, and for text that is damaged in every way a field can be.
#[test]
fn parse_reads_as_the_value_path() {
    let m = make_match(&MatchSpec::id("KR_1"));
    let good = capture_timeline(&make_timeline(&m, &TimelineSpec::default())).to_json_string();
    let mut texts = vec![good.clone()];
    // Each field replaced by values of every other kind.
    for key in ["interval", "t", "frames", "pos", "items", "kills", "objectives"] {
        for odd in ["null", "7", "-1.5e3", "\"x\"", "true", "[]", "{}", "[null]", "[[]]", "[[null,\"a\",{},[1]]]", "{\"1\":null}", "{\"x\":[[1,2]],\"2\":[[1],null,7,{}]}", "[1,[2],{\"a\":3}]"] {
            let mut v: serde_json::Value = serde_json::from_str(&good).unwrap();
            v[key] = serde_json::from_str(odd).unwrap();
            texts.push(v.to_string());
        }
    }
    for t in [
        "{}",
        "[]",
        "null",
        "3",
        "\"x\"",
        "",
        "{",
        "{\"t\":[1,2",
        "{\"t\":[1,2]} x",
        "{\"t\":[1],\"t\":[2,3]}",
        "{\"frames\":{\"1\":[[1,2,3,4,5,6,7]],\"1\":[[9]]}}",
        "{\"items\":[[1,2,3,4],[1,2,3,4,5],[1,2,3,4,null],[1,2,3,4,\"x\",9],[1],7,null]}",
        "{\"kills\":[[1,2,3,[4,null,\"x\"],5,6,7],[1,2,3,null,5],[1,2,3],[],{}]}",
        "{\"objectives\":[[1,100,\"DRAGON\",\"FIRE\"],[1,null,3,null],[1,100],\"x\"]}",
        "{\"interval\":1e400,\"t\":[1e-400,18446744073709551616,-9223372036854775809,0.1]}",
        "{\"v\":1,\"unknown\":{\"a\":[1,{\"b\":null}]},\"interval\":60000}",
    ] {
        texts.push(t.to_string());
    }
    for t in &texts {
        assert_eq!(TimelineCapture::parse(t), TimelineCapture::parse_value(t), "{}", &t[..t.len().min(120)]);
    }
    assert!(TimelineCapture::parse(&good).unwrap().frames.len() == 10);
}
