//! Port of `server/zip.ts`: ZIP files for exporting and importing data (0.18.0). A streaming writer (ZIP64 where a
//! size or an offset needs it; a data descriptor for streamed files) and a reader that checks each entry's CRC-32.
//! The writer writes to `<file>.partial` and renames it at the end, so a syncing folder never uploads half an archive.

use anyhow::{anyhow, bail, Result};
use chrono::{Datelike, Local, Timelike};
use flate2::read::DeflateDecoder;
use flate2::write::DeflateEncoder;
use flate2::Compression;
use std::fs::File;
use std::io::{BufWriter, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

const SIG_LOCAL: u32 = 0x04034b50;
const SIG_CENTRAL: u32 = 0x02014b50;
const SIG_DESCRIPTOR: u32 = 0x08074b50;
const SIG_EOCD: u32 = 0x06054b50;
const SIG_EOCD64: u32 = 0x06064b50;
const SIG_LOCATOR64: u32 = 0x07064b50;
const MAX32: u64 = 0xffffffff;
const MAX16: u64 = 0xffff;
const FLAG_DESCRIPTOR: u16 = 1 << 3;
const FLAG_UTF8: u16 = 1 << 11;
const DEFLATE: u16 = 8;
const STORE: u16 = 0;

/// MS-DOS date and time (local time, 2 s steps).
fn dos_time(t: std::time::SystemTime) -> (u16, u16) {
    let d: chrono::DateTime<Local> = t.into();
    let year = d.year().max(1980) as u16;
    let time = ((d.hour() as u16) << 11) | ((d.minute() as u16) << 5) | (d.second() as u16 / 2);
    let date = ((year - 1980) << 9) | ((d.month() as u16) << 5) | d.day() as u16;
    (time, date)
}

struct Written {
    name: Vec<u8>,
    flags: u16,
    crc: u32,
    compressed: u64,
    size: u64,
    offset: u64,
    method: u16,
    zip64: bool,
    time: u16,
    date: u16,
}

/// The ZIP64 extended information extra field (0x0001) holding these 8-byte values.
fn zip64_extra(values: &[u64]) -> Vec<u8> {
    let mut b = Vec::with_capacity(4 + 8 * values.len());
    b.extend_from_slice(&1u16.to_le_bytes());
    b.extend_from_slice(&((8 * values.len()) as u16).to_le_bytes());
    for v in values {
        b.extend_from_slice(&v.to_le_bytes());
    }
    b
}

struct Counter<W: Write> {
    inner: W,
    n: u64,
}
impl<W: Write> Write for Counter<W> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let k = self.inner.write(buf)?;
        self.n += k as u64;
        Ok(k)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.inner.flush()
    }
}

pub struct ZipWriter {
    file: PathBuf,
    partial: PathBuf,
    out: Option<BufWriter<File>>,
    offset: u64,
    entries: Vec<Written>,
    level: u32,
}

impl ZipWriter {
    pub fn new(file: &Path) -> ZipWriter {
        ZipWriter::with_level(file, 6)
    }

    pub fn with_level(file: &Path, level: u32) -> ZipWriter {
        ZipWriter {
            file: file.to_path_buf(),
            partial: PathBuf::from(format!("{}.partial", file.display())),
            out: None,
            offset: 0,
            entries: vec![],
            level,
        }
    }

    fn out(&mut self) -> Result<&mut BufWriter<File>> {
        if self.out.is_none() {
            let _ = std::fs::remove_file(&self.partial);
            self.out = Some(BufWriter::with_capacity(1 << 20, File::create(&self.partial)?));
        }
        Ok(self.out.as_mut().unwrap())
    }

    fn write(&mut self, buf: &[u8]) -> Result<()> {
        self.out()?.write_all(buf)?;
        self.offset += buf.len() as u64;
        Ok(())
    }

    /// A small entry from memory: sizes known up front, no data descriptor.
    pub fn add_buffer(&mut self, name: &str, raw: &[u8]) -> Result<()> {
        self.add_buffer_at(name, raw, std::time::SystemTime::now())
    }

    pub fn add_buffer_at(&mut self, name: &str, raw: &[u8], mtime: std::time::SystemTime) -> Result<()> {
        let mut enc = DeflateEncoder::new(Vec::new(), Compression::new(self.level));
        enc.write_all(raw)?;
        let packed = enc.finish()?;
        let method = if packed.len() < raw.len() { DEFLATE } else { STORE };
        let body: &[u8] = if method == DEFLATE { &packed } else { raw };
        let name_buf = name.as_bytes().to_vec();
        let (time, date) = dos_time(mtime);
        let crc = crc32fast::hash(raw);
        let size = raw.len() as u64;
        let zip64 = size >= MAX32 || self.offset >= MAX32;
        let extra = if zip64 { zip64_extra(&[size, body.len() as u64]) } else { vec![] };
        let mut h = Vec::with_capacity(30);
        h.extend_from_slice(&SIG_LOCAL.to_le_bytes());
        h.extend_from_slice(&(if zip64 { 45u16 } else { 20 }).to_le_bytes());
        h.extend_from_slice(&FLAG_UTF8.to_le_bytes());
        h.extend_from_slice(&method.to_le_bytes());
        h.extend_from_slice(&time.to_le_bytes());
        h.extend_from_slice(&date.to_le_bytes());
        h.extend_from_slice(&crc.to_le_bytes());
        h.extend_from_slice(&(if zip64 { MAX32 as u32 } else { body.len() as u32 }).to_le_bytes());
        h.extend_from_slice(&(if zip64 { MAX32 as u32 } else { size as u32 }).to_le_bytes());
        h.extend_from_slice(&(name_buf.len() as u16).to_le_bytes());
        h.extend_from_slice(&(extra.len() as u16).to_le_bytes());
        let entry = Written {
            name: name_buf.clone(),
            flags: FLAG_UTF8,
            crc,
            compressed: body.len() as u64,
            size,
            offset: self.offset,
            method,
            zip64,
            time,
            date,
        };
        self.write(&h)?;
        self.write(&name_buf)?;
        self.write(&extra)?;
        let body = body.to_vec();
        self.write(&body)?;
        self.entries.push(entry);
        Ok(())
    }

    /// A file, streamed and deflated: its sizes and CRC follow it in a ZIP64 data descriptor.
    pub fn add_file(&mut self, name: &str, file: &Path, on_bytes: &mut dyn FnMut(u64)) -> Result<()> {
        let meta = std::fs::metadata(file)?;
        let name_buf = name.as_bytes().to_vec();
        let (time, date) = dos_time(meta.modified().unwrap_or_else(|_| std::time::SystemTime::now()));
        let extra = zip64_extra(&[0, 0]);
        let mut h = Vec::with_capacity(30);
        h.extend_from_slice(&SIG_LOCAL.to_le_bytes());
        h.extend_from_slice(&45u16.to_le_bytes());
        h.extend_from_slice(&(FLAG_UTF8 | FLAG_DESCRIPTOR).to_le_bytes());
        h.extend_from_slice(&DEFLATE.to_le_bytes());
        h.extend_from_slice(&time.to_le_bytes());
        h.extend_from_slice(&date.to_le_bytes());
        h.extend_from_slice(&0u32.to_le_bytes());
        h.extend_from_slice(&(MAX32 as u32).to_le_bytes());
        h.extend_from_slice(&(MAX32 as u32).to_le_bytes());
        h.extend_from_slice(&(name_buf.len() as u16).to_le_bytes());
        h.extend_from_slice(&(extra.len() as u16).to_le_bytes());
        let offset = self.offset;
        self.write(&h)?;
        self.write(&name_buf)?;
        self.write(&extra)?;
        let level = self.level;
        let out = self.out()?;
        let mut enc = DeflateEncoder::new(Counter { inner: out, n: 0 }, Compression::new(level));
        let mut src = File::open(file)?;
        let mut buf = vec![0u8; 1 << 20];
        let mut hasher = crc32fast::Hasher::new();
        let mut size = 0u64;
        loop {
            let n = src.read(&mut buf)?;
            if n == 0 {
                break;
            }
            hasher.update(&buf[..n]);
            size += n as u64;
            on_bytes(size);
            enc.write_all(&buf[..n])?;
        }
        let compressed = enc.finish()?.n;
        self.offset += compressed;
        let crc = hasher.finalize();
        let mut d = Vec::with_capacity(24);
        d.extend_from_slice(&SIG_DESCRIPTOR.to_le_bytes());
        d.extend_from_slice(&crc.to_le_bytes());
        d.extend_from_slice(&compressed.to_le_bytes());
        d.extend_from_slice(&size.to_le_bytes());
        self.write(&d)?;
        self.entries.push(Written {
            name: name_buf,
            flags: FLAG_UTF8 | FLAG_DESCRIPTOR,
            crc,
            compressed,
            size,
            offset,
            method: DEFLATE,
            zip64: true,
            time,
            date,
        });
        Ok(())
    }

    /// The central directory and the end records; then the archive takes its real name. Returns its size.
    pub fn finish(mut self) -> Result<(PathBuf, u64)> {
        let cd_start = self.offset;
        let entries = std::mem::take(&mut self.entries);
        for e in &entries {
            let big = e.zip64 || e.size >= MAX32 || e.compressed >= MAX32 || e.offset >= MAX32;
            let extra = if big { zip64_extra(&[e.size, e.compressed, e.offset]) } else { vec![] };
            let mut c = vec![0u8; 46];
            c[0..4].copy_from_slice(&SIG_CENTRAL.to_le_bytes());
            c[4..6].copy_from_slice(&45u16.to_le_bytes());
            c[6..8].copy_from_slice(&(if big { 45u16 } else { 20 }).to_le_bytes());
            c[8..10].copy_from_slice(&e.flags.to_le_bytes());
            c[10..12].copy_from_slice(&e.method.to_le_bytes());
            c[12..14].copy_from_slice(&e.time.to_le_bytes());
            c[14..16].copy_from_slice(&e.date.to_le_bytes());
            c[16..20].copy_from_slice(&e.crc.to_le_bytes());
            c[20..24].copy_from_slice(&(if big { MAX32 as u32 } else { e.compressed as u32 }).to_le_bytes());
            c[24..28].copy_from_slice(&(if big { MAX32 as u32 } else { e.size as u32 }).to_le_bytes());
            c[28..30].copy_from_slice(&(e.name.len() as u16).to_le_bytes());
            c[30..32].copy_from_slice(&(extra.len() as u16).to_le_bytes());
            c[42..46].copy_from_slice(&(if big { MAX32 as u32 } else { e.offset as u32 }).to_le_bytes());
            self.write(&c)?;
            self.write(&e.name)?;
            self.write(&extra)?;
        }
        let cd_size = self.offset - cd_start;
        let n = entries.len() as u64;
        let needs64 = entries.iter().any(|e| e.zip64) || n >= MAX16 || cd_start >= MAX32 || cd_size >= MAX32;
        if needs64 {
            let eocd64_at = self.offset;
            let mut r = vec![0u8; 56];
            r[0..4].copy_from_slice(&SIG_EOCD64.to_le_bytes());
            r[4..12].copy_from_slice(&44u64.to_le_bytes());
            r[12..14].copy_from_slice(&45u16.to_le_bytes());
            r[14..16].copy_from_slice(&45u16.to_le_bytes());
            r[24..32].copy_from_slice(&n.to_le_bytes());
            r[32..40].copy_from_slice(&n.to_le_bytes());
            r[40..48].copy_from_slice(&cd_size.to_le_bytes());
            r[48..56].copy_from_slice(&cd_start.to_le_bytes());
            let mut l = vec![0u8; 20];
            l[0..4].copy_from_slice(&SIG_LOCATOR64.to_le_bytes());
            l[8..16].copy_from_slice(&eocd64_at.to_le_bytes());
            l[16..20].copy_from_slice(&1u32.to_le_bytes());
            self.write(&r)?;
            self.write(&l)?;
        }
        let mut e = vec![0u8; 22];
        e[0..4].copy_from_slice(&SIG_EOCD.to_le_bytes());
        e[8..10].copy_from_slice(&(n.min(MAX16) as u16).to_le_bytes());
        e[10..12].copy_from_slice(&(n.min(MAX16) as u16).to_le_bytes());
        e[12..16].copy_from_slice(&(if needs64 { MAX32 as u32 } else { cd_size as u32 }).to_le_bytes());
        e[16..20].copy_from_slice(&(if needs64 { MAX32 as u32 } else { cd_start as u32 }).to_le_bytes());
        self.write(&e)?;
        let out = self.out.take().unwrap();
        let f = out.into_inner().map_err(|e| anyhow!("{e}"))?;
        f.sync_all()?;
        drop(f);
        std::fs::rename(&self.partial, &self.file)?;
        Ok((self.file.clone(), self.offset))
    }

    /// Stops and deletes what was written (a cancelled or failed export leaves nothing behind).
    pub fn abort(mut self) {
        self.out = None;
        let _ = std::fs::remove_file(&self.partial);
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct ZipEntry {
    pub name: String,
    pub method: u16,
    pub crc: u32,
    pub compressed: u64,
    pub size: u64,
    pub offset: u64,
}

fn u16_at(b: &[u8], i: usize) -> u64 {
    u16::from_le_bytes([b[i], b[i + 1]]) as u64
}
fn u32_at(b: &[u8], i: usize) -> u64 {
    u32::from_le_bytes(b[i..i + 4].try_into().unwrap()) as u64
}
fn u64_at(b: &[u8], i: usize) -> u64 {
    u64::from_le_bytes(b[i..i + 8].try_into().unwrap())
}

fn read_at(f: &mut File, pos: u64, len: usize) -> Result<Vec<u8>> {
    f.seek(SeekFrom::Start(pos))?;
    let mut b = vec![0u8; len];
    let mut got = 0;
    while got < len {
        let n = f.read(&mut b[got..])?;
        if n == 0 {
            break;
        }
        got += n;
    }
    b.truncate(got);
    Ok(b)
}

/// The archive's entries, from its central directory. Fails with a readable message when it is not a ZIP file.
pub fn read_zip(file: &Path) -> Result<Vec<ZipEntry>> {
    let mut f = File::open(file)?;
    let size = f.metadata()?.len();
    let tail_len = size.min(22 + MAX16 + 20);
    let tail = read_at(&mut f, size - tail_len, tail_len as usize)?;
    let mut at: Option<usize> = None;
    if tail.len() >= 22 {
        let mut i = tail.len() - 22;
        loop {
            if u32_at(&tail, i) == SIG_EOCD as u64 {
                at = Some(i);
                break;
            }
            if i == 0 {
                break;
            }
            i -= 1;
        }
    }
    let Some(at) = at else { bail!("This file is not a ZIP archive (or it is incomplete: still downloading or syncing?).") };
    let mut count = u16_at(&tail, at + 10);
    let mut cd_size = u32_at(&tail, at + 12);
    let mut cd_start = u32_at(&tail, at + 16);
    if at >= 20 && u32_at(&tail, at - 20) == SIG_LOCATOR64 as u64 {
        let r = read_at(&mut f, u64_at(&tail, at - 20 + 8), 56)?;
        if r.len() < 56 || u32_at(&r, 0) != SIG_EOCD64 as u64 {
            bail!("The ZIP archive is damaged (ZIP64 end record).");
        }
        count = u64_at(&r, 32);
        cd_size = u64_at(&r, 40);
        cd_start = u64_at(&r, 48);
    }
    let cd = read_at(&mut f, cd_start, cd_size as usize)?;
    let mut entries = Vec::new();
    let mut p = 0usize;
    for _ in 0..count {
        if p + 46 > cd.len() || u32_at(&cd, p) != SIG_CENTRAL as u64 {
            bail!("The ZIP archive is damaged (central directory).");
        }
        let method = u16_at(&cd, p + 10) as u16;
        let crc = u32_at(&cd, p + 16) as u32;
        let mut compressed = u32_at(&cd, p + 20);
        let mut sz = u32_at(&cd, p + 24);
        let name_len = u16_at(&cd, p + 28) as usize;
        let extra_len = u16_at(&cd, p + 30) as usize;
        let comment_len = u16_at(&cd, p + 32) as usize;
        let mut offset = u32_at(&cd, p + 42);
        let name = String::from_utf8_lossy(&cd[p + 46..p + 46 + name_len]).into_owned();
        let extra = &cd[p + 46 + name_len..p + 46 + name_len + extra_len];
        let mut x = 0usize;
        while x + 4 <= extra.len() {
            let id = u16_at(extra, x);
            let len = u16_at(extra, x + 2) as usize;
            if id == 1 {
                let mut y = x + 4;
                if sz == MAX32 {
                    sz = u64_at(extra, y);
                    y += 8;
                }
                if compressed == MAX32 {
                    compressed = u64_at(extra, y);
                    y += 8;
                }
                if offset == MAX32 {
                    offset = u64_at(extra, y);
                }
            }
            x += 4 + len;
        }
        if method != DEFLATE && method != STORE {
            bail!("Unsupported compression in {name}.");
        }
        entries.push(ZipEntry { name, method, crc, compressed, size: sz, offset });
        p += 46 + name_len + extra_len + comment_len;
    }
    Ok(entries)
}

fn data_start(f: &mut File, e: &ZipEntry) -> Result<u64> {
    let h = read_at(f, e.offset, 30)?;
    if h.len() < 30 || u32_at(&h, 0) != SIG_LOCAL as u64 {
        bail!("The ZIP archive is damaged ({}).", e.name);
    }
    Ok(e.offset + 30 + u16_at(&h, 26) + u16_at(&h, 28))
}

struct CrcCheck<W: Write> {
    inner: W,
    hasher: crc32fast::Hasher,
    n: u64,
}
impl<W: Write> Write for CrcCheck<W> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let k = self.inner.write(buf)?;
        self.hasher.update(&buf[..k]);
        self.n += k as u64;
        Ok(k)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.inner.flush()
    }
}

/// Writes one entry to `dest` (streamed), checking its size and CRC-32.
pub fn extract_entry(file: &Path, e: &ZipEntry, dest: &Path, on_bytes: &mut dyn FnMut(u64)) -> Result<()> {
    let mut f = File::open(file)?;
    let start = data_start(&mut f, e)?;
    f.seek(SeekFrom::Start(start))?;
    let src = f.take(e.compressed);
    let mut out = CrcCheck { inner: BufWriter::new(File::create(dest)?), hasher: crc32fast::Hasher::new(), n: 0 };
    let mut reader: Box<dyn Read> = if e.method == DEFLATE && e.compressed > 0 { Box::new(DeflateDecoder::new(src)) } else { Box::new(src) };
    let mut buf = vec![0u8; 1 << 20];
    loop {
        let n = reader.read(&mut buf).map_err(|_| anyhow!("{} in the ZIP archive is damaged (its check failed): download it again.", e.name))?;
        if n == 0 {
            break;
        }
        out.write_all(&buf[..n])?;
        on_bytes(out.n);
    }
    out.flush()?;
    if out.n != e.size || out.hasher.clone().finalize() != e.crc {
        bail!("{} in the ZIP archive is damaged (its check failed): download it again.", e.name);
    }
    Ok(())
}

/// One small entry into memory (the manifest), checked.
pub fn read_entry(file: &Path, e: &ZipEntry) -> Result<Vec<u8>> {
    if e.size > 64 << 20 {
        bail!("{} is too large to read into memory.", e.name);
    }
    let mut f = File::open(file)?;
    let start = data_start(&mut f, e)?;
    let b = read_at(&mut f, start, e.compressed as usize)?;
    let raw = if e.method == DEFLATE {
        let mut out = Vec::new();
        DeflateDecoder::new(&b[..]).read_to_end(&mut out).map_err(|_| anyhow!("{} in the ZIP archive is damaged: download it again.", e.name))?;
        out
    } else {
        b
    };
    if raw.len() as u64 != e.size || crc32fast::hash(&raw) != e.crc {
        bail!("{} in the ZIP archive is damaged: download it again.", e.name);
    }
    Ok(raw)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_written_archive_reads_back() {
        let dir = tempfile::tempdir().unwrap();
        let big = dir.path().join("big.db");
        let mut content: Vec<u8> = (0..(3u32 << 20)).map(|i| (i.wrapping_mul(2654435761) >> 13) as u8).collect();
        content.extend(b"irelia".iter().cycle().take(3 << 20));
        std::fs::write(&big, &content).unwrap();
        let out = dir.path().join("Agurin-EUW_dataset.zip");
        let mut w = ZipWriter::new(&out);
        w.add_buffer("manifest.json", r#"{"kind":"dataset","name":"Ågurin"}"#.as_bytes()).unwrap();
        w.add_file("Agurin-EUW_dataset.db", &big, &mut |_| {}).unwrap();
        w.add_buffer("empty.txt", b"").unwrap();
        w.add_buffer("games.csv", "match,win\nKR_1,1\n".repeat(1000).as_bytes()).unwrap();
        assert!(PathBuf::from(format!("{}.partial", out.display())).exists() && !out.exists());
        w.finish().unwrap();
        assert!(out.exists());
        let entries = read_zip(&out).unwrap();
        assert_eq!(entries.iter().map(|e| e.name.as_str()).collect::<Vec<_>>(), ["manifest.json", "Agurin-EUW_dataset.db", "empty.txt", "games.csv"]);
        let m: serde_json::Value = serde_json::from_slice(&read_entry(&out, &entries[0]).unwrap()).unwrap();
        assert_eq!(m["name"], "Ågurin");
        let back = dir.path().join("back.db");
        extract_entry(&out, &entries[1], &back, &mut |_| {}).unwrap();
        assert_eq!(std::fs::read(&back).unwrap(), content);
        assert_eq!(read_entry(&out, &entries[2]).unwrap().len(), 0);
        let py = std::process::Command::new("python3")
            .args(["-I", "-c", "import sys,zipfile;z=zipfile.ZipFile(sys.argv[1]);print(z.testzip());print(len(z.namelist()))"])
            .arg(&out)
            .output();
        if let Ok(o) = py {
            if o.status.success() {
                assert_eq!(String::from_utf8_lossy(&o.stdout).trim().replace("\r\n", "\n").replace('\n', "|"), "None|4");
            }
        }
    }

    #[test]
    fn a_damaged_or_incomplete_archive_says_so() {
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("a.zip");
        let mut w = ZipWriter::new(&out);
        w.add_buffer("manifest.json", br#"{"kind":"dataset"}"#).unwrap();
        w.finish().unwrap();
        let half = dir.path().join("half.zip");
        std::fs::write(&half, &std::fs::read(&out).unwrap()[..40]).unwrap();
        assert!(read_zip(&half).unwrap_err().to_string().contains("not a ZIP archive"));
        let mut bytes = std::fs::read(&out).unwrap();
        bytes[30 + "manifest.json".len() + 2] ^= 0xff;
        let bad = dir.path().join("bad.zip");
        std::fs::write(&bad, &bytes).unwrap();
        let e = read_zip(&bad).unwrap();
        assert!(read_entry(&bad, &e[0]).unwrap_err().to_string().contains("damaged"));
    }

    #[test]
    fn abort_leaves_nothing_behind() {
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("x.zip");
        let mut w = ZipWriter::new(&out);
        w.add_buffer("a.txt", b"hello").unwrap();
        w.abort();
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
    }
}
