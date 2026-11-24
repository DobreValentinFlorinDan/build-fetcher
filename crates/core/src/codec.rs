//! Port of `server/codec.ts`: compressed blobs (the Riot API cache and the database's timelines).
//!
//!   blob = [1 = zstd][dictionary id, uint16 little-endian][zstd frame]      dictionary 0: none
//!
//! Each frame is zstd level 9 with a content checksum and a raw-content dictionary (a typical example of what the
//! database holds). Blobs written by 0.14.0–0.22.0 (Node's zlib, libzstd) read here and the other way round.

use anyhow::{anyhow, bail, Result};
use parking_lot::Mutex;
use rusqlite::{params, Connection};
use std::collections::BTreeMap;
use std::sync::Arc;

const FORMAT_ZSTD: u8 = 1;
const HEADER: usize = 3;
/// zstd's level (0.14.1).
pub const LEVEL: i32 = 9;
/// How much of each typical example goes into a dictionary.
const SAMPLE_BYTES: usize = 64 * 1024;

#[derive(Clone, Debug)]
pub struct Dictionary {
    pub id: u16,
    pub bytes: Arc<Vec<u8>>,
}

fn compressor(dict: Option<&[u8]>) -> Result<zstd::bulk::Compressor<'static>> {
    let mut c = match dict {
        Some(d) => zstd::bulk::Compressor::with_dictionary(LEVEL, d)?,
        None => zstd::bulk::Compressor::new(LEVEL)?,
    };
    c.include_checksum(true)?;
    Ok(c)
}

/// `raw` compressed with `dict` (None: none).
pub fn pack(raw: &[u8], dict: Option<&Dictionary>) -> Result<Vec<u8>> {
    let mut c = compressor(dict.map(|d| d.bytes.as_slice()))?;
    let frame = c.compress(raw)?;
    let mut out = Vec::with_capacity(HEADER + frame.len());
    out.push(FORMAT_ZSTD);
    out.extend_from_slice(&dict.map(|d| d.id).unwrap_or(0).to_le_bytes());
    out.extend_from_slice(&frame);
    Ok(out)
}

/// The dictionary id a blob was written with.
pub fn dictionary_of(blob: &[u8]) -> u16 {
    blob[1] as u16 | ((blob[2] as u16) << 8)
}

/// The bytes `pack` was given. Fails on a blob it can't read (unknown format, a dictionary `dicts` doesn't have,
/// corruption).
pub fn unpack(blob: &[u8], dicts: &dyn Fn(u16) -> Option<Arc<Vec<u8>>>) -> Result<Vec<u8>> {
    if blob.len() < HEADER || blob[0] != FORMAT_ZSTD {
        bail!("Not a compressed blob");
    }
    let id = dictionary_of(blob);
    let dictionary = if id != 0 { dicts(id) } else { None };
    if id != 0 && dictionary.is_none() {
        bail!("Compressed with dictionary {id}, which this database doesn't have");
    }
    let frame = &blob[HEADER..];
    let mut d = match &dictionary {
        Some(d) => zstd::stream::read::Decoder::with_dictionary(frame, d)?,
        None => zstd::stream::read::Decoder::with_dictionary(frame, &[])?,
    };
    let mut out = Vec::new();
    std::io::Read::read_to_end(&mut d, &mut out).map_err(|e| anyhow!("zstd: {e}"))?;
    Ok(out)
}

/// A raw-content dictionary from typical examples (the first 64 KB of each).
pub fn dictionary_from(samples: &[&[u8]]) -> Vec<u8> {
    let mut out = Vec::new();
    for s in samples {
        out.extend_from_slice(&s[..s.len().min(SAMPLE_BYTES)]);
    }
    out
}

pub const CODEC_SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS codec_dicts (
  id         INTEGER PRIMARY KEY,
  bytes      BLOB NOT NULL,
  made_at    INTEGER NOT NULL,
  made_from  TEXT NOT NULL               -- what the examples were (match ids)
);";

/// The dictionaries of one database (its codec_dicts table): the newest is the one new blobs are written with.
/// Another connection may add one meanwhile: an id this one doesn't know is read again from the table.
#[derive(Default)]
pub struct Dictionaries {
    known: Mutex<BTreeMap<u16, Arc<Vec<u8>>>>,
    newest: Mutex<Option<Dictionary>>,
}

impl Dictionaries {
    pub fn new(db: &Connection) -> Self {
        let d = Dictionaries::default();
        d.reload(db);
        d
    }

    pub fn reload(&self, db: &Connection) {
        // A database from before 0.14.0 opened without its migration has none.
        let rows: Vec<(i64, Vec<u8>)> = match db.prepare_cached("SELECT id, bytes FROM codec_dicts ORDER BY id") {
            Ok(mut st) => match st.query_map([], |r| Ok((r.get(0)?, r.get(1)?))) {
                Ok(it) => it.filter_map(|x| x.ok()).collect(),
                Err(_) => return,
            },
            Err(_) => return,
        };
        let mut known = self.known.lock();
        for (id, bytes) in &rows {
            known.entry(*id as u16).or_insert_with(|| Arc::new(bytes.clone()));
        }
        if let Some((id, _)) = rows.last() {
            *self.newest.lock() = Some(Dictionary { id: *id as u16, bytes: known[&(*id as u16)].clone() });
        }
    }

    pub fn get(&self, db: &Connection, id: u16) -> Option<Arc<Vec<u8>>> {
        if !self.known.lock().contains_key(&id) {
            self.reload(db);
        }
        self.known.lock().get(&id).cloned()
    }

    /// The dictionaries known now, without reading the table (for threads that only decode).
    pub fn snapshot(&self) -> BTreeMap<u16, Arc<Vec<u8>>> {
        self.known.lock().clone()
    }

    /// Every dictionary this connection knows, by id.
    pub fn all(&self, db: &Connection) -> Vec<(u16, Arc<Vec<u8>>)> {
        self.reload(db);
        self.known.lock().iter().map(|(k, v)| (*k, v.clone())).collect()
    }

    /// The dictionary new blobs are written with (None: none made yet).
    pub fn current(&self) -> Option<Dictionary> {
        self.newest.lock().clone()
    }

    /// Makes one from typical examples and makes it the current one (or takes one another connection just made).
    pub fn create(&self, db: &Connection, samples: &[&[u8]], made_from: &str) -> Result<Dictionary> {
        self.reload(db);
        if let Some(d) = self.current() {
            return Ok(d);
        }
        let bytes = dictionary_from(samples);
        db.execute(
            "INSERT INTO codec_dicts(id, bytes, made_at, made_from) VALUES(1, ?, ?, ?) ON CONFLICT(id) DO NOTHING",
            params![bytes, crate::util::now_ms(), made_from],
        )?;
        self.reload(db);
        self.current().ok_or_else(|| anyhow!("no dictionary after create"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_with_and_without_a_dictionary() {
        let raw = br#"{"metadata":{"matchId":"KR_1"},"info":{"frames":[1,2,3]}}"#.repeat(20);
        let none = pack(&raw, None).unwrap();
        assert_eq!(none[0], 1);
        assert_eq!(dictionary_of(&none), 0);
        assert_eq!(unpack(&none, &|_| None).unwrap(), raw);
        let dict = Dictionary { id: 7, bytes: Arc::new(dictionary_from(&[&raw])) };
        let with = pack(&raw, Some(&dict)).unwrap();
        assert_eq!(dictionary_of(&with), 7);
        assert!(with.len() < none.len());
        let b = dict.bytes.clone();
        assert_eq!(unpack(&with, &|id| if id == 7 { Some(b.clone()) } else { None }).unwrap(), raw);
        assert!(unpack(&with, &|_| None).is_err());
        assert!(unpack(b"xyz", &|_| None).is_err());
    }
}
