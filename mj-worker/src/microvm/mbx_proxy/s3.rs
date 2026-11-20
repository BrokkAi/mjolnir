use std::time::{Duration, SystemTime};

use anyhow::{Context, Result, ensure};
use chrono::{DateTime, Utc};
use hmac::{Hmac, KeyInit, Mac};
use mbx_cache_core::{InstanceRoleCredentials, S3Credentials, TemporaryCredentials};
use reqwest::{Client, Response, StatusCode, Url};
use serde::Deserialize;
use sha2::{Digest as _, Sha256};
use tokio::sync::Mutex;

use super::MbxRemote;

// AWS instance-profile credentials begin rotating five minutes before expiry.
// Use the same refresh margin as mbx-cache-core's S3 client.
const RENEWAL_MARGIN: Duration = Duration::from_secs(5 * 60);
// IMDS can return the same credentials during the renewal margin. Match
// mbx-cache-core's renewal backoff instead of fetching on each cache request.
const RENEWAL_RETRY: Duration = Duration::from_secs(30);

pub(super) struct S3 {
    base: Url,
    prefix: String,
    region: String,
    client: Client,
    provider: InstanceRoleCredentials,
    role: Mutex<Role>,
}

struct Role {
    temporary: TemporaryCredentials,
    refresh_after: SystemTime,
    last_error: Option<String>,
}

pub(super) enum ReadError {
    Other(anyhow::Error),
    S3 {
        status: StatusCode,
        code: Option<String>,
    },
}

impl ReadError {
    pub(super) fn diagnostic(&self) -> String {
        match self {
            Self::Other(error) => format!("{error:#}"),
            Self::S3 { status, code } => match code {
                Some(code) => code.clone(),
                None => format!("S3 HTTP {}", status.as_u16()),
            },
        }
    }

    pub(super) fn access_denied(&self) -> bool {
        matches!(self, Self::S3 { code: Some(code), .. } if code == "AccessDenied")
    }
}

impl<E: Into<anyhow::Error>> From<E> for ReadError {
    fn from(error: E) -> Self {
        Self::Other(error.into())
    }
}

impl S3 {
    pub(super) async fn connect(remote: &MbxRemote) -> Result<Option<Self>> {
        let Some(provider) = InstanceRoleCredentials::from_env().map_err(core_error)? else {
            return Ok(None);
        };
        let role = match provider.fetch().await {
            Ok(role) => role,
            // Core's public IMDS provider erases the HTTP status. Match the
            // two specific no-role diagnostics in mbx-cache-core 0.21.
            Err(error)
                if error
                    .chain()
                    .any(|cause| cause.to_string() == "the instance has no IAM role attached")
                    || error.to_string() == "could not list the instance's IAM roles"
                        && error.chain().any(|cause| {
                            cause.to_string() == "the metadata service answered 404 Not Found"
                        }) =>
            {
                return Ok(None);
            }
            Err(error) => return Err(core_error(error)),
        };
        let region = match &remote.region {
            Some(region) => region.clone(),
            None => provider.region().await.map_err(core_error)?,
        };
        let endpoint = std::env::var("MBX_REMOTE_S3_ENDPOINT")
            .ok()
            .filter(|endpoint| !endpoint.is_empty())
            .map(|endpoint| endpoint.parse())
            .transpose()?;
        Self::new(remote, region, provider, role, endpoint).map(Some)
    }

    pub(super) fn new(
        remote: &MbxRemote,
        region: String,
        provider: InstanceRoleCredentials,
        role: TemporaryCredentials,
        endpoint: Option<Url>,
    ) -> Result<Self> {
        ensure!(
            !region.is_empty()
                && region
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-'),
            "invalid S3 region {region:?}"
        );
        let bucket = &remote.bucket;
        let domain = if region.starts_with("cn-") {
            "amazonaws.com.cn"
        } else {
            "amazonaws.com"
        };
        let base = if let Some(mut url) = endpoint {
            ensure!(
                matches!(url.scheme(), "http" | "https")
                    && url.host().is_some()
                    && url.username().is_empty()
                    && url.password().is_none()
                    && url.query().is_none()
                    && url.fragment().is_none(),
                "invalid S3 endpoint"
            );
            let path = format!("{}/{bucket}/", url.path().trim_end_matches('/'));
            url.set_path(&path);
            url
        } else if bucket.contains('.') {
            format!("https://s3.{region}.{domain}/{bucket}/").parse()?
        } else {
            format!("https://{bucket}.s3.{region}.{domain}/").parse()?
        };
        let client = Client::builder()
            .no_proxy()
            .no_gzip()
            .no_brotli()
            .no_deflate()
            .no_zstd()
            .redirect(reqwest::redirect::Policy::none())
            // Match mbx's default connect and response inactivity deadlines.
            .connect_timeout(Duration::from_secs(30))
            .read_timeout(Duration::from_secs(30))
            .build()?;
        Ok(Self {
            base,
            // The literal prefix the IAM policy names; percent escapes are refused.
            prefix: remote.prefix.clone(),
            region,
            client,
            provider,
            role: Mutex::new(Role {
                refresh_after: role.expires_at - RENEWAL_MARGIN,
                temporary: role,
                last_error: None,
            }),
        })
    }

    async fn credentials(&self) -> Result<S3Credentials> {
        let mut role = self.role.lock().await;
        let now = SystemTime::now();
        if role.refresh_after <= now {
            match self.provider.fetch().await {
                Ok(fresh) => {
                    role.temporary = fresh;
                    role.last_error = None;
                }
                Err(error) => {
                    let message = format!("{error:#}");
                    if role.last_error.is_none() {
                        eprintln!("could not refresh the EC2 instance role credentials: {message}");
                    }
                    role.last_error = Some(message);
                }
            }
            role.refresh_after =
                (role.temporary.expires_at - RENEWAL_MARGIN).max(SystemTime::now() + RENEWAL_RETRY);
        }
        ensure!(
            role.temporary.expires_at > SystemTime::now(),
            "EC2 instance role credentials have expired{}",
            role.last_error
                .as_ref()
                .map(|error| format!(": {error}"))
                .unwrap_or_default()
        );
        Ok(role.temporary.credentials.clone())
    }

    pub(super) async fn get(
        &self,
        namespace: &str,
        route: &str,
    ) -> Result<Option<Response>, ReadError> {
        let key = if self.prefix.is_empty() {
            format!("{namespace}{route}")
        } else {
            format!("{}/{namespace}{route}", self.prefix)
        };
        let url = format!("{}{}", self.base, encode_key(&key))
            .parse()
            .context("construct S3 object URL")?;
        let credentials = self.credentials().await?;
        let response = signed_get(&self.client, url, &self.region, &credentials, Utc::now())
            .send()
            .await
            .context("read S3")?;
        let status = response.status();
        if status == StatusCode::OK {
            return Ok(Some(response));
        }

        #[derive(Deserialize)]
        struct S3Error {
            #[serde(rename = "Code")]
            code: String,
        }
        // Match the S3 diagnostic body limit in mbx-cache-core.
        const MAX_ERROR_BODY_BYTES: usize = 8 * 1024;
        let code = read_body(response, MAX_ERROR_BODY_BYTES)
            .await
            .ok()
            .and_then(|bytes| quick_xml::de::from_reader::<_, S3Error>(bytes.as_slice()).ok())
            .map(|error| error.code);
        match (status, code.as_deref()) {
            (_, Some("NoSuchKey")) | (StatusCode::NOT_FOUND, None) => Ok(None),
            _ => Err(ReadError::S3 { status, code }),
        }
    }
}

pub(super) async fn read_body(mut response: Response, max_bytes: usize) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await? {
        ensure!(
            chunk.len() <= max_bytes - bytes.len(),
            "S3 response exceeds {max_bytes} bytes"
        );
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
}

fn encode_key(key: &str) -> String {
    use std::fmt::Write as _;

    let mut encoded = String::with_capacity(key.len());
    for byte in key.bytes() {
        if byte.is_ascii_alphanumeric() || b"-._~/".contains(&byte) {
            encoded.push(char::from(byte));
        } else {
            write!(encoded, "%{byte:02X}").expect("write to a String");
        }
    }
    encoded
}

fn signed_get(
    client: &Client,
    url: Url,
    region: &str,
    role: &S3Credentials,
    now: DateTime<Utc>,
) -> reqwest::RequestBuilder {
    const EMPTY_SHA256: &str = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";
    let date = now.format("%Y%m%d").to_string();
    let timestamp = now.format("%Y%m%dT%H%M%SZ");
    let host = &url[url::Position::BeforeHost..url::Position::AfterPort];
    let mut headers =
        format!("host:{host}\nx-amz-content-sha256:{EMPTY_SHA256}\nx-amz-date:{timestamp}\n");
    let mut names = "host;x-amz-content-sha256;x-amz-date";
    if let Some(token) = &role.session_token {
        headers.push_str(&format!("x-amz-security-token:{token}\n"));
        names = "host;x-amz-content-sha256;x-amz-date;x-amz-security-token";
    }
    let canonical = format!("GET\n{}\n\n{headers}\n{names}\n{EMPTY_SHA256}", url.path());
    let scope = format!("{date}/{region}/s3/aws4_request");
    let to_sign = format!(
        "AWS4-HMAC-SHA256\n{timestamp}\n{scope}\n{}",
        hex::encode(Sha256::digest(canonical))
    );
    let secret = format!("AWS4{}", role.secret_access_key);
    let mut key = hmac(secret.as_bytes(), &date);
    for input in [region, "s3", "aws4_request", &to_sign] {
        key = hmac(&key, input);
    }
    let authorization = format!(
        "AWS4-HMAC-SHA256 Credential={}/{scope}, SignedHeaders={names}, Signature={}",
        role.access_key_id,
        hex::encode(key)
    );
    let mut request = client
        .get(url)
        .header("authorization", authorization)
        .header("x-amz-content-sha256", EMPTY_SHA256)
        .header("x-amz-date", timestamp.to_string());
    if let Some(token) = &role.session_token {
        request = request.header("x-amz-security-token", token);
    }
    request
}

fn hmac(key: &[u8], data: &str) -> [u8; 32] {
    let mut mac = Hmac::<Sha256>::new_from_slice(key).expect("HMAC accepts keys of any size");
    mac.update(data.as_bytes());
    mac.finalize().into_bytes().into()
}

pub(super) fn core_error(error: impl std::fmt::Display + std::fmt::Debug) -> anyhow::Error {
    anyhow::anyhow!("{error:#}")
}

#[cfg(test)]
mod tests {
    use super::*;

    // Cross-check the raw GET with the independently calculated vector in
    // mr-boxington's crates/mbx-cache-core/src/sigv4.rs.
    #[test]
    fn mbx_proxy_sigv4_matches_mbx() {
        let _ = rustls::crypto::ring::default_provider().install_default();
        let role = S3Credentials {
            access_key_id: "AKIDEXAMPLE".into(),
            secret_access_key: "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY".into(),
            session_token: None,
        };
        let request = signed_get(
            &Client::new(),
            "https://examplebucket.s3.amazonaws.com/test.txt"
                .parse()
                .unwrap(),
            "us-east-1",
            &role,
            DateTime::from_timestamp(1_440_938_160, 0).unwrap(),
        )
        .build()
        .unwrap();
        assert_eq!(
            request.headers()["authorization"],
            "AWS4-HMAC-SHA256 Credential=AKIDEXAMPLE/20150830/us-east-1/s3/aws4_request, \
             SignedHeaders=host;x-amz-content-sha256;x-amz-date, \
             Signature=bbfdf4d3c3eab24da182f8f790e0c7d8e2a20658191717a6546076effa9f5a5e"
        );
    }
}
