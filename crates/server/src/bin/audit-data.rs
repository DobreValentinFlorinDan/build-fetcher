//! Port of `scripts/audit-data.ts`: `npm run audit:data [-- <data folder>] [--out <file.json>]`
//! (cargo run --release -p irelia-server --bin audit-data -- …)
//!
//! Read-only audit of the stored data (crates/server/src/audit.rs): no API calls, no writes to the data folder. Prints a
//! compact summary and writes the full report as JSON (audit-<date>.json in the current folder unless --out says
//! otherwise). Without a folder it uses the installed app's (config.json → dataDir, else …/irelia-companion/data), then
//! ./data.

use std::io::Write;
use std::path::{Path, PathBuf};

fn default_data_dir() -> PathBuf {
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

fn main() -> anyhow::Result<()> {
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    let out = match args.iter().position(|a| a == "--out") {
        Some(i) => {
            let v = args.get(i + 1).cloned().unwrap_or_default();
            args.drain(i..(i + 2).min(args.len()));
            v
        }
        None => format!("audit-{}.json", chrono::Utc::now().format("%Y-%m-%d")),
    };
    let data_dir = irelia::util::resolve(args.first().map(PathBuf::from).unwrap_or_else(default_data_dir));
    eprintln!("Auditing {} (read-only, no API calls)…", data_dir.display());
    let report = irelia_server::audit::audit_data(&data_dir, irelia::util::now_ms() as f64, 39, &|step, part| {
        let label = irelia_server::audit::AUDIT_STEPS.get(step.wrapping_sub(1)).copied().unwrap_or("");
        let part = part.map(|x| format!(" {}%", irelia::js::num_to_string(irelia::js::round(x * 100.0)))).unwrap_or_default();
        eprint!("\r  {label}{part}…            ");
        let _ = std::io::stderr().flush();
    })?;
    eprintln!();
    println!("{}", irelia_server::audit::format_audit(&report));
    std::fs::write(&out, format!("{}\n", irelia::js::stringify_pretty(&report)))?;
    eprintln!("\nFull report: {}", irelia::util::resolve(&out).display());
    Ok(())
}
