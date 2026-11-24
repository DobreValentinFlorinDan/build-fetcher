//! Store measurements on a synthetic set (docs/MEASUREMENTS.md, "Store").
//!
//!   cargo run --release -p bf-store --example measure -- [games=50000] [seed=7] [timeline_share=0.1]
//!
//! Generates games from the seed with `bf_core::testkit::Synth` (3 patches, 3 platforms, 3% defective games, Irelia
//! featured in 5% and tier A), ingests them in batches of 1000 into a fresh database in a temporary folder, then
//! runs the clean-up twice (the learner's window, then a budget below what's used). Prints one line per figure.

use bf_core::build::{Patch, RankBand};
use bf_core::testkit::{load_test_static_data, Synth, SynthOptions};
use bf_store::cleanup::OtherParts;
use bf_store::{Ingest, LearnWindow, Outcome, Store, StoreOptions};
use std::collections::HashSet;
use std::time::Instant;

fn arg<T: std::str::FromStr>(i: usize, d: T) -> T {
    std::env::args().nth(i).and_then(|a| a.parse().ok()).unwrap_or(d)
}

fn mb(b: u64) -> String {
    format!("{:.1} MB", b as f64 / 1_048_576.0)
}

fn main() -> anyhow::Result<()> {
    let n: u64 = arg(1, 50_000);
    let seed: u64 = arg(2, 7);
    let tl: f64 = arg(3, 0.1);
    let dir = tempfile::tempdir()?;
    let data = load_test_static_data();
    let store = Store::open(dir.path(), data.clone(), StoreOptions::default())?;
    store.set_tier_a(&HashSet::from([39]))?;
    let per_patch = n.div_ceil(3);
    let synth = Synth::new(
        SynthOptions {
            seed,
            timeline_share: tl,
            versions: vec!["16.17.700.1".into(), "16.18.704.1234".into(), "16.19.712.4471".into()],
            games_per_version: per_patch,
            featured: Some((39, "MIDDLE".into())),
            featured_share: 0.05,
            noise_share: 0.03,
            ..Default::default()
        },
        data,
    );
    println!("games {n}, seed {seed}, timeline share {tl}, cores {}", bf_core::sys::cores());
    let rss0 = bf_core::sys::own_rss().unwrap_or(0);
    let (mut gen_s, mut ingest_s, mut peak) = (0.0, 0.0, rss0);
    let mut outcomes = Vec::new();
    let mut raw_json = 0u64;
    let batch = 1000;
    let mut i = 0;
    while i < n {
        let t = Instant::now();
        let games: Vec<_> = {
            use rayon::prelude::*;
            (i..(i + batch).min(n)).into_par_iter().map(|k| synth.game(k)).collect()
        };
        raw_json += games.iter().map(|g| g.m.to_string().len() as u64 + g.timeline.as_ref().map(|t| t.to_string().len() as u64).unwrap_or(0)).sum::<u64>();
        gen_s += t.elapsed().as_secs_f64();
        let items: Vec<Ingest> = games.iter().map(|g| Ingest { m: &g.m, timeline: g.timeline.as_ref(), rank: RankBand::MasterPlus, me: None }).collect();
        let t = Instant::now();
        outcomes.extend(store.ingest(&items)?);
        ingest_s += t.elapsed().as_secs_f64();
        peak = peak.max(bf_core::sys::own_rss().unwrap_or(0));
        i += batch;
    }
    let counted = outcomes.iter().filter(|o| matches!(o, Outcome::Counted { .. })).count();
    let kept = outcomes.iter().filter(|o| matches!(o, Outcome::Counted { kept: true, .. })).count();
    let validation = outcomes.iter().filter(|o| **o == Outcome::Validation).count();
    let dropped = outcomes.iter().filter(|o| matches!(o, Outcome::Dropped(_))).count();
    println!("outcomes: counted {counted} (records kept {kept}), validation {validation}, dropped {dropped} {:?}", store.drops());
    println!("raw match-v5 JSON generated (compact): {}", mb(raw_json));
    println!("generation {gen_s:.1} s; ingestion {ingest_s:.1} s = {:.0} games/s", n as f64 / ingest_s);
    println!("RSS before {} peak {}", mb(rss0), mb(peak));
    let other = OtherParts::standard(dir.path(), None);
    // Settle the WAL so the file sizes are the database's.
    let st = store.cleanup(&LearnWindow { patches: vec![] }, u64::MAX, &other)?;
    let _ = st;
    let status = store.storage_status(u64::MAX, &other)?;
    println!("database on disk {} ({} bytes)", mb(status.used_bytes), status.used_bytes);
    for (part, b) in &status.parts {
        println!("  part {part}: {}", mb(*b));
    }
    let (rows_counts, rows_options, rows_pairs, rows_seen, blob_bytes, records, vblob, vrecords, tl_records) = rows(&store.db_path)?;
    println!("rows: counts {rows_counts}, options {rows_options}, pairs {rows_pairs}, seen {rows_seen}, records {records} ({tl_records} with timeline), validation {vrecords}");
    let part = |name: &str| status.parts.iter().find(|p| p.0 == name).map(|p| p.1).unwrap_or(0);
    println!("bytes per record (blob only): {:.0}; with its table and index: {:.0}", blob_bytes as f64 / records.max(1) as f64, part("records") as f64 / records.max(1) as f64);
    let (with_tl, without_tl) = per_kind(&store.db_path)?;
    println!("bytes per record blob: with timeline {with_tl:.0}, without {without_tl:.0}");
    println!("bytes per validation record: {:.0}", part("validation") as f64 / vrecords.max(1) as f64);
    println!("bytes per count row (counts+options+pairs pages / count rows): {:.1}", part("counts") as f64 / rows_counts.max(1) as f64);
    println!("count rows per counted game: {:.1}; database bytes per counted game: {:.0}", rows_counts as f64 / counted.max(1) as f64, status.used_bytes as f64 / (counted + validation).max(1) as f64);
    let _ = vblob;

    // Clean-up 1: the learner keeps the two newest patches.
    let window = LearnWindow { patches: vec![Patch { major: 16, minor: 19 }, Patch { major: 16, minor: 18 }] };
    let rep = store.cleanup(&window, u64::MAX, &other)?;
    println!("cleanup (window 16.18-16.19): {} ms, freed {} ({} records), now {}", rep.millis, mb(rep.freed_bytes), rep.records_deleted, mb(rep.after_bytes));
    // Clean-up 2: a budget of 60% of what is used: the oldest patch's counts go too.
    let budget = rep.after_bytes * 6 / 10;
    let rep = store.cleanup(&window, budget, &other)?;
    println!(
        "cleanup (budget {}): {} ms, freed {}, patches trimmed {:?}, {} count rows, now {} (over budget: {})",
        mb(budget),
        rep.millis,
        mb(rep.freed_bytes),
        rep.patches_trimmed.iter().map(|p| p.to_string()).collect::<Vec<_>>(),
        rep.count_rows_deleted,
        mb(rep.after_bytes),
        rep.over_budget
    );
    println!("validation records after clean-up: {}", bf_core::build::DataSource::validation_records(&store)?.len());
    println!("RSS end {}", mb(bf_core::sys::own_rss().unwrap_or(0)));
    Ok(())
}

#[allow(clippy::type_complexity)]
fn rows(db: &std::path::Path) -> anyhow::Result<(i64, i64, i64, i64, i64, i64, i64, i64, i64)> {
    let c = rusqlite::Connection::open(db)?;
    let q = |sql: &str| -> anyhow::Result<i64> { Ok(c.query_row(sql, [], |r| r.get(0))?) };
    Ok((
        q("SELECT COUNT(*) FROM counts")?,
        q("SELECT COUNT(*) FROM options")?,
        q("SELECT COUNT(*) FROM pairs")?,
        q("SELECT COUNT(*) FROM seen")?,
        q("SELECT COALESCE(SUM(length(blob)), 0) FROM matches")?,
        q("SELECT COUNT(*) FROM matches")?,
        q("SELECT COALESCE(SUM(length(blob)), 0) FROM validation")?,
        q("SELECT COUNT(*) FROM validation")?,
        q("SELECT COUNT(*) FROM matches WHERE has_timeline = 1")?,
    ))
}

fn per_kind(db: &std::path::Path) -> anyhow::Result<(f64, f64)> {
    let c = rusqlite::Connection::open(db)?;
    let q = |t: i64| -> anyhow::Result<f64> { Ok(c.query_row("SELECT COALESCE(AVG(length(blob)), 0) FROM matches WHERE has_timeline = ?", [t], |r| r.get(0))?) };
    Ok((q(1)?, q(0)?))
}
