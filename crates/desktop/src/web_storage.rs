//! The dashboard's own conveniences from 0.x: Electron kept its localStorage (the tab you were on, whether the details
//! show) in Chromium's LevelDB under %APPDATA%\irelia-companion\Local Storage\leveldb, which WebView2 doesn't read. The
//! first 1.x start reads them from a copy of that folder (the original is never touched) and the bridge script puts
//! them back into the page's localStorage once. The lane already lived in config.json.
//!
//! Chromium's layout: keys `_<origin>\0<key>`, the key and the value each a byte saying how they are encoded (1:
//! Latin-1, 0: UTF-16LE) followed by the text.

use std::path::Path;

/// The keys 1.x carries over (web/src: App.tsx `tab`, components/common.tsx `showDetails`).
pub const CARRIED: [&str; 2] = ["tab", "showDetails"];

fn decode(b: &[u8]) -> Option<String> {
    let (kind, rest) = b.split_first()?;
    match kind {
        1 => Some(rest.iter().map(|&c| c as char).collect()),
        0 if rest.len() % 2 == 0 => String::from_utf16(&rest.chunks(2).map(|c| u16::from_le_bytes([c[0], c[1]])).collect::<Vec<_>>()).ok(),
        _ => None,
    }
}

fn copy_dir(from: &Path, to: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(to)?;
    for e in std::fs::read_dir(from)? {
        let e = e?;
        if e.file_type()?.is_file() && e.file_name() != "LOCK" {
            std::fs::copy(e.path(), to.join(e.file_name()))?;
        }
    }
    Ok(())
}

/// The carried-over keys 0.x's dashboard stored for http://127.0.0.1:<port> (the preferred port's first).
pub fn electron_local_storage(config_dir: &Path, preferred_port: u16) -> Vec<(String, String)> {
    let src = config_dir.join("Local Storage").join("leveldb");
    if !src.is_dir() {
        return vec![];
    }
    let Ok(tmp) = tempfile_dir() else { return vec![] };
    if copy_dir(&src, &tmp).is_err() {
        let _ = std::fs::remove_dir_all(&tmp);
        return vec![];
    }
    let mut found: Vec<(u16, String, String)> = vec![];
    if let Ok(mut db) = rusty_leveldb::DB::open(&tmp, rusty_leveldb::Options::default()) {
        if let Ok(mut it) = db.new_iter() {
            use rusty_leveldb::LdbIterator;
            while let Some((k, v)) = it.next() {
                let Some(rest) = k.strip_prefix(b"_http://127.0.0.1:") else { continue };
                let Some(nul) = rest.iter().position(|&c| c == 0) else { continue };
                let Ok(port) = std::str::from_utf8(&rest[..nul]).unwrap_or("").parse::<u16>() else { continue };
                let (Some(key), Some(value)) = (decode(&rest[nul + 1..]), decode(&v)) else { continue };
                if CARRIED.contains(&key.as_str()) {
                    found.push((port, key, value));
                }
            }
        }
    }
    let _ = std::fs::remove_dir_all(&tmp);
    // The preferred port's values win over another port's.
    found.sort_by_key(|(p, _, _)| (*p != preferred_port, *p));
    let mut out: Vec<(String, String)> = vec![];
    for (_, k, v) in found {
        if !out.iter().any(|(x, _)| *x == k) {
            out.push((k, v));
        }
    }
    out
}

fn tempfile_dir() -> std::io::Result<std::path::PathBuf> {
    let d = std::env::temp_dir().join(format!("irelia-web-storage-{}-{}", std::process::id(), irelia::util::now_ms()));
    std::fs::create_dir_all(&d)?;
    Ok(d)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn latin(s: &str) -> Vec<u8> {
        let mut v = vec![1u8];
        v.extend(s.bytes());
        v
    }

    #[test]
    fn reads_what_chromium_stored_for_the_dashboard() {
        let dir = tempfile::tempdir().unwrap();
        let ldb = dir.path().join("Local Storage").join("leveldb");
        {
            let mut db = rusty_leveldb::DB::open(&ldb, rusty_leveldb::Options::default()).unwrap();
            let key = |port: u16, k: &str| {
                let mut v = format!("_http://127.0.0.1:{port}").into_bytes();
                v.push(0);
                v.extend(latin(k));
                v
            };
            db.put(b"META:http://127.0.0.1:45174", b"x").unwrap();
            db.put(&key(45174, "tab"), &latin("data")).unwrap();
            db.put(&key(51000, "tab"), &latin("games")).unwrap();
            let mut utf16 = vec![0u8];
            utf16.extend("0".encode_utf16().flat_map(|c| c.to_le_bytes()));
            db.put(&key(51000, "showDetails"), &utf16).unwrap();
            db.put(&key(45174, "somethingElse"), &latin("1")).unwrap();
            db.flush().unwrap();
        }
        let got = electron_local_storage(dir.path(), 45174);
        assert_eq!(got, vec![("tab".to_string(), "data".to_string()), ("showDetails".to_string(), "0".to_string())]);
        assert!(electron_local_storage(&dir.path().join("none"), 45174).is_empty());
    }
}
