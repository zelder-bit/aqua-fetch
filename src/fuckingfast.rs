//! fuckingfast.co links: read the file page for name, size and direct link, then
//! download it through the engine with Range resume.
//!
//! Clean-room implementation written from a behaviour description, not from
//! JDownloader's GPL plugin or any port of it.
//!
//! Flow: GET https://fuckingfast.co/<id> -> POST /f/<id>/dl (empty body, result
//! ignored) -> GET the direct link with Referer and the same cookies.
use crate::engine::{self, BASE_DELAY_MS, Event, RETRIES};
use crate::model::{Item, Status, percent_decode, safe_rel};
use regex::Regex;
use reqwest::header::{ACCEPT, CONTENT_LENGTH, COOKIE, HeaderMap, HeaderValue, REFERER, SET_COOKIE, USER_AGENT};
use reqwest::{Client, Url};
use serde::Deserialize;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const HOST: &str = "fuckingfast.co";
const BASE: &str = "https://fuckingfast.co";
/// Used when the settings give no `user_agent`. A cf_clearance cookie only works with
/// the user agent (and IP) of the browser that solved the check.
pub const DEFAULT_UA: &str = "Mozilla/5.0 (X11; Linux x86_64; rv:140.0) Gecko/20100101 Firefox/140.0";
const ACCEPT_HTML: &str = "text/html,application/xhtml+xml,application/xml;q=0.9,*/*;q=0.8";
/// File pages are a few KB; this only stops a hostile server from filling memory.
const MAX_PAGE: usize = 2 << 20;
const GENERIC_TITLES: [&str; 3] = ["fuckingfast", "fuckingfast.co", "fuckingfast - download"];
const CHALLENGE: &str = "Cloudflare check: copy cf_clearance from a browser that opened the page into fuckingfast.json, with that browser's user agent, then Retry";

#[derive(Debug, Clone, PartialEq)]
pub struct Link {
    /// 12 ASCII letters or digits.
    pub id: String,
    /// Percent-decoded `#fragment`. Only a file-name fallback; never sent to the server.
    pub fragment: Option<String>,
}

impl Link {
    pub fn page(&self) -> String {
        format!("{BASE}/{}", self.id)
    }
    pub fn callback(&self) -> String {
        format!("{BASE}/f/{}/dl", self.id)
    }
    /// Same file, however the link was written.
    pub fn key(&self) -> String {
        format!("{HOST}://{}", self.id)
    }
}

/// None when `url` is not a fuckingfast.co file link.
pub fn parse(url: &str) -> Option<Link> {
    let (scheme, rest) = url.trim().split_once("://")?;
    if !scheme.eq_ignore_ascii_case("https") && !scheme.eq_ignore_ascii_case("http") {
        return None;
    }
    let (rest, fragment) = rest.split_once('#').map_or((rest, None), |(r, f)| (r, Some(f)));
    let (host, path) = rest.split_once('/')?;
    let host = host.to_ascii_lowercase();
    if host != HOST && host != format!("www.{HOST}") {
        return None;
    }
    let id = path.strip_suffix('/').unwrap_or(path);
    if id.len() != 12 || !id.bytes().all(|b| b.is_ascii_alphanumeric()) {
        return None;
    }
    let fragment = fragment.map(percent_decode).filter(|f| !f.trim().is_empty());
    Some(Link { id: id.into(), fragment })
}

/// A page title or fragment as one safe file name (path separators become `_`).
pub fn file_name(name: &str) -> PathBuf {
    safe_rel(&name.replace(['/', '\\'], "_"))
}

/// No Debug: `direct` is a signed link.
#[derive(Default)]
pub struct Page {
    pub title: Option<String>,
    pub size: Option<u64>,
    pub direct: Option<String>,
}

static TITLE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?is)<title[^>]*>(.*?)</title>").unwrap());
static META: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?is)<meta\b[^>]*>").unwrap());
static ATTR: LazyLock<Regex> = LazyLock::new(|| Regex::new(r#"(?is)([a-z:_-]+)\s*=\s*(?:"([^"]*)"|'([^']*)')"#).unwrap());
static SIZE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"Size:\s*([0-9]+(?:[.,][0-9]+)?)\s*(B|KB|MB|GB|TB|PB|KiB|MiB|GiB|TiB|PiB)").unwrap());
static OPEN: LazyLock<Regex> = LazyLock::new(|| Regex::new(r#"window\.open\(\s*["']([^"']+)["']"#).unwrap());
static LOCATION: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"(?:^|[^\w.$-])(?:window\.|document\.)?location(?:\.href)?\s*=\s*["']([^"']+)["']"#).unwrap());

/// Reads title, size and direct link from a file page. Relative links resolve against `page_url`.
pub fn parse_page(html: &str, page_url: &str) -> Page {
    let og = || {
        META.find_iter(html).find_map(|m| {
            let attrs: Vec<(String, String)> = ATTR
                .captures_iter(m.as_str())
                .map(|c| (c[1].to_ascii_lowercase(), c.get(2).or(c.get(3)).map_or("", |v| v.as_str()).to_string()))
                .collect();
            let get = |k: &str| attrs.iter().find(|(n, _)| n == k).map(|(_, v)| v.as_str());
            (get("property") == Some("og:title")).then(|| get("content").and_then(clean)).flatten()
        })
    };
    let title = TITLE.captures(html).and_then(|c| clean(&c[1])).or_else(og);
    let size = SIZE.captures(html).and_then(|c| {
        let v: f64 = c[1].replace(',', ".").parse().ok()?;
        let pow = match &c[2] {
            "B" => 0,
            "KB" | "KiB" => 1,
            "MB" | "MiB" => 2,
            "GB" | "GiB" => 3,
            "TB" | "TiB" => 4,
            _ => 5,
        };
        Some((v * 1024f64.powi(pow)).round() as u64)
    });
    let direct = OPEN.captures(html).or_else(|| LOCATION.captures(html)).and_then(|c| {
        let raw = unescape(&c[1]).replace("\\/", "/");
        let url = Url::parse(page_url).ok()?.join(raw.trim()).ok()?;
        matches!(url.scheme(), "http" | "https").then(|| url.to_string())
    });
    Page { title, size, direct }
}

/// Unescaped, whitespace collapsed, and None for empty or site-name-only titles.
fn clean(s: &str) -> Option<String> {
    let t = unescape(s).split_whitespace().collect::<Vec<_>>().join(" ");
    (!t.is_empty() && !GENERIC_TITLES.iter().any(|g| t.eq_ignore_ascii_case(g))).then_some(t)
}

fn unescape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(i) = rest.find('&') {
        out.push_str(&rest[..i]);
        rest = &rest[i..];
        let end = rest.find(';').filter(|&e| e <= 12);
        match end.and_then(|e| Some((e, entity(&rest[1..e])?))) {
            Some((e, c)) => {
                out.push(c);
                rest = &rest[e + 1..];
            }
            None => {
                out.push('&');
                rest = &rest[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

fn entity(name: &str) -> Option<char> {
    Some(match name {
        "amp" => '&',
        "lt" => '<',
        "gt" => '>',
        "quot" => '"',
        "apos" => '\'',
        "nbsp" => '\u{a0}',
        _ => {
            let n = name.strip_prefix('#')?;
            let v = match n.strip_prefix(['x', 'X']) {
                Some(h) => u32::from_str_radix(h, 16).ok()?,
                None => n.parse().ok()?,
            };
            char::from_u32(v)?
        }
    })
}

/// Cloudflare's JS challenge, from the `cf-mitigated` header or the challenge page body.
pub fn is_challenge(headers: &HeaderMap, body: &str) -> bool {
    let header = headers.get("cf-mitigated").and_then(|v| v.to_str().ok());
    header.is_some_and(|v| v.trim().eq_ignore_ascii_case("challenge"))
        || ["Just a moment...", "_cf_chl_opt", "/cdn-cgi/challenge-platform/"].iter().any(|m| body.contains(m))
}

// ---- cookies and settings ----

/// No Debug on purpose: the value is a secret.
#[derive(Clone)]
struct Cookie {
    /// Lowercase, no leading dot.
    domain: String,
    /// Also sent to subdomains (`.example.com` / `TRUE` in cookies.txt / a `Domain=` attribute).
    sub: bool,
    name: String,
    value: String,
}

fn in_scope(domain: &str) -> bool {
    domain == HOST || domain.ends_with(".fuckingfast.co")
}

impl Cookie {
    fn new(domain: &str, sub: bool, name: &str, value: &str) -> Option<Cookie> {
        let d = domain.trim().to_ascii_lowercase();
        let sub = sub || d.starts_with('.');
        let name = name.trim();
        (!name.is_empty()).then(|| Cookie { domain: d.trim_start_matches('.').into(), sub, name: name.into(), value: value.trim().into() })
    }
    fn matches(&self, host: &str) -> bool {
        host == self.domain || (self.sub && host.strip_suffix(self.domain.as_str()).is_some_and(|p| p.ends_with('.')))
    }
}

/// The Cookie header for `host`; a later cookie with the same name wins.
fn cookie_header(cookies: &[Cookie], host: &str) -> Option<String> {
    let mut seen = HashSet::new();
    let mut v: Vec<String> =
        cookies.iter().rev().filter(|c| c.matches(host) && seen.insert(c.name.as_str())).map(|c| format!("{}={}", c.name, c.value)).collect();
    v.reverse();
    (!v.is_empty()).then(|| v.join("; "))
}

/// A pasted `a=b; c=d` header, sent to fuckingfast.co and its subdomains.
fn raw_cookies(raw: &str) -> Vec<Cookie> {
    let raw = raw.trim();
    let raw = raw.get(..7).filter(|p| p.eq_ignore_ascii_case("cookie:")).map_or(raw, |_| &raw[7..]);
    raw.split(';').filter_map(|p| p.split_once('=')).filter_map(|(n, v)| Cookie::new(HOST, true, n, v)).collect()
}

/// Netscape cookies.txt rows for fuckingfast.co that have not expired.
fn netscape(text: &str, now: u64) -> Vec<Cookie> {
    text.lines()
        .filter_map(|l| {
            let l = l.strip_prefix("#HttpOnly_").unwrap_or(l);
            let f: Vec<&str> = l.trim_end_matches('\r').split('\t').collect();
            if l.starts_with('#') || f.len() < 7 {
                return None;
            }
            let exp: u64 = f[4].trim().parse().unwrap_or(0);
            if exp != 0 && exp < now {
                return None;
            }
            Cookie::new(f[0], f[1].eq_ignore_ascii_case("TRUE"), f[5], f[6]).filter(|c| in_scope(&c.domain))
        })
        .collect()
}

/// Cookies a response from `host` set, scoped the way a browser would.
fn set_cookies(headers: &HeaderMap, host: &str) -> Vec<Cookie> {
    headers
        .get_all(SET_COOKIE)
        .iter()
        .filter_map(|v| {
            let mut parts = v.to_str().ok()?.split(';');
            let (name, value) = parts.next()?.split_once('=')?;
            let attrs: Vec<(&str, &str)> = parts.filter_map(|a| a.split_once('=')).map(|(k, v)| (k.trim(), v.trim())).collect();
            let attr = |n: &str| attrs.iter().find(|(k, _)| k.eq_ignore_ascii_case(n)).map(|(_, v)| *v);
            // A deletion must not replace the user's own cookie of that name.
            if value.trim().is_empty() || attr("max-age").and_then(|a| a.parse::<i64>().ok()).is_some_and(|a| a <= 0) {
                return None;
            }
            let domain = attr("domain").map(|d| d.trim_start_matches('.').to_ascii_lowercase());
            match domain {
                Some(d) if d.contains('.') && (host == d || host.ends_with(&format!(".{d}"))) => Cookie::new(&d, true, name, value),
                Some(_) => None,
                None => Cookie::new(host, false, name, value),
            }
        })
        .collect()
}

fn browser_cookies(browser: &str) -> Result<Vec<Cookie>, String> {
    let d = Some(vec![HOST.to_string()]);
    let got = match browser.trim().to_ascii_lowercase().as_str() {
        "firefox" => rookie::firefox(d),
        "librewolf" => rookie::librewolf(d),
        "zen" => rookie::zen(d),
        "chrome" => rookie::chrome(d),
        "chromium" => rookie::chromium(d),
        "brave" => rookie::brave(d),
        "edge" => rookie::edge(d),
        "vivaldi" => rookie::vivaldi(d),
        "opera" => rookie::opera(d),
        "any" => rookie::load(d),
        other => return Err(format!("Unknown browser \"{other}\" in fuckingfast.json")),
    };
    let got = got.map_err(|e| format!("Could not read {browser} cookies: {e}"))?;
    Ok(got.iter().filter_map(|c| Cookie::new(&c.domain, false, &c.name, &c.value)).filter(|c| in_scope(&c.domain)).collect())
}

/// `$XDG_CONFIG_HOME/aqua-fetch/fuckingfast.json`, usually under `~/.config`.
pub fn config_path() -> PathBuf {
    let cfg = std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from).filter(|p| p.is_absolute());
    let home = || PathBuf::from(std::env::var_os("HOME").unwrap_or_default());
    cfg.unwrap_or_else(|| home().join(".config")).join("aqua-fetch/fuckingfast.json")
}

/// Every field is optional. All cookie sources are merged; on a name clash the
/// raw `cookie` wins over `cookies_txt`, which wins over `browser`.
#[derive(Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
struct Config {
    cookie: Option<String>,
    cookies_txt: Option<PathBuf>,
    browser: Option<String>,
    user_agent: Option<String>,
}

/// UA and cookies for one download. No Debug: holds secrets.
struct Session {
    ua: String,
    cookies: Vec<Cookie>,
}

fn load_session(path: &Path) -> Result<Session, String> {
    let cfg: Config = match std::fs::read(path) {
        // serde_json errors name the position or the field, never a string value.
        Ok(b) => serde_json::from_slice(&b).map_err(|e| format!("{} is not valid: {e}", path.display()))?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Config::default(),
        Err(e) => return Err(format!("Cannot read {}: {e}", path.display())),
    };
    let mut cookies = Vec::new();
    if let Some(b) = &cfg.browser {
        cookies.extend(browser_cookies(b)?);
    }
    if let Some(p) = &cfg.cookies_txt {
        let t = std::fs::read_to_string(p).map_err(|e| format!("Cannot read {}: {e}", p.display()))?;
        let now = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs());
        cookies.extend(netscape(&t, now));
    }
    if let Some(c) = &cfg.cookie {
        cookies.extend(raw_cookies(c));
    }
    let ua = cfg.user_agent.map(|u| u.trim().to_string()).filter(|u| !u.is_empty());
    Ok(Session { ua: ua.unwrap_or_else(|| DEFAULT_UA.into()), cookies })
}

/// Read on every probe and download, so edited cookies apply on the next Retry without a restart.
async fn session() -> Result<Session, Status> {
    tokio::task::spawn_blocking(|| load_session(&config_path())).await.map_err(|e| Status::Failed(e.to_string()))?.map_err(Status::Failed)
}

fn header_value(s: &str) -> Result<HeaderValue, Status> {
    HeaderValue::from_str(s).map_err(|_| Status::Failed("A cookie or user agent in fuckingfast.json has characters a header cannot carry".into()))
}

fn headers(s: &Session, url: &Url, referer: Option<&str>, accept: Option<&str>) -> Result<HeaderMap, Status> {
    let mut h = HeaderMap::new();
    h.insert(USER_AGENT, header_value(&s.ua)?);
    if let Some(r) = referer {
        h.insert(REFERER, header_value(r)?);
    }
    if let Some(a) = accept {
        h.insert(ACCEPT, header_value(a)?);
    }
    if let Some(c) = url.host_str().and_then(|host| cookie_header(&s.cookies, host)) {
        let mut v = header_value(&c)?;
        v.set_sensitive(true);
        h.insert(COOKIE, v);
    }
    Ok(h)
}

// ---- page, probe and download ----

struct Meta {
    name: String,
    size: Option<u64>,
    /// Signed link: never logged or stored, only the host is safe to show.
    direct: Url,
}

type Fail = (Status, bool);

fn failed(msg: impl Into<String>) -> Status {
    Status::Failed(msg.into())
}

/// GETs the file page, retrying network errors and 5xx. Cookies it sets join the session.
async fn read_page(client: &Client, s: &mut Session, base: &str, link: &Link) -> Result<Meta, Status> {
    let url = Url::parse(&format!("{base}/{}", link.id)).map_err(|e| failed(e.to_string()))?;
    let mut attempt = 0;
    loop {
        match page_once(client, s, &url, link).await {
            Ok((meta, set)) => {
                s.cookies.extend(set);
                return Ok(meta);
            }
            Err((e, retry)) if !retry || attempt == RETRIES => return Err(e),
            Err(_) => tokio::time::sleep(Duration::from_millis(BASE_DELAY_MS << attempt)).await,
        }
        attempt += 1;
    }
}

async fn page_once(client: &Client, s: &Session, url: &Url, link: &Link) -> Result<(Meta, Vec<Cookie>), Fail> {
    let net = |e: reqwest::Error| (failed(format!("Network error: {}", engine::short(e))), true);
    let h = headers(s, url, None, Some(ACCEPT_HTML)).map_err(|e| (e, false))?;
    let mut resp = client.get(url.clone()).headers(h).send().await.map_err(net)?;
    let st = resp.status();
    let head = resp.headers().clone();
    let mut body = Vec::new();
    while let Some(c) = resp.chunk().await.map_err(net)? {
        body.extend_from_slice(&c);
        if body.len() > MAX_PAGE {
            break;
        }
    }
    let body = String::from_utf8_lossy(&body);
    let page = parse_page(&body, url.as_str());
    // A normal page can carry Cloudflare's challenge-platform script, so a found link wins over the markers.
    if let (true, Some(direct)) = (st.is_success(), &page.direct) {
        let direct = Url::parse(direct).map_err(|e| (failed(e.to_string()), false))?;
        let name = page.title.or_else(|| link.fragment.clone()).unwrap_or_else(|| link.id.clone());
        let set = set_cookies(&head, url.host_str().unwrap_or_default());
        return Ok((Meta { name, size: page.size, direct }, set));
    }
    let c = st.as_u16();
    if c == 404 {
        return Err((failed("Not found (HTTP 404): the file was removed or the link is wrong"), false));
    }
    // Body markers only count on an error reply, for the same reason.
    if is_challenge(&head, if st.is_success() { "" } else { &body }) {
        return Err((Status::Blocked(CHALLENGE.into()), false));
    }
    if !st.is_success() {
        return Err((failed(format!("HTTP {st}")), st.is_server_error() || c == 408 || c == 429));
    }
    Err((failed("No download link on the page (the site may have changed)"), false))
}

/// Name and size for a new row. The name falls back to the link's #fragment, then its id.
pub async fn probe(client: &Client, url: &str) -> Result<(String, Option<u64>), Status> {
    let link = parse(url).ok_or_else(|| failed("Not a fuckingfast.co link"))?;
    let mut s = session().await?;
    let m = read_page(client, &mut s, BASE, &link).await?;
    Ok((m.name, m.size))
}

/// Downloads `item.url` (a file page) to `item.dest`. The page is read again for a fresh direct link.
pub async fn download(client: &Client, item: &Item, on: impl FnMut(Event) + Send) -> Result<(), Status> {
    let link = parse(&item.url).ok_or_else(|| failed("Not a fuckingfast.co link"))?;
    let mut s = session().await?;
    run(client, &mut s, BASE, &link, item, on).await
}

async fn run(client: &Client, s: &mut Session, base: &str, link: &Link, item: &Item, on: impl FnMut(Event) + Send) -> Result<(), Status> {
    let m = read_page(client, s, base, link).await?;
    let page = format!("{base}/{}", link.id);
    // The site counts the download here; the file comes either way, so every failure is ignored.
    if let Ok(cb) = Url::parse(&format!("{base}/f/{}/dl", link.id))
        && let Ok(h) = headers(s, &cb, Some(&page), None)
        && let Ok(r) = client.post(cb.clone()).headers(h).header(CONTENT_LENGTH, 0).body("").timeout(Duration::from_secs(20)).send().await
    {
        s.cookies.extend(set_cookies(r.headers(), cb.host_str().unwrap_or_default()));
    }
    let h = headers(s, &m.direct, Some(&page), None)?;
    engine::download_from(client, item, m.direct.as_str(), &h, true, on).await.map_err(Status::Failed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::part_path;
    use std::sync::{Arc, Mutex};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    #[test]
    fn parses_links() {
        let l = parse("https://fuckingfast.co/abcDEF123456").unwrap();
        assert_eq!(l, Link { id: "abcDEF123456".into(), fragment: None });
        assert_eq!(l.page(), "https://fuckingfast.co/abcDEF123456");
        assert_eq!(l.callback(), "https://fuckingfast.co/f/abcDEF123456/dl");
        assert_eq!(l.key(), "fuckingfast.co://abcDEF123456");
        let l = parse("http://www.FuckingFast.co/abcDEF123456/#My%20Game%20v1.2.rar").unwrap();
        assert_eq!(l.id, "abcDEF123456");
        assert_eq!(l.fragment.as_deref(), Some("My Game v1.2.rar"));
        assert_eq!(l.page(), "https://fuckingfast.co/abcDEF123456");
        assert_eq!(parse("https://fuckingfast.co/abcDEF123456#").unwrap().fragment, None);
        for bad in [
            "https://fuckingfast.co/abcDEF12345",
            "https://fuckingfast.co/abcDEF1234567",
            "https://fuckingfast.co/abcDEF12345-",
            "https://fuckingfast.co/abcDEF12345é",
            "https://fuckingfast.co/f/abcDEF123456/dl",
            "https://fuckingfast.co/abcDEF123456?x=1",
            "https://fuckingfast.co/",
            "https://s12.fuckingfast.co/abcDEF123456",
            "https://fuckingfast.co.evil.com/abcDEF123456",
            "ftp://fuckingfast.co/abcDEF123456",
        ] {
            assert_eq!(parse(bad), None, "{bad}");
        }
        assert_eq!(file_name("a/b\\..x"), PathBuf::from("a_b_..x"));
        assert_eq!(file_name(".."), PathBuf::from("download"));
    }

    const PAGE: &str = "https://fuckingfast.co/abcDEF123456";

    #[test]
    fn scrapes_title_size_and_window_open() {
        let html = r#"<html><head><title>
              My  Game &amp; DLC  v1.2.rar </title></head><body>
            <span>Size: 1.5 GB | Downloads: 12345</span>
            <button onclick='window.open("https://s12.fuckingfast.co/dl/xyz?token=x&amp;expires=1")'>DL</button>
            <script>location.href = "/never-used";</script></body></html>"#;
        let p = parse_page(html, PAGE);
        assert_eq!(p.title.as_deref(), Some("My Game & DLC v1.2.rar"));
        assert_eq!(p.size, Some(1_610_612_736));
        assert_eq!(p.direct.as_deref(), Some("https://s12.fuckingfast.co/dl/xyz?token=x&expires=1"));
    }

    #[test]
    fn og_title_and_location_fallback() {
        let html = r#"<title>fuckingfast - Download</title>
            <meta content="Other &#39;Name&#x27;.zip" property="og:title">
            Size: 700,5 MiB <script>window.location.href = "/dl/abc?t=1&amp;e=2";</script>"#;
        let p = parse_page(html, PAGE);
        assert_eq!(p.title.as_deref(), Some("Other 'Name'.zip"));
        assert_eq!(p.size, Some((700.5f64 * 1024.0 * 1024.0).round() as u64));
        assert_eq!(p.direct.as_deref(), Some("https://fuckingfast.co/dl/abc?t=1&e=2"));
        let p = parse_page(r#"<script>location = "dl2"</script>"#, PAGE);
        assert_eq!(p.direct.as_deref(), Some("https://fuckingfast.co/dl2"));
        assert_eq!(parse_page(r#"<div data-location="eu" x.location="a" mylocation="b">"#, PAGE).direct, None);
    }

    #[test]
    fn generic_title_and_no_link() {
        let p = parse_page("<title>FuckingFast.co</title><p>Size: 12 B</p>", PAGE);
        assert_eq!((p.title, p.size, p.direct), (None, Some(12), None));
        assert_eq!(parse_page("<title>   </title>Size: 3 KiB", PAGE).size, Some(3072));
        assert_eq!(parse_page("Size: 2 PB", PAGE).size, Some(2 << 50));
        assert_eq!(parse_page("Size: big", PAGE).size, None);
        assert_eq!(unescape("a &amp;amp; &bogus; & &#9731; &#xZZ;"), "a &amp; &bogus; & ☃ &#xZZ;");
    }

    #[test]
    fn cloudflare_detection() {
        let mut h = HeaderMap::new();
        assert!(!is_challenge(&h, "<title>Game.rar</title>"));
        h.insert("cf-mitigated", HeaderValue::from_static("challenge"));
        assert!(is_challenge(&h, ""));
        let h = HeaderMap::new();
        assert!(is_challenge(&h, "<title>Just a moment...</title>"));
        assert!(is_challenge(&h, "<script>window._cf_chl_opt={}</script>"));
        assert!(is_challenge(&h, r#"<script src="/cdn-cgi/challenge-platform/h/b/orchestrate"></script>"#));
    }

    #[test]
    fn cookie_sources_and_scope() {
        let mut c = raw_cookies("Cookie: cf_clearance=abc; x = 1 ;junk");
        assert_eq!(c.len(), 2);
        let txt = "# Netscape HTTP Cookie File\n\
            .fuckingfast.co\tTRUE\t/\tTRUE\t0\tcf_clearance\tfromfile\n\
            #HttpOnly_fuckingfast.co\tFALSE\t/\tFALSE\t0\tonly\there\n\
            fuckingfast.co\tFALSE\t/\tFALSE\t100\told\tgone\n\
            .example.com\tTRUE\t/\tFALSE\t0\tother\tsite\n";
        let file = netscape(txt, 1000);
        assert_eq!(file.iter().map(|c| c.name.as_str()).collect::<Vec<_>>(), ["cf_clearance", "only"]);
        c.splice(0..0, file);
        // raw cookie (added last) wins on the name clash
        assert_eq!(cookie_header(&c, "fuckingfast.co").as_deref(), Some("only=here; cf_clearance=abc; x=1"));
        assert_eq!(cookie_header(&c, "s12.fuckingfast.co").as_deref(), Some("cf_clearance=abc; x=1"));
        assert_eq!(cookie_header(&c, "evilfuckingfast.co"), None);
        assert_eq!(cookie_header(&c, "example.com"), None);

        let mut h = HeaderMap::new();
        for v in [
            "sess=1; Path=/; HttpOnly",
            "wide=2; Domain=.fuckingfast.co",
            "bad=3; Domain=example.com",
            "tld=4; Domain=co",
            "cf_clearance=; Max-Age=0; Domain=.fuckingfast.co",
            "gone=x; Max-Age=0",
        ] {
            h.append(SET_COOKIE, HeaderValue::from_static(v));
        }
        let set = set_cookies(&h, "fuckingfast.co");
        assert_eq!(cookie_header(&set, "fuckingfast.co").as_deref(), Some("sess=1; wide=2"));
        assert_eq!(cookie_header(&set, "s1.fuckingfast.co").as_deref(), Some("wide=2"));
    }

    #[test]
    fn settings_file() {
        let dir = std::env::temp_dir().join(format!("aqua-ff-cfg-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("fuckingfast.json");
        let s = load_session(&p).unwrap();
        assert_eq!((s.ua.as_str(), s.cookies.len()), (DEFAULT_UA, 0));
        std::fs::write(dir.join("c.txt"), ".fuckingfast.co\tTRUE\t/\tTRUE\t0\ta\t1\n").unwrap();
        let cfg = serde_json::json!({"cookie": "b=2", "cookies_txt": dir.join("c.txt"), "user_agent": "UA/1"});
        std::fs::write(&p, cfg.to_string()).unwrap();
        let s = load_session(&p).unwrap();
        assert_eq!(s.ua, "UA/1");
        assert_eq!(cookie_header(&s.cookies, HOST).as_deref(), Some("a=1; b=2"));
        std::fs::write(&p, r#"{"cookie": "secretvalue", "cokie": 1}"#).unwrap();
        let e = load_session(&p).err().unwrap();
        assert!(e.contains("cokie") && !e.contains("secretvalue"), "{e}");
        std::fs::remove_dir_all(dir).unwrap();
    }

    // ---- end to end against a local server ----

    const ID: &str = "abcDEF123456";

    /// Answers each request with `reply(request text)` and logs the request.
    async fn serve(reply: impl Fn(&str) -> Vec<u8> + Send + Sync + 'static) -> (String, Arc<Mutex<Vec<String>>>) {
        let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", l.local_addr().unwrap());
        let log: Arc<Mutex<Vec<String>>> = Default::default();
        let (log2, reply) = (log.clone(), Arc::new(reply));
        tokio::spawn(async move {
            loop {
                let (mut s, _) = l.accept().await.unwrap();
                let (log, reply) = (log2.clone(), reply.clone());
                tokio::spawn(async move {
                    let mut req = Vec::new();
                    let mut buf = [0u8; 4096];
                    while !req.windows(4).any(|w| w == b"\r\n\r\n") {
                        let n = s.read(&mut buf).await.unwrap_or(0);
                        if n == 0 {
                            return;
                        }
                        req.extend_from_slice(&buf[..n]);
                    }
                    let text = String::from_utf8_lossy(&req).to_string();
                    log.lock().unwrap().push(text.clone());
                    let _ = s.write_all(&reply(&text)).await;
                    let _ = s.shutdown().await;
                });
            }
        });
        (base, log)
    }

    fn resp(head: &str, body: &[u8]) -> Vec<u8> {
        let mut r = format!("HTTP/1.1 {head}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len()).into_bytes();
        r.extend_from_slice(body);
        r
    }

    fn data() -> Vec<u8> {
        (0..50_000u32).map(|i| (i % 249) as u8).collect()
    }

    /// Page with a relative direct link and a session cookie; the file needs the token, cookie and Referer.
    fn site(req: &str) -> Vec<u8> {
        let line = req.lines().next().unwrap_or("");
        let has = |h: &str| req.lines().any(|l| l.eq_ignore_ascii_case(h));
        if line.starts_with(&format!("GET /{ID} ")) {
            let html = r#"<title>Game.rar</title>Size: 48.8 KB <script>window.open("/file.bin?token=x&amp;expires=1")</script>"#;
            return resp("200 OK\r\nContent-Type: text/html\r\nSet-Cookie: sess=s1; Path=/", html.as_bytes());
        }
        if line.starts_with(&format!("POST /f/{ID}/dl ")) {
            return resp("500 Oops", b"");
        }
        if line.starts_with("GET /file.bin?token=x&expires=1 ") && has("cookie: sess=s1") {
            let d = data();
            let from = req.lines().find_map(|l| l.to_ascii_lowercase().strip_prefix("range: bytes=")?.strip_suffix('-')?.parse::<usize>().ok());
            return match from {
                Some(n) => {
                    let head = format!("206 Partial Content\r\nContent-Range: bytes {n}-{}/{}", d.len() - 1, d.len());
                    resp(&head, &d[n..])
                }
                None => resp("200 OK\r\nContent-Type: application/octet-stream", &d),
            };
        }
        resp("403 Forbidden\r\nContent-Type: text/html", b"<title>nope</title>")
    }

    fn item(name: &str) -> (Item, PathBuf) {
        let dir = std::env::temp_dir().join(format!("aqua-ff-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let item = Item {
            id: 1,
            url: format!("https://fuckingfast.co/{ID}"),
            dest: dir.join("Game.rar"),
            status: Status::Queued,
            bytes: 0,
            total: None,
            etag: None,
            sha256: None,
            speed: 0.0,
        };
        (item, dir)
    }

    fn session() -> Session {
        Session { ua: "TestUA/1".into(), cookies: vec![] }
    }

    fn link() -> Link {
        parse(&format!("https://fuckingfast.co/{ID}#Frag.rar")).unwrap()
    }

    #[tokio::test]
    async fn full_flow_with_resume() {
        let (base, log) = serve(site).await;
        let (item, dir) = item("flow");
        let c = Client::new();
        let m = read_page(&c, &mut session(), &base, &link()).await.ok().unwrap();
        assert_eq!((m.name.as_str(), m.size), ("Game.rar", Some(49_971)));

        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(part_path(&item.dest), &data()[..20_000]).unwrap();
        let r = run(&c, &mut session(), &base, &link(), &item, |_| {}).await;
        assert_eq!(r, Ok(()));
        assert_eq!(std::fs::read(&item.dest).unwrap(), data());
        let log = log.lock().unwrap();
        let referer = format!("referer: {base}/{ID}\r\n").to_ascii_lowercase();
        let post = log.iter().find(|r| r.starts_with("POST ")).unwrap().to_ascii_lowercase();
        assert!(post.contains("content-length: 0\r\n") && post.contains(&referer), "{post}");
        let get = log.last().unwrap().to_ascii_lowercase();
        assert!(get.contains("range: bytes=20000-\r\n") && !get.contains("if-range"), "{get}");
        assert!(get.contains(&referer) && get.contains("user-agent: testua/1\r\n"), "{get}");
        assert!(!log.iter().any(|r| r.contains("Frag")), "fragment must not be sent");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn challenge_404_and_html_file() {
        let (base, log) = serve(|req| {
            if req.starts_with(&format!("GET /{ID} ")) {
                resp("403 Forbidden\r\ncf-mitigated: challenge\r\nContent-Type: text/html", b"<title>Just a moment...</title>")
            } else if req.starts_with("GET /zzzzzzzzzzzz ") {
                resp("404 Not Found", br#"gone <script src="/cdn-cgi/challenge-platform/scripts/jsd/main.js"></script>"#)
            } else if req.starts_with("GET /nolinknolink ") {
                resp("200 OK\r\nContent-Type: text/html", br#"<title>x</title><script src="/cdn-cgi/challenge-platform/x"></script>"#)
            } else if req.starts_with("GET /htmlhtmlhtml ") {
                resp("200 OK\r\nContent-Type: text/html", br#"window.open("/expired")"#)
            } else if req.starts_with("GET /flakyflakyfl ") {
                resp("503 Busy", b"")
            } else if req.starts_with("GET /expired ") {
                resp("200 OK\r\nContent-Type: text/html; charset=utf-8", b"<title>Just a moment...</title>")
            } else {
                resp("200 OK", b"")
            }
        })
        .await;
        let c = Client::new();
        let l = |id: &str| Link { id: id.into(), fragment: None };
        let err = |r: Result<Meta, Status>| r.err().unwrap();
        assert!(matches!(err(read_page(&c, &mut session(), &base, &link()).await), Status::Blocked(_)));
        assert!(matches!(err(read_page(&c, &mut session(), &base, &l("zzzzzzzzzzzz")).await), Status::Failed(m) if m.contains("404")));
        assert!(matches!(err(read_page(&c, &mut session(), &base, &l("nolinknolink")).await), Status::Failed(m) if m.contains("No download link")));
        let before = log.lock().unwrap().len();
        assert!(matches!(err(read_page(&c, &mut session(), &base, &l("flakyflakyfl")).await), Status::Failed(m) if m.contains("503")));
        assert_eq!(log.lock().unwrap().len() - before, RETRIES as usize + 1);

        let (item, dir) = item("html");
        let r = run(&c, &mut session(), &base, &l("htmlhtmlhtml"), &item, |_| {}).await;
        assert!(matches!(r, Err(Status::Failed(m)) if m.contains("web page")));
        assert!(!item.dest.exists());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    #[ignore = "network; set FFDL_TEST_URL, and FFDL_TEST_COOKIE / FFDL_TEST_UA when Cloudflare asks"]
    async fn fuckingfast_live() {
        let url = std::env::var("FFDL_TEST_URL").expect("set FFDL_TEST_URL");
        let link = parse(&url).expect("FFDL_TEST_URL is not a fuckingfast.co file link");
        let mut s = Session {
            ua: std::env::var("FFDL_TEST_UA").unwrap_or_else(|_| DEFAULT_UA.into()),
            cookies: raw_cookies(&std::env::var("FFDL_TEST_COOKIE").unwrap_or_default()),
        };
        let c = Client::builder().connect_timeout(Duration::from_secs(20)).read_timeout(Duration::from_secs(60)).build().unwrap();
        let m = read_page(&c, &mut s, BASE, &link).await.unwrap_or_else(|e| panic!("{e:?}"));
        println!("name={:?} size={:?} direct host={:?}", m.name, m.size, m.direct.host_str());
        let (mut item, dir) = item("live");
        item.url = link.page();
        // Fetch about 15 seconds' worth, then stop: enough to prove the file streams.
        match tokio::time::timeout(Duration::from_secs(15), run(&c, &mut s, BASE, &link, &item, |_| {})).await {
            Ok(r) => r.unwrap_or_else(|e| panic!("{e:?}")),
            Err(_) => assert!(part_path(&item.dest).metadata().unwrap().len() > 0),
        }
        let _ = std::fs::remove_dir_all(dir);
    }
}
