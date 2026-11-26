//! Types shared by the UI, the download engine and the saved queue.
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Status {
    Queued,
    /// An S3 bucket/prefix row whose objects are being listed.
    Listing,
    Downloading,
    Verifying,
    Paused,
    Done,
    Failed(String),
    /// Needs something only the user can give (fresh Cloudflare cookies), then Retry.
    Blocked(String),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Item {
    pub id: u64,
    pub url: String,
    /// Absolute final path. Data is written to `part_path(&dest)` until complete.
    pub dest: PathBuf,
    pub status: Status,
    pub bytes: u64,
    pub total: Option<u64>,
    /// Validator from the first response, sent as `If-Range` when resuming.
    pub etag: Option<String>,
    /// Expected SHA-256, lowercase hex.
    pub sha256: Option<String>,
    #[serde(skip)]
    pub speed: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Saved {
    pub dir: PathBuf,
    pub parallel: usize,
    pub next_id: u64,
    pub items: Vec<Item>,
}

pub fn part_path(dest: &Path) -> PathBuf {
    let mut s = dest.as_os_str().to_owned();
    s.push(".part");
    s.into()
}

/// Turns an untrusted URL path or S3 key into a relative path that cannot
/// leave the download folder: no `..`, no absolute parts, no control chars.
pub fn safe_rel(raw: &str) -> PathBuf {
    let p: PathBuf = raw
        .split(['/', '\\'])
        .filter(|c| !c.is_empty() && *c != "." && *c != "..")
        .map(|c| c.chars().map(|ch| if ch.is_control() { '_' } else { ch }).collect::<String>())
        .collect();
    if p.as_os_str().is_empty() { "download".into() } else { p }
}

/// File name for a plain URL: last path segment, percent-decoded, sanitised.
pub fn file_name(url: &str) -> PathBuf {
    let path = url.split(['?', '#']).next().unwrap_or("");
    let path = path.split_once("://").map_or(path, |(_, rest)| rest.split_once('/').map_or("", |(_, p)| p));
    let last = path.rsplit('/').next().unwrap_or("");
    // Decode first, then keep only the last component, so %2F..%2F cannot traverse.
    let decoded = percent_decode(last);
    safe_rel(decoded.rsplit(['/', '\\']).next().unwrap_or(""))
}

pub fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        let hex = (b[i] == b'%').then(|| s.get(i + 1..i + 3)).flatten();
        match hex.and_then(|h| u8::from_str_radix(h, 16).ok()) {
            Some(v) => {
                out.push(v);
                i += 3;
            }
            None => {
                out.push(b[i]);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Returns `p`, or `p` with " (n)" before the extension, so that `taken` is false.
pub fn unique(p: PathBuf, taken: &dyn Fn(&Path) -> bool) -> PathBuf {
    if !taken(&p) {
        return p;
    }
    let stem = p.file_stem().unwrap_or_default().to_string_lossy().into_owned();
    let ext = p.extension().map(|e| format!(".{}", e.to_string_lossy())).unwrap_or_default();
    (1..)
        .map(|n| p.with_file_name(format!("{stem} ({n}){ext}")))
        .find(|c| !taken(c))
        .unwrap()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn safe_rel_cannot_escape() {
        assert_eq!(safe_rel("../../etc/passwd"), PathBuf::from("etc/passwd"));
        assert_eq!(safe_rel("/abs/./x"), PathBuf::from("abs/x"));
        assert_eq!(safe_rel("a\\..\\b"), PathBuf::from("a/b"));
        assert_eq!(safe_rel("../.."), PathBuf::from("download"));
        assert!(safe_rel("/a/b").is_relative());
    }

    #[test]
    fn file_name_from_url() {
        assert_eq!(file_name("https://h.example/a/my%20data.csv?x=1#f"), PathBuf::from("my data.csv"));
        assert_eq!(file_name("https://h.example/a/%2e%2e%2f%2e%2e%2fetc%2fpasswd"), PathBuf::from("passwd"));
        assert_eq!(file_name("https://h.example/"), PathBuf::from("download"));
        assert_eq!(file_name("https://h.example"), PathBuf::from("download"));
    }

    #[test]
    fn unique_adds_counter() {
        let taken = |p: &Path| p == Path::new("/d/a.csv") || p == Path::new("/d/a (1).csv");
        assert_eq!(unique("/d/a.csv".into(), &taken), PathBuf::from("/d/a (2).csv"));
        assert_eq!(unique("/d/b.csv".into(), &taken), PathBuf::from("/d/b.csv"));
    }
}
