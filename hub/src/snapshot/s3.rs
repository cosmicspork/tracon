//! A minimal S3 client: PUT, GET, DELETE, and a prefix LIST, signed with
//! SigV4. Three verbs do not justify an SDK; this is the wire contract and
//! nothing else, and it works against DigitalOcean Spaces as well as S3.

use hmac::{Hmac, Mac};
use sha2::{Digest, Sha256};

use super::objects::ObjectStore;

type HmacSha256 = Hmac<Sha256>;

#[derive(Clone, Debug)]
pub struct S3Config {
    /// e.g. `https://nyc3.digitaloceanspaces.com`
    pub endpoint: String,
    pub region: String,
    pub bucket: String,
    pub access_key: String,
    pub secret_key: String,
}

impl S3Config {
    /// From `TRACON_HUB_SNAPSHOT_{ENDPOINT,REGION,BUCKET,ACCESS_KEY,SECRET_KEY}`.
    pub fn from_env() -> Option<Self> {
        let get = |k: &str| std::env::var(format!("TRACON_HUB_SNAPSHOT_{k}")).ok();
        Some(Self {
            endpoint: get("ENDPOINT")?,
            region: get("REGION").unwrap_or_else(|| "us-east-1".into()),
            bucket: get("BUCKET")?,
            access_key: get("ACCESS_KEY")?,
            secret_key: get("SECRET_KEY")?,
        })
    }
}

pub struct S3 {
    cfg: S3Config,
    http: reqwest::blocking::Client,
}

impl S3 {
    pub fn new(cfg: S3Config) -> Self {
        Self {
            cfg,
            http: reqwest::blocking::Client::builder()
                .timeout(std::time::Duration::from_secs(300))
                .build()
                .expect("http client"),
        }
    }

    fn host(&self) -> String {
        let e = self.cfg.endpoint.trim_end_matches('/');
        let bare = e.split("://").nth(1).unwrap_or(e);
        format!("{}.{bare}", self.cfg.bucket)
    }

    fn scheme(&self) -> &str {
        if self.cfg.endpoint.starts_with("http://") {
            "http"
        } else {
            "https"
        }
    }

    fn request(
        &self,
        method: &str,
        key: &str,
        query: &[(&str, &str)],
        body: &[u8],
    ) -> std::io::Result<(u16, Vec<u8>)> {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let (date, datetime) = amz_dates(now);
        let path = format!("/{}", uri_encode(key, true));
        let mut q: Vec<(String, String)> = query
            .iter()
            .map(|(k, v)| (uri_encode(k, false), uri_encode(v, false)))
            .collect();
        q.sort();
        let canonical_query = q
            .iter()
            .map(|(k, v)| format!("{k}={v}"))
            .collect::<Vec<_>>()
            .join("&");
        let host = self.host();
        let payload_hash = hex::encode(Sha256::digest(body));
        let canonical_headers =
            format!("host:{host}\nx-amz-content-sha256:{payload_hash}\nx-amz-date:{datetime}\n");
        let signed_headers = "host;x-amz-content-sha256;x-amz-date";
        let canonical_request = format!(
            "{method}\n{path}\n{canonical_query}\n{canonical_headers}\n{signed_headers}\n{payload_hash}"
        );
        let scope = format!("{date}/{}/s3/aws4_request", self.cfg.region);
        let string_to_sign = format!(
            "AWS4-HMAC-SHA256\n{datetime}\n{scope}\n{}",
            hex::encode(Sha256::digest(canonical_request.as_bytes()))
        );
        let k_date = hmac(
            format!("AWS4{}", self.cfg.secret_key).as_bytes(),
            date.as_bytes(),
        );
        let k_region = hmac(&k_date, self.cfg.region.as_bytes());
        let k_service = hmac(&k_region, b"s3");
        let k_signing = hmac(&k_service, b"aws4_request");
        let signature = hex::encode(hmac(&k_signing, string_to_sign.as_bytes()));
        let authorization = format!(
            "AWS4-HMAC-SHA256 Credential={}/{scope}, SignedHeaders={signed_headers}, Signature={signature}",
            self.cfg.access_key
        );
        let url = format!(
            "{}://{host}{path}{}",
            self.scheme(),
            if canonical_query.is_empty() {
                String::new()
            } else {
                format!("?{canonical_query}")
            }
        );
        let m = reqwest::Method::from_bytes(method.as_bytes()).expect("method");
        let res = self
            .http
            .request(m, &url)
            .header("host", &host)
            .header("x-amz-content-sha256", &payload_hash)
            .header("x-amz-date", &datetime)
            .header("authorization", authorization)
            .body(body.to_vec())
            .send()
            .map_err(|e| std::io::Error::other(e.to_string()))?;
        let status = res.status().as_u16();
        let bytes = res
            .bytes()
            .map_err(|e| std::io::Error::other(e.to_string()))?
            .to_vec();
        Ok((status, bytes))
    }
}

fn hmac(key: &[u8], data: &[u8]) -> Vec<u8> {
    let mut m = HmacSha256::new_from_slice(key).expect("hmac key");
    m.update(data);
    m.finalize().into_bytes().to_vec()
}

/// `YYYYMMDD` and `YYYYMMDDTHHMMSSZ` from Unix seconds, without a date crate.
fn amz_dates(secs: u64) -> (String, String) {
    let days = secs / 86_400;
    let rem = secs % 86_400;
    let (h, m, s) = (rem / 3600, (rem % 3600) / 60, rem % 60);
    // Civil from days (Howard Hinnant's algorithm).
    let z = days as i64 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let mo = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if mo <= 2 { y + 1 } else { y };
    let date = format!("{y:04}{mo:02}{d:02}");
    (date.clone(), format!("{date}T{h:02}{m:02}{s:02}Z"))
}

fn uri_encode(s: &str, keep_slash: bool) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            b'/' if keep_slash => out.push('/'),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

impl ObjectStore for S3 {
    fn put(&self, key: &str, bytes: &[u8]) -> std::io::Result<()> {
        let (st, body) = self.request("PUT", key, &[], bytes)?;
        if !(200..300).contains(&st) {
            return Err(std::io::Error::other(format!(
                "PUT {key}: {st} {}",
                String::from_utf8_lossy(&body)
            )));
        }
        Ok(())
    }
    fn get(&self, key: &str) -> std::io::Result<Vec<u8>> {
        let (st, body) = self.request("GET", key, &[], &[])?;
        if st != 200 {
            return Err(std::io::Error::other(format!("GET {key}: {st}")));
        }
        Ok(body)
    }
    fn list(&self, prefix: &str) -> std::io::Result<Vec<String>> {
        let (st, body) = self.request("GET", "", &[("list-type", "2"), ("prefix", prefix)], &[])?;
        if st != 200 {
            return Err(std::io::Error::other(format!("LIST {prefix}: {st}")));
        }
        let text = String::from_utf8_lossy(&body);
        let mut keys = Vec::new();
        let mut rest = text.as_ref();
        while let Some(i) = rest.find("<Key>") {
            let after = &rest[i + 5..];
            let Some(j) = after.find("</Key>") else { break };
            keys.push(after[..j].to_string());
            rest = &after[j + 6..];
        }
        Ok(keys)
    }
    fn delete(&self, key: &str) -> std::io::Result<()> {
        let (st, _) = self.request("DELETE", key, &[], &[])?;
        if !(200..300).contains(&st) && st != 404 {
            return Err(std::io::Error::other(format!("DELETE {key}: {st}")));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dates_and_encoding_are_what_sigv4_expects() {
        assert_eq!(amz_dates(0).1, "19700101T000000Z");
        assert_eq!(amz_dates(1_787_921_000).0, "20260828");
        assert_eq!(uri_encode("a b/c", true), "a%20b/c");
        assert_eq!(uri_encode("a b/c", false), "a%20b%2Fc");
    }
}
