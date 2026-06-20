use anyhow::{Context, Result, bail};
use hmac::{Hmac, Mac};
use reqwest::Client;
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::env;
use time::{OffsetDateTime, format_description::FormatItem, macros::format_description};

type HmacSha256 = Hmac<Sha256>;

const AMZ_DATE_FORMAT: &[FormatItem<'_>] =
    format_description!("[year][month][day]T[hour][minute][second]Z");
const DATE_FORMAT: &[FormatItem<'_>] = format_description!("[year][month][day]");

/// Minimal AWS credentials loaded from the Lambda/runtime environment.
#[derive(Clone, Debug)]
pub struct AwsCredentials {
    access_key_id: String,
    secret_access_key: String,
    session_token: Option<String>,
}

impl AwsCredentials {
    /// Loads AWS credentials from standard environment variables.
    pub fn from_env() -> Option<Self> {
        Some(Self {
            access_key_id: env::var("AWS_ACCESS_KEY_ID").ok()?,
            secret_access_key: env::var("AWS_SECRET_ACCESS_KEY").ok()?,
            session_token: env::var("AWS_SESSION_TOKEN").ok(),
        })
    }
}

/// Tiny signed AWS HTTP client for the DynamoDB JSON protocol.
///
/// Hand-rolled SigV4 keeps the Lambda binary small by avoiding the AWS SDK.
#[derive(Clone)]
pub struct AwsJsonClient {
    client: Client,
    region: String,
    credentials: Option<AwsCredentials>,
}

impl AwsJsonClient {
    /// Creates a signed client for one AWS region.
    pub fn new(region: impl Into<String>) -> Self {
        Self {
            client: Client::new(),
            region: region.into(),
            credentials: AwsCredentials::from_env(),
        }
    }

    /// Returns true when credentials are available.
    pub fn has_credentials(&self) -> bool {
        self.credentials.is_some()
    }

    /// Sends a JSON 1.0 request to an AWS JSON-protocol service such as DynamoDB.
    pub async fn post_json(&self, service: &str, target: &str, body: Value) -> Result<Value> {
        let credentials = self
            .credentials
            .as_ref()
            .context("AWS credentials are not available in the environment")?;
        let content_type = "application/x-amz-json-1.0";
        let host = format!("{service}.{}.amazonaws.com", self.region);
        let endpoint = format!("https://{host}/");
        let body = body.to_string();
        let now = OffsetDateTime::now_utc();
        let amz_date = now.format(AMZ_DATE_FORMAT)?;
        let date = now.format(DATE_FORMAT)?;
        let payload_hash = sha256_hex(body.as_bytes());

        let (canonical_headers, signed_headers) = json_canonical_headers(
            content_type,
            &host,
            &amz_date,
            target,
            credentials.session_token.as_deref(),
        );

        let canonical_request =
            format!("POST\n/\n\n{canonical_headers}\n{signed_headers}\n{payload_hash}");
        let credential_scope = format!("{date}/{}/{service}/aws4_request", self.region);
        let string_to_sign = format!(
            "AWS4-HMAC-SHA256\n{amz_date}\n{credential_scope}\n{}",
            sha256_hex(canonical_request.as_bytes())
        );
        let signing_key =
            signing_key(&credentials.secret_access_key, &date, &self.region, service)?;
        let signature = hmac_hex(&signing_key, string_to_sign.as_bytes())?;
        let authorization = format!(
            "AWS4-HMAC-SHA256 Credential={}/{credential_scope}, SignedHeaders={signed_headers}, Signature={signature}",
            credentials.access_key_id
        );

        let mut request = self
            .client
            .post(endpoint)
            .header("content-type", content_type)
            .header("x-amz-date", amz_date)
            .header("x-amz-target", target)
            .header("authorization", authorization)
            .body(body);
        if let Some(token) = &credentials.session_token {
            request = request.header("x-amz-security-token", token);
        }

        let response = request.send().await?;
        let status = response.status();
        let text = response.text().await?;
        if !status.is_success() {
            bail!("AWS {service} {target} failed with HTTP {status}: {text}");
        }
        Ok(serde_json::from_str(&text)?)
    }
}

/// Builds sorted canonical headers for AWS JSON-protocol requests.
fn json_canonical_headers(
    content_type: &str,
    host: &str,
    amz_date: &str,
    target: &str,
    session_token: Option<&str>,
) -> (String, String) {
    let signed_headers = if session_token.is_some() {
        "content-type;host;x-amz-date;x-amz-security-token;x-amz-target"
    } else {
        "content-type;host;x-amz-date;x-amz-target"
    }
    .to_string();
    let mut canonical_headers =
        format!("content-type:{content_type}\nhost:{host}\nx-amz-date:{amz_date}\n");
    if let Some(token) = session_token {
        canonical_headers.push_str(&format!("x-amz-security-token:{token}\n"));
    }
    canonical_headers.push_str(&format!("x-amz-target:{target}\n"));
    (canonical_headers, signed_headers)
}

/// Returns lower-case SHA-256 hex.
fn sha256_hex(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

/// Computes the AWS SigV4 signing key.
fn signing_key(secret: &str, date: &str, region: &str, service: &str) -> Result<Vec<u8>> {
    let k_date = hmac_bytes(format!("AWS4{secret}").as_bytes(), date.as_bytes())?;
    let k_region = hmac_bytes(&k_date, region.as_bytes())?;
    let k_service = hmac_bytes(&k_region, service.as_bytes())?;
    hmac_bytes(&k_service, b"aws4_request")
}

/// Computes HMAC-SHA256 bytes.
fn hmac_bytes(key: &[u8], msg: &[u8]) -> Result<Vec<u8>> {
    let mut mac = HmacSha256::new_from_slice(key)?;
    mac.update(msg);
    Ok(mac.finalize().into_bytes().to_vec())
}

/// Computes HMAC-SHA256 as lower-case hex.
fn hmac_hex(key: &[u8], msg: &[u8]) -> Result<String> {
    Ok(hex::encode(hmac_bytes(key, msg)?))
}

#[cfg(test)]
mod tests {
    use super::{hmac_hex, json_canonical_headers, sha256_hex, signing_key};

    #[test]
    fn sha256_hex_matches_known_value() {
        assert_eq!(
            sha256_hex(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }

    #[test]
    fn json_headers_with_session_token_are_sorted() {
        let (canonical_headers, signed_headers) = json_canonical_headers(
            "application/x-amz-json-1.0",
            "dynamodb.us-east-1.amazonaws.com",
            "20260620T000000Z",
            "DynamoDB_20120810.GetItem",
            Some("token"),
        );

        assert_eq!(
            signed_headers,
            "content-type;host;x-amz-date;x-amz-security-token;x-amz-target"
        );
        assert!(
            canonical_headers.find("x-amz-security-token").unwrap()
                < canonical_headers.find("x-amz-target").unwrap()
        );
    }

    #[test]
    fn json_headers_without_session_token_omit_security_token() {
        let (canonical_headers, signed_headers) = json_canonical_headers(
            "application/x-amz-json-1.0",
            "dynamodb.us-east-1.amazonaws.com",
            "20260620T000000Z",
            "DynamoDB_20120810.GetItem",
            None,
        );

        assert_eq!(signed_headers, "content-type;host;x-amz-date;x-amz-target");
        assert!(canonical_headers.contains("content-type:application/x-amz-json-1.0\n"));
        assert!(canonical_headers.contains("host:dynamodb.us-east-1.amazonaws.com\n"));
        assert!(!canonical_headers.contains("x-amz-security-token"));
    }

    #[test]
    fn hmac_and_signing_key_are_stable() {
        assert_eq!(
            hmac_hex(b"key", b"The quick brown fox jumps over the lazy dog").unwrap(),
            "f7bc83f430538424b13298e6aa6fb143ef4d59a14946175997479dbc2d1a3cd8"
        );

        let key = signing_key(
            "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY",
            "20120215",
            "us-east-1",
            "iam",
        )
        .unwrap();
        assert_eq!(key.len(), 32);
    }
}
