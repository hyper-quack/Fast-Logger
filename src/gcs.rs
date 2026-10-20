//! Google Cloud Storage uploads over the JSON API.
//!
//! Credentials, in order: `upload.access_token` / `ROTLOG_GCS_TOKEN`;
//! `upload.credentials` / `GOOGLE_APPLICATION_CREDENTIALS` / the gcloud ADC file
//! (service-account keys are exchanged with a self-signed RS256 JWT,
//! authorized-user files with their refresh token); finally the GCE / Cloud Run /
//! GKE metadata server.

use std::fs::{self, File};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::Deserialize;

use crate::config::UploadConfig;
use crate::upload::{Uploaded, Uploader};

const SCOPE: &str = "https://www.googleapis.com/auth/devstorage.read_write";
const GOOGLE_TOKEN_URI: &str = "https://oauth2.googleapis.com/token";

pub struct Gcs {
    agent: ureq::Agent,
    endpoint: String,
    bucket: String,
    auth: Auth,
    token: Mutex<Option<(String, Instant)>>,
}

enum Auth {
    Static(String),
    ServiceAccount { email: String, key_id: Option<String>, key_der: Vec<u8>, token_uri: String },
    User { client_id: String, client_secret: String, refresh_token: String, token_uri: String },
    Metadata { host: String },
}

#[derive(Deserialize)]
struct CredFile {
    #[serde(rename = "type")]
    kind: String,
    client_email: Option<String>,
    private_key: Option<String>,
    private_key_id: Option<String>,
    token_uri: Option<String>,
    client_id: Option<String>,
    client_secret: Option<String>,
    refresh_token: Option<String>,
}

#[derive(Deserialize)]
struct TokenResponse {
    access_token: String,
    #[serde(default)]
    expires_in: Option<u64>,
}

#[derive(Deserialize)]
struct ObjectMeta {
    #[serde(default)]
    size: Option<String>,
}

impl Gcs {
    pub fn new(cfg: &UploadConfig) -> Result<Gcs, String> {
        if cfg.bucket.is_empty() {
            return Err("upload.bucket is not set".into());
        }
        let agent = ureq::Agent::config_builder()
            .http_status_as_error(false)
            .timeout_global(Some(cfg.timeout.0))
            .timeout_connect(Some(Duration::from_secs(10)))
            .user_agent(concat!("rotlog/", env!("CARGO_PKG_VERSION")))
            .build()
            .new_agent();
        Ok(Gcs {
            agent,
            endpoint: cfg.endpoint.trim_end_matches('/').to_string(),
            bucket: cfg.bucket.clone(),
            auth: resolve_auth(cfg)?,
            token: Mutex::new(None),
        })
    }

    pub fn auth_kind(&self) -> String {
        match &self.auth {
            Auth::Static(_) => "static access token".into(),
            Auth::ServiceAccount { email, .. } => format!("service account {email}"),
            Auth::User { .. } => "authorized user (gcloud application-default login)".into(),
            Auth::Metadata { host } => format!("metadata server ({host})"),
        }
    }

    fn token(&self) -> Result<String, String> {
        let mut cached = self.token.lock().unwrap_or_else(|e| e.into_inner());
        if let Some((t, exp)) = cached.as_ref()
            && Instant::now() < *exp
        {
            return Ok(t.clone());
        }
        let (token, ttl) = self.fetch_token()?;
        // Refresh a minute early.
        let exp = Instant::now() + Duration::from_secs(ttl.saturating_sub(60).max(30));
        *cached = Some((token.clone(), exp));
        Ok(token)
    }

    fn invalidate_token(&self) {
        *self.token.lock().unwrap_or_else(|e| e.into_inner()) = None;
    }

    fn fetch_token(&self) -> Result<(String, u64), String> {
        let resp = match &self.auth {
            Auth::Static(t) => return Ok((t.clone(), 3600)),
            Auth::ServiceAccount { email, key_id, key_der, token_uri } => {
                let jwt = sign_jwt(email, key_id.as_deref(), key_der, token_uri)?;
                self.agent.post(token_uri).send_form([
                    ("grant_type", "urn:ietf:params:oauth:grant-type:jwt-bearer"),
                    ("assertion", jwt.as_str()),
                ])
            }
            Auth::User { client_id, client_secret, refresh_token, token_uri } => {
                self.agent.post(token_uri).send_form([
                    ("grant_type", "refresh_token"),
                    ("client_id", client_id.as_str()),
                    ("client_secret", client_secret.as_str()),
                    ("refresh_token", refresh_token.as_str()),
                ])
            }
            Auth::Metadata { host } => self
                .agent
                .get(&format!("http://{host}/computeMetadata/v1/instance/service-accounts/default/token"))
                .header("Metadata-Flavor", "Google")
                .config()
                .timeout_global(Some(Duration::from_secs(3)))
                .build()
                .call(),
        };
        let mut resp = resp.map_err(|e| match &self.auth {
            Auth::Metadata { .. } => format!(
                "no Google credentials found (set upload.credentials or GOOGLE_APPLICATION_CREDENTIALS, \
                 run `gcloud auth application-default login`, or run on GCP): metadata server: {e}"
            ),
            _ => format!("token request failed: {e}"),
        })?;
        let status = resp.status().as_u16();
        let body = resp.body_mut().read_to_string().unwrap_or_default();
        if status != 200 {
            return Err(format!("token request failed: HTTP {status}: {}", snippet(&body)));
        }
        let t: TokenResponse = serde_json::from_str(&body).map_err(|e| format!("invalid token response: {e}"))?;
        Ok((t.access_token, t.expires_in.unwrap_or(3600)))
    }

    fn object_size(&self, object: &str) -> Result<Option<u64>, String> {
        let url = format!("{}/storage/v1/b/{}/o/{}?fields=size", self.endpoint, pct(&self.bucket), pct(object));
        let token = self.token()?;
        let mut r = self
            .agent
            .get(&url)
            .header("Authorization", &format!("Bearer {token}"))
            .call()
            .map_err(|e| e.to_string())?;
        let status = r.status().as_u16();
        let body = r.body_mut().read_to_string().unwrap_or_default();
        match status {
            200 => Ok(serde_json::from_str::<ObjectMeta>(&body).ok().and_then(|m| m.size).and_then(|s| s.parse().ok())),
            404 => Ok(None),
            s => Err(format!("HTTP {s}: {}", snippet(&body))),
        }
    }
}

impl Uploader for Gcs {
    fn upload(&self, path: &Path, object: &str, content_type: &str) -> Result<Uploaded, String> {
        let size = fs::metadata(path).map_err(|e| e.to_string())?.len();
        let url = format!(
            "{}/upload/storage/v1/b/{}/o?uploadType=media&ifGenerationMatch=0&name={}",
            self.endpoint,
            pct(&self.bucket),
            pct(object)
        );
        let mut last_err = String::new();
        let mut auth_retried = false;
        for attempt in 0..4u32 {
            if attempt > 0 {
                std::thread::sleep(Duration::from_secs(1 << (attempt - 1)));
            }
            let token = self.token()?;
            let file = File::open(path).map_err(|e| e.to_string())?;
            let resp = self
                .agent
                .post(&url)
                .header("Authorization", &format!("Bearer {token}"))
                .header("Content-Type", content_type)
                .send(file);
            let mut r = match resp {
                Ok(r) => r,
                Err(e) => {
                    last_err = e.to_string();
                    continue;
                }
            };
            let status = r.status().as_u16();
            let body = r.body_mut().read_to_string().unwrap_or_default();
            match status {
                200 | 201 => {
                    let remote = serde_json::from_str::<ObjectMeta>(&body)
                        .ok()
                        .and_then(|m| m.size)
                        .and_then(|s| s.parse::<u64>().ok());
                    return match remote {
                        Some(n) if n != size => Err(format!("size mismatch after upload: local {size}, remote {n}")),
                        _ => Ok(Uploaded { bytes: size, existed: false }),
                    };
                }
                // ifGenerationMatch=0 failed: the object exists (e.g. uploaded before a crash).
                412 => {
                    return match self.object_size(object) {
                        Ok(Some(n)) if n == size => Ok(Uploaded { bytes: size, existed: true }),
                        Ok(Some(n)) => {
                            Err(format!("{} already exists with a different size ({n} vs {size})", self.url(object)))
                        }
                        Ok(None) => Err("precondition failed but object not found".into()),
                        // objectCreator-only credentials cannot read metadata; the name is
                        // unique per host/stream/period, so the object is ours.
                        Err(e) if e.starts_with("HTTP 403") => Ok(Uploaded { bytes: size, existed: true }),
                        Err(e) => Err(e),
                    };
                }
                401 if !auth_retried => {
                    auth_retried = true;
                    self.invalidate_token();
                    last_err = format!("HTTP 401: {}", snippet(&body));
                }
                408 | 429 | 500..=599 => last_err = format!("HTTP {status}: {}", snippet(&body)),
                _ => return Err(format!("HTTP {status}: {}", snippet(&body))),
            }
        }
        Err(last_err)
    }

    fn url(&self, object: &str) -> String {
        format!("gs://{}/{object}", self.bucket)
    }

    fn check(&self) -> Result<String, String> {
        self.token()?;
        let mut out = format!("credentials: {} (token OK)\n", self.auth_kind());
        let url = format!("{}/storage/v1/b/{}?fields=name,location", self.endpoint, pct(&self.bucket));
        let token = self.token()?;
        let r = self.agent.get(&url).header("Authorization", &format!("Bearer {token}")).call();
        match r {
            Ok(mut r) => {
                let status = r.status().as_u16();
                let body = r.body_mut().read_to_string().unwrap_or_default();
                match status {
                    200 => out.push_str(&format!("bucket: gs://{} reachable {}", self.bucket, snippet(&body))),
                    // objectCreator may upload without bucket read access.
                    403 => out.push_str(&format!(
                        "bucket: gs://{} metadata not readable (403); uploads still work with roles/storage.objectCreator",
                        self.bucket
                    )),
                    404 => return Err(format!("bucket gs://{} not found", self.bucket)),
                    s => return Err(format!("bucket check: HTTP {s}: {}", snippet(&body))),
                }
            }
            Err(e) => return Err(format!("bucket check: {e}")),
        }
        Ok(out)
    }
}

fn resolve_auth(cfg: &UploadConfig) -> Result<Auth, String> {
    if let Some(t) =
        cfg.access_token.clone().or_else(|| std::env::var("ROTLOG_GCS_TOKEN").ok()).filter(|t| !t.is_empty())
    {
        return Ok(Auth::Static(t));
    }
    let explicit =
        cfg.credentials.clone().or_else(|| std::env::var_os("GOOGLE_APPLICATION_CREDENTIALS").map(PathBuf::from));
    let path = match explicit {
        Some(p) => Some(p),
        None => adc_path().filter(|p| p.exists()),
    };
    if let Some(path) = path {
        let text = fs::read_to_string(&path).map_err(|e| format!("{}: {e}", path.display()))?;
        let c: CredFile = serde_json::from_str(&text).map_err(|e| format!("{}: {e}", path.display()))?;
        let missing = |f: &str| format!("{}: missing '{f}'", path.display());
        return match c.kind.as_str() {
            "service_account" => {
                let pem = c.private_key.ok_or_else(|| missing("private_key"))?;
                Ok(Auth::ServiceAccount {
                    email: c.client_email.ok_or_else(|| missing("client_email"))?,
                    key_id: c.private_key_id,
                    key_der: pem_to_der(&pem).ok_or_else(|| format!("{}: unreadable private_key", path.display()))?,
                    token_uri: c.token_uri.unwrap_or_else(|| GOOGLE_TOKEN_URI.into()),
                })
            }
            "authorized_user" => Ok(Auth::User {
                client_id: c.client_id.ok_or_else(|| missing("client_id"))?,
                client_secret: c.client_secret.ok_or_else(|| missing("client_secret"))?,
                refresh_token: c.refresh_token.ok_or_else(|| missing("refresh_token"))?,
                token_uri: c.token_uri.unwrap_or_else(|| GOOGLE_TOKEN_URI.into()),
            }),
            other => Err(format!("{}: unsupported credential type '{other}'", path.display())),
        };
    }
    let host = std::env::var("GCE_METADATA_HOST").unwrap_or_else(|_| "metadata.google.internal".into());
    Ok(Auth::Metadata { host })
}

fn adc_path() -> Option<PathBuf> {
    if cfg!(windows) {
        std::env::var_os("APPDATA").map(|a| PathBuf::from(a).join("gcloud/application_default_credentials.json"))
    } else {
        std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config/gcloud/application_default_credentials.json"))
    }
}

fn sign_jwt(email: &str, key_id: Option<&str>, key_der: &[u8], aud: &str) -> Result<String, String> {
    use ring::{rand::SystemRandom, signature};
    let now = SystemTime::now().duration_since(UNIX_EPOCH).map_err(|e| e.to_string())?.as_secs();
    let mut header = serde_json::json!({ "alg": "RS256", "typ": "JWT" });
    if let Some(k) = key_id {
        header["kid"] = k.into();
    }
    let claims = serde_json::json!({ "iss": email, "scope": SCOPE, "aud": aud, "iat": now, "exp": now + 3600 });
    let input = format!("{}.{}", b64url(header.to_string().as_bytes()), b64url(claims.to_string().as_bytes()));
    let key = signature::RsaKeyPair::from_pkcs8(key_der).map_err(|e| format!("invalid service account key: {e}"))?;
    let mut sig = vec![0u8; key.public().modulus_len()];
    key.sign(&signature::RSA_PKCS1_SHA256, &SystemRandom::new(), input.as_bytes(), &mut sig)
        .map_err(|_| "signing the token request failed".to_string())?;
    Ok(format!("{input}.{}", b64url(&sig)))
}

const B64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

/// Unpadded base64url.
pub fn b64url(data: &[u8]) -> String {
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let n =
            (chunk[0] as u32) << 16 | (*chunk.get(1).unwrap_or(&0) as u32) << 8 | *chunk.get(2).unwrap_or(&0) as u32;
        for i in 0..=chunk.len() {
            let c = B64[(n >> (18 - 6 * i) & 63) as usize];
            out.push(match c {
                b'+' => '-',
                b'/' => '_',
                c => c as char,
            });
        }
    }
    out
}

fn b64_decode(s: &str) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(s.len() * 3 / 4);
    let (mut acc, mut bits) = (0u32, 0);
    for c in s.bytes() {
        let v = match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'+' | b'-' => 62,
            b'/' | b'_' => 63,
            b'=' | b'\n' | b'\r' | b' ' | b'\t' => continue,
            _ => return None,
        };
        acc = acc << 6 | v as u32;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
        }
    }
    Some(out)
}

fn pem_to_der(pem: &str) -> Option<Vec<u8>> {
    let body: String = pem.lines().filter(|l| !l.starts_with("-----")).collect();
    b64_decode(&body)
}

/// Percent-encodes everything but RFC 3986 unreserved characters.
fn pct(s: &str) -> String {
    let mut out = String::with_capacity(s.len() * 3);
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

fn snippet(s: &str) -> String {
    let s = s.trim();
    if s.len() <= 300 {
        return s.to_string();
    }
    let mut end = 300;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &s[..end])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64() {
        assert_eq!(b64url(b""), "");
        assert_eq!(b64url(b"f"), "Zg");
        assert_eq!(b64url(b"fo"), "Zm8");
        assert_eq!(b64url(b"foo"), "Zm9v");
        assert_eq!(b64url(&[0xfb, 0xff]), "-_8");
        assert_eq!(b64_decode("Zm9vYmE=").unwrap(), b"fooba");
        assert_eq!(pct("logs/a b.log"), "logs%2Fa%20b.log");
    }
}
