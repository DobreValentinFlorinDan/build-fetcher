//! Port of `server/model/runEval.ts` / `evalBase.ts` (the parts that read a data folder) and `formatEval`: an
//! evaluation of the stored games, its report as text, and where it is saved.

use super::evaluate::{eval_defaults, evaluate, EvalInput, EvalOptions, EvalOutput, EvalProgress, EvalReport, StepTime};
use super::profiles::{ChampionProfiles, PROFILE_DEFAULTS};
use super::weights::{patch_age, patch_order, RegionPatchFn};
use crate::db::Db;
use crate::ddragon::StaticData;
use crate::engine::exclusivity::build_exclusivity_index;
use crate::engine::recommend::EngineSettings;
use crate::js;
use crate::patches::{region_patches, Fallback};
use crate::scope::{default_scope, load_scope, ScrapeScope};
use anyhow::Result;
use std::collections::HashMap;
use std::sync::Arc;

/// The patches the draft model reads (service.ts modelPatches): each region's current and previous patch, and with
/// `window` > 2 every stored patch within that many of a current one.
pub fn model_patches(db: &Db, data: &StaticData, window: f64) -> Result<Vec<String>> {
    let mut keep: Vec<String> = Vec::new();
    let add = |p: &str, keep: &mut Vec<String>| {
        if !keep.iter().any(|k| k == p) {
            keep.push(p.to_string());
        }
    };
    add(&data.patch, &mut keep);
    add(&data.previous_patch, &mut keep);
    let versions = db.game_versions()?;
    let fb = Fallback { patch: data.patch.clone(), previous_patch: data.previous_patch.clone() };
    let regions = region_patches(&versions, &fb, &[], crate::util::now_ms());
    for (_, r) in &regions {
        add(&r.patch, &mut keep);
        add(&r.previous_patch, &mut keep);
    }
    if window > 2.0 {
        let order = patch_order(data);
        let mut currents = vec![data.patch.clone()];
        currents.extend(regions.iter().map(|(_, r)| r.patch.clone()));
        let mut seen: Vec<String> = Vec::new();
        for v in &versions {
            if !seen.contains(&v.patch) {
                seen.push(v.patch.clone());
            }
        }
        for p in seen {
            if currents.iter().any(|c| matches!(patch_age(&order, &p, c), Some(a) if a >= 0.0 && a < window)) {
                add(&p, &mut keep);
            }
        }
    }
    Ok(keep)
}

/// What an evaluation reads (evalBase.ts evalReads).
pub struct EvalReads {
    pub scope: ScrapeScope,
    pub patches: Vec<String>,
    pub read: Vec<String>,
    pub sources: Vec<String>,
}

pub fn eval_reads(db: &Db, data: &StaticData, quick: bool) -> Result<EvalReads> {
    let scope = load_scope().unwrap_or_else(|_| default_scope());
    let memory = if quick { None } else { eval_defaults(EngineSettings::default()).memory };
    let patches = model_patches(db, data, 2.0)?;
    let read = match &memory {
        Some(m) => model_patches(db, data, m.patches)?,
        None => patches.clone(),
    };
    let mut sources: Vec<String> = Vec::new();
    for s in scope.draft_sources.iter().chain(scope.build_sources.iter()).map(|s| s.as_str()).chain(std::iter::once("me")) {
        if !sources.iter().any(|x| x == s) {
            sources.push(s.to_string());
        }
    }
    Ok(EvalReads { scope, patches, read, sources })
}

/// The evaluation's options for this scope; `quick`: --quick's.
pub fn eval_options(scope: &ScrapeScope, quick: bool) -> EvalOptions {
    let mut o = eval_defaults(EngineSettings::default());
    o.build_sources = scope.build_sources.clone();
    o.weights.region_weights = scope.region_weights.clone();
    o.weights.own_region_boost = scope.own_region_boost;
    o.weights.patch_weights = scope.patch_weights.clone();
    o.weights.hotfixes = scope.hotfixes.clone();
    // 2.2.0's candidates: a full evaluation measures each against today's method and the app takes what wins.
    o.state_sample = true;
    o.effective_window = true;
    if quick {
        o.bandwidths = vec![0.7];
        o.ks = vec![100.0];
        o.lambdas = vec![1.0];
        o.live = false;
        o.bootstrap = 300;
        o.memory = None;
        o.scoring = false;
        o.tuning = None;
        o.swaps = false;
        o.state_sample = false;
        o.effective_window = false;
    }
    o
}

/// Each region's current and previous patch, as the evaluation's region_patch.
pub fn region_patch_fn(db: &Db, data: &StaticData) -> Result<RegionPatchFn> {
    let fb = Fallback { patch: data.patch.clone(), previous_patch: data.previous_patch.clone() };
    let regions: HashMap<String, (String, String)> = region_patches(&db.game_versions()?, &fb, &[], crate::util::now_ms())
        .into_iter()
        .map(|(p, r)| (p, (r.patch, r.previous_patch)))
        .collect();
    Ok(Arc::new(move |p: &str| regions.get(p).cloned()))
}

pub struct RunOptions<'a> {
    pub quick: bool,
    pub threads: usize,
    pub log: Box<dyn Fn(&str) + Sync + 'a>,
    pub progress: Box<dyn Fn(EvalProgress) + Sync + 'a>,
    pub opts: Option<EvalOptions>,
    pub salt: Option<String>,
    pub base: Option<super::evaluate::EvalBaseData>,
    pub control: Option<Arc<super::evaluate::EvalControl>>,
}

/// One full evaluation of the stored games (measure(), without saving).
pub fn measure(db: &Db, data: Arc<StaticData>, o: RunOptions) -> Result<EvalOutput> {
    let reads = eval_reads(db, &data, o.quick)?;
    let opts = o.opts.unwrap_or_else(|| eval_options(&reads.scope, o.quick));
    let profiles = Arc::new(ChampionProfiles::new(&db.champion_totals(&reads.patches)?, &db.older_champion_totals(&reads.patches)?, data.clone(), PROFILE_DEFAULTS));
    let region_patch = region_patch_fn(db, &data)?;
    let rows = db.model_games(&reads.read, &reads.sources)?;
    let participants = db.model_participants(&reads.read, &reads.sources)?;
    let input = EvalInput {
        rows,
        timelines_of: None,
        classic_patches: Some(reads.patches.clone()),
        participants,
        data: data.clone(),
        excl: Arc::new(build_exclusivity_index(&data)),
        profiles,
        region_patch,
        rec_log: db.recent_recommendations(10_000)?,
        starting_item_window_ms: crate::config::STARTING_ITEM_WINDOW_MS,
        log: o.log,
        progress: o.progress,
        threads: o.threads,
        // A --quick check measures too little to be a base: no salt, no next base.
        salt: if o.base.is_some() || (o.quick && o.salt.is_none()) { None } else { o.salt.or_else(|| Some(random_hex(16))) },
        base: o.base,
        control: o.control,
        prepared_hook: None,
    };
    evaluate(input, &opts)
}

/// `crypto.randomBytes(n).toString('hex')`.
pub fn random_hex(n: usize) -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let mut seed = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0) as u64 ^ (std::process::id() as u64) << 32;
    let mut out = String::new();
    for _ in 0..n {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        out.push_str(&format!("{:02x}", (seed >> 24) as u8));
    }
    let h = sha2::Sha256::digest(out.as_bytes());
    use sha2::Digest;
    h[..n].iter().map(|b| format!("{b:02x}")).collect()
}

fn pts(x: f64) -> String {
    format!("{}{} pts", if x >= 0.0 { "+" } else { "" }, js::to_fixed(x * 100.0, 2))
}

fn pad_end(s: &str, n: usize) -> String {
    let len = s.encode_utf16().count();
    if len >= n {
        s.to_string()
    } else {
        format!("{}{}", s, " ".repeat(n - len))
    }
}
fn pad_start(s: &str, n: usize) -> String {
    let len = s.encode_utf16().count();
    if len >= n {
        s.to_string()
    } else {
        format!("{}{}", " ".repeat(n - len), s)
    }
}

/// `new Date(ms).toISOString().slice(0, 16)`.
pub fn iso_minutes(ms: f64) -> String {
    let d = chrono::DateTime::from_timestamp_millis(ms as i64).unwrap_or_default();
    d.format("%Y-%m-%dT%H:%M").to_string()
}

fn pct0(x: f64) -> String {
    js::num_to_string(js::round(x * 100.0))
}

/// The report as text (formatEval).
pub fn format_eval(r: &EvalReport) -> String {
    let mut lines: Vec<String> = Vec::new();
    lines.push(format!("Irelia Companion build evaluation · {}", iso_minutes(r.evaluated_at)));
    lines.push(format!("{} games from {} pilots, {}-fold cross-validation grouped by pilot", js::num_to_string(r.games), js::num_to_string(r.pilots), js::num_to_string(r.folds)));
    lines.push(String::new());
    if let Some(g) = &r.grouping {
        lines.push(format!(
            "Grouped drafts (a trial, docs/EVAL-SCALING.md): {} groups of {} games, spread {} sd around their centre. Close to an exact evaluation's numbers, not the same.",
            js::thousands(g.groups),
            js::thousands(g.games),
            js::to_fixed(g.spread, 3)
        ));
        lines.push(String::new());
    }
    lines.push("Game-state model (B2)".into());
    lines.push("---------------------".into());
    match &r.state {
        Some(s) => {
            lines.push(format!(
                "λ = {}: Brier {} (always the base rate: {}), log loss {} ({}) on {} held-out frames",
                js::num_to_string(s.lambda),
                js::to_fixed(s.brier, 4),
                js::to_fixed(s.base_brier, 4),
                js::to_fixed(s.log_loss, 4),
                js::to_fixed(s.base_log_loss, 4),
                js::num_to_string(s.frames)
            ));
            lines.push(format!(
                "  calibration: {}",
                s.calibration
                    .iter()
                    .map(|b| format!("{}-{} %: {} % of {}", pct0(b.from), pct0(b.to), pct0(b.observed), js::num_to_string(b.n)))
                    .collect::<Vec<_>>()
                    .join(" · ")
            ));
            if let Some(sm) = &s.sample {
                let row = |name: &str, x: &super::evaluate::SampleScore| {
                    format!(
                        "  {}: λ = {}, log loss {}, Brier {}, ECE {} ({} rows a fold)",
                        name,
                        js::num_to_string(x.lambda),
                        js::to_fixed(x.log_loss, 4),
                        js::to_fixed(x.brier, 4),
                        js::to_fixed(x.ece, 4),
                        js::num_to_string(js::round(x.rows))
                    )
                };
                lines.push("Sample (2.2.0 candidate): today's even stride against one frame drawn at random per 5-minute interval".into());
                lines.push(row("even stride", &sm.even));
                lines.push(row("one frame per 5 minutes", &sm.interval));
                lines.push(format!("  kept: {} (the lower held-out log loss)", if sm.kept == "5min" { "one frame per 5 minutes" } else { "the even stride" }));
            }
        }
        None => lines.push("not enough timelines to fit it".into()),
    }
    lines.push(String::new());
    lines.push("Draft-adjusted vs lane baseline (B3)".into());
    lines.push("------------------------------------".into());
    lines.push("Lift = WPA of held-out games that followed a model's pick minus those that didn't (estimates).".into());
    for g in &r.grid {
        lines.push(format!("  h={} k={}: draft {}, baseline {}, difference {}", js::num_to_string(g.h), js::num_to_string(g.k), pts(g.draft), pts(g.baseline), pts(g.delta)));
    }
    lines.push(format!("chosen: h={}, k={}", js::num_to_string(r.chosen.h), js::num_to_string(r.chosen.k)));
    for d in &r.decisions {
        lines.push(format!(
            "  {} draft {} (followed {} %) · baseline {} (followed {} %)",
            pad_end(&d.key, 14),
            pad_start(&pts(d.draft), 11),
            pct0(d.followed_draft),
            pad_start(&pts(d.baseline), 11),
            pct0(d.followed_baseline)
        ));
    }
    lines.push(format!(
        "lift: draft {}, baseline {}, difference {} (95 % CI {} to {}, pilot bootstrap)",
        pts(r.lift.draft),
        pts(r.lift.baseline),
        pts(r.lift.delta),
        pts(r.lift.low),
        pts(r.lift.high)
    ));
    lines.push(format!(
        "Brier on the option taken: draft {}, baseline {} ({} decisions)",
        js::to_fixed(r.brier.draft, 4),
        js::to_fixed(r.brier.baseline, 4),
        js::num_to_string(r.brier.n)
    ));
    if let Some(w) = &r.lift.within_pilot {
        lines.push(format!("within each pilot (reported only): draft {}, baseline {}, difference {}", pts(w.draft), pts(w.baseline), pts(w.delta)));
    }
    if let Some(w) = &r.window {
        lines.push(String::new());
        lines.push("Effective window (2.2.0 candidate)".into());
        lines.push("----------------------------------".into());
        lines.push(format!(
            "An item's ΔW ends at its sale or upgrade (the chance then minus before it was bought) instead of the result minus the chance before; {} held-out item decisions end early.",
            js::num_to_string(w.items)
        ));
        let row = |name: &str, x: &super::evaluate::WindowLift| {
            format!("  {}: lift difference {} (95 % CI {} to {}), draft {}, baseline {}, Brier {}", name, pts(x.delta), pts(x.low), pts(x.high), pts(x.draft), pts(x.baseline), js::to_fixed(x.brier, 4))
        };
        lines.push(row("today's ΔW", &w.today));
        lines.push(row("effective window", &w.window));
        lines.push(format!("kept: {} ({})", if w.kept == "window" { "the effective window" } else { "today's ΔW" }, w.gate_reason));
    }
    if let Some(s) = &r.stability {
        lines.push(String::new());
        lines.push("Live stability (B5)".into());
        lines.push("-------------------".into());
        lines.push(format!(
            "{} held-out timelines, {} minutes: \"Next\" flips {} times per game within a slot ({} slot changes per game)",
            js::num_to_string(s.games),
            js::num_to_string(s.minutes),
            js::to_fixed(s.flips_per_game, 2),
            js::to_fixed(s.slot_changes_per_game, 1)
        ));
    }
    if let Some(m) = &r.memory {
        lines.push(String::new());
        lines.push("Patch memory".into());
        lines.push("------------".into());
        lines.push(format!(
            "Games of up to {} patches (today's included), fading per patch, against today's and the previous patch alone; {} games from older patches.",
            js::num_to_string(m.patches),
            js::num_to_string(m.older_games)
        ));
        if !m.grid.is_empty() {
            for g in &m.grid {
                lines.push(format!("  fade {} per patch: patch memory {}, two patches {}, difference {}", js::num_to_string(g.decay), pts(g.memory), pts(g.classic), pts(g.delta)));
            }
            lines.push(format!("chosen: fade {} per patch", js::num_to_string(m.decay)));
            lines.push(format!(
                "all games: patch memory {}, two patches {}, difference {} (95 % CI {} to {})",
                pts(m.full.memory),
                pts(m.full.classic),
                pts(m.full.delta),
                pts(m.full.low),
                pts(m.full.high)
            ));
            lines.push(format!(
                "early in a patch ({} of {} current-patch games): patch memory {}, two patches {}, difference {} (95 % CI {} to {})",
                js::num_to_string(m.early.kept_games),
                js::num_to_string(m.early.current_games),
                pts(m.early.memory),
                pts(m.early.classic),
                pts(m.early.delta),
                pts(m.early.low),
                pts(m.early.high)
            ));
            lines.push(format!("Brier on the option taken: patch memory {}, two patches {}", js::to_fixed(m.full.brier, 4), js::to_fixed(m.full.classic_brier, 4)));
        }
        lines.push(format!("Patch memory: {} — {}", m.gate.to_uppercase(), m.gate_reason));
    }
    if let Some(c) = &r.scoring {
        lines.push(String::new());
        lines.push("Runes and summoners against the draft".into());
        lines.push("-------------------------------------".into());
        lines.push("Keystone, secondary tree, summoners, start and skill maxed first scored against each game's expected win rate before it started (draft, lane, patch, region, pilot standing, lane opponent), against today's scoring by the overall win rate.".into());
        if let Some(e) = &c.expected {
            lines.push(format!(
                "expected win rate on {} held-out games: Brier {} (overall win rate {}), log loss {} ({}){}",
                js::num_to_string(e.games),
                js::to_fixed(e.brier, 4),
                js::to_fixed(e.base_brier, 4),
                js::to_fixed(e.log_loss, 4),
                js::to_fixed(e.base_log_loss, 4),
                e.penalty
                    .map(|p| format!(
                        "; penalties {} (draft, pilot, patch, region) and {} (lane opponent)",
                        js::num_to_string(p.lambda),
                        js::num_to_string(p.opponent_lambda)
                    ))
                    .unwrap_or_default()
            ));
        }
        if !c.decisions.is_empty() {
            lines.push("Lift = how much more often than its expected win rate a held-out game that followed the pick was won, minus those that didn't.".into());
            for d in &c.decisions {
                lines.push(format!(
                    "  {} against the draft {} · today {} · picks differ in {} % of games",
                    pad_end(&d.key, 14),
                    pad_start(&pts(d.fair), 11),
                    pad_start(&pts(d.classic), 11),
                    pct0(d.changed)
                ));
            }
            lines.push(format!(
                "lift: against the draft {}, today {}, difference {} (95 % CI {} to {}, pilot bootstrap)",
                pts(c.adjusted.fair),
                pts(c.adjusted.classic),
                pts(c.adjusted.delta),
                pts(c.adjusted.low),
                pts(c.adjusted.high)
            ));
            lines.push(format!("by the overall win rate instead: against the draft {}, today {}, difference {}", pts(c.raw.fair), pts(c.raw.classic), pts(c.raw.delta)));
            lines.push(format!(
                "Brier on the option taken: against the draft {}, today {} ({} decisions)",
                js::to_fixed(c.brier.fair, 4),
                js::to_fixed(c.brier.classic, 4),
                js::num_to_string(c.brier.n)
            ));
        }
        lines.push(format!("Runes and summoners against the draft: {} — {}", c.gate.to_uppercase(), c.gate_reason));
    }
    if let Some(t) = &r.tuning {
        let name = |s: &str| match s {
            "pilotStrength" => "pilot standing's strength",
            "itemK" => "item decisions' prior",
            _ => "fade within a patch (days to half)",
        };
        lines.push(String::new());
        lines.push("Fine-tuning".into());
        lines.push("-----------".into());
        lines.push("Each setting against the chosen draft model on the same held-out games; the better ones together on all of them.".into());
        for g in &t.grid {
            lines.push(format!("  {} {}: tuned {}, chosen {}, difference {}", name(&g.setting), js::num_to_string(g.value), pts(g.tuned), pts(g.current), pts(g.delta)));
        }
        lines.push(format!(
            "chosen: strength {}, item prior {}, fade {}",
            js::num_to_string(t.choice.pilot_strength),
            t.choice.item_k.map(js::num_to_string).unwrap_or_else(|| "as the rest".into()),
            t.choice.recency_days.map(js::num_to_string).unwrap_or_else(|| "none".into())
        ));
        if let Some(f) = &t.full {
            lines.push(format!(
                "all games: tuned {}, chosen {}, difference {} (95 % CI {} to {}, pilot bootstrap)",
                pts(f.tuned),
                pts(f.current),
                pts(f.delta),
                pts(f.low),
                pts(f.high)
            ));
            lines.push(format!("Brier on the option taken: tuned {}, chosen {}", js::to_fixed(f.brier, 4), js::to_fixed(f.current_brier, 4)));
        }
        lines.push(format!("Fine-tuning: {} — {}", t.gate.to_uppercase(), t.gate_reason));
    }
    if let Some(w) = &r.swaps {
        lines.push(String::new());
        lines.push("Late-game swaps".into());
        lines.push("---------------".into());
        lines.push("At shop visits from 20:00 with every slot full: those that did what the draft model advises for the item they held (keep it, or sell it for another) against those that didn't, compared with keeping every item; each game's visits count once together.".into());
        lines.push(format!(
            "{} situations in {} held-out games; sold in {} %, advised to sell in {} %",
            js::num_to_string(w.situations),
            js::num_to_string(w.games),
            pct0(w.swapped),
            pct0(w.advised)
        ));
        if w.lift.low.is_finite() {
            lines.push(format!(
                "lift: the advice {}, keeping {}, difference {} (95 % CI {} to {}, pilot bootstrap)",
                pts(w.lift.advice),
                pts(w.lift.keep),
                pts(w.lift.delta),
                pts(w.lift.low),
                pts(w.lift.high)
            ));
        }
        lines.push(format!("Late-game swaps: {} — {}", w.gate.to_uppercase(), w.gate_reason));
    }
    lines.push(String::new());
    lines.push("Yours".into());
    lines.push("-----".into());
    match &r.yours {
        None => lines.push("no game of yours is linked to a shown build yet".into()),
        Some(y) => lines.push(format!(
            "{} linked games{}: followed the draft build {} ({} won), didn't {} ({} won)",
            js::num_to_string(y.linked),
            if y.linked < 20.0 { " (needs 20 to compare)" } else { "" },
            js::num_to_string(y.followed.games),
            js::num_to_string(y.followed.wins),
            js::num_to_string(y.other.games),
            js::num_to_string(y.other.wins)
        )),
    }
    lines.push(String::new());
    lines.push(format!("Gate: {} — {}", r.gate.to_uppercase(), r.gate_reason));
    lines.push(if r.gate == "passed" {
        "The app now shows the draft-adjusted build by default (you can still switch).".into()
    } else {
        "The app keeps the lane baseline by default (you can still switch to the draft-adjusted build).".into()
    });
    lines.join("\n")
}

fn secs(ms: f64) -> String {
    format!("{} s", js::to_fixed(ms / 1000.0, 1))
}
fn pct(ms: f64, total: f64) -> String {
    format!("{} %", js::num_to_string(if total > 0.0 { js::round((ms / total) * 100.0) } else { 0.0 }))
}

/// A step's kind: its label without its numbers (`/\d[\d,.]*/g` → "…").
pub fn step_kind(label: &str) -> String {
    let mut out = String::new();
    let chars: Vec<char> = label.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        if chars[i].is_ascii_digit() {
            i += 1;
            while i < chars.len() && (chars[i].is_ascii_digit() || chars[i] == ',' || chars[i] == '.') {
                i += 1;
            }
            out.push('…');
        } else {
            out.push(chars[i]);
            i += 1;
        }
    }
    out
}

/// Time by step (formatTimings).
pub fn format_timings(timings: &[StepTime]) -> String {
    let total: f64 = timings.iter().map(|s| s.ms).sum();
    let mut lines = vec!["Time by step (0.20.0)".to_string(), "---------------------".to_string()];
    for s in timings {
        lines.push(format!("  {}  {}  {}", pad_start(&secs(s.ms), 9), pad_start(&pct(s.ms, total), 5), s.label));
    }
    lines.push(format!("  {}         in all", pad_start(&secs(total), 9)));
    let mut kinds: indexmap::IndexMap<String, (f64, usize)> = indexmap::IndexMap::new();
    for s in timings {
        let k = kinds.entry(step_kind(&s.label)).or_insert((0.0, 0));
        k.0 += s.ms;
        k.1 += 1;
    }
    if kinds.len() < timings.len() {
        lines.push(String::new());
        lines.push("By kind, largest first:".into());
        let mut v: Vec<(String, (f64, usize))> = kinds.into_iter().collect();
        v.sort_by(|a, b| crate::engine::recommend::js_order(b.1 .0 - a.1 .0));
        for (k, (ms, n)) in v {
            lines.push(format!("  {}  {}  {}{}", pad_start(&secs(ms), 9), pad_start(&pct(ms, total), 5), k, if n > 1 { format!(" ({n} steps)") } else { String::new() }));
        }
    }
    lines.join("\n")
}

// ------------------------------------------------------------------------------------------- a data folder's evaluation

use super::eval_base::{
    eval_key, keep_verdicts, plan_evaluation, portable_key, read_base, read_base_header, regions_now, save_base, update_from, verdict_shift, BaseHeader,
    UPDATE_RULES,
};
use super::service::{eval_dir, EvalSummary, FullRef, Gate, GateChoice, GateDecay, GatePenalty, Mark};

/// What evaluate_data_folder takes.
pub struct FolderOptions<'a> {
    pub quick: bool,
    pub auto: bool,
    /// 'auto' (the plan's), 'update' (add to the base, whatever the plan says) or 'full'.
    pub path: &'a str,
    pub threads: usize,
    pub log: Box<dyn Fn(&str) + Sync + 'a>,
    pub progress: Box<dyn Fn(EvalProgress) + Sync + 'a>,
    /// false: the report is returned, not saved.
    pub save: bool,
    pub control: Option<Arc<super::evaluate::EvalControl>>,
    /// Grouped drafts (0.20.0, model/groups.rs): a trial with the drafts in this many groups (0: about 40 games each).
    /// Always a full evaluation, saved as eval-grouped-…; it never becomes the last evaluation the app reads
    /// (latest.json) nor a base for quick updates, since its numbers are close to an exact evaluation's, not the same.
    pub grouping: Option<usize>,
    /// Use GPU (0.13.0; crate::gpu): the draft similarities on the graphics card, checked first; none in champ select
    /// or a game.
    pub gpu: bool,
}

pub struct FolderResult {
    pub report: EvalReport,
    pub text: String,
    pub file: String,
    pub threads: usize,
    pub path: &'static str,
    pub reason: String,
    pub timings: Vec<StepTime>,
    pub ms: f64,
}

/// What a run took: its report and its wall time (evalCompare.ts's ComparedRun).
pub struct ComparedRun<'a> {
    pub report: &'a EvalReport,
    pub ms: f64,
}

/// Port of `evalCompare.ts` `compareReports`: an exact evaluation and a grouped-drafts trial of the same games side by
/// side: time, every verdict, the lifts, Brier scores and per-decision numbers, and how often held-out games followed
/// each model's pick. What to look at before grouping becomes the default: docs/EVAL-SCALING.md, "Adopting it".
pub fn compare_reports(exact: ComparedRun, grouped: ComparedRun) -> String {
    let secs = |ms: f64| format!("{} s", js::to_fixed(ms / 1000.0, 1));
    let pts = |x: f64| format!("{}{} pts", if x >= 0.0 { "+" } else { "" }, js::to_fixed(x * 100.0, 2));
    let (a, b) = (exact.report, grouped.report);
    let mut lines: Vec<String> = vec!["Exact evaluation against grouped drafts (0.20.0 trial)".into(), "-----------------------------------------------------".into()];
    lines.push(format!(
        "Games: {} (exact) and {} (grouped){}",
        js::thousands(a.games),
        js::thousands(b.games),
        if a.games != b.games { ": NOT THE SAME GAMES, the rest does not compare" } else { "" }
    ));
    if let Some(g) = &b.grouping {
        lines.push(format!("Groups: {}, spread {} sd around their centre, made in {}", js::thousands(g.groups), js::to_fixed(g.spread, 3), secs(g.ms)));
    }
    let less = if exact.ms > 0.0 { format!("{} % less", js::num_to_string(js::round((1.0 - grouped.ms / exact.ms) * 100.0))) } else { "n/a".into() };
    lines.push(format!("Time: {} exact, {} grouped ({less})", secs(exact.ms), secs(grouped.ms)));
    lines.push(String::new());
    let verdicts: [(&str, Option<String>, Option<String>); 5] = [
        ("Draft model gate", Some(a.gate.clone()), Some(b.gate.clone())),
        ("Patch memory", a.memory.as_ref().map(|m| m.gate.clone()), b.memory.as_ref().map(|m| m.gate.clone())),
        ("Runes and summoners against the draft", a.scoring.as_ref().map(|m| m.gate.clone()), b.scoring.as_ref().map(|m| m.gate.clone())),
        ("Fine-tuning", a.tuning.as_ref().map(|m| m.gate.clone()), b.tuning.as_ref().map(|m| m.gate.clone())),
        ("Late-game swaps", a.swaps.as_ref().map(|m| m.gate.clone()), b.swaps.as_ref().map(|m| m.gate.clone())),
    ];
    let mut flips = 0;
    lines.push("Verdicts (exact → grouped):".into());
    for (name, x, y) in verdicts {
        if x.is_none() && y.is_none() {
            continue;
        }
        let same = x == y;
        if !same {
            flips += 1;
        }
        lines.push(format!(
            "  {}{name}: {} → {}",
            if same { "  " } else { "≠ " },
            x.as_deref().unwrap_or("not run"),
            y.as_deref().unwrap_or("not run")
        ));
    }
    lines.push(String::new());
    lines.push(format!(
        "Draft model lift: {} ({} to {}) → {} ({} to {}), moved {}",
        pts(a.lift.delta),
        pts(a.lift.low),
        pts(a.lift.high),
        pts(b.lift.delta),
        pts(b.lift.low),
        pts(b.lift.high),
        pts(b.lift.delta - a.lift.delta)
    ));
    lines.push(format!(
        "Brier, draft model: {} → {}; baseline {} → {}",
        js::to_fixed(a.brier.draft, 4),
        js::to_fixed(b.brier.draft, 4),
        js::to_fixed(a.brier.baseline, 4),
        js::to_fixed(b.brier.baseline, 4)
    ));
    lines.push(format!(
        "Chosen settings: h={}, k={} → h={}, k={}",
        js::num_to_string(a.chosen.h),
        js::num_to_string(a.chosen.k),
        js::num_to_string(b.chosen.h),
        js::num_to_string(b.chosen.k)
    ));
    lines.push(String::new());
    lines.push("Per decision: lift (exact → grouped), held-out games that followed the draft model's pick (exact → grouped)".into());
    let mut moved: f64 = 0.0;
    for d in &a.decisions {
        let Some(e) = b.decisions.iter().find(|x| x.key == d.key) else {
            lines.push(format!("  {}: missing in the grouped run", d.key));
            continue;
        };
        moved = moved.max((e.draft - d.draft).abs());
        let follow = if d.followed_draft > 0.0 {
            format!(
                "{} → {} ({})",
                js::thousands(d.followed_draft),
                js::thousands(e.followed_draft),
                pts((e.followed_draft - d.followed_draft) / d.followed_draft).replace(" pts", " %")
            )
        } else {
            "n/a".into()
        };
        lines.push(format!("  {:<14} {} → {}   followed {follow}", d.key, pts(d.draft), pts(e.draft)));
    }
    lines.push(String::new());
    lines.push(format!(
        "Summary: {}; the largest per-decision move {}; the draft model's lift moved {}.",
        if flips == 0 { "every verdict the same".to_string() } else { format!("{flips} verdict{} differ", if flips > 1 { "s" } else { "" }) },
        pts(moved),
        pts(b.lift.delta - a.lift.delta)
    ));
    lines.join("\n")
}

/// The newest patch among the stored games' builds, first seen up to `before`.
pub fn newest_patch(versions: &[crate::patches::VersionRow], before: f64) -> Option<String> {
    let mut best: Option<String> = None;
    for v in versions {
        if (v.first_seen as f64) <= before && best.as_ref().map(|b| crate::ddragon::compare_patch(&v.patch, b) > 0.0).unwrap_or(true) {
            best = Some(v.patch.clone());
        }
    }
    best
}

/// The mark an evaluation records as it starts.
pub fn eval_mark(db: &Db) -> Mark {
    let (row, games) = db.game_mark();
    Mark { row: Some(row as f64), games: games as f64, patch: newest_patch(&db.game_versions().unwrap_or_default(), f64::INFINITY) }
}

struct Measured {
    out: EvalOutput,
    mark: Mark,
    data_version: String,
    read: Vec<String>,
    key: String,
    portable: String,
    regions: Vec<(String, super::eval_base::RegionPair)>,
    current: f64,
    ms: f64,
}

fn measure_folder(db: &Db, data: &Arc<StaticData>, o: &FolderOptions, base: Option<&super::eval_base::Base>) -> Result<Measured> {
    let mark = eval_mark(db);
    let data_version = db.data_version();
    let reads = eval_reads(db, data, o.quick)?;
    let mut plain = eval_options(&reads.scope, o.quick);
    if o.grouping.is_some() {
        plain.grouping = o.grouping;
    }
    let mut opts = plain.clone();
    if let Some(b) = base {
        opts.update = Some(update_from(&b.data, &b.header, plain.max_grid_games));
    }
    let started = std::time::Instant::now();
    let _gpu = if o.gpu {
        let control = o.control.clone();
        let busy: Option<crate::gpu::accelerate::Busy> = control.map(|c| Arc::new(move || c.busy.load(std::sync::atomic::Ordering::SeqCst)) as crate::gpu::accelerate::Busy);
        crate::gpu::accelerate::start_gpu("thread 1", busy)
    } else {
        None
    };
    let out = measure(db, data.clone(), RunOptions {
        quick: o.quick,
        threads: o.threads,
        log: Box::new(|l: &str| (o.log)(l)),
        progress: Box::new(|p: EvalProgress| (o.progress)(p)),
        opts: Some(opts),
        salt: None,
        base: base.map(|b| b.data.clone()),
        control: o.control.clone(),
    })?;
    let current = match &mark.patch {
        Some(p) => db.model_game_count(std::slice::from_ref(p), &reads.sources) as f64,
        None => 0.0,
    };
    Ok(Measured {
        key: eval_key(&plain, &reads.sources, data),
        portable: portable_key(&plain, &reads.sources, data),
        regions: regions_now(db, data)?,
        out,
        mark,
        data_version,
        read: reads.read,
        current,
        ms: started.elapsed().as_secs_f64() * 1000.0,
    })
}

/// Evaluates the stored games and saves the report next to the settings (evals/), with the next quick update's base and
/// the summary the app reads (latest.json).
pub fn evaluate_data_folder(db: &Db, data: Arc<StaticData>, o: FolderOptions) -> Result<FolderResult> {
    if let Some(g) = o.grouping {
        let run = measure_folder(db, &data, &o, None)?;
        let reason = format!("grouped drafts trial ({})", if g > 0 { format!("{g} groups") } else { "groups of about 40 games".into() });
        return save(db, run, &o, "full", reason, None);
    }
    let want = if o.quick { "full" } else { o.path };
    let mut reason = if o.quick { "quick check (--quick)".to_string() } else { "full evaluation, as asked".to_string() };
    if want != "full" {
        let plan = plan_evaluation(db, &data, crate::util::now_ms() as f64, None, None, None)?;
        let base = if want == "update" || plan.path != "full" { read_base() } else { None };
        match base {
            Some(base) => {
                let mut run = measure_folder(db, &data, &o, Some(&base))?;
                let before = base.data.report.clone().expect("a base has its report");
                let (decisive, close) = verdict_shift(&before, &run.out.report);
                let u = run.out.report.update.clone();
                let stale = u.as_ref().and_then(|u| u.missing).unwrap_or(0.0) + u.as_ref().and_then(|u| u.changed).unwrap_or(0.0);
                let gone = stale > UPDATE_RULES.max_missing * base.data.known.len() as f64;
                if want == "update" || (decisive.is_none() && !gone) {
                    let why = if plan.path == "update" || plan.path == "none" { plan.reason.clone() } else { format!("a quick update, as asked ({})", plan.reason) };
                    if !close.is_empty() {
                        run.out.report = keep_verdicts(&before, &run.out.report, &close);
                    }
                    if let Some(up) = run.out.report.update.as_mut() {
                        up.full_at = base.header.full_at;
                        up.reason = why.clone();
                        if !close.is_empty() {
                            up.close = Some(close.clone());
                        }
                    }
                    return save(db, run, &o, "update", why, Some(&base.header));
                }
                reason = if gone {
                    format!(
                        "{} of the {} games the last full evaluation measured are no longer held out or changed since",
                        crate::js::thousands(stale),
                        crate::js::thousands(base.data.known.len() as f64)
                    )
                } else {
                    format!("the new games move {}'s verdict clearly, so a full evaluation decides it", decisive.unwrap_or_default())
                };
                (o.log)(&format!("quick update not kept: {reason}"));
            }
            None => {
                reason = if plan.path == "full" { plan.reason.clone() } else { "no base to add the new games to".into() };
            }
        }
    }
    let run = measure_folder(db, &data, &o, None)?;
    save(db, run, &o, "full", reason, None)
}

fn stamp(ms: f64) -> String {
    let dt = chrono::DateTime::from_timestamp_millis(ms as i64).unwrap_or_default();
    dt.format("%Y-%m-%d-%H-%M").to_string()
}

fn save(_db: &Db, run: Measured, o: &FolderOptions, ran: &'static str, reason: String, header: Option<&BaseHeader>) -> Result<FolderResult> {
    let report = run.out.report.clone();
    let timings = run.out.timings.clone();
    let mut text = String::new();
    if ran == "update" {
        text += &format!("Quick update of the full evaluation of {}: {reason}.\n\n", iso_minutes(header.unwrap().full_at));
    }
    text += &format_eval(&report);
    if !timings.is_empty() {
        text += &format!("\n\n{}", format_timings(&timings));
    }
    let result = |text: String, file: String| FolderResult { report: report.clone(), text, file, threads: o.threads, path: ran, reason: reason.clone(), timings: timings.clone(), ms: run.ms };
    if !o.save {
        return Ok(result(text, String::new()));
    }
    let dir = eval_dir();
    std::fs::create_dir_all(&dir)?;
    let trial = report.grouping.is_some();
    let name = format!("eval-{}{}{}{}", if trial { "grouped-" } else { "" }, if o.auto { "auto-" } else { "" }, if ran == "update" { "update-" } else { "" }, stamp(report.evaluated_at));
    let file = dir.join(format!("{name}.txt"));
    let mut json = serde_json::to_value(&report)?;
    json["timings"] = serde_json::to_value(&timings)?;
    std::fs::write(dir.join(format!("{name}.json")), format!("{}\n", serde_json::to_string_pretty(&json)?))?;
    std::fs::write(&file, format!("{text}\n"))?;
    let file_s = file.to_string_lossy().to_string();
    // A grouped trial is kept beside the reports and nothing else: the app's verdicts and the next quick update stay the
    // last exact evaluation's.
    if trial {
        return Ok(result(text, file_s));
    }
    let (full_at, full_mark) = match (ran, header) {
        ("update", Some(h)) => (h.full_at, h.full_mark.clone()),
        _ => (report.evaluated_at, run.mark.clone()),
    };
    if ran == "update" && read_base_header().map(|h| h.last_at) != header.map(|h| h.last_at) {
        return Ok(result(format!("{text}\n(Not kept: another evaluation finished meanwhile.)"), file_s));
    }
    // A --quick check measures too little to be the next quick update's base.
    if let Some(next) = run.out.next.as_ref().filter(|_| !o.quick && o.grouping.is_none()) {
        save_base(next, BaseHeader {
            logic: 0.0,
            full_at,
            full_mark: full_mark.clone(),
            full_held_out: if ran == "update" { header.unwrap().full_held_out } else { next.known.len() as f64 },
            full_patch: if ran == "update" { header.unwrap().full_patch.clone() } else { run.mark.patch.clone() },
            full_current: if ran == "update" { header.unwrap().full_current } else { run.current },
            last_at: report.evaluated_at,
            last_mark: run.mark.clone(),
            known: 0.0,
            patches: run.read.clone(),
            regions: run.regions.clone(),
            key: run.key.clone(),
            portable: Some(run.portable.clone()),
            imported_from: None,
        })?;
    }
    let summary = EvalSummary {
        evaluated_at: report.evaluated_at,
        gate: report.gate.clone(),
        bandwidth: report.chosen.h,
        k: report.chosen.k,
        lambda: report.state.as_ref().map(|s| s.lambda).unwrap_or(1.0),
        data_version: run.data_version.clone(),
        memory: report.memory.as_ref().map(|m| GateDecay { gate: m.gate.clone(), decay: m.decay }),
        scoring: report.scoring.as_ref().map(|s| GatePenalty { gate: s.gate.clone(), penalty: s.expected.as_ref().and_then(|e| e.penalty) }),
        tuning: report.tuning.as_ref().map(|t| GateChoice { gate: t.gate.clone(), choice: t.choice }),
        swaps: report.swaps.as_ref().map(|s| Gate { gate: s.gate.clone() }),
        sample: report.state.as_ref().and_then(|s| s.sample.as_ref()).map(|s| s.kept.clone()),
        window: report.window.as_ref().map(|w| Gate { gate: w.gate.clone() }),
        mark: Some(run.mark.clone()),
        path: Some(ran.into()),
        full: Some(FullRef { evaluated_at: full_at, mark: full_mark }),
        imported_from: None,
    };
    write_summary(&summary)?;
    if o.auto {
        prune_auto_reports(&dir, 20);
    }
    Ok(result(text, file_s))
}

/// latest.json, as 0.22.0 writes it (Infinity as null).
pub fn write_summary(summary: &EvalSummary) -> Result<()> {
    std::fs::create_dir_all(eval_dir())?;
    std::fs::write(eval_dir().join("latest.json"), format!("{}\n", serde_json::to_string_pretty(summary)?))?;
    Ok(())
}

/// Keeps the newest `keep` automatic reports (their names sort by time).
pub fn prune_auto_reports(dir: &std::path::Path, keep: usize) {
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    let mut names: Vec<(String, String)> = Vec::new();
    for e in rd.flatten() {
        let f = e.file_name().to_string_lossy().to_string();
        let Some(stem) = f.strip_suffix(".txt").or_else(|| f.strip_suffix(".json")) else { continue };
        let Some(rest) = stem.strip_prefix("eval-auto-") else { continue };
        let st = rest.strip_prefix("update-").unwrap_or(rest);
        if st.chars().all(|c| c.is_ascii_digit() || c == '-') && !st.is_empty() {
            let key = (st.to_string(), stem.to_string());
            if !names.contains(&key) {
                names.push(key);
            }
        }
    }
    names.sort();
    let n = names.len();
    for (_, name) in names.into_iter().take(n.saturating_sub(keep)) {
        for ext in ["txt", "json"] {
            let _ = std::fs::remove_file(dir.join(format!("{name}.{ext}")));
        }
    }
}

/// How many stored games an evaluation started now would read.
pub fn eval_size(db: &Db, data: &StaticData) -> f64 {
    match eval_reads(db, data, false) {
        Ok(r) => db.model_game_count(&r.read, &r.sources) as f64,
        Err(_) => 0.0,
    }
}
