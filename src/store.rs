//! Queue and settings on disk, as JSON.
use crate::model::{Saved, Status};
use std::{env, fs, io, path::PathBuf};

pub fn path() -> PathBuf {
    let state = env::var_os("XDG_STATE_HOME").map(PathBuf::from).filter(|p| p.is_absolute());
    let home = || PathBuf::from(env::var_os("HOME").unwrap_or_default());
    state.unwrap_or_else(|| home().join(".local/state")).join("aqua-fetch/queue.json")
}

fn default_dir() -> PathBuf {
    PathBuf::from(env::var_os("HOME").unwrap_or_default()).join("Downloads")
}

/// A missing or unreadable file gives an empty queue. Rows that were active
/// when the app last closed come back paused (listings come back queued).
/// A file that exists but cannot be read as a queue is kept as `queue.json.bad`
/// (the second value says so), never silently overwritten.
pub fn load(path: &std::path::Path) -> (Saved, bool) {
    let parsed = fs::read(path).ok().map(|b| serde_json::from_slice::<Saved>(&b));
    let bad = matches!(parsed, Some(Err(_)));
    if bad {
        let _ = fs::rename(path, path.with_extension("json.bad"));
    }
    let mut s = parsed.and_then(Result::ok).unwrap_or(Saved { dir: default_dir(), parallel: 4, next_id: 1, items: vec![] });
    s.parallel = s.parallel.clamp(1, 16);
    for i in &mut s.items {
        match i.status {
            Status::Downloading | Status::Verifying => i.status = Status::Paused,
            Status::Listing => i.status = Status::Queued,
            _ => {}
        }
    }
    (s, bad)
}

/// Writes to a temp file then renames, so a crash never leaves half a queue.
pub fn save(path: &std::path::Path, s: &Saved) -> io::Result<()> {
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension("json.tmp");
    fs::write(&tmp, serde_json::to_vec(s)?)?;
    fs::rename(tmp, path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Item;

    #[test]
    fn round_trip_pauses_active_rows() {
        let p = env::temp_dir().join(format!("aqua-fetch-test-{}/q.json", std::process::id()));
        let item = |id, status| Item {
            id,
            url: "https://h/x".into(),
            dest: "/d/x".into(),
            status,
            bytes: 5,
            total: Some(9),
            etag: Some("\"e\"".into()),
            sha256: None,
            speed: 3.0,
        };
        let s = Saved {
            dir: "/d".into(),
            parallel: 99,
            next_id: 7,
            items: vec![item(1, Status::Downloading), item(2, Status::Failed("boom".into())), item(3, Status::Listing)],
        };
        save(&p, &s).unwrap();
        let (l, bad) = load(&p);
        assert!(!bad);
        assert_eq!((l.parallel, l.next_id, l.items.len()), (16, 7, 3));
        assert_eq!(l.items[0].status, Status::Paused);
        assert_eq!(l.items[0].bytes, 5);
        assert_eq!(l.items[1].status, Status::Failed("boom".into()));
        assert_eq!(l.items[2].status, Status::Queued);
        fs::remove_dir_all(p.parent().unwrap()).unwrap();
        assert!(load(&p).0.items.is_empty());
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(&p, "{ not json").unwrap();
        assert!(load(&p).1);
        assert!(p.with_extension("json.bad").exists() && !p.exists());
        fs::remove_dir_all(p.parent().unwrap()).unwrap();
    }
}
