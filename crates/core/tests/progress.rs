//! Port of server/progress.test.ts: the time left from the last run and the action's own pace, the evaluation's steps
//! weighed by what they took, and the timings kept next to the settings.

use irelia::progress::*;
use serde_json::json;

fn t(size: f64, ms: f64, at: f64) -> Timing {
    Timing { size, ms, at, ..Default::default() }
}

#[test]
fn progress_suite() {
    let dir = tempfile::tempdir().unwrap();
    std::env::set_var("IRELIA_CONFIG_DIR", dir.path());

    // the expected time counts down, then the action's own pace takes over as it goes
    let t0 = 1_000_000;
    let p = Progress::starting_at(Some(60_000.0), t0);
    assert_eq!(p.info_at(Some(0.0), t0), json!({ "fraction": 0, "etaMs": 60_000, "startedAt": t0, "at": t0 }));
    // Too early for its pace: the expected time minus what has passed.
    assert_eq!(p.info_at(Some(0.005), t0 + 2000)["etaMs"], json!(58_000));
    // A quarter done after 10 s: its pace says 30 s more, the expected time 50 s; halfway between at a quarter.
    assert_eq!(p.info_at(Some(0.25), t0 + 10_000)["etaMs"], json!(40_000));
    // From halfway, its pace alone.
    assert_eq!(p.info_at(Some(0.5), t0 + 20_000)["etaMs"], json!(20_000));
    // Past the expected time with nothing counted: nothing left by it.
    assert_eq!(Progress::starting_at(Some(5000.0), t0).info_at(None, t0 + 8000)["etaMs"], json!(0));
    // Nothing to go on: no estimate until its pace says something.
    let fresh = Progress::starting_at(None, t0);
    assert_eq!(fresh.info_at(None, t0 + 5000)["etaMs"], json!(null));
    assert_eq!(fresh.info_at(Some(0.1), t0 + 5000)["etaMs"], json!(45_000));
    assert_eq!(fresh.info_at(Some(1.5), t0 + 5000)["fraction"], json!(1), "held to 0-1");

    // steps weigh what they took last time, else the same
    assert_eq!(step_fraction(3, 4, 0.5, None), 2.5 / 4.0);
    assert_eq!(step_fraction(3, 4, 0.5, Some(&[10.0, 5.0, 70.0, 15.0])), 0.5);
    assert_eq!(step_fraction(3, 4, 0.5, Some(&[10.0, 5.0, 70.0])), 2.5 / 4.0, "weights for another number of steps");
    assert_eq!(step_fraction(1, 4, 0.0, Some(&[10.0, 5.0, 70.0, 15.0])), 0.0);
    assert_eq!(step_fraction(4, 4, 1.0, Some(&[10.0, 5.0, 70.0, 15.0])), 1.0);

    // an action's time is kept, the newest first, and scales the next estimate to its size
    assert!(expected_ms("evaluate", 54_000.0, None).unwrap() > 60_000.0);
    assert_eq!(expected_ms("audit", 1000.0, None), None);
    record_timing("audit", t(1000.0, 30_000.0, 1.0));
    record_timing("audit", Timing { steps: Some(vec![1.0, 2.0, 3.0]), ..t(2000.0, 50_000.0, 2.0) });
    assert_eq!(last_timing("audit"), Some(Timing { steps: Some(vec![1.0, 2.0, 3.0]), ..t(2000.0, 50_000.0, 2.0) }));
    assert_eq!(expected_ms("audit", 3000.0, None), Some(75_000.0));
    record_timing("evaluate", t(10_000.0, 60_000.0, 3.0));
    assert!(expected_ms("evaluate", 20_000.0, None).unwrap() > 120_000.0);
    // On more threads (0.11.0): the share that runs on one stays, the rest is shared out.
    assert_eq!(
        expected_ms("evaluate", 10_000.0, Some(4.0)).unwrap().round(),
        (60_000.0 * (EVAL_SERIAL_SHARE + (1.0 - EVAL_SERIAL_SHARE) / 4.0)).round()
    );
    assert_eq!(expected_ms("evaluate", 10_000.0, Some(1.0)), Some(60_000.0));
    // A run on four threads says what one would take, and what eight would.
    record_timing("evaluate", Timing { threads: Some(4.0), bytes_per_game: Some(22_000.0), ..t(10_000.0, 30_000.0, 4.0) });
    let one = 30_000.0 / (EVAL_SERIAL_SHARE + (1.0 - EVAL_SERIAL_SHARE) / 4.0);
    assert_eq!(expected_ms("evaluate", 10_000.0, Some(1.0)).unwrap().round(), one.round());
    assert!(expected_ms("evaluate", 10_000.0, Some(8.0)).unwrap() < 30_000.0);
    // That measure was taken before the threads' layout had a number (layout 1).
    assert_eq!(thread_measure(1.0), Some(22_000.0));
    assert_eq!(thread_measure(2.0), None, "another layout's measure says nothing about this one");
    record_timing("evaluate", Timing { threads: Some(4.0), bytes_per_game: Some(10_000.0), layout: Some(2.0), ..t(10_000.0, 25_000.0, 5.0) });
    assert_eq!(thread_measure(2.0), Some(10_000.0));
    // A run on one thread measures nothing: the last measure stays.
    record_timing("evaluate", Timing { threads: Some(1.0), ..t(10_000.0, 80_000.0, 6.0) });
    assert_eq!(thread_measure(2.0), Some(10_000.0));
    // Other actions don't scale with threads.
    assert_eq!(expected_ms("audit", 2000.0, Some(8.0)), Some(50_000.0));
    // A run with nothing done or no time says nothing.
    record_timing("audit", t(0.0, 10.0, 4.0));
    assert_eq!(last_timing("audit").unwrap().at, 2.0);
    // Runs kept: the newest five.
    for i in 0..7 {
        record_timing("backfill", t(100.0, 1000.0 + i as f64, 10.0 + i as f64));
    }
    let kept: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(dir.path().join("timings.json")).unwrap()).unwrap();
    let ats: Vec<f64> = kept["backfill"].as_array().unwrap().iter().map(|t| t["at"].as_f64().unwrap()).collect();
    assert_eq!(ats, [16.0, 15.0, 14.0, 13.0, 12.0]);
}
