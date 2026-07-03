//! Minimal Wikimedia OAuth2 authorization-code client.
//!
//! OAuth2 uses a browser redirect rather than the OAuth1 out-of-band verifier used elsewhere in
//! the bot. After the callback, the bot stores encrypted access/refresh tokens and uses bearer
//! authentication for Commons API requests.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use url::Url;

/// OAuth2 endpoints on the central wiki.
#[derive(Clone, Debug)]
pub struct OAuth2Endpoints {
    /// User-facing authorization endpoint.
    pub authorize: String,
    /// Token exchange/refresh endpoint.
    pub token: String,
    /// User profile endpoint.
    pub profile: String,
}

impl OAuth2Endpoints {
    /// Endpoints for Wikimedia projects (OAuth is centralised on meta.wikimedia.org).
    pub fn wikimedia() -> Self {
        let base = "https://meta.wikimedia.org/w/rest.php/oauth2";
        Self {
            authorize: format!("{base}/authorize"),
            token: format!("{base}/access_token"),
            profile: format!("{base}/resource/profile"),
        }
    }
}

/// OAuth2 client credentials from `Special:OAuthConsumerRegistration`.
#[derive(Clone)]
pub struct OAuth2Consumer {
    /// Client id / consumer key.
    pub client_id: String,
    /// Client secret.
    pub client_secret: String,
    /// Redirect URL registered for this consumer.
    pub redirect_url: String,
}

/// Stored OAuth2 token set.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct OAuth2Token {
    /// Bearer access token.
    pub access_token: String,
    /// Refresh token used to rotate an expired access token.
    pub refresh_token: String,
    /// Unix timestamp when the access token expires.
    pub expires_at: i64,
}

/// OAuth2 client for Wikimedia's authorization-code flow.
#[derive(Clone)]
pub struct OAuth2Client {
    http: reqwest::Client,
    endpoints: OAuth2Endpoints,
    consumer: OAuth2Consumer,
}

impl OAuth2Client {
    /// Builds a client from OAuth2 consumer credentials and central-wiki endpoints.
    pub fn new(
        consumer: OAuth2Consumer,
        endpoints: OAuth2Endpoints,
        user_agent: &str,
    ) -> Result<Self> {
        let http = reqwest::Client::builder().user_agent(user_agent).build()?;
        Ok(Self {
            http,
            endpoints,
            consumer,
        })
    }

    /// Returns the URL a user opens to authorize this bot.
    pub fn authorize_url(&self, state: &str) -> Result<String> {
        let mut url =
            Url::parse(&self.endpoints.authorize).context("invalid OAuth2 authorize URL")?;
        url.query_pairs_mut()
            .append_pair("response_type", "code")
            .append_pair("client_id", &self.consumer.client_id)
            .append_pair("redirect_uri", &self.consumer.redirect_url)
            .append_pair("state", state);
        Ok(url.to_string())
    }

    /// Exchanges a browser callback code for access and refresh tokens.
    pub async fn exchange_code(&self, code: &str) -> Result<OAuth2Token> {
        let response = self
            .token_request(&[
                ("grant_type", "authorization_code"),
                ("code", code),
                ("client_id", &self.consumer.client_id),
                ("client_secret", &self.consumer.client_secret),
                ("redirect_uri", &self.consumer.redirect_url),
            ])
            .await?;
        response.into_token(None)
    }

    /// Refreshes an expired access token, keeping the previous refresh token if not rotated.
    pub async fn refresh(&self, refresh_token: &str) -> Result<OAuth2Token> {
        let response = self
            .token_request(&[
                ("grant_type", "refresh_token"),
                ("refresh_token", refresh_token),
                ("client_id", &self.consumer.client_id),
                ("client_secret", &self.consumer.client_secret),
            ])
            .await?;
        response.into_token(Some(refresh_token))
    }

    /// Reads the Commons/Wikimedia username attached to an OAuth2 bearer token.
    pub async fn username(&self, access_token: &str) -> Result<String> {
        let profile: OAuth2Profile = self
            .http
            .get(&self.endpoints.profile)
            .bearer_auth(access_token)
            .send()
            .await?
            .error_for_status()?
            .json()
            .await
            .context("OAuth2 profile response was not valid JSON")?;
        profile
            .username
            .context("OAuth2 profile response is missing username")
    }

    /// Returns the bearer authorization header value for this token.
    pub fn bearer(access_token: &str) -> String {
        format!("Bearer {access_token}")
    }

    /// Sends one OAuth2 token endpoint form request.
    async fn token_request(&self, form: &[(&str, &str)]) -> Result<OAuth2TokenResponse> {
        self.http
            .post(&self.endpoints.token)
            .form(form)
            .send()
            .await?
            .error_for_status()?
            .json()
            .await
            .context("OAuth2 token response was not valid JSON")
    }
}

#[derive(Deserialize)]
struct OAuth2TokenResponse {
    access_token: String,
    refresh_token: Option<String>,
    expires_in: Option<u64>,
}

impl OAuth2TokenResponse {
    /// Converts a token endpoint response into the encrypted profile token model.
    fn into_token(self, previous_refresh_token: Option<&str>) -> Result<OAuth2Token> {
        let refresh_token = self
            .refresh_token
            .or_else(|| previous_refresh_token.map(str::to_string))
            .context("OAuth2 token response is missing refresh_token")?;
        let expires_in = self.expires_in.unwrap_or(3600).min(i64::MAX as u64) as i64;
        Ok(OAuth2Token {
            access_token: self.access_token,
            refresh_token,
            expires_at: now_ts().saturating_add(expires_in),
        })
    }
}

#[derive(Deserialize)]
struct OAuth2Profile {
    username: Option<String>,
}

/// Returns the current UTC Unix timestamp.
fn now_ts() -> i64 {
    time::OffsetDateTime::now_utc().unix_timestamp()
}

#[cfg(test)]
mod tests {
    use super::{OAuth2Consumer, OAuth2Endpoints};

    #[test]
    fn authorize_url_contains_required_parameters() {
        let client = super::OAuth2Client::new(
            OAuth2Consumer {
                client_id: "client".into(),
                client_secret: "secret".into(),
                redirect_url: "https://example.org/oauth2/callback".into(),
            },
            OAuth2Endpoints {
                authorize: "https://meta.wikimedia.org/w/rest.php/oauth2/authorize".into(),
                token: "https://meta.wikimedia.org/w/rest.php/oauth2/access_token".into(),
                profile: "https://meta.wikimedia.org/w/rest.php/oauth2/resource/profile".into(),
            },
            "test",
        )
        .unwrap();

        let url = client.authorize_url("state value").unwrap();
        assert!(url.contains("response_type=code"));
        assert!(url.contains("client_id=client"));
        assert!(url.contains("redirect_uri=https%3A%2F%2Fexample.org%2Foauth2%2Fcallback"));
        assert!(url.contains("state=state+value"));
    }
}
