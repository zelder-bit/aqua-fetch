//! S3 links: recognise them, list bucket prefixes and download objects.
//! Requests are signed when AWS credentials are configured (see `aws`), else anonymous.
use crate::aws::{self, Creds, UNSIGNED};
use crate::engine::{self, Event};
use crate::model::Item;
use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
use serde::Deserialize;
use std::time::SystemTime;

#[derive(Debug, Clone, PartialEq)]
pub enum Link {
    /// One object. `url` is its https URL (as given, query intact, for https input).
    File { url: String, bucket: String, region: Option<String>, key: String },
    /// A bucket or prefix to list.
    Prefix { bucket: String, region: Option<String>, prefix: String },
}

#[derive(Debug, Clone, PartialEq)]
pub struct Object {
    pub key: String,
    pub size: u64,
    pub url: String,
}

/// None when `url` is not an S3 link (then the caller treats it as a plain HTTP URL).
pub fn parse(url: &str) -> Option<Link> {
    let (scheme, rest) = url.split_once("://")?;
    let is_s3 = scheme.eq_ignore_ascii_case("s3");
    // s3:// keys are literal; only real URLs carry a query string or percent-encoding.
    let rest = if is_s3 { rest } else { rest.split(['?', '#']).next().unwrap_or("") };
    let (host, path) = rest.split_once('/').unwrap_or((rest, ""));
    let (bucket, region, key) = if scheme.eq_ignore_ascii_case("s3") {
        (host, None, path)
    } else if scheme.eq_ignore_ascii_case("https") || scheme.eq_ignore_ascii_case("http") {
        let (bucket, region, path_style) = split_host(host)?;
        if path_style {
            let (b, k) = path.split_once('/').unwrap_or((path, ""));
            (b, region, k)
        } else {
            (bucket, region, path)
        }
    } else {
        return None;
    };
    // Bucket names are 3-63 chars of [a-z0-9.-]; this also rejects empty and odd hosts.
    if bucket.len() < 3 || !bucket.bytes().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'.' || c == b'-') {
        return None;
    }
    let key = if is_s3 { key.to_string() } else { percent_decode(key) };
    if key.is_empty() || key.ends_with('/') {
        return Some(Link::Prefix { bucket: bucket.into(), region: region.map(Into::into), prefix: key });
    }
    // https input is returned as given (query intact); s3:// gets a built URL.
    let url = if is_s3 { format!("{}/{}", base(bucket, None), encode_key(&key)) } else { url.to_string() };
    Some(Link::File { url, bucket: bucket.into(), region: region.map(Into::into), key })
}

/// Splits an amazonaws.com host into (bucket, region, path_style). For path-style the bucket is empty.
fn split_host(host: &str) -> Option<(&str, Option<&str>, bool)> {
    let h = host.strip_suffix(".amazonaws.com")?;
    if h == "s3" {
        return Some(("", None, true));
    }
    if let Some(r) = h.strip_prefix("s3.").or_else(|| h.strip_prefix("s3-")) {
        return (!r.contains('.')).then_some(("", region_name(r), true));
    }
    if let Some(b) = h.strip_suffix(".s3") {
        return Some((b, None, false));
    }
    let (b, r) = h.rfind(".s3.").map(|i| (&h[..i], &h[i + 4..])).or_else(|| h.rfind(".s3-").map(|i| (&h[..i], &h[i + 4..])))?;
    (!r.contains('.')).then_some((b, region_name(r), false))
}

/// `eu-west-1` style names only; accelerate, external-1 and website hosts are not regions.
fn region_name(r: &str) -> Option<&str> {
    (r.matches('-').count() >= 2 && r.ends_with(|c: char| c.is_ascii_digit()) && !r.starts_with("website")).then_some(r)
}

/// Endpoint for a bucket. Dotted names use path-style because virtual-hosted TLS fails for them.
fn base(bucket: &str, region: Option<&str>) -> String {
    let s3 = region.map_or("s3".to_string(), |r| format!("s3.{r}"));
    if bucket.contains('.') { format!("https://{s3}.amazonaws.com/{bucket}") } else { format!("https://{bucket}.{s3}.amazonaws.com") }
}

fn encode_key(key: &str) -> String {
    key.split('/').map(encode).collect::<Vec<_>>().join("/")
}

/// AWS's URI encoding: everything but unreserved characters as %XX.
pub(crate) fn encode(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || b"-._~".contains(&b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        let v = (b[i] == b'%').then(|| s.get(i + 1..i + 3)).flatten().and_then(|h| u8::from_str_radix(h, 16).ok());
        out.push(v.unwrap_or(b[i]));
        i += if v.is_some() { 3 } else { 1 };
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[derive(Deserialize)]
struct Listing {
    #[serde(rename = "Contents", default)]
    contents: Vec<Entry>,
    // Required on purpose: an <Error> document lacks it and so fails to parse as a listing.
    #[serde(rename = "IsTruncated")]
    truncated: bool,
    #[serde(rename = "NextContinuationToken")]
    next: Option<String>,
}

#[derive(Deserialize)]
struct Entry {
    #[serde(rename = "Key")]
    key: String,
    #[serde(rename = "Size")]
    size: u64,
}

#[derive(Deserialize)]
struct S3Error {
    #[serde(rename = "Code")]
    code: Option<String>,
    #[serde(rename = "Message")]
    message: Option<String>,
}

/// Keys with sizes (folder markers skipped) and the next continuation token if the page was truncated.
type Page = (Vec<(String, u64)>, Option<String>);
fn parse_listing(xml: &str) -> Result<Page, String> {
    let l: Listing = quick_xml::de::from_str(xml).map_err(|e| format!("unexpected S3 response: {e}"))?;
    let items = l.contents.into_iter().filter(|e| !e.key.ends_with('/')).map(|e| (e.key, e.size)).collect();
    Ok((items, if l.truncated { l.next } else { None }))
}

fn error_text(status: reqwest::StatusCode, body: &str, signed: bool) -> String {
    let mut msg = if status == reqwest::StatusCode::FORBIDDEN && signed {
        "your AWS credentials may not list this bucket (HTTP 403)".to_string()
    } else if status == reqwest::StatusCode::FORBIDDEN {
        "bucket does not allow public listing (HTTP 403)".to_string()
    } else {
        format!("S3 listing failed (HTTP {status})")
    };
    if let Ok(e) = quick_xml::de::from_str::<S3Error>(body) {
        let detail = [e.code, e.message].into_iter().flatten().collect::<Vec<_>>().join(": ");
        if !detail.is_empty() {
            msg = format!("{msg}: {detail}");
        }
    }
    msg
}

/// ListObjectsV2, following continuation tokens until done. Signed when credentials are configured.
pub async fn list(client: &reqwest::Client, bucket: &str, region: Option<&str>, prefix: &str) -> Result<Vec<Object>, String> {
    let (creds, note) = creds();
    list_with(client, creds.as_ref(), bucket, region, prefix).await.map_err(|e| add_note(e, note))
}

async fn list_with(client: &reqwest::Client, creds: Option<&Creds>, bucket: &str, region: Option<&str>, prefix: &str) -> Result<Vec<Object>, String> {
    let mut region = region.map(str::to_string);
    if let (Some(c), None) = (creds, &region) {
        region = Some(find_region(client, c, bucket).await);
    }
    let mut retried = false;
    let mut token: Option<String> = None;
    let mut out = Vec::new();
    loop {
        let root = base(bucket, region.as_deref());
        let mut url = format!("{root}?list-type=2&prefix={}", encode(prefix));
        if let Some(t) = &token {
            url.push_str(&format!("&continuation-token={}", encode(t)));
        }
        let mut req = client.get(&url);
        if let (Some(c), Some(r)) = (creds, &region) {
            req = req.headers(signed(c, &url, r)?);
        }
        let resp = req.send().await.map_err(|e| format!("S3 request failed: {}", engine::short(e)))?;
        let status = resp.status();
        let hint = resp.headers().get("x-amz-bucket-region").and_then(|v| v.to_str().ok()).map(str::to_string);
        // Wrong or unknown region: S3 names the right one. A signed request may have guessed it.
        let guessed = region.is_none() || creds.is_some();
        if let (false, true, Some(r), true) = (retried, guessed, hint, status.as_u16() == 301 || status.as_u16() == 400) {
            region = Some(r);
            retried = true;
            continue;
        }
        let body = resp.text().await.map_err(|e| format!("S3 response failed: {}", engine::short(e)))?;
        if !status.is_success() {
            return Err(error_text(status, &body, creds.is_some()));
        }
        let (items, next) = parse_listing(&body)?;
        out.extend(items.into_iter().map(|(key, size)| Object { url: format!("{root}/{}", encode_key(&key)), key, size }));
        match next {
            Some(t) => token = Some(t),
            None => return Ok(out),
        }
    }
}

/// Credentials, or None plus a note when the AWS setup could not be used (then requests go anonymous).
fn creds() -> (Option<Creds>, Option<String>) {
    match aws::load() {
        Ok(c) => (c, None),
        Err(note) => (None, Some(note)),
    }
}

fn add_note(e: String, note: Option<String>) -> String {
    match note {
        Some(n) => format!("{e}. Sent without AWS credentials: {n}"),
        None => e,
    }
}

/// The bucket's region from S3 itself (HEAD answers with it even when access is denied),
/// else the profile's region, else us-east-1.
async fn find_region(client: &reqwest::Client, c: &Creds, bucket: &str) -> String {
    // Path-style, so dotted bucket names work over TLS.
    let head = client.head(format!("https://s3.amazonaws.com/{}", encode(bucket))).send().await.ok();
    let hint = head.and_then(|r| r.headers().get("x-amz-bucket-region")?.to_str().ok().map(str::to_string));
    hint.or_else(|| c.region.clone()).unwrap_or_else(|| "us-east-1".into())
}

fn signed(c: &Creds, url: &str, region: &str) -> Result<HeaderMap, String> {
    let u = reqwest::Url::parse(url).map_err(|e| format!("Bad S3 URL: {e}"))?;
    let mut h = HeaderMap::new();
    for (k, v) in aws::sign(c, "GET", &u, region, &[], UNSIGNED, SystemTime::now()) {
        let mut v = HeaderValue::from_str(&v).map_err(|_| "AWS credentials contain characters a header cannot carry".to_string())?;
        v.set_sensitive(true);
        h.insert(HeaderName::from_bytes(k.as_bytes()).map_err(|e| e.to_string())?, v);
    }
    Ok(h)
}

/// Downloads an S3 object row: signed when credentials are configured, else exactly like a plain link.
pub async fn download(client: &reqwest::Client, item: &Item, on: impl FnMut(Event) + Send) -> Result<(), String> {
    let (creds, note) = creds();
    // A presigned link carries its own signature; S3 refuses a second one in a header.
    let presigned = reqwest::Url::parse(&item.url).is_ok_and(|u| u.query_pairs().any(|(k, _)| k == "X-Amz-Signature" || k == "Signature"));
    let (Some(c), Some(Link::File { bucket, region, key, .. }), false) = (creds, parse(&item.url), presigned) else {
        return engine::download(client, item, on).await.map_err(|e| add_note(e, note));
    };
    let region = match region {
        Some(r) => r,
        None => find_region(client, &c, &bucket).await,
    };
    // Rebuilt on the regional endpoint with a canonically encoded key, so the signature matches what is sent.
    let query = reqwest::Url::parse(&item.url).ok().and_then(|u| u.query().map(|q| format!("?{q}"))).unwrap_or_default();
    let url = format!("{}/{}{query}", base(&bucket, Some(&region)), encode_key(&key));
    signed(&c, &url, &region)?; // report bad URLs or credentials once, up front
    let sign = || signed(&c, &url, &region).unwrap_or_default();
    engine::download_from(client, item, &url, &sign, false, on).await
}

#[cfg(test)]
mod tests {
    use super::*;

    fn prefix(b: &str, r: Option<&str>, p: &str) -> Option<Link> {
        Some(Link::Prefix { bucket: b.into(), region: r.map(Into::into), prefix: p.into() })
    }

    #[test]
    fn parses_every_form() {
        assert_eq!(parse("s3://my-bucket"), prefix("my-bucket", None, ""));
        // s3:// keys are literal, https keys are percent-encoded.
        assert_eq!(parse("s3://my-bucket/a%20b/"), prefix("my-bucket", None, "a%20b/"));
        assert_eq!(parse("https://my-bucket.s3.amazonaws.com/a%20b/"), prefix("my-bucket", None, "a b/"));
        assert_eq!(parse("https://my-bucket.s3.amazonaws.com/"), prefix("my-bucket", None, ""));
        assert_eq!(parse("https://my-bucket.s3.eu-west-1.amazonaws.com/data/?x=1"), prefix("my-bucket", Some("eu-west-1"), "data/"));
        assert_eq!(parse("http://my-bucket.s3-us-west-2.amazonaws.com/d/"), prefix("my-bucket", Some("us-west-2"), "d/"));
        assert_eq!(parse("https://s3.amazonaws.com/my-bucket/d/e/"), prefix("my-bucket", None, "d/e/"));
        assert_eq!(parse("https://s3.us-east-2.amazonaws.com/my-bucket"), prefix("my-bucket", Some("us-east-2"), ""));
        assert_eq!(parse("https://a.b.c.s3.eu-west-1.amazonaws.com/x/"), prefix("a.b.c", Some("eu-west-1"), "x/"));
        let f = "https://my-bucket.s3.amazonaws.com/a/b%20c.zip?versionId=3";
        assert_eq!(parse(f), Some(file(f, "my-bucket", None, "a/b c.zip")));
        let f = "https://s3.eu-west-1.amazonaws.com/my-bucket/k.bin";
        assert_eq!(parse(f), Some(file(f, "my-bucket", Some("eu-west-1"), "k.bin")));
        for (f, b) in [("https://bkt.s3-accelerate.amazonaws.com/k", "bkt"), ("https://bkt.s3-external-1.amazonaws.com/k", "bkt"), ("https://s3-external-1.amazonaws.com/bkt/k", "bkt")] {
            assert_eq!(parse(f), Some(file(f, b, None, "k")), "{f}");
        }
    }

    #[test]
    fn rejects_non_s3() {
        for u in [
            "https://example.com/a.zip",
            "https://sqs.us-east-1.amazonaws.com/123/q",
            "https://ec2.us-east-1.amazonaws.com/",
            "https://s3.amazonaws.com.evil.com/b/k",
            "ftp://b.s3.amazonaws.com/k",
            "not a url",
        ] {
            assert_eq!(parse(u), None, "{u}");
        }
    }

    fn file(url: &str, bucket: &str, region: Option<&str>, key: &str) -> Link {
        Link::File { url: url.into(), bucket: bucket.into(), region: region.map(Into::into), key: key.into() }
    }

    #[test]
    fn s3_scheme_file_urls() {
        let f = "https://bkt.s3.amazonaws.com/dir/my%20file%26x.txt";
        assert_eq!(parse("s3://bkt/dir/my file&x.txt"), Some(file(f, "bkt", None, "dir/my file&x.txt")));
        assert_eq!(parse("s3://my.bkt/a b.txt"), Some(file("https://s3.amazonaws.com/my.bkt/a%20b.txt", "my.bkt", None, "a b.txt")));
    }

    const PAGE1: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<ListBucketResult xmlns="http://s3.amazonaws.com/doc/2006-03-01/"><Name>b</Name><Prefix>p/</Prefix><KeyCount>3</KeyCount><MaxKeys>3</MaxKeys><IsTruncated>true</IsTruncated><NextContinuationToken>tok/en+1</NextContinuationToken>
<Contents><Key>p/</Key><LastModified>2020-01-01T00:00:00.000Z</LastModified><Size>0</Size></Contents>
<Contents><Key>p/a &amp; b.txt</Key><LastModified>2020-01-01T00:00:00.000Z</LastModified><ETag>"x"</ETag><Size>12</Size><StorageClass>STANDARD</StorageClass></Contents>
<Contents><Key>p/c.bin</Key><Size>5000000000</Size></Contents></ListBucketResult>"#;

    #[test]
    fn listing_pages() {
        let (items, next) = parse_listing(PAGE1).unwrap();
        assert_eq!(items, vec![("p/a & b.txt".to_string(), 12), ("p/c.bin".to_string(), 5_000_000_000)]);
        assert_eq!(next.as_deref(), Some("tok/en+1"));
        let last = "<ListBucketResult><IsTruncated>false</IsTruncated><Contents><Key>z</Key><Size>1</Size></Contents></ListBucketResult>";
        assert_eq!(parse_listing(last).unwrap(), (vec![("z".to_string(), 1)], None));
        let empty = "<ListBucketResult><IsTruncated>false</IsTruncated></ListBucketResult>";
        assert_eq!(parse_listing(empty).unwrap(), (vec![], None));
    }

    #[test]
    fn listing_error_xml() {
        let xml = "<Error><Code>AccessDenied</Code><Message>Access Denied</Message></Error>";
        assert!(parse_listing(xml).is_err());
        let msg = error_text(reqwest::StatusCode::FORBIDDEN, xml, false);
        assert!(msg.contains("public listing") && msg.contains("AccessDenied: Access Denied"), "{msg}");
        assert_eq!(error_text(reqwest::StatusCode::NOT_FOUND, "junk", false), "S3 listing failed (HTTP 404 Not Found)");
    }

    #[tokio::test]
    #[ignore = "needs network"]
    async fn live_listing() {
        let c = reqwest::Client::new();
        let v = list(&c, "noaa-goes16", None, "ABI-L1b-RadC/2023/001/00/").await.unwrap();
        assert!(!v.is_empty());
        assert!(v[0].url.starts_with("https://noaa-goes16.s3"));
    }
}
