//! Downloads one item to disk with resume, retry and an optional SHA-256 check.
use crate::model::{Item, part_path};
use futures_util::StreamExt;
use reqwest::header::{CONTENT_RANGE, ETAG, IF_RANGE, LAST_MODIFIED, RANGE};
use sha2::{Digest, Sha256};
use std::time::{Duration, Instant};
use tokio::io::AsyncWriteExt;

#[derive(Debug, Clone, PartialEq)]
pub enum Event {
    /// Bytes on disk so far and the full size when the server gave one.
    Progress { bytes: u64, total: Option<u64> },
    /// Validator (ETag, else Last-Modified) of the response being saved; the caller stores it in Item.etag.
    Etag(String),
    Verifying,
}

/// Retries wait 1x, 2x, 4x this.
const BASE_DELAY_MS: u64 = if cfg!(test) { 10 } else { 1000 };
const RETRIES: u32 = 3;

/// (message, worth retrying)
type Fail = (String, bool);

/// Downloads `item.url` to `item.dest`. Returns Ok(()) once the finished file is at `item.dest`.
/// Cancel/pause = the caller drops this future; the `.part` file stays for a later resume.
pub async fn download(client: &reqwest::Client, item: &Item, mut on: impl FnMut(Event) + Send) -> Result<(), String> {
    if let Some(dir) = item.dest.parent() {
        tokio::fs::create_dir_all(dir).await.map_err(|e| format!("Cannot create folder: {e}"))?;
    }
    let mut etag = item.etag.clone();
    for attempt in 0..=RETRIES {
        match fetch(client, item, &mut etag, &mut on).await {
            Ok(()) => break,
            Err((msg, retry)) => {
                if !retry || attempt == RETRIES {
                    return Err(msg);
                }
                tokio::time::sleep(Duration::from_millis(BASE_DELAY_MS << attempt)).await;
            }
        }
    }
    let part = part_path(&item.dest);
    if let Some(want) = &item.sha256 {
        on(Event::Verifying);
        let p = part.clone();
        let got = tokio::task::spawn_blocking(move || hash_file(&p))
            .await
            .map_err(|e| e.to_string())?
            .map_err(|e| format!("Cannot read file to verify: {e}"))?;
        if !got.eq_ignore_ascii_case(want.trim()) {
            // Drop the bad data, so Retry downloads again instead of re-checking the same bytes.
            let _ = tokio::fs::remove_file(&part).await;
            return Err(format!("SHA-256 mismatch: expected {want}, got {got}"));
        }
    }
    if tokio::fs::try_exists(&item.dest).await.unwrap_or(false) {
        return Err("Destination file already exists".into());
    }
    tokio::fs::rename(&part, &item.dest).await.map_err(|e| format!("Cannot move finished file: {e}"))
}

fn hash_file(p: &std::path::Path) -> std::io::Result<String> {
    use std::io::Read;
    let mut f = std::fs::File::open(p)?;
    let mut h = Sha256::new();
    let mut buf = vec![0u8; 1 << 16];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
    }
    Ok(h.finalize().iter().map(|b| format!("{b:02x}")).collect())
}

/// One attempt: brings the .part file to full size, resuming when possible.
async fn fetch(client: &reqwest::Client, item: &Item, etag: &mut Option<String>, on: &mut impl FnMut(Event)) -> Result<(), Fail> {
    let part = part_path(&item.dest);
    let mut n = tokio::fs::metadata(&part).await.map(|m| m.len()).unwrap_or(0);
    if etag.is_none() {
        n = 0; // nothing to prove the old bytes belong to this file
    }
    let mut restarted = false;
    let resp = loop {
        let mut req = client.get(&item.url);
        if let (true, Some(v)) = (n > 0, etag.as_ref()) {
            req = req.header(RANGE, format!("bytes={n}-")).header(IF_RANGE, v);
        }
        let resp = req.send().await.map_err(|e| (format!("Network error: {}", short(&e)), true))?;
        if resp.status().as_u16() == 416 && n > 0 {
            let size = resp.headers().get(CONTENT_RANGE).and_then(|v| v.to_str().ok()).and_then(|v| v.rsplit('/').next()?.parse::<u64>().ok());
            if size == Some(n) {
                on(Event::Progress { bytes: n, total: Some(n) });
                return Ok(());
            }
            if restarted {
                return Err(("HTTP 416 Range Not Satisfiable".into(), false));
            }
            restarted = true;
            n = 0;
            let _ = tokio::fs::remove_file(&part).await;
            continue;
        }
        break resp;
    };
    let st = resp.status();
    if !st.is_success() {
        let c = st.as_u16();
        let retry = st.is_server_error() || c == 408 || c == 429;
        return Err((format!("HTTP {c} {}", st.canonical_reason().unwrap_or("")).trim_end().to_string(), retry));
    }
    let h = resp.headers();
    if st.as_u16() == 206 {
        // Some servers ignore If-Range. Appending the wrong range or a changed file would silently corrupt the result.
        let head = |k| h.get(k).and_then(|v| v.to_str().ok());
        let range_ok = head(CONTENT_RANGE).is_some_and(|v| v.starts_with(&format!("bytes {n}-")));
        let stored_is_etag = etag.as_deref().is_some_and(|e| e.starts_with('"') || e.starts_with("W/"));
        let tag_ok = !stored_is_etag || head(ETAG).is_none_or(|t| etag.as_deref() == Some(t));
        if !(range_ok && tag_ok) {
            *etag = None;
            let _ = tokio::fs::remove_file(&part).await;
            return Err(("Server could not resume this file; starting over".into(), true));
        }
    }
    let total = if st.as_u16() == 206 {
        h.get(CONTENT_RANGE)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.rsplit('/').next()?.parse::<u64>().ok())
            .or(resp.content_length().map(|l| n + l))
    } else {
        n = 0;
        let v = [ETAG, LAST_MODIFIED].iter().find_map(|k| h.get(k)?.to_str().ok()).map(str::to_string);
        if let Some(v) = &v {
            on(Event::Etag(v.clone()));
        }
        *etag = v;
        resp.content_length()
    };
    let mut file = tokio::fs::OpenOptions::new().create(true).write(true).append(n > 0).truncate(n == 0).open(&part).await.map_err(io_fail)?;
    on(Event::Progress { bytes: n, total });
    let mut last = Instant::now();
    let mut stream = resp.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|e| (format!("Network error: {}", short(&e)), true))?;
        file.write_all(&chunk).await.map_err(io_fail)?;
        n += chunk.len() as u64;
        if last.elapsed() >= Duration::from_millis(200) {
            last = Instant::now();
            on(Event::Progress { bytes: n, total });
        }
    }
    file.flush().await.map_err(io_fail)?;
    drop(file);
    on(Event::Progress { bytes: n, total });
    match total {
        Some(t) if n != t => Err((format!("Connection closed early ({n} of {t} bytes)"), true)),
        _ => Ok(()),
    }
}

fn io_fail(e: std::io::Error) -> Fail {
    (format!("Disk error: {e}"), false)
}

/// Outermost reqwest message only; the full chain is too long for a table row.
fn short(e: &reqwest::Error) -> String {
    let mut s = e.to_string();
    if s.len() > 80 {
        s.truncate(80);
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Status;
    use std::path::PathBuf;
    use std::sync::{Arc, Mutex};
    use tokio::io::AsyncReadExt;
    use tokio::net::TcpListener;

    const ETAG_V: &str = "\"v1\"";

    struct Srv {
        url: String,
        /// Range header (if any) of every request, in order.
        log: Arc<Mutex<Vec<Option<String>>>>,
    }

    /// `cut` = (bytes to send, number of first requests to cut off).
    async fn serve(body: Vec<u8>, cut: Option<(usize, usize)>) -> Srv {
        let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/f.bin", l.local_addr().unwrap());
        let log: Arc<Mutex<Vec<Option<String>>>> = Default::default();
        let log2 = log.clone();
        tokio::spawn(async move {
            loop {
                let (mut s, _) = l.accept().await.unwrap();
                let (body, log) = (body.clone(), log2.clone());
                tokio::spawn(async move {
                    let mut req = Vec::new();
                    let mut buf = [0u8; 1024];
                    while !req.windows(4).any(|w| w == b"\r\n\r\n") {
                        let n = s.read(&mut buf).await.unwrap_or(0);
                        if n == 0 {
                            return;
                        }
                        req.extend_from_slice(&buf[..n]);
                    }
                    let text = String::from_utf8_lossy(&req).to_string();
                    let hdr = |name: &str| {
                        text.lines().find_map(|l| {
                            let (k, v) = l.split_once(':')?;
                            k.eq_ignore_ascii_case(name).then(|| v.trim().to_string())
                        })
                    };
                    let range = hdr("range");
                    let idx = {
                        let mut g = log.lock().unwrap();
                        g.push(range.clone());
                        g.len() - 1
                    };
                    let size = body.len();
                    let rn = range.as_deref().and_then(|r| r.strip_prefix("bytes=")?.strip_suffix('-')?.parse::<usize>().ok());
                    let head = if text.contains(" /missing ") {
                        "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_string()
                    } else if let Some(n) = rn {
                        if n >= size {
                            format!("HTTP/1.1 416 Range Not Satisfiable\r\nContent-Range: bytes */{size}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
                        } else if hdr("if-range").as_deref() == Some(ETAG_V) {
                            format!(
                                "HTTP/1.1 206 Partial Content\r\nETag: {ETAG_V}\r\nContent-Range: bytes {n}-{}/{size}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                                size - 1,
                                size - n
                            )
                        } else {
                            format!("HTTP/1.1 200 OK\r\nETag: {ETAG_V}\r\nContent-Length: {size}\r\nConnection: close\r\n\r\n")
                        }
                    } else {
                        format!("HTTP/1.1 200 OK\r\nETag: {ETAG_V}\r\nContent-Length: {size}\r\nConnection: close\r\n\r\n")
                    };
                    let start = if head.starts_with("HTTP/1.1 206") { rn.unwrap() } else { 0 };
                    let mut data = &body[start.min(size)..];
                    if !head.starts_with("HTTP/1.1 20") {
                        data = &[];
                    }
                    if let Some((k, m)) = cut
                        && idx < m {
                            data = &data[..k.min(data.len())];
                        }
                    let _ = s.write_all(head.as_bytes()).await;
                    let _ = s.write_all(data).await;
                    let _ = s.shutdown().await;
                });
            }
        });
        Srv { url, log }
    }

    fn body() -> Vec<u8> {
        (0..300_000u32).map(|i| (i % 251) as u8).collect()
    }

    fn setup(name: &str, url: &str) -> (Item, PathBuf) {
        let dir = std::env::temp_dir().join(format!("aqua-engine-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let item = Item {
            id: 1,
            url: url.into(),
            dest: dir.join("sub").join("f.bin"),
            status: Status::Queued,
            bytes: 0,
            total: None,
            etag: None,
            sha256: None,
            speed: 0.0,
        };
        (item, dir)
    }

    async fn run(item: &Item) -> (Result<(), String>, Vec<Event>) {
        let mut ev = Vec::new();
        let r = download(&reqwest::Client::new(), item, |e| ev.push(e)).await;
        (r, ev)
    }

    fn seed_part(item: &Item, len: usize) {
        let p = part_path(&item.dest);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, &body()[..len]).unwrap();
    }

    #[tokio::test]
    async fn full_download() {
        let srv = serve(body(), None).await;
        let (item, dir) = setup("full", &srv.url);
        let (r, ev) = run(&item).await;
        assert_eq!(r, Ok(()));
        assert_eq!(std::fs::read(&item.dest).unwrap(), body());
        assert!(!part_path(&item.dest).exists());
        assert!(ev.contains(&Event::Etag(ETAG_V.into())));
        assert_eq!(ev.last(), Some(&Event::Progress { bytes: 300_000, total: Some(300_000) }));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn cut_connection_resumes() {
        let srv = serve(body(), Some((100_000, 1))).await;
        let (item, dir) = setup("cut", &srv.url);
        let (r, _) = run(&item).await;
        assert_eq!(r, Ok(()));
        assert_eq!(std::fs::read(&item.dest).unwrap(), body());
        let log = srv.log.lock().unwrap();
        assert_eq!(log.len(), 2);
        assert!(log[1].as_deref().is_some_and(|r| r.starts_with("bytes=") && r != "bytes=0-"));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn existing_part_with_etag_resumes() {
        let srv = serve(body(), None).await;
        let (mut item, dir) = setup("resume", &srv.url);
        item.etag = Some(ETAG_V.into());
        seed_part(&item, 120_000);
        let (r, _) = run(&item).await;
        assert_eq!(r, Ok(()));
        assert_eq!(std::fs::read(&item.dest).unwrap(), body());
        assert_eq!(srv.log.lock().unwrap().as_slice(), [Some("bytes=120000-".to_string())]);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn stale_etag_restarts() {
        let srv = serve(body(), None).await;
        let (mut item, dir) = setup("stale", &srv.url);
        item.etag = Some("\"old\"".into());
        seed_part(&item, 50_000);
        let (r, ev) = run(&item).await;
        assert_eq!(r, Ok(()));
        assert_eq!(std::fs::read(&item.dest).unwrap(), body());
        assert!(srv.log.lock().unwrap()[0].is_some());
        assert!(ev.contains(&Event::Etag(ETAG_V.into())));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn complete_part_via_416() {
        let srv = serve(body(), None).await;
        let (mut item, dir) = setup("c416", &srv.url);
        item.etag = Some(ETAG_V.into());
        seed_part(&item, 300_000);
        let (r, _) = run(&item).await;
        assert_eq!(r, Ok(()));
        assert_eq!(std::fs::read(&item.dest).unwrap(), body());
        assert_eq!(srv.log.lock().unwrap().len(), 1);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn sha256_checked() {
        let srv = serve(body(), None).await;
        let (mut item, dir) = setup("sha", &srv.url);
        item.sha256 = Some("00".repeat(32));
        let (r, ev) = run(&item).await;
        assert!(r.unwrap_err().starts_with("SHA-256 mismatch"));
        assert!(ev.contains(&Event::Verifying));
        assert!(!item.dest.exists() && !part_path(&item.dest).exists());

        // Same .part is now complete; the right hash (upper-case) must pass.
        let good: String = Sha256::digest(body()).iter().map(|b| format!("{b:02X}")).collect();
        item.sha256 = Some(good);
        item.etag = Some(ETAG_V.into());
        let (r, _) = run(&item).await;
        assert_eq!(r, Ok(()));
        assert!(item.dest.exists());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn not_found_is_not_retried() {
        let srv = serve(body(), None).await;
        let (item, dir) = setup("404", &srv.url.replace("f.bin", "missing"));
        let (r, _) = run(&item).await;
        assert!(r.unwrap_err().contains("404"));
        assert_eq!(srv.log.lock().unwrap().len(), 1);
        let _ = std::fs::remove_dir_all(dir);
    }
}
