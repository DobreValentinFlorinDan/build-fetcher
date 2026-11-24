//! Parity harness: 1.0's lane builds and draft recommendations for the drafts parity/ts-drafts.ts sampled.
//!
//!   parity_drafts <data-folder> <ts-drafts.json> <out.json> [--memory]

use irelia::db::{Db, OpenOptions};
use irelia::engine::exclusivity::build_exclusivity_index;
use irelia::engine::recommend::{recommend, EngineSettings};
use irelia::model::draft::{recommend_draft, DraftExtra, DraftQuery, DraftSettings};
use irelia::model::estimate::ESTIMATE_DEFAULTS;
use irelia::model::run_eval::{model_patches, region_patch_fn};
use irelia::model::service::build_from_db;
use irelia::model::weights::{weight_defaults, Memory};
use irelia::scope::default_scope;
use irelia::testkit::load_test_static_data;
use serde_json::{json, Value};
use std::sync::Arc;

fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let dir = std::path::PathBuf::from(&args[1]);
    let input: Value = serde_json::from_str(&std::fs::read_to_string(&args[2])?)?;
    let out = std::path::PathBuf::from(&args[3]);
    let memory_on = args.iter().any(|a| a == "--memory");
    let db = Db::open(&dir.join("irelia.db"), OpenOptions { migrate: false, busy_timeout_ms: 5000 })?;
    let data = load_test_static_data();
    let excl = build_exclusivity_index(&data);
    let rp = region_patch_fn(&db, &data)?;
    let memory = if memory_on { Some(Memory { decay: 0.75, patches: 6.0 }) } else { None };
    let scope = default_scope();
    let started = std::time::Instant::now();
    let built = build_from_db(&db, &data, &scope, 60_000.0, None, &memory, None, &rp, 1)?;
    let build_ms = started.elapsed().as_millis();
    let patches = model_patches(&db, &data, 2.0)?;
    let rows: Vec<Arc<irelia::db::IreliaGameRow>> = db.games_for_patches(&patches, &scope.build_sources)?.into_iter().map(Arc::new).collect();
    let mut weights = weight_defaults();
    weights.region_weights = scope.region_weights.clone();
    weights.own_platform = Some("KR".into());
    weights.own_region_boost = scope.own_region_boost;
    weights.memory = memory.clone();
    let settings = DraftSettings { weights, estimate: ESTIMATE_DEFAULTS, item_k: None };
    let engine = EngineSettings::default();
    let mut results = Vec::new();
    for d in input["drafts"].as_array().unwrap() {
        let lane: &'static str = if d["lane"] == "TOP" { "TOP" } else { "MIDDLE" };
        let ids = |v: &Value| v.as_array().unwrap().iter().map(|x| x.as_i64().unwrap()).collect::<Vec<_>>();
        let q = DraftQuery { lane, opponent: d["opponent"].as_i64(), enemies: ids(&d["enemies"]), allies: ids(&d["allies"]) };
        let rpc = rp.clone();
        let baseline = recommend(&rows, &data, &excl, q.opponent, &engine, Some(lane), &move |p| rpc(p), None);
        let draft = recommend_draft(&built.model, &q, &data, &excl, &rp, &baseline, &settings, &DraftExtra { detail: true, ..Default::default() });
        results.push(json!({ "q": d, "baseline": serde_json::to_value(&baseline)?, "draft": serde_json::to_value(&draft)? }));
    }
    let v = json!({ "drafts": input["drafts"], "results": results, "buildMs": build_ms, "games": built.prepared.len() });
    std::fs::write(&out, irelia::js::stringify(&v))?;
    eprintln!("[rs] {} drafts on {} games (model built in {} ms)", results.len(), built.prepared.len(), build_ms);
    Ok(())
}
