//! Small helpers shared by every module.

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

/// `Date.now()`.
pub fn now_ms() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as i64).unwrap_or(0)
}

/// `path.resolve(p)`: absolute, without `.` and `..` parts (no symlink resolution, as Node).
pub fn resolve(p: impl AsRef<Path>) -> PathBuf {
    let p = p.as_ref();
    let abs = if p.is_absolute() { p.to_path_buf() } else { std::env::current_dir().unwrap_or_default().join(p) };
    let mut out = PathBuf::new();
    for c in abs.components() {
        match c {
            std::path::Component::ParentDir => {
                out.pop();
            }
            std::path::Component::CurDir => {}
            c => out.push(c.as_os_str()),
        }
    }
    out
}

/// An environment variable, None when unset or empty.
pub fn env(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|v| !v.is_empty())
}

/// `process.loadEnvFile('.env')`: KEY=VALUE lines; existing variables win; quotes stripped; # comments.
pub fn load_env_file(file: &Path) {
    let Ok(text) = std::fs::read_to_string(file) else { return };
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let line = line.strip_prefix("export ").unwrap_or(line);
        let Some((k, v)) = line.split_once('=') else { continue };
        let k = k.trim();
        let mut v = v.trim().to_string();
        if (v.starts_with('"') && v.ends_with('"') && v.len() >= 2) || (v.starts_with('\'') && v.ends_with('\'') && v.len() >= 2) {
            v = v[1..v.len() - 1].to_string();
        } else if let Some(i) = v.find(" #") {
            v = v[..i].trim().to_string();
        }
        if std::env::var_os(k).is_none() {
            std::env::set_var(k, v);
        }
    }
}

/// Writes `bytes` to `file` atomically (a temporary file next to it, then a rename).
pub fn write_atomic(file: &Path, bytes: &[u8]) -> std::io::Result<()> {
    if let Some(dir) = file.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = file.with_extension(format!(
        "{}.{}.tmp",
        file.extension().and_then(|e| e.to_str()).unwrap_or(""),
        std::process::id()
    ));
    std::fs::write(&tmp, bytes)?;
    std::fs::rename(&tmp, file)
}

/// Recursive delete that tolerates Windows' transient sharing errors on just-closed files (fsUtil.ts `remove`).
pub fn remove(target: &Path) -> std::io::Result<()> {
    let mut last = Ok(());
    for _ in 0..6 {
        let r = match std::fs::symlink_metadata(target) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(e) => Err(e),
            Ok(m) if m.is_dir() => std::fs::remove_dir_all(target),
            Ok(_) => std::fs::remove_file(target),
        };
        match r {
            Ok(()) => return Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(e) => last = Err(e),
        }
        std::thread::sleep(std::time::Duration::from_millis(200));
    }
    last
}

/// A file found by `walk_files`.
#[derive(Clone, Debug)]
pub struct FileEntry {
    pub path: PathBuf,
    pub size: u64,
    pub mtime_ms: f64,
}

pub fn mtime_ms(m: &std::fs::Metadata) -> f64 {
    m.modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_secs_f64() * 1000.0)
        .unwrap_or(0.0)
}

/// Every regular file under `dir` (depth-first, in directory order). A missing directory yields nothing.
pub fn walk_files(dir: &Path) -> Vec<FileEntry> {
    let mut out = Vec::new();
    fn visit(dir: &Path, out: &mut Vec<FileEntry>) {
        let Ok(rd) = std::fs::read_dir(dir) else { return };
        let mut entries: Vec<_> = rd.filter_map(|e| e.ok()).collect();
        entries.sort_by_key(|e| e.file_name());
        let mut files = Vec::new();
        for e in entries {
            let Ok(ft) = e.file_type() else { continue };
            if ft.is_dir() {
                // (Node yields the files found so far before descending.)
                out.append(&mut files);
                visit(&e.path(), out);
            } else if ft.is_file() {
                if let Ok(m) = e.metadata() {
                    files.push(FileEntry { path: e.path(), size: m.len(), mtime_ms: mtime_ms(&m) });
                }
            }
        }
        out.append(&mut files);
    }
    visit(dir, &mut out);
    out
}

/// Total size and file count of a file or directory tree (0 when it does not exist).
pub fn measure(target: &Path) -> (u64, u64) {
    match std::fs::metadata(target) {
        Err(_) => (0, 0),
        Ok(m) if m.is_file() => (m.len(), 1),
        Ok(_) => {
            let files = walk_files(target);
            (files.iter().map(|f| f.size).sum(), files.len() as u64)
        }
    }
}

/// A random v4 UUID (crypto.randomUUID's shape), from the process's random source.
pub fn random_uuid() -> String {
    let h = random_hex(16);
    let mut b: Vec<u8> = (0..16).map(|i| u8::from_str_radix(&h[i * 2..i * 2 + 2], 16).unwrap_or(0)).collect();
    b[6] = (b[6] & 0x0f) | 0x40;
    b[8] = (b[8] & 0x3f) | 0x80;
    let x: String = b.iter().map(|v| format!("{v:02x}")).collect();
    format!("{}-{}-{}-{}-{}", &x[0..8], &x[8..12], &x[12..16], &x[16..20], &x[20..32])
}

/// `n` random bytes as hex (time, process id and a xorshift, hashed with SHA-256). Not for secrets.
pub fn random_hex(n: usize) -> String {
    use sha2::Digest;
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let mut seed = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0) as u64
        ^ ((std::process::id() as u64) << 32)
        ^ COUNTER.fetch_add(0x9E37_79B9_7F4A_7C15, Ordering::Relaxed);
    let mut out = String::new();
    for _ in 0..n.max(1) {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        out.push_str(&format!("{:02x}", (seed >> 24) as u8));
    }
    let h = sha2::Sha256::digest(out.as_bytes());
    h.iter().cycle().take(n).map(|b| format!("{b:02x}")).collect()
}
