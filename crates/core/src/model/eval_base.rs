//! Port of `server/model/evalBase.ts`: quick updates between full evaluations. A full evaluation saves its base next to
//! its report (evals/base.json + base.bin); a quick update adds the new held-out games' outcomes to it. planEvaluation
//! says which one Evaluate builds runs now. Nothing here is written to the data folder.
//!
//! base.bin: 0.22.0 wrote it with V8's serializer; 1.0 writes its own binary form (zstd). A base is only ever added to
//! by the app version that wrote it (the key holds the version), so the change costs nothing: 1.0's first evaluation is
//! a full one either way, and 0.22.0 treats 1.0's base as none.

use super::evaluate::{EvalBaseData, EvalOptions, EvalReport, EvalUpdate, GroupedOutcomes, KeyOutcomes, ScoringBase};
use super::expected::Penalty;
use super::run_eval::{eval_options, eval_reads, EvalReads};
use super::service::{eval_dir, read_evaluation, EvalSummary, Mark};
use crate::capture::CAPTURE_VERSION;
use crate::config::config;
use crate::db::Db;
use crate::ddragon::{compare_patch, StaticData};
use crate::patches::{region_patches, Fallback};
use anyhow::{anyhow, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::path::PathBuf;

/// How evaluate measures: a base saved by another version can't be added to.
pub const EVAL_LOGIC: f64 = 2.0;

pub struct UpdateRules {
    pub max_growth: f64,
    pub current_growth: f64,
    pub max_age_days: f64,
    pub max_missing: f64,
    pub margin: f64,
}
pub const UPDATE_RULES: UpdateRules = UpdateRules { max_growth: 0.2, current_growth: 0.3, max_age_days: 7.0, max_missing: 0.02, margin: 0.25 };

/// What Evaluate builds would run now, and why (EvaluationPlan).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EvalPlan {
    pub path: String,
    pub reason: String,
    pub new_games: f64,
    pub since_full: f64,
    pub full_at: Option<f64>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ImportedFrom {
    pub machine: String,
    pub exported_at: f64,
}

/// The base's header (base.json).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BaseHeader {
    pub logic: f64,
    pub full_at: f64,
    pub full_mark: Mark,
    pub full_held_out: f64,
    pub full_patch: Option<String>,
    pub full_current: f64,
    pub last_at: f64,
    pub last_mark: Mark,
    pub known: f64,
    pub patches: Vec<String>,
    pub regions: Vec<(String, RegionPair)>,
    pub key: String,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub portable: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub imported_from: Option<ImportedFrom>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RegionPair {
    pub patch: String,
    pub previous_patch: String,
}

/// The app's version (Cargo's): a new version may measure differently, so its first evaluation is a full one.
pub fn app_version() -> String {
    std::env::var("IRELIA_APP_VERSION").unwrap_or_else(|_| env!("CARGO_PKG_VERSION").to_string())
}

fn key_of(opts: &EvalOptions, sources: &[String], data: &StaticData, with_folder: bool) -> String {
    let mut o = opts.clone();
    o.update = None;
    let mut srcs = sources.to_vec();
    srcs.sort();
    let e = &opts.engine;
    let parts = json!({
        "logic": EVAL_LOGIC, "app": app_version(), "ddragon": data.version, "capture": CAPTURE_VERSION,
        "dataDir": if with_folder { Value::String(crate::util::resolve(config().data_dir).to_string_lossy().to_string()) } else { Value::Null },
        "options": format!("{:?}", o), "engine": format!("{:?}", e), "sources": srcs,
    });
    let h = Sha256::digest(crate::js::stringify(&parts).as_bytes());
    h.iter().map(|b| format!("{b:02x}")).collect::<String>()[..24].to_string()
}

pub fn eval_key(opts: &EvalOptions, sources: &[String], data: &StaticData) -> String {
    key_of(opts, sources, data, true)
}
pub fn portable_key(opts: &EvalOptions, sources: &[String], data: &StaticData) -> String {
    key_of(opts, sources, data, false)
}
pub fn current_key(db: &Db, data: &StaticData) -> Result<String> {
    let r = eval_reads(db, data, false)?;
    Ok(eval_key(&eval_options(&r.scope, false), &r.sources, data))
}
pub fn current_portable_key(db: &Db, data: &StaticData) -> Result<String> {
    let r = eval_reads(db, data, false)?;
    Ok(portable_key(&eval_options(&r.scope, false), &r.sources, data))
}

fn header_file() -> PathBuf {
    eval_dir().join("base.json")
}
pub fn base_file() -> PathBuf {
    eval_dir().join("base.bin")
}

pub fn read_base_header() -> Option<BaseHeader> {
    let t = std::fs::read_to_string(header_file()).ok()?;
    let h: BaseHeader = serde_json::from_str(&t).ok()?;
    (h.logic == EVAL_LOGIC && base_file().exists()).then_some(h)
}

// ---------------------------------------------------------------------------------- base.bin (1.0's form)

const MAGIC: &[u8] = b"IRELIA-BASE\x01";

struct W(Vec<u8>);
impl W {
    fn u64(&mut self, x: u64) {
        self.0.extend_from_slice(&x.to_le_bytes());
    }
    fn bytes(&mut self, b: &[u8]) {
        self.u64(b.len() as u64);
        self.0.extend_from_slice(b);
    }
    fn str(&mut self, s: &str) {
        self.bytes(s.as_bytes());
    }
    fn f64s(&mut self, xs: &[f64]) {
        self.u64(xs.len() as u64);
        for x in xs {
            self.0.extend_from_slice(&x.to_bits().to_le_bytes());
        }
    }
    fn opt(&mut self, some: bool) {
        self.0.push(some as u8);
    }
    fn json(&mut self, v: &impl Serialize) -> Result<()> {
        let s = serde_json::to_string(v)?;
        self.str(&s);
        Ok(())
    }
    fn grouped(&mut self, g: &GroupedOutcomes) {
        self.u64(g.pilots.len() as u64);
        for p in &g.pilots {
            self.str(p);
        }
        self.u64(g.by_key.len() as u64);
        for k in &g.by_key {
            self.u64(k.pilot.len() as u64);
            for x in &k.pilot {
                self.0.extend_from_slice(&x.to_le_bytes());
            }
            self.f64s(&k.wpa);
            self.bytes(&k.draft);
            self.bytes(&k.baseline);
            self.opt(k.w.is_some());
            if let Some(w) = &k.w {
                self.f64s(w);
            }
        }
        self.opt(g.keys.is_some());
        if let Some(keys) = &g.keys {
            self.u64(keys.len() as u64);
            for k in keys {
                self.str(k);
            }
        }
    }
}

struct R<'a>(&'a [u8], usize);
impl R<'_> {
    fn take(&mut self, n: usize) -> Result<&[u8]> {
        if self.1 + n > self.0.len() {
            return Err(anyhow!("base.bin is cut short"));
        }
        let s = &self.0[self.1..self.1 + n];
        self.1 += n;
        Ok(s)
    }
    fn u64(&mut self) -> Result<u64> {
        Ok(u64::from_le_bytes(self.take(8)?.try_into()?))
    }
    fn bytes(&mut self) -> Result<Vec<u8>> {
        let n = self.u64()? as usize;
        Ok(self.take(n)?.to_vec())
    }
    fn str(&mut self) -> Result<String> {
        Ok(String::from_utf8(self.bytes()?)?)
    }
    fn f64s(&mut self) -> Result<Vec<f64>> {
        let n = self.u64()? as usize;
        let b = self.take(n * 8)?;
        Ok(b.chunks_exact(8).map(|c| f64::from_bits(u64::from_le_bytes(c.try_into().unwrap()))).collect())
    }
    fn opt(&mut self) -> Result<bool> {
        Ok(self.take(1)?[0] == 1)
    }
    fn json<T: for<'de> Deserialize<'de>>(&mut self) -> Result<T> {
        Ok(serde_json::from_str(&self.str()?)?)
    }
    fn grouped(&mut self) -> Result<GroupedOutcomes> {
        let n = self.u64()? as usize;
        let mut pilots = Vec::with_capacity(n);
        for _ in 0..n {
            pilots.push(self.str()?);
        }
        let k = self.u64()? as usize;
        let mut by_key = Vec::with_capacity(k);
        for _ in 0..k {
            let m = self.u64()? as usize;
            let b = self.take(m * 4)?;
            let pilot = b.chunks_exact(4).map(|c| i32::from_le_bytes(c.try_into().unwrap())).collect();
            let wpa = self.f64s()?;
            let draft = self.bytes()?;
            let baseline = self.bytes()?;
            let w = if self.opt()? { Some(self.f64s()?) } else { None };
            by_key.push(KeyOutcomes { pilot, wpa, draft, baseline, w });
        }
        let keys = if self.opt()? {
            let n = self.u64()? as usize;
            let mut v = Vec::with_capacity(n);
            for _ in 0..n {
                v.push(self.str()?);
            }
            Some(v)
        } else {
            None
        };
        Ok(GroupedOutcomes { pilots, by_key, keys })
    }
}

pub fn encode_base(d: &EvalBaseData) -> Result<Vec<u8>> {
    let mut w = W(Vec::new());
    w.str(&d.salt);
    w.f64s(&d.known);
    w.f64s(&d.prints);
    w.opt(d.report.is_some());
    if let Some(r) = &d.report {
        w.json(r)?;
    }
    w.opt(d.final_.is_some());
    if let Some((g, a)) = &d.final_ {
        w.grouped(g);
        w.f64s(a);
    }
    w.opt(d.memory.is_some());
    if let Some((g, a, h)) = &d.memory {
        w.grouped(g);
        w.f64s(a);
        w.grouped(h);
    }
    w.opt(d.scoring.is_some());
    if let Some(s) = &d.scoring {
        w.grouped(&s.adjusted);
        w.grouped(&s.raw);
        w.f64s(&s.brier);
        w.u64(s.changed.len() as u64);
        for (k, v) in &s.changed {
            w.str(k);
            w.f64s(v);
        }
        w.f64s(&s.expected_p);
        w.f64s(&s.expected_y);
        w.f64s(&s.expected_pb);
        w.json(&s.penalties)?;
    }
    w.opt(d.tuning.is_some());
    if let Some((g, a)) = &d.tuning {
        w.grouped(g);
        w.f64s(a);
    }
    w.opt(d.swaps.is_some());
    if let Some((g, a)) = &d.swaps {
        w.grouped(g);
        w.f64s(a);
    }
    let mut out = MAGIC.to_vec();
    out.extend(zstd::encode_all(&w.0[..], 3)?);
    Ok(out)
}

pub fn decode_base(bytes: &[u8]) -> Result<EvalBaseData> {
    if !bytes.starts_with(MAGIC) {
        return Err(anyhow!("not a base saved by this version"));
    }
    let raw = zstd::decode_all(&bytes[MAGIC.len()..])?;
    let mut r = R(&raw, 0);
    let arr = |v: Vec<f64>, n: usize| -> Result<Vec<f64>> { if v.len() == n { Ok(v) } else { Err(anyhow!("base.bin has a wrong length")) } };
    let salt = r.str()?;
    let known = r.f64s()?;
    let prints = r.f64s()?;
    let report: Option<EvalReport> = if r.opt()? { Some(r.json()?) } else { None };
    let final_ = if r.opt()? {
        let g = r.grouped()?;
        let a = arr(r.f64s()?, 3)?;
        Some((g, [a[0], a[1], a[2]]))
    } else {
        None
    };
    let memory = if r.opt()? {
        let g = r.grouped()?;
        let a = arr(r.f64s()?, 2)?;
        let h = r.grouped()?;
        Some((g, [a[0], a[1]], h))
    } else {
        None
    };
    let scoring = if r.opt()? {
        let adjusted = r.grouped()?;
        let raw_ = r.grouped()?;
        let b = arr(r.f64s()?, 3)?;
        let n = r.u64()? as usize;
        let mut changed = Vec::with_capacity(n);
        for _ in 0..n {
            let k = r.str()?;
            let v = arr(r.f64s()?, 2)?;
            changed.push((k, [v[0], v[1]]));
        }
        let expected_p = r.f64s()?;
        let expected_y = r.f64s()?;
        let expected_pb = r.f64s()?;
        let penalties: Vec<Option<Penalty>> = r.json()?;
        Some(ScoringBase { adjusted, raw: raw_, brier: [b[0], b[1], b[2]], changed, expected_p, expected_y, expected_pb, penalties })
    } else {
        None
    };
    let tuning = if r.opt()? {
        let g = r.grouped()?;
        let a = arr(r.f64s()?, 2)?;
        Some((g, [a[0], a[1]]))
    } else {
        None
    };
    let swaps = if r.opt()? {
        let g = r.grouped()?;
        let a = arr(r.f64s()?, 4)?;
        Some((g, [a[0], a[1], a[2], a[3]]))
    } else {
        None
    };
    Ok(EvalBaseData { salt, known, prints, report, final_, memory, scoring, tuning, swaps })
}

/// Saves `data` as the base: its outcomes first (through a temporary file), then the header.
pub fn save_base(data: &EvalBaseData, mut header: BaseHeader) -> Result<()> {
    std::fs::create_dir_all(eval_dir())?;
    let tmp = eval_dir().join("base.bin.tmp");
    std::fs::write(&tmp, encode_base(data)?)?;
    std::fs::rename(&tmp, base_file())?;
    header.logic = EVAL_LOGIC;
    header.known = data.known.len() as f64;
    std::fs::write(header_file(), format!("{}\n", serde_json::to_string_pretty(&header)?))?;
    Ok(())
}

pub struct Base {
    pub header: BaseHeader,
    pub data: EvalBaseData,
}

/// The base, when one is saved by this version and its outcomes are its header's.
pub fn read_base() -> Option<Base> {
    let header = read_base_header()?;
    let data = decode_base(&std::fs::read(base_file()).ok()?).ok()?;
    let whole = data.report.as_ref().map(|r| r.evaluated_at) == Some(header.last_at) && data.known.len() as f64 == header.known && data.prints.len() == data.known.len();
    whole.then_some(Base { header, data })
}

/// What a quick update takes from `data`.
pub fn update_from(data: &EvalBaseData, header: &BaseHeader, max_grid_games: usize) -> EvalUpdate {
    let r = data.report.as_ref().expect("a base has its report");
    EvalUpdate {
        chosen: r.chosen,
        lambda: r.state.as_ref().map(|s| s.lambda).unwrap_or(1.0),
        decay: r.memory.as_ref().filter(|m| !m.grid.is_empty()).map(|m| m.decay),
        tuning: r.tuning.as_ref().filter(|t| t.full.is_some()).map(|t| t.choice),
        salt: data.salt.clone(),
        known: data.known.clone(),
        prints: data.prints.clone(),
        penalties: data.scoring.as_ref().map(|s| s.penalties.clone()),
        grid_stride: 1usize.max((header.full_held_out / max_grid_games as f64).ceil() as usize),
        sample: super::window::StateSample::from_name(r.state.as_ref().and_then(|s| s.sample.as_ref()).map(|s| s.kept.as_str())),
        window: r.window.as_ref().map(|w| w.kept == "window").unwrap_or(false),
    }
}

fn clear(low: f64, high: f64, passed: bool) -> bool {
    let m = UPDATE_RULES.margin * ((high - low) / 2.0);
    if passed {
        low > m
    } else {
        low < -m
    }
}

/// The verdicts a quick update would decide otherwise than its base: the first moved clearly across the line
/// (`decisive`), and those only moved onto it (`close`).
pub fn verdict_shift(before: &EvalReport, after: &EvalReport) -> (Option<String>, Vec<String>) {
    let iv = |l: f64, h: f64| (l, h);
    let gates: Vec<(&str, Option<String>, Option<String>, Vec<(f64, f64)>, bool)> = vec![
        ("the draft-adjusted builds", Some(before.gate.clone()), Some(after.gate.clone()), vec![iv(after.lift.low, after.lift.high)], false),
        (
            "patch memory",
            before.memory.as_ref().map(|m| m.gate.clone()),
            after.memory.as_ref().map(|m| m.gate.clone()),
            after.memory.as_ref().filter(|m| !m.grid.is_empty()).map(|m| vec![iv(m.full.low, m.full.high), iv(m.early.low, m.early.high)]).unwrap_or_default(),
            true,
        ),
        (
            "runes and summoners against the draft",
            before.scoring.as_ref().map(|s| s.gate.clone()),
            after.scoring.as_ref().map(|s| s.gate.clone()),
            after.scoring.as_ref().filter(|s| !s.decisions.is_empty()).map(|s| vec![iv(s.adjusted.low, s.adjusted.high)]).unwrap_or_default(),
            false,
        ),
        (
            "fine-tuning",
            before.tuning.as_ref().map(|t| t.gate.clone()),
            after.tuning.as_ref().map(|t| t.gate.clone()),
            after.tuning.as_ref().and_then(|t| t.full.as_ref()).map(|f| vec![iv(f.low, f.high)]).unwrap_or_default(),
            false,
        ),
        (
            "late-game swaps",
            before.swaps.as_ref().map(|s| s.gate.clone()),
            after.swaps.as_ref().map(|s| s.gate.clone()),
            after.swaps.as_ref().map(|s| vec![iv(s.lift.low, s.lift.high)]).unwrap_or_default(),
            false,
        ),
    ];
    let mut close = Vec::new();
    for (name, was, now, intervals, any) in gates {
        if was == now {
            continue;
        }
        let passed = now.as_deref() == Some("passed");
        let finite: Vec<(f64, f64)> = intervals.into_iter().filter(|x| x.0.is_finite() && x.1.is_finite()).collect();
        let decisive = !finite.is_empty() && if passed && any { finite.iter().any(|x| clear(x.0, x.1, true)) } else { finite.iter().all(|x| clear(x.0, x.1, passed)) };
        if decisive {
            return (Some(name.to_string()), close);
        }
        close.push(name.to_string());
    }
    (None, close)
}

/// `after` with `before`'s verdict on the gates named, each saying so.
pub fn keep_verdicts(before: &EvalReport, after: &EvalReport, names: &[String]) -> EvalReport {
    let note = |reason: &str, was: &str| format!("{reason} (the new games moved it too little to change the full evaluation's {}: it stands)", was.to_uppercase());
    let mut r = after.clone();
    let has = |n: &str| names.iter().any(|x| x == n);
    if has("the draft-adjusted builds") {
        r.gate_reason = note(&r.gate_reason, &before.gate);
        r.gate = before.gate.clone();
    }
    if has("patch memory") {
        if let (Some(m), Some(b)) = (r.memory.as_mut(), before.memory.as_ref()) {
            m.gate_reason = note(&m.gate_reason, &b.gate);
            m.gate = b.gate.clone();
        }
    }
    if has("runes and summoners against the draft") {
        if let (Some(m), Some(b)) = (r.scoring.as_mut(), before.scoring.as_ref()) {
            m.gate_reason = note(&m.gate_reason, &b.gate);
            m.gate = b.gate.clone();
        }
    }
    if has("fine-tuning") {
        if let (Some(m), Some(b)) = (r.tuning.as_mut(), before.tuning.as_ref()) {
            m.gate_reason = note(&m.gate_reason, &b.gate);
            m.gate = b.gate.clone();
        }
    }
    if has("late-game swaps") {
        if let (Some(m), Some(b)) = (r.swaps.as_mut(), before.swaps.as_ref()) {
            m.gate_reason = note(&m.gate_reason, &b.gate);
            m.gate = b.gate.clone();
        }
    }
    r
}

fn games_since(db: &Db, mark: &Mark) -> f64 {
    let (_, games) = db.game_mark();
    let after = mark.row.map(|r| db.games_after_row(r as i64) as f64).unwrap_or(0.0);
    (games as f64 - mark.games).max(0.0).max(after)
}

fn day(ms: f64) -> String {
    let dt = chrono::DateTime::from_timestamp_millis(ms as i64).unwrap_or_default();
    // en-GB day + short month ("21 Oct"): in the computer's own time zone.
    let local = dt.with_timezone(&chrono::Local);
    local.format("%-d %b").to_string()
}

fn count(n: f64) -> String {
    crate::js::thousands(n)
}

/// Each region's current and previous patch now, sorted.
pub fn regions_now(db: &Db, data: &StaticData) -> Result<Vec<(String, RegionPair)>> {
    let fb = Fallback { patch: data.patch.clone(), previous_patch: data.previous_patch.clone() };
    let mut v: Vec<(String, RegionPair)> = region_patches(&db.game_versions()?, &fb, &[], crate::util::now_ms())
        .into_iter()
        .map(|(p, r)| (p, RegionPair { patch: r.patch, previous_patch: r.previous_patch }))
        .collect();
    v.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(v)
}

/// What Evaluate builds runs now.
pub fn plan_evaluation(db: &Db, data: &StaticData, now: f64, header: Option<Option<BaseHeader>>, summary: Option<Option<EvalSummary>>, key: Option<String>) -> Result<EvalPlan> {
    let header = header.unwrap_or_else(read_base_header);
    let summary = summary.unwrap_or_else(read_evaluation);
    let total = db.game_mark().1 as f64;
    let new_games = match summary.as_ref().and_then(|s| s.mark.as_ref()) {
        Some(m) => games_since(db, m),
        None => total,
    };
    let full = |reason: String, since_full: f64, full_at: Option<f64>| EvalPlan { path: "full".into(), reason, new_games, since_full, full_at };
    let Some(summary) = summary else { return Ok(full("no evaluation yet".into(), total, None)) };
    let Some(header) = header else { return Ok(full("no full evaluation saved by this version yet".into(), new_games, None)) };
    let fa = Some(header.full_at);
    let since_full = games_since(db, &header.full_mark);
    if summary.evaluated_at != header.last_at {
        return Ok(full("the last evaluation saved nothing to add to".into(), since_full, fa));
    }
    let EvalReads { read, sources, .. } = eval_reads(db, data, false)?;
    let mut newer: Vec<&String> = read.iter().filter(|p| !header.patches.contains(p)).collect();
    newer.sort_by(|a, b| crate::engine::recommend::js_order(compare_patch(b, a)));
    if let Some(n) = newer.first() {
        return Ok(full(format!("new patch {n}"), since_full, fa));
    }
    if read.len() != header.patches.len() {
        return Ok(full("the patches an evaluation reads changed".into(), since_full, fa));
    }
    let regions = regions_now(db, data)?;
    let moved = regions.iter().find(|(platform, r)| header.regions.iter().find(|x| &x.0 == platform).map(|x| &x.1) != Some(r));
    if moved.is_some() || regions.len() != header.regions.len() {
        return Ok(full(
            match moved {
                Some((p, r)) => format!("{p} is on patch {} since the full evaluation", r.patch),
                None => "the regions stored changed".into(),
            },
            since_full,
            fa,
        ));
    }
    let key = match key {
        Some(k) => k,
        None => current_key(db, data)?,
    };
    if key != header.key {
        return Ok(full("the app, Data Dragon, the data folder or settings the evaluation depends on changed".into(), since_full, fa));
    }
    if since_full > UPDATE_RULES.max_growth * header.full_held_out {
        return Ok(full(
            format!("{} games since the full evaluation of {} (over {} % more)", count(since_full), day(header.full_at), crate::js::num_to_string(crate::js::round(UPDATE_RULES.max_growth * 100.0))),
            since_full,
            fa,
        ));
    }
    if let Some(fp) = &header.full_patch {
        let current = db.model_game_count(std::slice::from_ref(fp), &sources) as f64 - header.full_current;
        if current > UPDATE_RULES.current_growth * header.full_current {
            return Ok(full(
                format!("{} more games of patch {fp} since the full evaluation of {}, which had {}", count(current), day(header.full_at), count(header.full_current)),
                since_full,
                fa,
            ));
        }
    }
    if now - header.full_at > UPDATE_RULES.max_age_days * 86_400_000.0 {
        return Ok(full(format!("the last full evaluation is over {} days old ({})", crate::js::num_to_string(UPDATE_RULES.max_age_days), day(header.full_at)), since_full, fa));
    }
    if new_games == 0.0 {
        return Ok(EvalPlan { path: "none".into(), reason: "up to date: no new games since the last evaluation".into(), new_games, since_full, full_at: fa });
    }
    Ok(EvalPlan {
        path: "update".into(),
        new_games,
        since_full,
        full_at: fa,
        reason: format!("{} new games since the last evaluation (full evaluation of {})", count(new_games), day(header.full_at)),
    })
}
