//! `MatchRecord` ↔ compact bytes, then zstd (the blob format of `bf_core::codec`, so `codec::unpack` reads it).
//!
//! The encoding is varints (LEB128; zigzag for signed values) in a fixed field order, with deltas where values grow
//! (frame gold and XP, event and purchase times) so zstd sees small repeated numbers. It is quantized, on purpose:
//! - gold (frames and gold earned) and frame XP to the nearest 10;
//! - times to whole seconds (`start_ms` keeps whole seconds; purchases and events already are).
//!
//! Nothing a model or a count needs is finer than that, and it saves about a fifth of the bytes. `quantize` gives
//! the record exactly as it will read back, so tests and callers can compare.
//!
//! Records are small (1-6 KB raw), and small zstd frames compress badly on their own, so the store makes a
//! raw-content dictionary from its first records (`codec::Dictionaries`, table `codec_dicts`) and packs every later
//! record with it.

use anyhow::{anyhow, bail, Result};
use bf_core::build::{Frame, GameVersion, MatchRecord, Patch, Platform, PlayerRecord, RankBand, Role, TeamEvent, TeamEventKind};
use bf_core::codec::Dictionary;
use std::sync::Arc;

const FORMAT: u8 = 1;

// ------------------------------------------------------------------------------------------------ small codes

pub fn platform_code(p: Option<Platform>) -> u8 {
    match p {
        None => 0,
        Some(Platform::Eun1) => 1,
        Some(Platform::Euw1) => 2,
        Some(Platform::Kr) => 3,
    }
}
pub fn platform_from(c: u64) -> Option<Platform> {
    match c {
        1 => Some(Platform::Eun1),
        2 => Some(Platform::Euw1),
        3 => Some(Platform::Kr),
        _ => None,
    }
}
pub fn rank_code(r: Option<RankBand>) -> u8 {
    match r {
        None => 0,
        Some(RankBand::MasterPlus) => 1,
        Some(RankBand::Diamond) => 2,
        Some(RankBand::Mine) => 3,
    }
}
pub fn rank_from(c: u64) -> Option<RankBand> {
    match c {
        1 => Some(RankBand::MasterPlus),
        2 => Some(RankBand::Diamond),
        3 => Some(RankBand::Mine),
        _ => None,
    }
}
fn role_code(r: Option<Role>) -> u64 {
    r.map(|r| r.index() as u64 + 1).unwrap_or(0)
}
fn role_from(c: u64) -> Option<Role> {
    (1..=5).contains(&c).then(|| Role::ALL[(c - 1) as usize])
}
/// Patches as one integer (16.23 → 1623), ordered like `Patch`.
pub fn patch_code(p: Patch) -> i64 {
    p.major as i64 * 100 + p.minor as i64
}
pub fn patch_from(c: i64) -> Patch {
    Patch { major: (c / 100) as u16, minor: (c % 100) as u16 }
}
const EVENT_KINDS: [TeamEventKind; 8] =
    [TeamEventKind::Kill, TeamEventKind::Tower, TeamEventKind::Inhibitor, TeamEventKind::Dragon, TeamEventKind::Herald, TeamEventKind::Baron, TeamEventKind::Grubs, TeamEventKind::Atakhan];

fn q10(x: u32) -> u32 {
    (x + 5) / 10
}

/// The record as it reads back after encoding (see the module comment).
pub fn quantize(r: &MatchRecord) -> MatchRecord {
    let mut r = r.clone();
    r.start_ms = r.start_ms.div_euclid(1000) * 1000;
    for p in &mut r.players {
        p.gold_earned = q10(p.gold_earned) * 10;
    }
    for f in r.frames.iter_mut().flatten() {
        f.total_gold = q10(f.total_gold) * 10;
        f.xp = q10(f.xp) * 10;
    }
    r
}

// ------------------------------------------------------------------------------------------------ encoding

struct W(Vec<u8>);
impl W {
    fn u(&mut self, mut v: u64) {
        while v >= 0x80 {
            self.0.push((v as u8) | 0x80);
            v >>= 7;
        }
        self.0.push(v as u8);
    }
    fn i(&mut self, v: i64) {
        self.u(((v << 1) ^ (v >> 63)) as u64);
    }
    fn s(&mut self, s: &str) {
        self.u(s.len() as u64);
        self.0.extend_from_slice(s.as_bytes());
    }
}

struct R<'a> {
    b: &'a [u8],
    at: usize,
}
impl R<'_> {
    fn u(&mut self) -> Result<u64> {
        let mut v = 0u64;
        let mut shift = 0;
        loop {
            let byte = *self.b.get(self.at).ok_or_else(|| anyhow!("record truncated"))?;
            self.at += 1;
            if shift >= 64 {
                bail!("record varint too long");
            }
            v |= ((byte & 0x7f) as u64) << shift;
            if byte & 0x80 == 0 {
                return Ok(v);
            }
            shift += 7;
        }
    }
    fn i(&mut self) -> Result<i64> {
        let u = self.u()?;
        Ok(((u >> 1) as i64) ^ -((u & 1) as i64))
    }
    fn n(&mut self, max: u64) -> Result<usize> {
        let v = self.u()?;
        if v > max {
            bail!("record count {v} out of range");
        }
        Ok(v as usize)
    }
    fn s(&mut self) -> Result<String> {
        let n = self.n(256)?;
        let end = self.at + n;
        let s = std::str::from_utf8(self.b.get(self.at..end).ok_or_else(|| anyhow!("record truncated"))?)?.to_string();
        self.at = end;
        Ok(s)
    }
}

pub fn encode(r: &MatchRecord) -> Vec<u8> {
    let mut w = W(Vec::with_capacity(1024));
    w.0.push(FORMAT);
    w.s(&r.match_id);
    w.u(platform_code(r.platform) as u64);
    match r.version {
        Some(v) => {
            w.u(1);
            w.u(v.patch.major as u64);
            w.u(v.patch.minor as u64);
            w.u(v.build as u64);
        }
        None => w.u(0),
    }
    w.u(rank_code(r.rank) as u64);
    w.i(r.start_ms.div_euclid(1000));
    w.u(r.duration_s as u64);
    w.u(r.players.len() as u64);
    for p in &r.players {
        w.u(p.champion as u64);
        w.u(role_code(p.role));
        w.u((p.team == 200) as u64 | (p.win as u64) << 1 | (p.is_me as u64) << 2);
        for x in p.summoners {
            w.u(x as u64);
        }
        w.u(p.rune_style as u64);
        for x in p.perks {
            w.u(x as u64);
        }
        w.u(p.rune_sub_style as u64);
        for x in p.sub_perks {
            w.u(x as u64);
        }
        for x in p.shards {
            w.u(x as u64);
        }
        for x in p.final_items {
            w.u(x as u64);
        }
        w.u(p.kills as u64);
        w.u(p.deaths as u64);
        w.u(p.assists as u64);
        w.u(q10(p.gold_earned) as u64);
        w.u(p.cs as u64);
        w.u(p.champ_level as u64);
        w.u(p.physical_damage_to_champions as u64);
        w.u(p.magic_damage_to_champions as u64);
        w.u(p.true_damage_to_champions as u64);
        w.u(p.purchases.len() as u64);
        let mut t = 0i64;
        for (item, s) in &p.purchases {
            w.i(*item);
            w.i(*s as i64 - t);
            t = *s as i64;
        }
        // Skill slots 1-4 in two bits each.
        w.u(p.skill_order.len() as u64);
        for chunk in p.skill_order.chunks(4) {
            let mut b = 0u8;
            for (k, s) in chunk.iter().enumerate() {
                b |= (s.saturating_sub(1) & 3) << (2 * k);
            }
            w.0.push(b);
        }
    }
    w.u(r.frames.len() as u64);
    for fs in &r.frames {
        w.u(fs.len() as u64);
        let mut prev = Frame::default();
        let mut pm = -1i64;
        for f in fs {
            w.i(f.minute as i64 - pm);
            w.i(q10(f.total_gold) as i64 - q10(prev.total_gold) as i64);
            w.i(f.cs as i64 - prev.cs as i64);
            w.i(f.level as i64 - prev.level as i64);
            w.i(q10(f.xp) as i64 - q10(prev.xp) as i64);
            pm = f.minute as i64;
            prev = *f;
        }
    }
    w.u(r.events.len() as u64);
    let mut t = 0i64;
    for e in &r.events {
        w.i(e.second as i64 - t);
        t = e.second as i64;
        let k = EVENT_KINDS.iter().position(|x| *x == e.kind).unwrap_or(0) as u64;
        w.u((e.team == 200) as u64 | k << 1);
    }
    w.0
}

pub fn decode(b: &[u8]) -> Result<MatchRecord> {
    let mut r = R { b, at: 0 };
    let f = *b.first().ok_or_else(|| anyhow!("empty record"))?;
    if f != FORMAT {
        bail!("record format {f} unknown (newer Build Fetcher?)");
    }
    r.at = 1;
    let mut m = MatchRecord { match_id: r.s()?, platform: platform_from(r.u()?), ..Default::default() };
    if r.u()? == 1 {
        let major = r.u()? as u16;
        let minor = r.u()? as u16;
        let build = r.u()? as u32;
        m.version = Some(GameVersion { patch: Patch { major, minor }, build });
    }
    m.rank = rank_from(r.u()?);
    m.start_ms = r.i()? * 1000;
    m.duration_s = r.u()? as u32;
    let n = r.n(10)?;
    for _ in 0..n {
        let mut p = PlayerRecord { champion: r.u()? as i64, role: role_from(r.u()?), ..Default::default() };
        let flags = r.u()?;
        p.team = if flags & 1 == 1 { 200 } else { 100 };
        p.win = flags & 2 != 0;
        p.is_me = flags & 4 != 0;
        p.summoners = [r.u()? as i64, r.u()? as i64];
        p.rune_style = r.u()? as i64;
        for x in &mut p.perks {
            *x = r.u()? as i64;
        }
        p.rune_sub_style = r.u()? as i64;
        for x in &mut p.sub_perks {
            *x = r.u()? as i64;
        }
        for x in &mut p.shards {
            *x = r.u()? as i64;
        }
        for x in &mut p.final_items {
            *x = r.u()? as i64;
        }
        p.kills = r.u()? as u16;
        p.deaths = r.u()? as u16;
        p.assists = r.u()? as u16;
        p.gold_earned = r.u()? as u32 * 10;
        p.cs = r.u()? as u16;
        p.champ_level = r.u()? as u8;
        p.physical_damage_to_champions = r.u()? as u32;
        p.magic_damage_to_champions = r.u()? as u32;
        p.true_damage_to_champions = r.u()? as u32;
        let np = r.n(1000)?;
        let mut t = 0i64;
        for _ in 0..np {
            let item = r.i()?;
            t += r.i()?;
            p.purchases.push((item, t.max(0) as u32));
        }
        let ns = r.n(64)?;
        for k in 0..ns.div_ceil(4) {
            let byte = *b.get(r.at).ok_or_else(|| anyhow!("record truncated"))?;
            r.at += 1;
            for j in 0..4 {
                if k * 4 + j < ns {
                    p.skill_order.push(((byte >> (2 * j)) & 3) + 1);
                }
            }
        }
        m.players.push(p);
    }
    let nf = r.n(10)?;
    for _ in 0..nf {
        let len = r.n(200)?;
        let mut fs = Vec::with_capacity(len);
        let mut prev = (0i64, 0i64, 0i64, 0i64, 0i64);
        let mut pm = -1i64;
        for _ in 0..len {
            pm += r.i()?;
            prev.0 += r.i()?;
            prev.1 += r.i()?;
            prev.2 += r.i()?;
            prev.3 += r.i()?;
            fs.push(Frame { minute: pm as u16, total_gold: (prev.0 * 10) as u32, cs: prev.1 as u16, level: prev.2 as u8, xp: (prev.3 * 10) as u32 });
        }
        m.frames.push(fs);
    }
    let ne = r.n(10_000)?;
    let mut t = 0i64;
    for _ in 0..ne {
        t += r.i()?;
        let c = r.u()?;
        m.events.push(TeamEvent { second: t.max(0) as u32, team: if c & 1 == 1 { 200 } else { 100 }, kind: *EVENT_KINDS.get((c >> 1) as usize).unwrap_or(&TeamEventKind::Kill) });
    }
    Ok(m)
}

// ------------------------------------------------------------------------------------------------ compression

/// Packs and unpacks records, keeping zstd contexts (with the dictionary loaded) across calls: building one per
/// record would cost more than compressing it.
pub struct RecordCodec {
    dict: Option<Dictionary>,
    comp: zstd::bulk::Compressor<'static>,
    decomp: std::collections::HashMap<u16, zstd::bulk::Decompressor<'static>>,
}

/// zstd level for records: 9 as `bf_core::codec` (records are written once and read many times).
const LEVEL: i32 = bf_core::codec::LEVEL;
/// The biggest record we accept back (a 60-minute game with every event is ~40 KB raw).
const MAX_RAW: usize = 4 * 1024 * 1024;

impl RecordCodec {
    pub fn new(dict: Option<Dictionary>) -> Result<RecordCodec> {
        let mut comp = match &dict {
            Some(d) => zstd::bulk::Compressor::with_dictionary(LEVEL, &d.bytes)?,
            None => zstd::bulk::Compressor::new(LEVEL)?,
        };
        comp.include_checksum(true)?;
        Ok(RecordCodec { dict, comp, decomp: Default::default() })
    }

    pub fn dictionary_id(&self) -> u16 {
        self.dict.as_ref().map(|d| d.id).unwrap_or(0)
    }

    /// The codec blob: [1][dictionary id LE][zstd frame], as `bf_core::codec::pack` writes.
    pub fn pack(&mut self, r: &MatchRecord) -> Result<Vec<u8>> {
        let raw = encode(r);
        let frame = self.comp.compress(&raw)?;
        let mut out = Vec::with_capacity(3 + frame.len());
        out.push(1);
        out.extend_from_slice(&self.dictionary_id().to_le_bytes());
        out.extend_from_slice(&frame);
        Ok(out)
    }

    pub fn unpack(&mut self, blob: &[u8], dicts: &dyn Fn(u16) -> Option<Arc<Vec<u8>>>) -> Result<MatchRecord> {
        if blob.len() < 4 || blob[0] != 1 {
            bail!("not a record blob");
        }
        let id = bf_core::codec::dictionary_of(blob);
        if !self.decomp.contains_key(&id) {
            let d = if id == 0 {
                zstd::bulk::Decompressor::new()?
            } else {
                let bytes = dicts(id).ok_or_else(|| anyhow!("record packed with dictionary {id}, which this database doesn't have"))?;
                zstd::bulk::Decompressor::with_dictionary(&bytes)?
            };
            self.decomp.insert(id, d);
        }
        let raw = self.decomp.get_mut(&id).unwrap().decompress(&blob[3..], MAX_RAW)?;
        decode(&raw)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    pub fn sample() -> MatchRecord {
        let players = (0..10)
            .map(|i| PlayerRecord {
                champion: 1 + i as i64 * 17,
                role: Some(Role::ALL[i % 5]),
                team: if i < 5 { 100 } else { 200 },
                win: i < 5,
                is_me: i == 2,
                summoners: [4, 12],
                rune_style: 8000,
                perks: [8010, 9111, 9104, 8299],
                rune_sub_style: 8400,
                sub_perks: [8444, 8473],
                shards: [5008, 5008, 5011],
                final_items: [3078, 3074, 3053, 0, 0, 3047, 3340],
                kills: 4,
                deaths: 3,
                assists: 7,
                gold_earned: 12_345,
                cs: 211,
                champ_level: 16,
                physical_damage_to_champions: 18_000,
                magic_damage_to_champions: 2_000,
                true_damage_to_champions: 900,
                purchases: vec![(1055, 5), (2003, 6), (1001, 300), (3078, 720), (-1055, 1300), (3074, 1400)],
                skill_order: vec![1, 3, 2, 1, 1, 4, 1, 3, 1, 3, 4, 3, 3, 2, 2, 4, 2],
            })
            .collect();
        MatchRecord {
            match_id: "EUW1_7234567890".into(),
            platform: Some(Platform::Euw1),
            version: GameVersion::parse("16.23.712.4471"),
            rank: Some(RankBand::MasterPlus),
            start_ms: 1_790_000_123_456,
            duration_s: 1834,
            players,
            frames: (0..10).map(|i| (0..31).map(|m| Frame { minute: m, total_gold: 500 + m as u32 * 377 + i, cs: m * 7, level: (1 + m / 2).min(18) as u8, xp: m as u32 * 613 }).collect()).collect(),
            events: vec![
                TeamEvent { second: 361, team: 100, kind: TeamEventKind::Kill },
                TeamEvent { second: 540, team: 200, kind: TeamEventKind::Dragon },
                TeamEvent { second: 1500, team: 100, kind: TeamEventKind::Atakhan },
            ],
        }
    }

    #[test]
    fn round_trips_quantized() {
        let r = sample();
        let back = decode(&encode(&r)).unwrap();
        assert_eq!(back, quantize(&r));
        assert_eq!(back.players[0].gold_earned, 12_350);
        assert_eq!(back.start_ms, 1_790_000_123_000);
        let mut c = RecordCodec::new(None).unwrap();
        let blob = c.pack(&r).unwrap();
        assert_eq!(c.unpack(&blob, &|_| None).unwrap(), quantize(&r));
        // bf_core::codec reads the same blobs.
        assert_eq!(bf_core::codec::unpack(&blob, &|_| None).unwrap(), encode(&r));
        let no_tl = MatchRecord { frames: vec![], events: vec![], ..r.clone() };
        assert_eq!(decode(&encode(&no_tl)).unwrap(), quantize(&no_tl));
    }

    #[test]
    fn a_dictionary_shrinks_records_and_is_needed_to_read_them() {
        let r = sample();
        let raw = encode(&r);
        let d = Dictionary { id: 1, bytes: Arc::new(bf_core::codec::dictionary_from(&[&raw])) };
        let mut plain = RecordCodec::new(None).unwrap();
        let mut with = RecordCodec::new(Some(d.clone())).unwrap();
        let mut other = r.clone();
        other.match_id = "EUW1_7234567891".into();
        other.players[3].kills = 9;
        let a = plain.pack(&other).unwrap();
        let b = with.pack(&other).unwrap();
        assert!(b.len() < a.len() / 2, "{} vs {}", b.len(), a.len());
        let bytes = d.bytes.clone();
        assert_eq!(with.unpack(&b, &|id| (id == 1).then(|| bytes.clone())).unwrap(), quantize(&other));
        assert!(RecordCodec::new(None).unwrap().unpack(&b, &|_| None).is_err());
    }

    #[test]
    fn corrupt_blobs_fail_cleanly() {
        let raw = encode(&sample());
        for cut in [0, 1, 5, raw.len() / 2, raw.len() - 1] {
            assert!(decode(&raw[..cut]).is_err(), "cut at {cut}");
        }
        assert!(decode(&[9, 0, 0]).is_err());
        let mut c = RecordCodec::new(None).unwrap();
        assert!(c.unpack(b"xyz", &|_| None).is_err());
    }
}
