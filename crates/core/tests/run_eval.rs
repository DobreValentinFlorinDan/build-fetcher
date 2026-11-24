//! Port of server/model/runEval.test.ts: evaluating a data folder (eval:builds and the app's Evaluate builds) on a real
//! database. Every report ends with the time each step took; a grouped-drafts trial is saved beside the reports as
//! eval-grouped-… and changes nothing the app reads (latest.json, the quick update's base, the evaluation history).

use irelia::db::Db;
use irelia::model::eval_history::eval_history;
use irelia::model::run_eval::{compare_reports, evaluate_data_folder, ComparedRun, FolderOptions};
use irelia::model::service::eval_dir;
use irelia::testkit::load_test_static_data;
use irelia::testkit_model::{ap_ad_games, store_games, ApAd};

fn options(grouping: Option<usize>) -> FolderOptions<'static> {
    FolderOptions {
        quick: true,
        auto: false,
        path: "full",
        threads: 1,
        log: Box::new(|_| {}),
        progress: Box::new(|_| {}),
        save: true,
        control: None,
        grouping,
        gpu: false,
    }
}

#[test]
fn a_grouped_trial_is_saved_beside_the_reports_and_changes_nothing_the_app_reads() {
    let root = tempfile::tempdir().unwrap();
    std::env::set_var("IRELIA_CONFIG_DIR", root.path().join("config"));
    std::env::set_var("IRELIA_DATA_DIR", root.path().join("data"));
    std::fs::create_dir_all(root.path().join("data")).unwrap();
    let data = load_test_static_data();
    let db = Db::open_default(&root.path().join("data").join("irelia.db")).unwrap();
    store_games(&db, &ap_ad_games(400, 40, 9, ApAd { no_effect: false, random_opponent: true }), Some(&data.patch));

    let exact = evaluate_data_folder(&db, data.clone(), options(None)).unwrap();
    assert!(exact.report.games > 0.0, "the games were read");
    assert!(exact.text.contains("Time by step (0.20.0)"));
    assert_eq!(exact.timings[0].label, "Reading the stored games");
    let dir = eval_dir();
    let latest = std::fs::read_to_string(dir.join("latest.json")).unwrap();
    let saved: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(exact.file.replace(".txt", ".json")).unwrap()).unwrap();
    assert!(saved["timings"].as_array().map(|t| t.len() > 2).unwrap_or(false), "the JSON report has its timings");

    let grouped = evaluate_data_folder(&db, data.clone(), options(Some(0))).unwrap();
    assert!(grouped.report.grouping.as_ref().map(|g| g.groups > 1.0).unwrap_or(false));
    assert!(std::path::Path::new(&grouped.file).file_name().unwrap().to_string_lossy().starts_with("eval-grouped-"));
    assert!(grouped.text.contains("Grouped drafts (a trial"), "{}", grouped.text);
    assert_eq!(std::fs::read_to_string(dir.join("latest.json")).unwrap(), latest, "latest.json is the exact evaluation's");
    assert_eq!(eval_history(&dir, 50).len(), 1, "the history shows the exact evaluation only");
    assert_eq!(grouped.report.games, exact.report.games);

    let text = compare_reports(ComparedRun { report: &exact.report, ms: exact.ms }, ComparedRun { report: &grouped.report, ms: grouped.ms });
    assert!(regex_like(&text), "{text}");
    assert!(text.contains("Summary: "));
}

/// "Draft model gate: (passed|failed) → (passed|failed)"
fn regex_like(text: &str) -> bool {
    ["passed", "failed"].iter().any(|a| ["passed", "failed"].iter().any(|b| text.contains(&format!("Draft model gate: {a} → {b}"))))
}
