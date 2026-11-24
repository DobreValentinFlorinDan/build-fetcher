//! Port of `scripts/eval-builds.ts`: `npm run eval:builds [-- <data folder>] [--quick] [--threads N] [--auto | --update]`
//! (cargo run --release -p irelia-server --bin eval-builds -- …)
//!
//! Offline evaluation of the draft-aware model against the lane baseline: grouped cross-validation by pilot, no Riot API
//! calls (Data Dragon comes from the data folder's cache, or its CDN). Opening the database upgrades an older one exactly
//! as the app does; nothing else is written to the data folder. The report is printed and saved under
//! %APPDATA%\irelia-companion\evals, where the app reads the gate result. --quick tries one bandwidth and prior and skips
//! the live replay. --threads N: on N threads (the same report); by default as many as the app would use.
//! A full evaluation unless given --auto (what the app's Evaluate builds runs) or --update (a quick update, or a full
//! evaluation when there is no full one to add to).
//!
//! Grouped drafts (0.20.0, docs/EVAL-SCALING.md), a trial the app doesn't use yet:
//!   --grouped [N]          a full evaluation with the drafts in N groups (default: about 40 games each), saved as
//!                          eval-grouped-…; the app's verdicts and its next quick update stay the last exact evaluation's.
//!   --compare-grouped [N]  a full exact evaluation (saved as usual), then the grouped trial on the same games, and the two
//!                          side by side, saved as eval-compare-….txt.

use irelia::config::config;
use irelia::db::{Db, OpenOptions};
use irelia::model::eval_base::plan_evaluation;
use irelia::model::run_eval::{compare_reports, eval_size, evaluate_data_folder, ComparedRun, FolderOptions};
use irelia::model::service::eval_dir;
use std::path::{Path, PathBuf};
use std::sync::Arc;

fn fail(msg: &str) -> ! {
    eprintln!("{msg}");
    std::process::exit(1)
}

/// The installed app's data folder (config.json → dataDir, else …/irelia-companion/data), then ./data.
pub fn default_data_dir() -> PathBuf {
    if let Ok(d) = std::env::var("IRELIA_DATA_DIR") {
        return PathBuf::from(d);
    }
    let app_dir = irelia::app_config::app_config_dir();
    if let Some(d) = irelia::app_config::get_str("dataDir") {
        if Path::new(&d).join("irelia.db").exists() {
            return PathBuf::from(d);
        }
    }
    if app_dir.join("data").join("irelia.db").exists() {
        return app_dir.join("data");
    }
    irelia::util::resolve("data")
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let has = |f: &str| args.iter().any(|a| a == f);
    let quick = has("--quick");
    let threads_at = args.iter().position(|a| a == "--threads");
    let threads: Option<usize> = threads_at.map(|i| match args.get(i + 1).and_then(|s| s.parse::<usize>().ok()) {
        Some(n) if n >= 1 => n,
        _ => fail("--threads takes a whole number, 1 or more"),
    });
    let (auto, update) = (has("--auto"), has("--update"));
    if auto && update {
        fail("--auto or --update, not both");
    }
    if quick && (auto || update) {
        fail("--quick is a full evaluation with one bandwidth and prior: leave out --auto and --update");
    }
    let asked = if auto { "auto" } else if update { "update" } else { "full" };
    let grouped_at = args.iter().position(|a| a == "--grouped" || a == "--compare-grouped");
    let compare = has("--compare-grouped");
    let groups_arg: Option<usize> = grouped_at.and_then(|i| args.get(i + 1)).filter(|s| !s.is_empty() && s.chars().all(|c| c.is_ascii_digit())).and_then(|s| s.parse().ok());
    let grouping = grouped_at.map(|_| groups_arg.unwrap_or(0));
    if grouping.is_some() && (quick || auto || update) {
        fail("--grouped and --compare-grouped are full evaluations: leave out --quick, --auto and --update");
    }
    if has("--grouped") && compare {
        fail("--grouped or --compare-grouped, not both");
    }
    let folder = args.iter().enumerate().find(|(i, a)| {
        !a.starts_with("--") && Some(i.wrapping_sub(1)) != threads_at && !(groups_arg.is_some() && Some(i.wrapping_sub(1)) == grouped_at)
    });
    // The data folder must be known before the config is read.
    let dir = folder.map(|(_, f)| PathBuf::from(f)).unwrap_or_else(default_data_dir);
    std::env::set_var("IRELIA_DATA_DIR", irelia::util::resolve(&dir));
    let cfg = config();
    if !cfg.db_path.exists() {
        fail(&format!("No database at {}", cfg.db_path.display()));
    }
    let db = Db::open(&cfg.db_path, OpenOptions { migrate: true, ..Default::default() })?;
    let data = Arc::new(irelia_server::app::static_data(None).await?);
    if asked == "auto" {
        let plan = plan_evaluation(&db, &data, irelia::util::now_ms() as f64, None, None, None)?;
        if plan.path == "none" {
            eprintln!("Nothing to evaluate ({}). Without --auto it runs a full evaluation.", plan.reason);
            return Ok(());
        }
    }
    eprintln!("Evaluating {} (no Riot API calls)…", cfg.data_dir.display());
    let threads = threads.unwrap_or_else(|| irelia_server::system::planned_threads(eval_size(&db, &data)) as usize);
    let started = std::time::Instant::now();
    let options = |path: &'static str, grouping: Option<usize>| FolderOptions {
        quick,
        auto: false,
        path,
        threads,
        log: Box::new(|l: &str| eprintln!("  {l}")),
        progress: Box::new(|_| {}),
        save: true,
        control: None,
        grouping,
        gpu: false,
    };
    if grouping.is_none() || compare {
        let exact = evaluate_data_folder(&db, data.clone(), options(asked, None))?;
        println!("{}", exact.text);
        eprintln!(
            "\n({} s) {}: {}. Saved as {}",
            irelia::js::num_to_string(irelia::js::round(started.elapsed().as_secs_f64())),
            if exact.path == "update" { "Quick update" } else { "Full evaluation" },
            exact.reason,
            exact.file
        );
        if compare {
            eprintln!("\nNow the same games with grouped drafts…");
            let grouped = evaluate_data_folder(&db, data.clone(), options("full", grouping))?;
            println!("\n{}", grouped.text);
            let text = compare_reports(ComparedRun { report: &exact.report, ms: exact.ms }, ComparedRun { report: &grouped.report, ms: grouped.ms });
            let stamp = chrono::Utc::now().format("%Y-%m-%d-%H-%M").to_string();
            let file = eval_dir().join(format!("eval-compare-{stamp}.txt"));
            std::fs::write(&file, format!("{text}\n"))?;
            println!("\n{text}");
            eprintln!("\nGrouped trial saved as {}; the comparison as {}", grouped.file, file.display());
        }
    } else {
        let grouped = evaluate_data_folder(&db, data.clone(), options("full", grouping))?;
        println!("{}", grouped.text);
        eprintln!(
            "\n({} s) Grouped drafts trial. Saved as {}",
            irelia::js::num_to_string(irelia::js::round(started.elapsed().as_secs_f64())),
            grouped.file
        );
    }
    Ok(())
}
