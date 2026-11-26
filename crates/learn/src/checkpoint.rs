//! Versioned checkpoints on disk, in the directory the caller gives (data/checkpoints/):
//!
//! - `manifest.json`: every checkpoint with its held-out metrics, which ones are active, the training cursor;
//! - `v000012.logistic.bin`, `v000013.shared-nn.bin`: one model each (binary, zstd through bf_core::codec). Weights
//!   are stored as exact f64 bits, so a reloaded model predicts bit for bit like the one saved;
//! - `contexts.bin`: recent situations per champion-role (the recommender's "contexts like the user's");
//! - `learning.log`: one line per update.
//!
//! The manifest is written last and atomically, so a crash mid-update leaves the previous state in force.

use crate::features::{PatchClock, Situation};
use crate::logistic::{LogisticModel, DIM};
use crate::nn::{SharedNet, PARAMS};
use crate::teamnet::{TeamNet, TEAM_PARAMS};
use anyhow::{bail, Context, Result};
use bf_core::build::{CheckpointInfo, Metrics};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};

pub const LOGISTIC: &str = "logistic";
pub const SHARED_NN: &str = "shared-nn";
/// Model step 3, the team network (1.3.0).
pub const TEAM_NN: &str = "team-nn";
/// Checkpoints kept per kind (besides the active ones).
pub const KEEP: usize = 8;
/// Log lines kept in the file (older ones are cut when it grows past twice this).
const LOG_LINES: usize = 2000;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Entry {
    pub version: String,
    pub kind: String,
    pub created_ms: i64,
    pub metrics: Metrics,
    pub records_seen: u64,
    pub file: String,
    pub bytes: u64,
    /// For a network: the logistic version it was trained on top of.
    pub parent: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Manifest {
    pub format: u32,
    pub next: u64,
    /// The last record sequence number trained on (DataSource::records_after).
    pub cursor: u64,
    pub records_seen: u64,
    pub active_logistic: Option<String>,
    pub active_nn: Option<String>,
    pub clock: PatchClock,
    pub checkpoints: Vec<Entry>,
    /// The team network (1.3.0): games with a timeline learnt so far (towards its threshold), whether it is awake,
    /// how far it got through the stored backlog, and the version in force.
    #[serde(default)]
    pub timeline_seen: u64,
    #[serde(default)]
    pub team_awake: bool,
    #[serde(default)]
    pub team_cursor: u64,
    #[serde(default)]
    pub active_team: Option<String>,
}

impl Manifest {
    pub fn entry(&self, version: &str) -> Option<&Entry> {
        self.checkpoints.iter().find(|e| e.version == version)
    }
    pub fn infos(&self) -> Vec<CheckpointInfo> {
        self.checkpoints
            .iter()
            .rev()
            .map(|e| CheckpointInfo {
                version: e.version.clone(),
                kind: e.kind.clone(),
                created_ms: e.created_ms,
                metrics: e.metrics.clone(),
                records_seen: e.records_seen,
                active: Some(&e.version) == self.active_logistic.as_ref() || Some(&e.version) == self.active_nn.as_ref() || Some(&e.version) == self.active_team.as_ref(),
            })
            .collect()
    }
}

pub struct Store {
    pub dir: PathBuf,
}

fn put_u32(b: &mut Vec<u8>, x: u32) {
    b.extend_from_slice(&x.to_le_bytes());
}
fn put_u64(b: &mut Vec<u8>, x: u64) {
    b.extend_from_slice(&x.to_le_bytes());
}
fn put_f64(b: &mut Vec<u8>, x: f64) {
    b.extend_from_slice(&x.to_bits().to_le_bytes());
}

struct Reader<'a> {
    b: &'a [u8],
    at: usize,
}

impl Reader<'_> {
    fn take(&mut self, n: usize) -> Result<&[u8]> {
        if self.at + n > self.b.len() {
            bail!("checkpoint truncated");
        }
        let s = &self.b[self.at..self.at + n];
        self.at += n;
        Ok(s)
    }
    fn u8(&mut self) -> Result<u8> {
        Ok(self.take(1)?[0])
    }
    fn u32(&mut self) -> Result<u32> {
        Ok(u32::from_le_bytes(self.take(4)?.try_into()?))
    }
    fn u64(&mut self) -> Result<u64> {
        Ok(u64::from_le_bytes(self.take(8)?.try_into()?))
    }
    fn f64(&mut self) -> Result<f64> {
        Ok(f64::from_bits(self.u64()?))
    }
}

/// A logistic model as bytes: only the slots ever touched (most of the hashed space is zero).
pub fn encode_logistic(m: &LogisticModel) -> Vec<u8> {
    let mut b = b"BFL3".to_vec();
    put_u64(&mut b, m.t);
    put_u32(&mut b, m.pair_samples.len() as u32);
    for ((c, r), n) in &m.pair_samples {
        put_u64(&mut b, *c as u64);
        b.push(*r);
        put_u64(&mut b, *n);
    }
    put_f64(&mut b, m.calib[0]);
    put_f64(&mut b, m.calib[1]);
    for f in m.calib_win {
        put_f64(&mut b, f);
    }
    let nz: Vec<usize> = (0..DIM).filter(|j| m.w[*j] != 0.0 || m.m[*j] != 0.0 || m.v[*j] != 0.0).collect();
    put_u32(&mut b, nz.len() as u32);
    for j in nz {
        put_u32(&mut b, j as u32);
        put_f64(&mut b, m.w[j]);
        put_f64(&mut b, m.m[j]);
        put_f64(&mut b, m.v[j]);
    }
    b
}

pub fn decode_logistic(b: &[u8]) -> Result<LogisticModel> {
    let mut r = Reader { b, at: 0 };
    // BFL1 (no calibration: identity), BFL2 (the line) or BFL3 (the line and the window factors, 1.3.3).
    let version = match r.take(4)? {
        b"BFL1" => 1,
        b"BFL2" => 2,
        b"BFL3" => 3,
        _ => bail!("not a logistic checkpoint"),
    };
    let mut m = LogisticModel { t: r.u64()?, ..Default::default() };
    for _ in 0..r.u32()? {
        let c = r.u64()? as i64;
        let role = r.u8()?;
        m.pair_samples.insert((c, role), r.u64()?);
    }
    if version >= 2 {
        m.calib = [r.f64()?, r.f64()?];
    }
    if version >= 3 {
        for k in 0..4 {
            m.calib_win[k] = r.f64()?;
        }
    }
    for _ in 0..r.u32()? {
        let j = r.u32()? as usize;
        if j >= DIM {
            bail!("checkpoint slot out of range");
        }
        m.w[j] = r.f64()?;
        m.m[j] = r.f64()?;
        m.v[j] = r.f64()?;
    }
    Ok(m)
}

pub fn encode_nn(n: &SharedNet) -> Vec<u8> {
    let mut b = b"BFN1".to_vec();
    put_u64(&mut b, n.t);
    put_u32(&mut b, PARAMS as u32);
    for a in [&n.p, &n.m, &n.v] {
        for x in a.iter() {
            put_f64(&mut b, *x);
        }
    }
    b
}

pub fn decode_nn(b: &[u8]) -> Result<SharedNet> {
    let mut r = Reader { b, at: 0 };
    if r.take(4)? != b"BFN1" {
        bail!("not a network checkpoint");
    }
    let t = r.u64()?;
    if r.u32()? as usize != PARAMS {
        bail!("network checkpoint from a different architecture");
    }
    let mut read = || -> Result<Vec<f64>> { (0..PARAMS).map(|_| r.f64()).collect() };
    let (p, m, v) = (read()?, read()?, read()?);
    Ok(SharedNet { p, m, v, t })
}

pub fn encode_team(n: &TeamNet) -> Vec<u8> {
    let mut b = b"BFT1".to_vec();
    put_u64(&mut b, n.t);
    put_u32(&mut b, TEAM_PARAMS as u32);
    for a in [&n.p, &n.m, &n.v] {
        for x in a.iter() {
            put_f64(&mut b, *x);
        }
    }
    b
}

pub fn decode_team(b: &[u8]) -> Result<TeamNet> {
    let mut r = Reader { b, at: 0 };
    if r.take(4)? != b"BFT1" {
        bail!("not a team network checkpoint");
    }
    let t = r.u64()?;
    if r.u32()? as usize != TEAM_PARAMS {
        bail!("team network checkpoint from a different architecture");
    }
    let mut read = || -> Result<Vec<f64>> { (0..TEAM_PARAMS).map(|_| r.f64()).collect() };
    let (p, m, v) = (read()?, read()?, read()?);
    Ok(TeamNet { p, m, v, t })
}

fn no_dicts(_: u16) -> Option<std::sync::Arc<Vec<u8>>> {
    None
}

impl Store {
    pub fn open(dir: &Path) -> Result<Store> {
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        Ok(Store { dir: dir.to_path_buf() })
    }

    pub fn manifest(&self) -> Result<Manifest> {
        let f = self.dir.join("manifest.json");
        if !f.exists() {
            return Ok(Manifest { format: 1, next: 1, ..Default::default() });
        }
        let m: Manifest = serde_json::from_slice(&std::fs::read(&f)?).context("reading the checkpoint manifest")?;
        if m.format != 1 {
            bail!("checkpoint manifest format {} is newer than this app", m.format);
        }
        Ok(m)
    }

    pub fn save_manifest(&self, m: &Manifest) -> Result<()> {
        bf_core::util::write_atomic(&self.dir.join("manifest.json"), &serde_json::to_vec_pretty(m)?)?;
        Ok(())
    }

    /// Writes a model file; returns (file name, bytes on disk).
    pub fn write_model(&self, version: &str, kind: &str, raw: &[u8]) -> Result<(String, u64)> {
        let name = format!("{version}.{kind}.bin");
        let packed = bf_core::codec::pack(raw, None)?;
        bf_core::util::write_atomic(&self.dir.join(&name), &packed)?;
        Ok((name, packed.len() as u64))
    }

    pub fn read_model(&self, file: &str) -> Result<Vec<u8>> {
        let blob = std::fs::read(self.dir.join(file)).with_context(|| format!("reading checkpoint {file}"))?;
        bf_core::codec::unpack(&blob, &no_dicts)
    }

    pub fn load_logistic(&self, e: &Entry) -> Result<LogisticModel> {
        decode_logistic(&self.read_model(&e.file)?)
    }

    pub fn load_nn(&self, e: &Entry) -> Result<SharedNet> {
        decode_nn(&self.read_model(&e.file)?)
    }

    pub fn load_team(&self, e: &Entry) -> Result<TeamNet> {
        decode_team(&self.read_model(&e.file)?)
    }

    /// Deletes all but the newest KEEP checkpoints of each kind (never an active one), from the manifest and disk.
    pub fn prune(&self, m: &mut Manifest) {
        for kind in [LOGISTIC, SHARED_NN, TEAM_NN] {
            let of_kind: Vec<String> = m.checkpoints.iter().filter(|e| e.kind == kind).map(|e| e.version.clone()).collect();
            let drop = of_kind.len().saturating_sub(KEEP);
            for v in of_kind.into_iter().take(drop) {
                if Some(&v) == m.active_logistic.as_ref() || Some(&v) == m.active_nn.as_ref() || Some(&v) == m.active_team.as_ref() {
                    continue;
                }
                if let Some(i) = m.checkpoints.iter().position(|e| e.version == v) {
                    let e = m.checkpoints.remove(i);
                    let _ = std::fs::remove_file(self.dir.join(&e.file));
                }
            }
        }
    }

    pub fn save_contexts(&self, c: &BTreeMap<String, Vec<Situation>>) -> Result<()> {
        let raw = serde_json::to_vec(c)?;
        bf_core::util::write_atomic(&self.dir.join("contexts.bin"), &bf_core::codec::pack(&raw, None)?)?;
        Ok(())
    }

    pub fn load_contexts(&self) -> BTreeMap<String, Vec<Situation>> {
        std::fs::read(self.dir.join("contexts.bin"))
            .ok()
            .and_then(|b| bf_core::codec::unpack(&b, &no_dicts).ok())
            .and_then(|raw| serde_json::from_slice(&raw).ok())
            .unwrap_or_default()
    }

    pub fn log(&self, line: &str) {
        let f = self.dir.join("learning.log");
        let stamp = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
        if let Ok(mut file) = std::fs::OpenOptions::new().create(true).append(true).open(&f) {
            let _ = writeln!(file, "{stamp}  {line}");
        }
        if let Ok(text) = std::fs::read_to_string(&f) {
            let lines: Vec<&str> = text.lines().collect();
            if lines.len() > 2 * LOG_LINES {
                let _ = bf_core::util::write_atomic(&f, (lines[lines.len() - LOG_LINES..].join("\n") + "\n").as_bytes());
            }
        }
    }

    pub fn log_tail(&self, n: usize) -> Vec<String> {
        let text = std::fs::read_to_string(self.dir.join("learning.log")).unwrap_or_default();
        let lines: Vec<&str> = text.lines().collect();
        lines[lines.len().saturating_sub(n)..].iter().rev().map(|s| s.to_string()).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn models_round_trip_bit_for_bit() {
        let mut m = LogisticModel::default();
        m.w[5] = 0.1 + 0.2;
        m.m[70_000] = -1e-300;
        m.v[DIM - 1] = 3.5;
        m.t = 9;
        m.pair_samples.insert((39, 0), 12);
        m.calib = [0.8125, 0.0625];
        m.calib_win = [1.0, 1.25, 0.5, 2.0];
        assert_eq!(decode_logistic(&encode_logistic(&m)).unwrap(), m);
        let n = SharedNet::new(1);
        assert_eq!(decode_nn(&encode_nn(&n)).unwrap(), n);
        assert!(decode_nn(&encode_logistic(&m)).is_err());
    }

    /// A checkpoint from before 1.2.0 (no enemy region) loads with that region at zero and predicts exactly as before,
    /// whatever the enemy builds.
    #[test]
    fn checkpoints_before_the_enemy_region_load_and_predict_unchanged() {
        use crate::features::{GameState, Situation};
        use crate::logistic::{featurize, GLOBAL, PAIR};
        let mut m = LogisticModel::default();
        let mut s = Situation { champion: 39, role: Some(bf_core::build::Role::Top), items: vec![3153, 3078], boots: Some(3047), state: GameState { minute: 20.0, ..Default::default() }, ..Default::default() };
        let mut f = vec![];
        featurize(&s, &mut f);
        for (k, (j, _)) in f.iter().enumerate() {
            m.w[*j as usize] = 0.01 * (k as f64 + 1.0);
        }
        let bytes = encode_logistic(&m);
        // Only slots below the old dimension were written.
        let old = decode_logistic(&bytes).unwrap();
        assert!(old.w[GLOBAL + PAIR..].iter().all(|x| *x == 0.0));
        let before = old.logit(&s);
        s.enemy_items = vec![3075, 3742, 3047, 6333];
        s.enemy_tags = [1.0, 0.0, 0.0, 0.0, 0.0, 0.25, 0.5];
        featurize(&s, &mut f);
        assert!(f.iter().any(|(j, _)| *j as usize >= GLOBAL + PAIR), "enemy features go to the new region");
        assert_eq!(old.logit(&s), before);
    }
}
