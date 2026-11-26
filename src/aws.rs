//! AWS credentials from the environment or the shared config files (`~/.aws`),
//! and Signature Version 4 signing for S3 requests.
use crate::model::percent_decode;
use crate::s3::encode;
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

/// Sent as x-amz-content-sha256 for GET/HEAD: the body is not signed.
pub const UNSIGNED: &str = "UNSIGNED-PAYLOAD";

/// No Debug on purpose: holds secrets.
#[derive(Clone)]
pub struct Creds {
    key_id: String,
    secret: String,
    token: Option<String>,
    /// The profile's default region, used when a bucket's own region cannot be found.
    pub region: Option<String>,
}

/// Ok(None) when nothing is configured, so requests stay anonymous.
/// Err names what is wrong; it never contains a key.
pub fn load() -> Result<Option<Creds>, String> {
    load_with(&|k| std::env::var(k).ok())
}

fn load_with(env: &dyn Fn(&str) -> Option<String>) -> Result<Option<Creds>, String> {
    let var = |k: &str| env(k).map(|v| v.trim().to_string()).filter(|v| !v.is_empty());
    // Environment keys come first, whatever state the profile files are in.
    if let (Some(key_id), Some(secret)) = (var("AWS_ACCESS_KEY_ID"), var("AWS_SECRET_ACCESS_KEY")) {
        let region = var("AWS_REGION").or_else(|| var("AWS_DEFAULT_REGION")).or_else(|| profile_keys(&var).ok()?.region);
        return Ok(Some(Creds { key_id, secret, token: var("AWS_SESSION_TOKEN"), region }));
    }
    let p = profile_keys(&var)?;
    let region = var("AWS_REGION").or_else(|| var("AWS_DEFAULT_REGION")).or(p.region);
    match (p.key_id, p.secret) {
        (Some(key_id), Some(secret)) => Ok(Some(Creds { key_id, secret, token: p.token, region })),
        _ if p.indirect => Err(format!(
            "AWS profile \"{}\" gets its keys through SSO, a role or a helper program, which Aqua Fetch cannot use. \
             Run `eval \"$(aws configure export-credentials --profile {} --format env)\"`, then start Aqua Fetch from that terminal",
            p.name, p.name
        )),
        _ => Ok(None),
    }
}

/// What the profile files say. No Debug: holds secrets.
struct Profile {
    name: String,
    key_id: Option<String>,
    secret: Option<String>,
    token: Option<String>,
    region: Option<String>,
    /// Keys come from SSO, a role or credential_process.
    indirect: bool,
}

fn profile_keys(var: &dyn Fn(&str) -> Option<String>) -> Result<Profile, String> {
    let home = PathBuf::from(var("HOME").unwrap_or_default());
    let creds_file = var("AWS_SHARED_CREDENTIALS_FILE").map_or_else(|| home.join(".aws/credentials"), PathBuf::from);
    let config_file = var("AWS_CONFIG_FILE").map_or_else(|| home.join(".aws/config"), PathBuf::from);
    let named = var("AWS_PROFILE");
    let profile = named.clone().unwrap_or_else(|| "default".into());
    // The credentials file wins over the config file for the same key.
    let mut keys = section(&read(&creds_file)?, &profile);
    let config = read(&config_file)?;
    let mut from_config = section(&config, &format!("profile {profile}"));
    if profile == "default" {
        from_config.extend(section(&config, "default"));
    }
    if named.is_some() && keys.is_empty() && from_config.is_empty() {
        return Err(format!("AWS profile \"{profile}\" (from AWS_PROFILE) is not in {} or {}", creds_file.display(), config_file.display()));
    }
    keys.extend(from_config);
    let get = |k: &str| keys.iter().find(|(n, _)| n == k).map(|(_, v)| v.clone()).filter(|v| !v.is_empty());
    let indirect = ["sso_session", "sso_start_url", "credential_process", "role_arn", "web_identity_token_file"].iter().any(|k| get(k).is_some());
    Ok(Profile {
        key_id: get("aws_access_key_id"),
        secret: get("aws_secret_access_key"),
        token: get("aws_session_token"),
        region: get("region"),
        indirect,
        name: profile,
    })
}

/// A missing file reads as empty; any other read error is reported.
fn read(p: &Path) -> Result<String, String> {
    match std::fs::read_to_string(p) {
        Ok(t) => Ok(t),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(String::new()),
        Err(e) => Err(format!("Cannot read {}: {e}", p.display())),
    }
}

/// `key = value` lines of the INI section `[name]`, keys lowercased.
fn section(text: &str, name: &str) -> Vec<(String, String)> {
    let mut inside = false;
    let mut out = Vec::new();
    for line in text.lines().map(|l| l.trim().trim_start_matches('\u{feff}')) {
        if line.is_empty() || line.starts_with(['#', ';']) {
            continue;
        }
        if let Some(h) = line.strip_prefix('[').and_then(|h| h.strip_suffix(']')) {
            inside = h.split_whitespace().collect::<Vec<_>>().join(" ") == name;
        } else if inside && let Some((k, v)) = line.split_once('=') {
            out.push((k.trim().to_ascii_lowercase(), v.trim().to_string()));
        }
    }
    out
}

/// SigV4 headers for one S3 request: x-amz-content-sha256, x-amz-date, x-amz-security-token
/// (with a session token) and authorization. `extra` are further (name, value) headers to sign;
/// the caller sends those itself. Headers not signed here (Range, If-Range) may still be added.
pub fn sign(c: &Creds, method: &str, url: &reqwest::Url, region: &str, extra: &[(&str, &str)], payload: &str, now: SystemTime) -> Vec<(String, String)> {
    let amz = amz_date(now);
    let date = &amz[..8];
    let host = url.host_str().unwrap_or_default();
    let host = url.port().map_or_else(|| host.to_string(), |p| format!("{host}:{p}"));
    let mut h: Vec<(String, String)> =
        vec![("host".into(), host), ("x-amz-content-sha256".into(), payload.into()), ("x-amz-date".into(), amz.clone())];
    if let Some(t) = &c.token {
        h.push(("x-amz-security-token".into(), t.clone()));
    }
    h.extend(extra.iter().map(|(k, v)| (k.to_ascii_lowercase(), v.trim().to_string())));
    h.sort();
    let signed = h.iter().map(|(k, _)| k.as_str()).collect::<Vec<_>>().join(";");
    let headers: String = h.iter().map(|(k, v)| format!("{k}:{v}\n")).collect();
    // Raw split, not query_pairs(): S3 reads `+` literally, not as a space.
    let mut q: Vec<(String, String)> = url
        .query()
        .unwrap_or("")
        .split('&')
        .filter(|p| !p.is_empty())
        .map(|p| p.split_once('=').unwrap_or((p, "")))
        .map(|(k, v)| (encode(&percent_decode(k)), encode(&percent_decode(v))))
        .collect();
    q.sort();
    let query = q.iter().map(|(k, v)| format!("{k}={v}")).collect::<Vec<_>>().join("&");
    // S3 paths are signed as sent (each segment encoded once), not normalised.
    let canonical = format!("{method}\n{}\n{query}\n{headers}\n{signed}\n{payload}", url.path());
    let scope = format!("{date}/{region}/s3/aws4_request");
    let to_sign = format!("AWS4-HMAC-SHA256\n{amz}\n{scope}\n{}", hex(&Sha256::digest(canonical.as_bytes())));
    let mut key = hmac(format!("AWS4{}", c.secret).as_bytes(), date.as_bytes());
    for part in [region, "s3", "aws4_request"] {
        key = hmac(&key, part.as_bytes());
    }
    let sig = hex(&hmac(&key, to_sign.as_bytes()));
    let mut out: Vec<(String, String)> = h.into_iter().filter(|(k, _)| k.starts_with("x-amz-")).collect();
    out.push(("authorization".into(), format!("AWS4-HMAC-SHA256 Credential={}/{scope}, SignedHeaders={signed}, Signature={sig}", c.key_id)));
    out
}

/// HMAC-SHA256 (RFC 2104).
fn hmac(key: &[u8], msg: &[u8]) -> Vec<u8> {
    let mut k = [0u8; 64];
    if key.len() > 64 {
        k[..32].copy_from_slice(&Sha256::digest(key));
    } else {
        k[..key.len()].copy_from_slice(key);
    }
    let mut inner = Sha256::new();
    inner.update(k.map(|b| b ^ 0x36));
    inner.update(msg);
    let mut outer = Sha256::new();
    outer.update(k.map(|b| b ^ 0x5c));
    outer.update(inner.finalize());
    outer.finalize().to_vec()
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

/// `YYYYMMDDTHHMMSSZ` in UTC.
fn amz_date(t: SystemTime) -> String {
    let s = t.duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs());
    let (days, rem) = ((s / 86_400) as i64, s % 86_400);
    // Days to civil date (Howard Hinnant's algorithm).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!("{y:04}{m:02}{d:02}T{:02}{:02}{:02}Z", rem / 3600, rem % 3600 / 60, rem % 60)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::time::Duration;

    #[test]
    fn hmac_rfc4231() {
        // Test case 2.
        assert_eq!(hex(&hmac(b"Jefe", b"what do ya want for nothing?")), "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843");
        // Test case 6: key longer than the block.
        assert_eq!(
            hex(&hmac(&[0xaa; 131], b"Test Using Larger Than Block-Size Key - Hash Key First")),
            "60e431591ee0b67f0d8a26aacbf5b77f8e0bc6213728c5140546040f0ee37f54"
        );
    }

    #[test]
    fn dates() {
        assert_eq!(amz_date(UNIX_EPOCH), "19700101T000000Z");
        assert_eq!(amz_date(UNIX_EPOCH + Duration::from_secs(1_369_353_600)), "20130524T000000Z");
        assert_eq!(amz_date(UNIX_EPOCH + Duration::from_secs(951_782_399)), "20000228T235959Z");
        assert_eq!(amz_date(UNIX_EPOCH + Duration::from_secs(951_868_800)), "20000301T000000Z");
    }

    fn example() -> Creds {
        Creds { key_id: "AKIAIOSFODNN7EXAMPLE".into(), secret: "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY".into(), token: None, region: None }
    }

    const EMPTY: &str = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";
    const DAY: u64 = 1_369_353_600; // 2013-05-24, the date AWS's S3 examples use

    fn auth(h: &[(String, String)]) -> &str {
        &h.iter().find(|(k, _)| k == "authorization").unwrap().1
    }

    /// AWS's documented S3 SigV4 examples ("GET Object" and "GET Bucket (List Objects)").
    #[test]
    fn aws_examples() {
        let t = UNIX_EPOCH + Duration::from_secs(DAY);
        let url = reqwest::Url::parse("https://examplebucket.s3.amazonaws.com/test.txt").unwrap();
        let h = sign(&example(), "GET", &url, "us-east-1", &[("range", "bytes=0-9")], EMPTY, t);
        assert_eq!(
            auth(&h),
            "AWS4-HMAC-SHA256 Credential=AKIAIOSFODNN7EXAMPLE/20130524/us-east-1/s3/aws4_request, \
             SignedHeaders=host;range;x-amz-content-sha256;x-amz-date, \
             Signature=f0e8bdb87c964420e857bd35b5d6ed310bd44f0170aba48dd91039c6036bdb41"
        );
        assert!(h.iter().any(|(k, v)| k == "x-amz-date" && v == "20130524T000000Z"));
        assert!(!h.iter().any(|(k, _)| k == "host" || k == "range"), "caller sends those");

        let url = reqwest::Url::parse("https://examplebucket.s3.amazonaws.com/?max-keys=2&prefix=J").unwrap();
        let h = sign(&example(), "GET", &url, "us-east-1", &[], EMPTY, t);
        assert!(auth(&h).ends_with("Signature=34b48302e7b5fa45bde8084f4b7868a86f0a534bc59db6670ed5711ef69dc6f7"), "{}", auth(&h));
    }

    #[test]
    fn session_token_is_signed() {
        let mut c = example();
        c.token = Some("TOKEN".into());
        let url = reqwest::Url::parse("https://b.s3.eu-west-1.amazonaws.com/k").unwrap();
        let h = sign(&c, "GET", &url, "eu-west-1", &[], UNSIGNED, UNIX_EPOCH);
        assert!(h.iter().any(|(k, v)| k == "x-amz-security-token" && v == "TOKEN"));
        // `+` is literal in S3 queries; query_pairs() would turn it into a space.
        let plus = reqwest::Url::parse("https://b.s3.eu-west-1.amazonaws.com/k?versionId=a+b").unwrap();
        let enc = reqwest::Url::parse("https://b.s3.eu-west-1.amazonaws.com/k?versionId=a%2Bb").unwrap();
        let space = reqwest::Url::parse("https://b.s3.eu-west-1.amazonaws.com/k?versionId=a%20b").unwrap();
        let sig = |u| auth(&sign(&c, "GET", u, "eu-west-1", &[], UNSIGNED, UNIX_EPOCH)).to_string();
        assert_eq!(sig(&plus), sig(&enc));
        assert_ne!(sig(&plus), sig(&space));
        assert!(auth(&h).contains("/eu-west-1/s3/aws4_request, SignedHeaders=host;x-amz-content-sha256;x-amz-date;x-amz-security-token,"));
    }

    fn with_files(name: &str, creds: &str, config: &str, env: &[(&str, &str)]) -> Result<Option<Creds>, String> {
        let dir = std::env::temp_dir().join(format!("aqua-aws-{name}-{}", std::process::id()));
        std::fs::create_dir_all(dir.join(".aws")).unwrap();
        std::fs::write(dir.join(".aws/credentials"), creds).unwrap();
        std::fs::write(dir.join(".aws/config"), config).unwrap();
        let mut vars: HashMap<String, String> = env.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
        vars.insert("HOME".into(), dir.display().to_string());
        let r = load_with(&|k| vars.get(k).cloned());
        std::fs::remove_dir_all(dir).unwrap();
        r
    }

    #[test]
    fn profiles() {
        let creds = "# keys\n[default]\naws_access_key_id = AKDEFAULT\naws_secret_access_key = s1\n\n[work]\naws_access_key_id=AKWORK\naws_secret_access_key=s2\naws_session_token = t2\n";
        let config = "[default]\nregion = eu-west-1\n[profile  work]\nregion=us-west-2\n[profile sso]\nsso_session = corp\n";
        let c = with_files("default", creds, config, &[]).unwrap().unwrap();
        assert_eq!((c.key_id.as_str(), c.secret.as_str(), c.token.as_deref(), c.region.as_deref()), ("AKDEFAULT", "s1", None, Some("eu-west-1")));
        let c = with_files("work", creds, config, &[("AWS_PROFILE", "work")]).unwrap().unwrap();
        assert_eq!((c.key_id.as_str(), c.token.as_deref(), c.region.as_deref()), ("AKWORK", Some("t2"), Some("us-west-2")));
        // Environment keys and region win over the files.
        let env = [("AWS_ACCESS_KEY_ID", "AKENV"), ("AWS_SECRET_ACCESS_KEY", "s3"), ("AWS_REGION", "ap-south-1")];
        let c = with_files("env", creds, config, &env).unwrap().unwrap();
        assert_eq!((c.key_id.as_str(), c.secret.as_str(), c.region.as_deref()), ("AKENV", "s3", Some("ap-south-1")));
        // Keys may also live in the config file.
        let c = with_files("cfg", "", "[default]\naws_access_key_id=AKCFG\naws_secret_access_key=s4\n", &[]).unwrap().unwrap();
        assert_eq!(c.key_id, "AKCFG");

        assert!(with_files("none", "", "", &[]).unwrap().is_none());
        // Environment keys win even when the named profile is missing.
        let env = [("AWS_PROFILE", "nope"), ("AWS_ACCESS_KEY_ID", "AKENV"), ("AWS_SECRET_ACCESS_KEY", "s3")];
        assert_eq!(with_files("envfirst", creds, config, &env).unwrap().unwrap().key_id, "AKENV");
        // A BOM and `[profile default]` both work.
        let c = with_files("bom", "\u{feff}[default]\naws_access_key_id=AKBOM\naws_secret_access_key=s\n", "[profile default]\nregion=eu-north-1\n", &[]).unwrap().unwrap();
        assert_eq!((c.key_id.as_str(), c.region.as_deref()), ("AKBOM", Some("eu-north-1")));
        let e = with_files("sso", creds, config, &[("AWS_PROFILE", "sso")]).err().unwrap();
        assert!(e.contains("SSO") && e.contains("export-credentials"), "{e}");
        let e = with_files("missing", creds, config, &[("AWS_PROFILE", "nope")]).err().unwrap();
        assert!(e.contains("\"nope\""), "{e}");
        assert!(!e.contains("s1") && !e.contains("AKDEFAULT"));
    }
}
