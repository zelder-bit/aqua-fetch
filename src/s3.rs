//! Public (anonymous, unsigned) S3 links: recognise them and list bucket prefixes.
use serde::Deserialize;

#[derive(Debug, Clone, PartialEq)]
pub enum Link {
    /// One object; the String is its public https URL.
    File(String),
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
    Some(Link::File(if scheme.eq_ignore_ascii_case("s3") {
        format!("{}/{}", base(bucket, None), encode_key(&key))
    } else {
        url.to_string()
    }))
}

/// Splits an amazonaws.com host into (bucket, region, path_style). For path-style the bucket is empty.
fn split_host(host: &str) -> Option<(&str, Option<&str>, bool)> {
    let h = host.strip_suffix(".amazonaws.com")?;
    if h == "s3" {
        return Some(("", None, true));
    }
    if let Some(r) = h.strip_prefix("s3.").or_else(|| h.strip_prefix("s3-")) {
        return (!r.contains('.')).then_some(("", Some(r), true));
    }
    if let Some(b) = h.strip_suffix(".s3") {
        return Some((b, None, false));
    }
    let (b, r) = h.rfind(".s3.").map(|i| (&h[..i], &h[i + 4..])).or_else(|| h.rfind(".s3-").map(|i| (&h[..i], &h[i + 4..])))?;
    (!r.contains('.')).then_some((b, Some(r), false))
}

/// Endpoint for a bucket. Dotted names use path-style because virtual-hosted TLS fails for them.
fn base(bucket: &str, region: Option<&str>) -> String {
    let s3 = region.map_or("s3".to_string(), |r| format!("s3.{r}"));
    if bucket.contains('.') { format!("https://{s3}.amazonaws.com/{bucket}") } else { format!("https://{bucket}.{s3}.amazonaws.com") }
}

fn encode_key(key: &str) -> String {
    key.split('/').map(encode).collect::<Vec<_>>().join("/")
}

fn encode(s: &str) -> String {
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

fn error_text(status: reqwest::StatusCode, body: &str) -> String {
    let mut msg = if status == reqwest::StatusCode::FORBIDDEN {
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

/// Anonymous ListObjectsV2, following continuation tokens until done.
pub async fn list(client: &reqwest::Client, bucket: &str, region: Option<&str>, prefix: &str) -> Result<Vec<Object>, String> {
    let mut region = region.map(str::to_string);
    let mut retried = false;
    let mut token: Option<String> = None;
    let mut out = Vec::new();
    loop {
        let root = base(bucket, region.as_deref());
        let mut url = format!("{root}?list-type=2&prefix={}", encode(prefix));
        if let Some(t) = &token {
            url.push_str(&format!("&continuation-token={}", encode(t)));
        }
        let resp = client.get(&url).send().await.map_err(|e| format!("S3 request failed: {e}"))?;
        let status = resp.status();
        let hint = resp.headers().get("x-amz-bucket-region").and_then(|v| v.to_str().ok()).map(str::to_string);
        if let (false, None, Some(r), true) = (retried, &region, hint, status.as_u16() == 301 || status.as_u16() == 400) {
            region = Some(r);
            retried = true;
            continue;
        }
        let body = resp.text().await.map_err(|e| format!("S3 response failed: {e}"))?;
        if !status.is_success() {
            return Err(error_text(status, &body));
        }
        let (items, next) = parse_listing(&body)?;
        out.extend(items.into_iter().map(|(key, size)| Object { url: format!("{root}/{}", encode_key(&key)), key, size }));
        match next {
            Some(t) => token = Some(t),
            None => return Ok(out),
        }
    }
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
        assert_eq!(parse(f), Some(Link::File(f.into())));
        let f = "https://s3.eu-west-1.amazonaws.com/my-bucket/k.bin";
        assert_eq!(parse(f), Some(Link::File(f.into())));
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

    #[test]
    fn s3_scheme_file_urls() {
        assert_eq!(parse("s3://bkt/dir/my file&x.txt"), Some(Link::File("https://bkt.s3.amazonaws.com/dir/my%20file%26x.txt".into())));
        assert_eq!(parse("s3://my.bkt/a b.txt"), Some(Link::File("https://s3.amazonaws.com/my.bkt/a%20b.txt".into())));
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
        let msg = error_text(reqwest::StatusCode::FORBIDDEN, xml);
        assert!(msg.contains("public listing") && msg.contains("AccessDenied: Access Denied"), "{msg}");
        assert_eq!(error_text(reqwest::StatusCode::NOT_FOUND, "junk"), "S3 listing failed (HTTP 404 Not Found)");
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
