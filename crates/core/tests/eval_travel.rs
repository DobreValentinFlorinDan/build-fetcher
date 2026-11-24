//! Port of server/model/evalTravel.test.ts: an evaluation that travels with a backup (0.21.0), on real databases. A
//! computer evaluates its pool and backs it up; another one, with games of its own, imports the backup, takes the
//! evaluation (its verdicts apply at once) and its next evaluation is a quick update measuring only its own games. A
//! second import of an evaluation covering no more games is turned down.

use irelia::db::Db;
use irelia::model::eval_base::{plan_evaluation, read_base_header};
use irelia::model::eval_travel::{adopt_incoming_evaluation, incoming_evaluation};
use irelia::model::run_eval::{evaluate_data_folder, FolderOptions};
use irelia::model::service::{eval_dir, read_evaluation};
use irelia::testkit::load_test_static_data;
use irelia::testkit_model::{ap_ad_games, store_games, ApAd, SynthGame};
use irelia::transfer::{export_pool, import_export, inspect_export, hostname};
use std::path::Path;

fn quiet(_: &str, _: Option<f64>) {}

fn options(path: &'static str) -> FolderOptions<'static> {
    FolderOptions { quick: false, auto: false, path, threads: 1, log: Box::new(|_| {}), progress: Box::new(|_| {}), save: true, control: None, grouping: None, gpu: false }
}

fn now() -> f64 {
    irelia::util::now_ms() as f64
}

#[test]
fn a_backup_carries_the_evaluation_and_the_computer_importing_it_only_measures_its_own_games_next() {
    let root = tempfile::tempdir().unwrap();
    std::env::set_var("IRELIA_CONFIG_DIR", root.path().join("config"));
    let a_dir = root.path().join("pc-a");
    std::fs::create_dir_all(&a_dir).unwrap();
    std::env::set_var("IRELIA_DATA_DIR", &a_dir);
    let data = load_test_static_data();

    // Computer A: its pool, evaluated in full, then backed up.
    let full = {
        let a = Db::open_default(&a_dir.join("irelia.db")).unwrap();
        store_games(&a, &ap_ad_games(400, 40, 9, ApAd { no_effect: false, random_opponent: true }), Some(&data.patch));
        evaluate_data_folder(&a, data.clone(), options("full")).unwrap()
    };
    assert_eq!(full.path, "full");
    assert!(read_base_header().and_then(|h| h.portable).is_some(), "the base has its portable key");
    let ex = export_pool(&a_dir, &root.path().join("drive"), "test", &quiet).unwrap();
    assert_eq!(ex["evaluation"], serde_json::json!(true));
    let file = Path::new(ex["file"].as_str().unwrap()).to_path_buf();
    let (manifest, _) = inspect_export(&file).unwrap();
    assert!(manifest["evaluation"]["known"].as_f64().unwrap_or(0.0) > 0.0, "the manifest says it carries an evaluation");

    // Computer B: its own settings (nothing evaluated) and a fresh pool of 40 games A never had.
    std::fs::remove_dir_all(eval_dir()).unwrap();
    let b_dir = root.path().join("pc-b");
    std::fs::create_dir_all(&b_dir).unwrap();
    irelia::config::set_data_dir(&b_dir);
    {
        let b = Db::open_default(&b_dir.join("irelia.db")).unwrap();
        let own: Vec<SynthGame> = ap_ad_games(40, 8, 77, ApAd { no_effect: false, random_opponent: true })
            .into_iter()
            .enumerate()
            .map(|(i, g)| SynthGame { id: format!("KR_{}", 90_000 + i), pilot: format!("fresh{}", i % 8), ..g })
            .collect();
        store_games(&b, &own, Some(&data.patch));
        assert_eq!(plan_evaluation(&b, &data, now(), None, None, None).unwrap().path, "full", "before the import: never evaluated");
    }
    let imported = import_export(&b_dir, &file, true, &quiet).unwrap();
    assert_eq!(imported["added"], serde_json::json!(400));
    assert_eq!(imported["evaluation"], serde_json::json!(true));
    assert!(incoming_evaluation());

    let b = Db::open_default(&b_dir.join("irelia.db")).unwrap();
    let (adopted, message) = adopt_incoming_evaluation(&b, &data).unwrap();
    assert!(adopted, "{message}");
    assert!(message.contains("measures only the 40 games it didn't have"), "{message}");
    assert!(!incoming_evaluation(), "taken: nothing left waiting");
    let summary = read_evaluation().unwrap();
    assert_eq!(summary.gate, full.report.gate, "A's verdict applies on B at once");
    assert_eq!(summary.imported_from.as_ref().map(|i| i.machine.clone()), Some(hostname()));
    assert!(std::fs::read_dir(eval_dir()).unwrap().any(|e| {
        let n = e.unwrap().file_name().to_string_lossy().into_owned();
        n.starts_with("eval-imported-") && n.ends_with(".txt")
    }), "its report is there to read");
    let plan = plan_evaluation(&b, &data, now(), None, None, None).unwrap();
    assert_eq!((plan.path.as_str(), plan.new_games), ("update", 40.0), "{}", plan.reason);

    // B's next evaluation: a quick update of its own 40 games, A's measurements kept.
    let next = evaluate_data_folder(&b, data.clone(), options("auto")).unwrap();
    assert_eq!(next.path, "update", "{}", next.reason);
    let u = next.report.update.as_ref().unwrap();
    assert!(u.new_games > 0.0 && u.new_games <= 40.0, "measured {}", u.new_games);
    assert_eq!(u.missing, Some(0.0));

    // The same backup again: B's own evaluation now covers as many games, so it is turned down.
    drop(b);
    import_export(&b_dir, &file, true, &quiet).unwrap();
    let b = Db::open_default(&b_dir.join("irelia.db")).unwrap();
    let (adopted, message) = adopt_incoming_evaluation(&b, &data).unwrap();
    assert!(!adopted);
    assert!(message.contains("covers as many games or more"), "{message}");
    assert_eq!(read_evaluation().unwrap().evaluated_at, next.report.evaluated_at, "B's own evaluation stays");
}
