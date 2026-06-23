//! Minimal MediaWiki OAuth 1.0a (HMAC-SHA1) client for the out-of-band (OOB) flow.
//!
//! The bot has no public callback endpoint, so it uses the OOB flow:
//! 1. [`OAuthClient::initiate`] gets a temporary request token (`oauth_callback=oob`).
//! 2. The user opens [`OAuthClient::authorize_url`] on-wiki and is shown a short
//!    verifier code, which they paste back into the Telegram chat.
//! 3. [`OAuthClient::exchange`] turns the verifier into a long-lived access token.
//! 4. [`OAuthClient::api_authorization`] signs each Action API request with it.
//!
//! Wikimedia centralises OAuth on meta.wikimedia.org (see [`OAuthEndpoints::wikimedia`]).

use anyhow::{Result, bail};
use hmac::{Hmac, Mac};
use sha1::Sha1;

type HmacSha1 = Hmac<Sha1>;

/// OAuth 1.0a endpoints on the central wiki.
#[derive(Clone, Debug)]
pub struct OAuthEndpoints {
    /// `Special:OAuth/initiate` (request-token endpoint).
    pub initiate: String,
    /// `Special:OAuth/authorize` (user-facing authorization page).
    pub authorize: String,
    /// `Special:OAuth/token` (access-token endpoint).
    pub token: String,
}

impl OAuthEndpoints {
    /// Endpoints for Wikimedia projects (OAuth is centralised on meta.wikimedia.org).
    pub fn wikimedia() -> Self {
        let base = "https://meta.wikimedia.org";
        Self {
            initiate: format!("{base}/w/index.php?title=Special:OAuth/initiate"),
            authorize: format!("{base}/wiki/Special:OAuth/authorize"),
            token: format!("{base}/w/index.php?title=Special:OAuth/token"),
        }
    }
}

/// OAuth consumer (application) credentials from `Special:OAuthConsumerRegistration`.
#[derive(Clone)]
pub struct Consumer {
    /// Consumer key.
    pub key: String,
    /// Consumer secret.
    pub secret: String,
}

/// Percent-encodes per the OAuth/RFC 3986 unreserved set (`ALPHA DIGIT - . _ ~`).
pub fn percent_encode(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for byte in input.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                out.push(byte as char);
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

/// Decodes `%XX` escapes (and `+` as space) from a form-encoded value.
fn percent_decode(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'%' if index + 2 < bytes.len() => {
                match u8::from_str_radix(&input[index + 1..index + 3], 16) {
                    Ok(decoded) => {
                        out.push(decoded);
                        index += 3;
                    }
                    Err(_) => {
                        out.push(b'%');
                        index += 1;
                    }
                }
            }
            b'+' => {
                out.push(b' ');
                index += 1;
            }
            other => {
                out.push(other);
                index += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Splits a URL into its base (no query) and decoded query parameters.
fn split_url(url: &str) -> (String, Vec<(String, String)>) {
    match url.split_once('?') {
        Some((base, query)) => {
            let params = query
                .split('&')
                .filter(|pair| !pair.is_empty())
                .map(|pair| match pair.split_once('=') {
                    Some((key, value)) => (percent_decode(key), percent_decode(value)),
                    None => (percent_decode(pair), String::new()),
                })
                .collect();
            (base.to_string(), params)
        }
        None => (url.to_string(), Vec::new()),
    }
}

/// Builds the OAuth signature base string: `METHOD&base_url&sorted_encoded_params`.
fn signature_base_string(method: &str, base_url: &str, params: &[(String, String)]) -> String {
    let mut encoded: Vec<(String, String)> = params
        .iter()
        .map(|(key, value)| (percent_encode(key), percent_encode(value)))
        .collect();
    encoded.sort();
    let joined = encoded
        .iter()
        .map(|(key, value)| format!("{key}={value}"))
        .collect::<Vec<_>>()
        .join("&");
    format!(
        "{}&{}&{}",
        method.to_ascii_uppercase(),
        percent_encode(base_url),
        percent_encode(&joined)
    )
}

/// Computes the HMAC-SHA1 OAuth signature (base64) for a base string.
fn sign(base_string: &str, consumer_secret: &str, token_secret: &str) -> String {
    use base64::Engine;
    let key = format!(
        "{}&{}",
        percent_encode(consumer_secret),
        percent_encode(token_secret)
    );
    let mut mac = HmacSha1::new_from_slice(key.as_bytes()).expect("HMAC accepts any key length");
    mac.update(base_string.as_bytes());
    base64::engine::general_purpose::STANDARD.encode(mac.finalize().into_bytes())
}

/// A short random nonce for an OAuth request.
fn nonce() -> String {
    let value: u128 = rand::random();
    format!("{value:032x}")
}

/// Current unix timestamp in seconds.
fn unix_ts() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0)
}

/// Builds the `Authorization: OAuth …` header value for a signed request.
///
/// `extra` carries protocol params such as `oauth_callback`/`oauth_verifier`;
/// `body_params` are `application/x-www-form-urlencoded` params to include in the
/// signature (pass empty for multipart or JSON bodies, which OAuth does not sign).
fn authorization_header(
    method: &str,
    url: &str,
    consumer: &Consumer,
    token: Option<(&str, &str)>,
    extra: &[(&str, &str)],
    body_params: &[(String, String)],
) -> String {
    let (base_url, query_params) = split_url(url);
    let mut oauth_params: Vec<(String, String)> = vec![
        ("oauth_consumer_key".into(), consumer.key.clone()),
        ("oauth_nonce".into(), nonce()),
        ("oauth_signature_method".into(), "HMAC-SHA1".into()),
        ("oauth_timestamp".into(), unix_ts().to_string()),
        ("oauth_version".into(), "1.0".into()),
    ];
    if let Some((access_token, _)) = token {
        oauth_params.push(("oauth_token".into(), access_token.to_string()));
    }
    for (key, value) in extra {
        oauth_params.push(((*key).to_string(), (*value).to_string()));
    }

    // Signature is over oauth params + query params + form body params.
    let mut all = oauth_params.clone();
    all.extend(query_params);
    all.extend(body_params.iter().cloned());
    let base = signature_base_string(method, &base_url, &all);
    let token_secret = token.map(|(_, secret)| secret).unwrap_or("");
    let signature = sign(&base, &consumer.secret, token_secret);
    oauth_params.push(("oauth_signature".into(), signature));

    let header = oauth_params
        .iter()
        .map(|(key, value)| format!("{}=\"{}\"", percent_encode(key), percent_encode(value)))
        .collect::<Vec<_>>()
        .join(", ");
    format!("OAuth {header}")
}

/// Parses an OAuth token endpoint's form-encoded response into `(token, secret)`.
fn parse_token_response(body: &str) -> Result<(String, String)> {
    let mut token = None;
    let mut secret = None;
    for pair in body.split('&') {
        if let Some((key, value)) = pair.split_once('=') {
            match key {
                "oauth_token" => token = Some(percent_decode(value)),
                "oauth_token_secret" => secret = Some(percent_decode(value)),
                _ => {}
            }
        }
    }
    match (token, secret) {
        (Some(token), Some(secret)) => Ok((token, secret)),
        _ => bail!("OAuth token response missing oauth_token/oauth_token_secret: {body}"),
    }
}

/// MediaWiki OAuth 1.0a client for the out-of-band flow.
#[derive(Clone)]
pub struct OAuthClient {
    http: reqwest::Client,
    endpoints: OAuthEndpoints,
    consumer: Consumer,
}

impl OAuthClient {
    /// Builds a client from consumer credentials and central-wiki endpoints.
    pub fn new(consumer: Consumer, endpoints: OAuthEndpoints, user_agent: &str) -> Result<Self> {
        let http = reqwest::Client::builder().user_agent(user_agent).build()?;
        Ok(Self {
            http,
            endpoints,
            consumer,
        })
    }

    /// Step 1: obtains a temporary request token for the OOB flow.
    pub async fn initiate(&self) -> Result<(String, String)> {
        let url = self.endpoints.initiate.clone();
        let header = authorization_header(
            "GET",
            &url,
            &self.consumer,
            None,
            &[("oauth_callback", "oob")],
            &[],
        );
        let body = self
            .http
            .get(&url)
            .header("Authorization", header)
            .send()
            .await?
            .error_for_status()?
            .text()
            .await?;
        parse_token_response(&body)
    }

    /// The on-wiki URL the user opens to authorize; OOB then shows them a verifier code.
    pub fn authorize_url(&self, request_token: &str) -> String {
        format!(
            "{}?oauth_token={}&oauth_consumer_key={}",
            self.endpoints.authorize,
            percent_encode(request_token),
            percent_encode(&self.consumer.key)
        )
    }

    /// Step 3: exchanges the pasted verifier for a long-lived access token + secret.
    pub async fn exchange(
        &self,
        request_token: &str,
        request_secret: &str,
        verifier: &str,
    ) -> Result<(String, String)> {
        let url = self.endpoints.token.clone();
        let header = authorization_header(
            "GET",
            &url,
            &self.consumer,
            Some((request_token, request_secret)),
            &[("oauth_verifier", verifier)],
            &[],
        );
        let body = self
            .http
            .get(&url)
            .header("Authorization", header)
            .send()
            .await?
            .error_for_status()?
            .text()
            .await?;
        parse_token_response(&body)
    }

    /// Builds the `Authorization` header that signs an Action API request with an
    /// access token. `body_params` should be empty for multipart/JSON bodies.
    pub fn api_authorization(
        &self,
        method: &str,
        url: &str,
        access_token: &str,
        access_secret: &str,
        body_params: &[(String, String)],
    ) -> String {
        authorization_header(
            method,
            url,
            &self.consumer,
            Some((access_token, access_secret)),
            &[],
            body_params,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::{percent_encode, sign, signature_base_string, split_url};

    #[test]
    fn percent_encodes_reserved_characters() {
        assert_eq!(
            percent_encode("Ladies + Gentlemen"),
            "Ladies%20%2B%20Gentlemen"
        );
        assert_eq!(
            percent_encode("Special:OAuth/initiate"),
            "Special%3AOAuth%2Finitiate"
        );
        assert_eq!(percent_encode("aA1-._~"), "aA1-._~");
    }

    #[test]
    fn splits_url_into_base_and_params() {
        let (base, params) =
            split_url("https://meta.wikimedia.org/w/index.php?title=Special:OAuth/initiate");
        assert_eq!(base, "https://meta.wikimedia.org/w/index.php");
        assert_eq!(
            params,
            vec![("title".to_string(), "Special:OAuth/initiate".to_string())]
        );
    }

    // Twitter's published worked example pins the OAuth 1.0a base-string construction
    // (parameter normalisation, sorting, double percent-encoding) byte-for-byte.
    #[test]
    fn builds_known_oauth_base_string() {
        let params = vec![
            (
                "status".to_string(),
                "Hello Ladies + Gentlemen, a signed OAuth request!".to_string(),
            ),
            ("include_entities".to_string(), "true".to_string()),
            (
                "oauth_consumer_key".to_string(),
                "xvz1evFS4wEEPTGEFPHBog".to_string(),
            ),
            (
                "oauth_nonce".to_string(),
                "kYjzVBB8Y0ZFabxSWbWovY3uYSQ2pTgmZeNu2VS4cg".to_string(),
            ),
            (
                "oauth_signature_method".to_string(),
                "HMAC-SHA1".to_string(),
            ),
            ("oauth_timestamp".to_string(), "1318622958".to_string()),
            (
                "oauth_token".to_string(),
                "370773112-GmHxMAgYyLbNEtIKZeRNFsMKPR9EyMZeS9weJAEb".to_string(),
            ),
            ("oauth_version".to_string(), "1.0".to_string()),
        ];
        let base = signature_base_string(
            "post",
            "https://api.twitter.com/1/statuses/update.json",
            &params,
        );
        assert_eq!(
            base,
            "POST&https%3A%2F%2Fapi.twitter.com%2F1%2Fstatuses%2Fupdate.json&include_entities%3Dtrue%26oauth_consumer_key%3Dxvz1evFS4wEEPTGEFPHBog%26oauth_nonce%3DkYjzVBB8Y0ZFabxSWbWovY3uYSQ2pTgmZeNu2VS4cg%26oauth_signature_method%3DHMAC-SHA1%26oauth_timestamp%3D1318622958%26oauth_token%3D370773112-GmHxMAgYyLbNEtIKZeRNFsMKPR9EyMZeS9weJAEb%26oauth_version%3D1.0%26status%3DHello%2520Ladies%2520%252B%2520Gentlemen%252C%2520a%2520signed%2520OAuth%2520request%2521"
        );
    }

    // Signing is HMAC-SHA1 (20 bytes) → base64 (28 chars, one '=' pad), deterministic
    // for fixed inputs and sensitive to the key. Construction correctness is pinned above.
    #[test]
    fn signing_is_deterministic_hmac_sha1_base64() {
        let base = "POST&https%3A%2F%2Fexample.org%2Fw%2Fapi.php&oauth_nonce%3Dabc";
        let signature = sign(base, "consumer-secret", "token-secret");
        assert_eq!(signature.len(), 28);
        assert!(signature.ends_with('='));
        assert_eq!(signature, sign(base, "consumer-secret", "token-secret"));
        assert_ne!(signature, sign(base, "consumer-secret", "other-secret"));
    }
}
