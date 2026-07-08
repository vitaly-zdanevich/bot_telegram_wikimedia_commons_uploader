use crate::models::{License, UploadProvenance};
use crate::oauth::OAuthClient;
use anyhow::{Context, Result, bail};
use reqwest::{Client, multipart};
use serde_json::{Value, json};
use std::path::PathBuf;

/// Attribution category added to every file uploaded by this bot.
pub const BOT_CATEGORY: &str =
    "Uploaded with Telegram bot @wikimedia_commons_uploader_bot by Vitaly Zdanevich";
/// Maximum filename stem length in bytes, leaving room for `File:` and the extension.
const MAX_FILENAME_STEM_BYTES: usize = 200;
/// Wikidata Action API endpoint used for exact camera-model lookup.
const WIKIDATA_API_URL: &str = "https://www.wikidata.org/w/api.php";
/// SDC `source of file` value: original creation by uploader.
const Q_ORIGINAL_CREATION_BY_UPLOADER: &str = "Q66458942";
/// SDC `copyright license` value: Creative Commons Attribution 4.0 International.
const Q_CC_BY_4_0: &str = "Q20007257";
/// SDC `copyright license` value: Creative Commons Attribution-ShareAlike 4.0 International.
const Q_CC_BY_SA_4_0: &str = "Q18199165";
/// SDC `copyright license` value: Creative Commons CC0.
const Q_CC0: &str = "Q6938433";
/// SDC `copyright status` value: copyrighted.
const Q_COPYRIGHTED: &str = "Q50423863";
/// SDC `copyright status` value: public domain.
const Q_PUBLIC_DOMAIN: &str = "Q19652";
/// Wikibase globe URI for Earth coordinates.
const EARTH_GLOBE: &str = "http://www.wikidata.org/entity/Q2";
/// Edit summary used for best-effort MediaInfo updates.
const SDC_EDIT_SUMMARY: &str = "Add upload structured data";

/// Client for the Wikimedia Commons Action API.
///
/// Bot-password uploads use the user's own account; archive uploads can reuse one
/// cookie session across the batch.
#[derive(Clone)]
pub struct CommonsClient {
    api_url: String,
    user_agent: String,
    proxy: Option<String>,
    oauth: Option<OAuthClient>,
    ignore_exists_normalized_warning: bool,
}

/// Result of attempting an upload.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum UploadOutcome {
    /// The file was uploaded; carries the canonical title and file-page URL.
    Success { title: String, url: String },
    /// Commons declined the upload; carries a user-facing explanation.
    Failed {
        message: String,
        /// Whether `message` is trusted Telegram HTML built by this module.
        html: bool,
    },
}

/// How an upload authenticates to Commons.
#[derive(Clone)]
pub enum UploadAuth {
    /// Bot-password login (`User@label` + token) over a cookie session.
    BotPassword {
        /// Bot-password username (`Account@label`).
        username: String,
        /// Bot-password token.
        password: String,
    },
    /// OAuth 1.0a access token + secret, signed per request.
    OAuth1 {
        /// OAuth access token.
        token: String,
        /// OAuth access token secret.
        secret: String,
    },
    /// OAuth2 bearer access token.
    OAuth2 {
        /// Bearer access token.
        access_token: String,
    },
}

/// A logged-in bot-password Commons session.
pub struct CommonsBotPasswordSession {
    client: Client,
}

/// One upload request.
pub struct UploadRequest {
    /// How to authenticate the upload.
    pub auth: UploadAuth,
    /// Target filename including extension, without the `File:` prefix.
    pub filename: String,
    /// File content to upload.
    pub data: UploadData,
    /// Wikitext description page contents.
    pub wikitext: String,
    /// Upload comment / edit summary.
    pub comment: String,
    /// Structured Data on Commons statements to add after a successful upload.
    pub structured_data: Option<StructuredDataRequest>,
}

/// Structured Data on Commons statements derived from the upload metadata.
#[derive(Clone, Debug, PartialEq)]
pub struct StructuredDataRequest {
    /// Whether the upload source is the uploader's own work.
    pub source_is_own_work: bool,
    /// Commons username of the uploading account.
    pub author_username: String,
    /// Optional author override from a caption/default; skips uploader-as-creator SDC.
    pub author_override: Option<String>,
    /// Chosen built-in license.
    pub license: License,
    /// Optional custom license template/key overriding `license`.
    pub license_override: Option<String>,
    /// Optional GPS latitude in decimal degrees.
    pub latitude: Option<f64>,
    /// Optional GPS longitude in decimal degrees.
    pub longitude: Option<f64>,
    /// Optional EXIF camera model string.
    pub camera_model: Option<String>,
}

/// File content for a Commons upload.
pub enum UploadData {
    /// Small or converted upload held in memory.
    Bytes(Vec<u8>),
    /// Large local file streamed from disk.
    File { path: PathBuf, len: u64 },
}

/// Authentication header strategy for regular form-encoded Action API edits.
#[derive(Clone, Copy)]
enum ApiAuthorization<'a> {
    /// Cookie-authenticated bot-password session.
    None,
    /// Prebuilt bearer-style header (OAuth2).
    Header(&'a str),
    /// OAuth1 must sign form body parameters for non-multipart requests.
    OAuth1 {
        oauth: &'a OAuthClient,
        token: &'a str,
        secret: &'a str,
    },
}

impl UploadData {
    /// Returns the upload size in bytes.
    pub fn len(&self) -> u64 {
        match self {
            Self::Bytes(bytes) => bytes.len() as u64,
            Self::File { len, .. } => *len,
        }
    }

    /// Returns true when the upload has no bytes.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

impl CommonsClient {
    /// Creates a Commons client for an API endpoint and User-Agent.
    ///
    /// `oauth` enables uploading via OAuth 1.0a (when a consumer is configured);
    /// bot-password uploads work regardless.
    pub fn new(
        api_url: impl Into<String>,
        user_agent: impl Into<String>,
        proxy: Option<String>,
        oauth: Option<OAuthClient>,
        ignore_exists_normalized_warning: bool,
    ) -> Self {
        Self {
            api_url: api_url.into(),
            user_agent: user_agent.into(),
            proxy,
            oauth,
            ignore_exists_normalized_warning,
        }
    }

    /// Builds a fresh cookie-aware client so one login session spans the request chain.
    ///
    /// Routes through `COMMONS_PROXY` when set, so uploads can use a non-blocked IP.
    fn session_client(&self) -> Result<Client> {
        let mut builder = Client::builder()
            .user_agent(&self.user_agent)
            .cookie_store(true);
        if let Some(proxy) = &self.proxy {
            builder =
                builder.proxy(reqwest::Proxy::all(proxy).context("invalid COMMONS_PROXY URL")?);
        }
        builder
            .build()
            .context("failed to build Commons HTTP client")
    }

    /// Validates a bot password by performing a real login (used during onboarding).
    pub async fn validate_credentials(&self, username: &str, password: &str) -> Result<()> {
        let client = self.session_client()?;
        self.login(&client, username, password).await
    }

    /// Logs in, fetches a CSRF token, and uploads the file.
    ///
    /// Transport failures return `Err`; anything Commons itself rejects (bad credentials,
    /// existing filename, duplicate, abuse filter, …) returns `Ok(UploadOutcome::Failed)`
    /// with a message meant to be shown to the user.
    pub async fn upload(&self, request: &UploadRequest) -> Result<UploadOutcome> {
        match &request.auth {
            UploadAuth::BotPassword { username, password } => {
                self.upload_bot_password(request, username, password).await
            }
            UploadAuth::OAuth1 { token, secret } => {
                self.upload_oauth1(request, token, secret).await
            }
            UploadAuth::OAuth2 { access_token } => self.upload_oauth2(request, access_token).await,
        }
    }

    /// Uploads using a bot-password login over a cookie session.
    async fn upload_bot_password(
        &self,
        request: &UploadRequest,
        username: &str,
        password: &str,
    ) -> Result<UploadOutcome> {
        match self.login_bot_password_client(username, password).await {
            Ok(client) => self.upload_logged_in(&client, request).await,
            Err(error) => Ok(UploadOutcome::Failed {
                message: format!("{error}"),
                html: false,
            }),
        }
    }

    /// Opens one logged-in bot-password session for repeated uploads.
    pub async fn bot_password_session(
        &self,
        username: &str,
        password: &str,
    ) -> Result<CommonsBotPasswordSession> {
        self.login_bot_password_client(username, password)
            .await
            .map(|client| CommonsBotPasswordSession { client })
    }

    async fn login_bot_password_client(&self, username: &str, password: &str) -> Result<Client> {
        let mut attempt = 1;
        loop {
            let client = self.session_client()?;
            if let Err(error) = self.login(&client, username, password).await {
                let message = format!("{error}");
                if attempt == 1 && is_transient_login_failure(&message) {
                    tracing::warn!(
                        username,
                        error = %message,
                        "Commons bot-password login session expired; retrying once"
                    );
                    attempt += 1;
                    continue;
                }
                bail!("{message}");
            }
            return Ok(client);
        }
    }

    /// Uploads through an already logged-in bot-password session.
    pub async fn upload_with_bot_password_session(
        &self,
        session: &CommonsBotPasswordSession,
        request: &UploadRequest,
    ) -> Result<UploadOutcome> {
        self.upload_logged_in(&session.client, request).await
    }

    async fn upload_logged_in(
        &self,
        client: &Client,
        request: &UploadRequest,
    ) -> Result<UploadOutcome> {
        let csrf_token = self.fetch_token(client, "csrf").await?;
        let response = self
            .post_upload_form(client, request, &csrf_token, None, false)
            .await?;
        if self.should_retry_exists_normalized_warning(&response, &request.filename) {
            let response = self
                .post_upload_form(client, request, &csrf_token, None, true)
                .await?;
            let outcome = interpret_upload_response(&response, &request.filename);
            self.add_structured_data_best_effort(
                client,
                request,
                &csrf_token,
                ApiAuthorization::None,
                &outcome,
            )
            .await;
            return Ok(outcome);
        }
        let outcome = interpret_upload_response(&response, &request.filename);
        self.add_structured_data_best_effort(
            client,
            request,
            &csrf_token,
            ApiAuthorization::None,
            &outcome,
        )
        .await;
        Ok(outcome)
    }

    /// Uploads using an OAuth 1.0a access token, signing each request.
    async fn upload_oauth1(
        &self,
        request: &UploadRequest,
        token: &str,
        secret: &str,
    ) -> Result<UploadOutcome> {
        let oauth = self
            .oauth
            .as_ref()
            .context("OAuth is not configured on this bot")?;
        let client = self.session_client()?;

        // CSRF token, fetched as the OAuth-identified user.
        let csrf_url = format!(
            "{}?action=query&meta=tokens&type=csrf&format=json",
            self.api_url
        );
        let csrf_auth = oauth.api_authorization("GET", &csrf_url, token, secret, &[]);
        let csrf_response: Value = client
            .get(&csrf_url)
            .header("Authorization", csrf_auth)
            .send()
            .await?
            .error_for_status()?
            .json()
            .await
            .context("Commons token response was not valid JSON")?;
        let csrf_token = csrf_response
            .get("query")
            .and_then(|query| query.get("tokens"))
            .and_then(|tokens| tokens.get("csrftoken"))
            .and_then(Value::as_str)
            .context("Commons response is missing csrftoken")?
            .to_string();

        // Multipart upload, signed (OAuth does not sign multipart bodies).
        let upload_auth = oauth.api_authorization("POST", &self.api_url, token, secret, &[]);
        let response = self
            .post_upload_form(&client, request, &csrf_token, Some(&upload_auth), false)
            .await?;
        if self.should_retry_exists_normalized_warning(&response, &request.filename) {
            let retry_auth = oauth.api_authorization("POST", &self.api_url, token, secret, &[]);
            let response = self
                .post_upload_form(&client, request, &csrf_token, Some(&retry_auth), true)
                .await?;
            let outcome = interpret_upload_response(&response, &request.filename);
            self.add_structured_data_best_effort(
                &client,
                request,
                &csrf_token,
                ApiAuthorization::OAuth1 {
                    oauth,
                    token,
                    secret,
                },
                &outcome,
            )
            .await;
            return Ok(outcome);
        }
        let outcome = interpret_upload_response(&response, &request.filename);
        self.add_structured_data_best_effort(
            &client,
            request,
            &csrf_token,
            ApiAuthorization::OAuth1 {
                oauth,
                token,
                secret,
            },
            &outcome,
        )
        .await;
        Ok(outcome)
    }

    /// Uploads using an OAuth2 bearer access token.
    async fn upload_oauth2(
        &self,
        request: &UploadRequest,
        access_token: &str,
    ) -> Result<UploadOutcome> {
        let client = self.session_client()?;
        let authorization = format!("Bearer {access_token}");
        let csrf_url = format!(
            "{}?action=query&meta=tokens&type=csrf&format=json",
            self.api_url
        );
        let csrf_response: Value = client
            .get(&csrf_url)
            .header("Authorization", &authorization)
            .send()
            .await?
            .error_for_status()?
            .json()
            .await
            .context("Commons token response was not valid JSON")?;
        let csrf_token = csrf_response
            .get("query")
            .and_then(|query| query.get("tokens"))
            .and_then(|tokens| tokens.get("csrftoken"))
            .and_then(Value::as_str)
            .context("Commons response is missing csrftoken")?
            .to_string();

        let response = self
            .post_upload_form(&client, request, &csrf_token, Some(&authorization), false)
            .await?;
        if self.should_retry_exists_normalized_warning(&response, &request.filename) {
            let response = self
                .post_upload_form(&client, request, &csrf_token, Some(&authorization), true)
                .await?;
            let outcome = interpret_upload_response(&response, &request.filename);
            self.add_structured_data_best_effort(
                &client,
                request,
                &csrf_token,
                ApiAuthorization::Header(&authorization),
                &outcome,
            )
            .await;
            return Ok(outcome);
        }
        let outcome = interpret_upload_response(&response, &request.filename);
        self.add_structured_data_best_effort(
            &client,
            request,
            &csrf_token,
            ApiAuthorization::Header(&authorization),
            &outcome,
        )
        .await;
        Ok(outcome)
    }

    /// Posts one multipart upload form, optionally signed for OAuth and with warning override.
    async fn post_upload_form(
        &self,
        client: &Client,
        request: &UploadRequest,
        csrf_token: &str,
        authorization: Option<&str>,
        ignore_warnings: bool,
    ) -> Result<Value> {
        let mut builder = client.post(&self.api_url);
        if let Some(authorization) = authorization {
            builder = builder.header("Authorization", authorization);
        }
        builder
            .multipart(build_upload_form(request, csrf_token, ignore_warnings).await?)
            .send()
            .await?
            .error_for_status()?
            .json()
            .await
            .context("Commons upload response was not valid JSON")
    }

    /// Adds SDC after a successful upload; logs failures because the file upload already worked.
    async fn add_structured_data_best_effort(
        &self,
        client: &Client,
        request: &UploadRequest,
        csrf_token: &str,
        authorization: ApiAuthorization<'_>,
        outcome: &UploadOutcome,
    ) {
        let Some(structured_data) = &request.structured_data else {
            return;
        };
        let UploadOutcome::Success { title, .. } = outcome else {
            return;
        };
        if let Err(error) = self
            .add_structured_data(client, title, structured_data, csrf_token, authorization)
            .await
        {
            tracing::warn!(
                target_filename = title,
                error = %format!("{error:#}"),
                "failed to add Structured Data on Commons"
            );
        }
    }

    /// Adds upload-derived Structured Data on Commons in one MediaInfo edit.
    async fn add_structured_data(
        &self,
        client: &Client,
        title: &str,
        structured_data: &StructuredDataRequest,
        csrf_token: &str,
        authorization: ApiAuthorization<'_>,
    ) -> Result<()> {
        let claims = self.structured_data_claims(client, structured_data).await;
        if claims.is_empty() {
            return Ok(());
        }
        let file_title = file_title(title);
        let entity_id = self.file_mediainfo_id(client, &file_title).await?;
        let data = json!({ "claims": claims }).to_string();
        let response = self
            .post_wbeditentity(
                client,
                &[
                    ("action", "wbeditentity"),
                    ("id", entity_id.as_str()),
                    ("data", data.as_str()),
                    ("summary", SDC_EDIT_SUMMARY),
                    ("token", csrf_token),
                    ("format", "json"),
                ],
                authorization,
            )
            .await?;
        if api_error_code(&response) == Some("no-such-entity") {
            let response = self
                .post_wbeditentity(
                    client,
                    &[
                        ("action", "wbeditentity"),
                        ("new", "mediainfo"),
                        ("site", "commonswiki"),
                        ("title", file_title.as_str()),
                        ("data", data.as_str()),
                        ("summary", SDC_EDIT_SUMMARY),
                        ("token", csrf_token),
                        ("format", "json"),
                    ],
                    authorization,
                )
                .await?;
            ensure_api_success(&response, "Commons SDC create response")?;
            return Ok(());
        }
        ensure_api_success(&response, "Commons SDC edit response")
    }

    /// Builds SDC statements that can be inferred without guessing.
    async fn structured_data_claims(
        &self,
        client: &Client,
        structured_data: &StructuredDataRequest,
    ) -> Vec<Value> {
        let mut claims = Vec::new();
        if structured_data.source_is_own_work {
            claims.push(item_statement("P7482", Q_ORIGINAL_CREATION_BY_UPLOADER));
        }
        if let Some(license) = resolved_structured_data_license(
            structured_data.license,
            structured_data.license_override.as_deref(),
        ) {
            if let Some(qid) = license_qid(license) {
                claims.push(item_statement("P275", qid));
            }
            claims.push(item_statement("P6216", copyright_status_qid(license)));
        }
        if structured_data.source_is_own_work
            && structured_data.author_override.is_none()
            && let Some(qid) = self
                .commons_user_wikidata_item(client, &structured_data.author_username)
                .await
        {
            claims.push(item_statement("P170", &qid));
        }
        if let (Some(latitude), Some(longitude)) =
            (structured_data.latitude, structured_data.longitude)
            && valid_coordinates(latitude, longitude)
        {
            claims.push(coordinate_statement("P1259", latitude, longitude));
        }
        if let Some(camera_model) = structured_data
            .camera_model
            .as_deref()
            .map(str::trim)
            .filter(|model| !model.is_empty())
            && let Some(qid) = self.wikidata_exact_item(client, camera_model).await
        {
            claims.push(item_statement("P4082", &qid));
        }
        claims
    }

    /// Returns the Commons MediaInfo entity ID (`M<pageid>`) for an uploaded file.
    async fn file_mediainfo_id(&self, client: &Client, file_title: &str) -> Result<String> {
        let response: Value = client
            .get(&self.api_url)
            .query(&[
                ("action", "query"),
                ("prop", "info"),
                ("titles", file_title),
                ("format", "json"),
                ("formatversion", "2"),
            ])
            .send()
            .await?
            .error_for_status()?
            .json()
            .await
            .context("Commons page-info response was not valid JSON")?;
        let page_id = response
            .get("query")
            .and_then(|query| query.get("pages"))
            .and_then(Value::as_array)
            .and_then(|pages| pages.first())
            .and_then(|page| page.get("pageid"))
            .and_then(Value::as_i64)
            .with_context(|| {
                format!("Commons page-info response is missing pageid for {file_title}")
            })?;
        Ok(format!("M{page_id}"))
    }

    /// Looks up the Wikidata item linked from `User:<account>`, if the user page has one.
    async fn commons_user_wikidata_item(&self, client: &Client, username: &str) -> Option<String> {
        let account = account_name(username);
        let title = format!("User:{account}");
        let response: Value = match client
            .get(&self.api_url)
            .query(&[
                ("action", "query"),
                ("prop", "pageprops"),
                ("titles", &title),
                ("format", "json"),
                ("formatversion", "2"),
            ])
            .send()
            .await
            .and_then(reqwest::Response::error_for_status)
        {
            Ok(response) => match response.json().await {
                Ok(value) => value,
                Err(error) => {
                    tracing::warn!(
                        username = account,
                        error = %error,
                        "Commons user-page Wikidata lookup returned invalid JSON"
                    );
                    return None;
                }
            },
            Err(error) => {
                tracing::warn!(
                    username = account,
                    error = %error,
                    "Commons user-page Wikidata lookup failed"
                );
                return None;
            }
        };
        response
            .get("query")
            .and_then(|query| query.get("pages"))
            .and_then(Value::as_array)
            .and_then(|pages| pages.first())
            .and_then(|page| page.get("pageprops"))
            .and_then(|props| props.get("wikibase_item"))
            .and_then(Value::as_str)
            .map(str::to_string)
    }

    /// Searches Wikidata and returns a QID only for an exact label or alias match.
    async fn wikidata_exact_item(&self, client: &Client, search: &str) -> Option<String> {
        let response: Value = match client
            .get(WIKIDATA_API_URL)
            .query(&[
                ("action", "wbsearchentities"),
                ("search", search),
                ("language", "en"),
                ("type", "item"),
                ("limit", "5"),
                ("format", "json"),
            ])
            .send()
            .await
            .and_then(reqwest::Response::error_for_status)
        {
            Ok(response) => match response.json().await {
                Ok(value) => value,
                Err(error) => {
                    tracing::warn!(
                        search,
                        error = %error,
                        "Wikidata exact-item lookup returned invalid JSON"
                    );
                    return None;
                }
            },
            Err(error) => {
                tracing::warn!(
                    search,
                    error = %error,
                    "Wikidata exact-item lookup failed"
                );
                return None;
            }
        };
        let needle = search.trim().to_ascii_lowercase();
        response
            .get("search")
            .and_then(Value::as_array)?
            .iter()
            .find(|result| {
                result
                    .get("label")
                    .and_then(Value::as_str)
                    .is_some_and(|label| label.eq_ignore_ascii_case(&needle))
                    || result
                        .get("aliases")
                        .and_then(Value::as_array)
                        .is_some_and(|aliases| {
                            aliases
                                .iter()
                                .filter_map(Value::as_str)
                                .any(|alias| alias.trim().eq_ignore_ascii_case(needle.as_str()))
                        })
            })
            .and_then(|result| result.get("id"))
            .and_then(Value::as_str)
            .map(str::to_string)
    }

    /// Posts a `wbeditentity` form, optionally authenticated by OAuth.
    async fn post_wbeditentity(
        &self,
        client: &Client,
        form: &[(&str, &str)],
        authorization: ApiAuthorization<'_>,
    ) -> Result<Value> {
        let mut builder = client.post(&self.api_url);
        match authorization {
            ApiAuthorization::None => {}
            ApiAuthorization::Header(authorization) => {
                builder = builder.header("Authorization", authorization);
            }
            ApiAuthorization::OAuth1 {
                oauth,
                token,
                secret,
            } => {
                let body_params = form
                    .iter()
                    .map(|(key, value)| ((*key).to_string(), (*value).to_string()))
                    .collect::<Vec<_>>();
                let authorization =
                    oauth.api_authorization("POST", &self.api_url, token, secret, &body_params);
                builder = builder.header("Authorization", authorization);
            }
        }
        builder
            .form(form)
            .send()
            .await?
            .error_for_status()?
            .json()
            .await
            .context("Commons SDC response was not valid JSON")
    }

    /// Returns true when a retry with `ignorewarnings=1` is allowed for this warning only.
    fn should_retry_exists_normalized_warning(&self, response: &Value, filename: &str) -> bool {
        if !self.ignore_exists_normalized_warning {
            return false;
        }
        if !is_only_exists_normalized_warning(response) {
            return false;
        }
        tracing::warn!(
            target_filename = filename,
            "Commons returned only exists-normalized; retrying upload with ignorewarnings"
        );
        true
    }

    /// Returns the Commons username behind an OAuth access token (for author attribution).
    pub async fn oauth_username(&self, token: &str, secret: &str) -> Result<String> {
        let oauth = self
            .oauth
            .as_ref()
            .context("OAuth is not configured on this bot")?;
        let client = self.session_client()?;
        let url = format!("{}?action=query&meta=userinfo&format=json", self.api_url);
        let auth = oauth.api_authorization("GET", &url, token, secret, &[]);
        let response: Value = client
            .get(&url)
            .header("Authorization", auth)
            .send()
            .await?
            .error_for_status()?
            .json()
            .await
            .context("Commons userinfo response was not valid JSON")?;
        response
            .get("query")
            .and_then(|query| query.get("userinfo"))
            .and_then(|info| info.get("name"))
            .and_then(Value::as_str)
            .map(str::to_string)
            .context("Commons userinfo is missing the user name")
    }

    /// Returns existing Commons file titles whose content SHA-1 matches (duplicate check).
    pub async fn find_by_sha1(&self, sha1: &str) -> Result<Vec<String>> {
        let client = self.session_client()?;
        let response: Value = client
            .get(&self.api_url)
            .query(&[
                ("action", "query"),
                ("list", "allimages"),
                ("aisha1", sha1),
                ("ailimit", "10"),
                ("format", "json"),
                ("formatversion", "2"),
            ])
            .send()
            .await?
            .error_for_status()?
            .json()
            .await
            .context("Commons allimages response was not valid JSON")?;
        Ok(response
            .get("query")
            .and_then(|query| query.get("allimages"))
            .and_then(Value::as_array)
            .map(|images| {
                images
                    .iter()
                    .filter_map(|image| {
                        image
                            .get("title")
                            .and_then(Value::as_str)
                            .map(str::to_string)
                    })
                    .collect()
            })
            .unwrap_or_default())
    }

    /// Returns the subset of category names that do not yet exist on Commons.
    pub async fn missing_categories(&self, categories: &[String]) -> Result<Vec<String>> {
        if categories.is_empty() {
            return Ok(Vec::new());
        }
        let client = self.session_client()?;
        let titles = categories
            .iter()
            .map(|category| format!("Category:{category}"))
            .collect::<Vec<_>>()
            .join("|");
        let response: Value = client
            .get(&self.api_url)
            .query(&[
                ("action", "query"),
                ("prop", "info"),
                ("titles", &titles),
                ("format", "json"),
                ("formatversion", "2"),
            ])
            .send()
            .await?
            .error_for_status()?
            .json()
            .await
            .context("Commons category info response was not valid JSON")?;
        Ok(response
            .get("query")
            .and_then(|query| query.get("pages"))
            .and_then(Value::as_array)
            .map(|pages| {
                pages
                    .iter()
                    .filter(|page| page.get("missing").is_some())
                    .filter_map(|page| page.get("title").and_then(Value::as_str))
                    .map(|title| title.strip_prefix("Category:").unwrap_or(title).to_string())
                    .collect()
            })
            .unwrap_or_default())
    }

    /// Counts public files uploaded by a Commons user.
    pub async fn upload_count_by_user(&self, username: &str) -> Result<u64> {
        let client = self.session_client()?;
        let mut count = 0_u64;
        let mut continue_token: Option<String> = None;
        loop {
            let mut query = vec![
                ("action".to_string(), "query".to_string()),
                ("list".to_string(), "allimages".to_string()),
                ("aiuser".to_string(), username.to_string()),
                ("ailimit".to_string(), "500".to_string()),
                ("format".to_string(), "json".to_string()),
                ("formatversion".to_string(), "2".to_string()),
            ];
            if let Some(token) = &continue_token {
                query.push(("aicontinue".to_string(), token.clone()));
            }
            let response: Value = client
                .get(&self.api_url)
                .query(&query)
                .send()
                .await?
                .error_for_status()?
                .json()
                .await
                .context("Commons allimages response was not valid JSON")?;
            count = count.saturating_add(
                response
                    .get("query")
                    .and_then(|query| query.get("allimages"))
                    .and_then(Value::as_array)
                    .map_or(0, |images| images.len() as u64),
            );
            continue_token = response
                .get("continue")
                .and_then(|value| value.get("aicontinue"))
                .and_then(Value::as_str)
                .map(str::to_string);
            if continue_token.is_none() {
                return Ok(count);
            }
        }
    }

    /// Logs in with a bot password (legacy `action=login`, which bot passwords use).
    async fn login(&self, client: &Client, username: &str, password: &str) -> Result<()> {
        let login_token = self.fetch_token(client, "login").await?;
        let response: Value = client
            .post(&self.api_url)
            .form(&[
                ("action", "login"),
                ("lgname", username),
                ("lgpassword", password),
                ("lgtoken", &login_token),
                ("format", "json"),
            ])
            .send()
            .await?
            .error_for_status()?
            .json()
            .await
            .context("Commons login response was not valid JSON")?;
        let login = response.get("login");
        let result = login
            .and_then(|value| value.get("result"))
            .and_then(Value::as_str)
            .unwrap_or_default();
        if result != "Success" {
            bail!(
                "{}",
                login_failure_message(&login_failure_reason(login, result))
            );
        }
        Ok(())
    }

    /// Fetches a login or CSRF token via `action=query&meta=tokens`.
    async fn fetch_token(&self, client: &Client, kind: &str) -> Result<String> {
        let response: Value = client
            .get(&self.api_url)
            .query(&[
                ("action", "query"),
                ("meta", "tokens"),
                ("type", kind),
                ("format", "json"),
            ])
            .send()
            .await?
            .error_for_status()?
            .json()
            .await
            .context("Commons token response was not valid JSON")?;
        let field = format!("{kind}token");
        response
            .get("query")
            .and_then(|query| query.get("tokens"))
            .and_then(|tokens| tokens.get(&field))
            .and_then(Value::as_str)
            .map(str::to_string)
            .with_context(|| format!("Commons response is missing {field}"))
    }
}

/// Extracts a Commons API error code from a JSON response.
fn api_error_code(response: &Value) -> Option<&str> {
    response
        .get("error")
        .and_then(|error| error.get("code"))
        .and_then(Value::as_str)
}

/// Converts an unexpected API error response into a regular `anyhow` error.
fn ensure_api_success(response: &Value, context: &str) -> Result<()> {
    if let Some(error) = response.get("error") {
        let code = error.get("code").and_then(Value::as_str).unwrap_or("error");
        let info = error.get("info").and_then(Value::as_str).unwrap_or("");
        bail!("{context}: {code}: {info}");
    }
    Ok(())
}

/// Extracts a human-readable reason from a failed login response.
fn login_failure_reason(login: Option<&Value>, fallback: &str) -> String {
    let Some(login) = login else {
        return fallback.to_string();
    };
    if let Some(reason) = login.get("reason") {
        if let Some(text) = reason.as_str() {
            return text.to_string();
        }
        if let Some(text) = reason.get("text").and_then(Value::as_str) {
            return text.to_string();
        }
    }
    if fallback.is_empty() {
        "incorrect bot-password username or token".to_string()
    } else {
        fallback.to_string()
    }
}

fn is_transient_login_failure(message: &str) -> bool {
    let lower = message.to_ascii_lowercase();
    lower.contains("unable to continue login")
        || lower.contains("session most likely timed out")
        || lower.contains("session timed out")
}

/// Resolves a built-in or known custom license override for SDC mapping.
fn resolved_structured_data_license(
    license: License,
    license_override: Option<&str>,
) -> Option<License> {
    let Some(override_value) = license_override
        .map(str::trim)
        .filter(|value| !value.is_empty())
    else {
        return Some(license);
    };
    License::parse(&license_override_key(override_value))
}

/// Normalizes a user-provided license key/template enough to match known built-in licenses.
fn license_override_key(value: &str) -> String {
    let mut value = value.trim();
    if let Some(inner) = value
        .strip_prefix("{{")
        .and_then(|inner| inner.strip_suffix("}}"))
    {
        value = inner.trim();
    }
    if let Some(rest) = value.strip_prefix("self|") {
        value = rest.trim();
    }
    value.split('|').next().unwrap_or(value).trim().to_string()
}

/// Maps built-in licenses to Wikidata license items for SDC P275.
fn license_qid(license: License) -> Option<&'static str> {
    match license {
        License::CcBy40 => Some(Q_CC_BY_4_0),
        License::CcBySa40 => Some(Q_CC_BY_SA_4_0),
        License::Cc0 => Some(Q_CC0),
        License::PdRussiaExpired | License::PdRussia | License::PdRusEmpire => None,
    }
}

/// Maps built-in licenses to Wikidata copyright-status items for SDC P6216.
fn copyright_status_qid(license: License) -> &'static str {
    match license {
        License::CcBy40 | License::CcBySa40 => Q_COPYRIGHTED,
        License::Cc0 | License::PdRussiaExpired | License::PdRussia | License::PdRusEmpire => {
            Q_PUBLIC_DOMAIN
        }
    }
}

/// Builds the canonical file page title expected by `wbeditentity`.
fn file_title(title: &str) -> String {
    if title.starts_with("File:") {
        title.to_string()
    } else {
        format!("File:{title}")
    }
}

/// Returns true when decimal coordinates are inside real latitude/longitude ranges.
fn valid_coordinates(latitude: f64, longitude: f64) -> bool {
    latitude.is_finite()
        && longitude.is_finite()
        && (-90.0..=90.0).contains(&latitude)
        && (-180.0..=180.0).contains(&longitude)
}

/// Builds one normal-rank Wikibase item statement.
fn item_statement(property: &str, qid: &str) -> Value {
    let numeric_id = qid
        .strip_prefix('Q')
        .and_then(|id| id.parse::<u64>().ok())
        .expect("hard-coded Wikidata item IDs must be Q-prefixed numbers");
    json!({
        "mainsnak": {
            "snaktype": "value",
            "property": property,
            "datavalue": {
                "value": {
                    "entity-type": "item",
                    "numeric-id": numeric_id,
                    "id": qid,
                },
                "type": "wikibase-entityid",
            },
        },
        "type": "statement",
        "rank": "normal",
    })
}

/// Builds one normal-rank globe-coordinate statement.
fn coordinate_statement(property: &str, latitude: f64, longitude: f64) -> Value {
    json!({
        "mainsnak": {
            "snaktype": "value",
            "property": property,
            "datavalue": {
                "value": {
                    "latitude": latitude,
                    "longitude": longitude,
                    "precision": 0.000001,
                    "globe": EARTH_GLOBE,
                },
                "type": "globecoordinate",
            },
        },
        "type": "statement",
        "rank": "normal",
    })
}

/// Builds the user-facing message for a failed bot-password login, including recovery steps.
fn login_failure_message(reason: &str) -> String {
    if is_transient_login_failure(reason) {
        return format!(
            "❌ Couldn't log in to Commons because Commons expired the login session while \
             connecting: {reason}.\n\nThis is usually transient. Please retry the upload; if it \
             repeats, run /start to reconnect your account."
        );
    }
    format!(
        "❌ Couldn't log in to Commons: {reason}.\n\nYour bot password may be wrong, expired, or \
         revoked. Create a fresh one at https://commons.wikimedia.org/wiki/Special:BotPasswords \
         (tick \"Upload new files\" and \"Create, edit, and move pages\"), then run /start to \
         reconnect your account."
    )
}

/// Builds the multipart form for an `action=upload` request.
async fn build_upload_form(
    request: &UploadRequest,
    csrf_token: &str,
    ignore_warnings: bool,
) -> Result<multipart::Form> {
    let file_part = match &request.data {
        UploadData::Bytes(bytes) => {
            multipart::Part::bytes(bytes.clone()).file_name(request.filename.clone())
        }
        UploadData::File { path, .. } => multipart::Part::file(path)
            .await
            .with_context(|| format!("failed to open upload file {}", path.display()))?
            .file_name(request.filename.clone()),
    };
    let mut form = multipart::Form::new()
        .text("action", "upload")
        .text("filename", request.filename.clone())
        .text("comment", request.comment.clone())
        .text("text", request.wikitext.clone())
        .text("token", csrf_token.to_string())
        .text("format", "json");
    if ignore_warnings {
        form = form.text("ignorewarnings", "1");
    }
    Ok(form.part("file", file_part))
}

/// Turns an `action=upload` response into a success or user-facing failure.
fn interpret_upload_response(response: &Value, fallback_title: &str) -> UploadOutcome {
    if let Some(error) = response.get("error") {
        let code = error.get("code").and_then(Value::as_str).unwrap_or("error");
        let info = error.get("info").and_then(Value::as_str).unwrap_or("");
        return UploadOutcome::Failed {
            message: friendly_error(code, info),
            html: false,
        };
    }
    let upload = response.get("upload");
    let result = upload
        .and_then(|value| value.get("result"))
        .and_then(Value::as_str)
        .unwrap_or_default();
    match result {
        "Success" => {
            let title = upload
                .and_then(|value| value.get("filename"))
                .and_then(Value::as_str)
                .unwrap_or(fallback_title)
                .to_string();
            UploadOutcome::Success {
                url: file_page_url(&title),
                title,
            }
        }
        "Warning" => {
            let warnings = upload.and_then(|value| value.get("warnings"));
            tracing::warn!(
                target_filename = fallback_title,
                warnings = %warnings
                    .map(|value| value.to_string())
                    .unwrap_or_else(|| "null".to_string()),
                "Commons refused upload with warnings"
            );
            UploadOutcome::Failed {
                message: describe_warnings(warnings),
                html: true,
            }
        }
        other => UploadOutcome::Failed {
            message: format!("❌ Commons returned an unexpected result: {other}"),
            html: false,
        },
    }
}

/// Builds a user-facing message from `action=upload` warnings.
fn describe_warnings(warnings: Option<&Value>) -> String {
    let Some(map) = warnings.and_then(Value::as_object) else {
        return "❌ Commons reported a warning and did not upload the file.".to_string();
    };
    let mut reasons = Vec::new();
    if let Some(name) = map.get("exists").and_then(Value::as_str) {
        reasons.push(format!(
            "a file named {} already exists",
            warning_file_link(name)
        ));
    }
    if map.contains_key("exists-normalized") {
        let names = warning_string_values(map.get("exists-normalized"));
        if names.is_empty() {
            reasons.push(
                "a file with the same normalized Commons filename already exists".to_string(),
            );
        } else {
            let links = names
                .iter()
                .map(|name| warning_file_link(name))
                .collect::<Vec<_>>()
                .join(", ");
            reasons.push(format!(
                "a file with the same normalized Commons filename already exists: {}",
                links
            ));
        }
    }
    if let Some(duplicates) = map.get("duplicate").and_then(Value::as_array) {
        let names: Vec<&str> = duplicates.iter().filter_map(Value::as_str).collect();
        if !names.is_empty() {
            let links = names
                .iter()
                .map(|name| warning_file_link(name))
                .collect::<Vec<_>>()
                .join(", ");
            reasons.push(format!("it is a duplicate of: {links}"));
        }
    }
    if map.contains_key("was-deleted") {
        reasons.push("a file with this name was previously deleted".to_string());
    }
    if let Some(name) = map.get("duplicate-archive").and_then(Value::as_str) {
        reasons.push(format!(
            "a deleted file {} was a duplicate",
            warning_file_link(name)
        ));
    }
    if map.contains_key("badfilename") {
        reasons.push("the filename is not allowed".to_string());
    }
    if reasons.is_empty() {
        let keys: Vec<&str> = map.keys().map(String::as_str).collect();
        return format!("❌ Commons did not upload the file: {}.", keys.join(", "));
    }
    format!(
        "❌ Commons did not upload the file because {}.",
        reasons.join("; ")
    )
}

/// Detects the only Commons warning this bot may automatically retry with `ignorewarnings=1`.
fn is_only_exists_normalized_warning(response: &Value) -> bool {
    let result = response
        .get("upload")
        .and_then(|value| value.get("result"))
        .and_then(Value::as_str);
    if result != Some("Warning") {
        return false;
    }
    let Some(warnings) = response
        .get("upload")
        .and_then(|value| value.get("warnings"))
        .and_then(Value::as_object)
    else {
        return false;
    };
    warnings.len() == 1 && warnings.contains_key("exists-normalized")
}

/// Extracts displayable string values from the mixed upload-warning JSON shapes.
fn warning_string_values(value: Option<&Value>) -> Vec<String> {
    match value {
        Some(Value::String(value)) => vec![value.to_string()],
        Some(Value::Array(values)) => values
            .iter()
            .filter_map(Value::as_str)
            .map(str::to_string)
            .collect(),
        Some(Value::Object(values)) => values
            .values()
            .filter_map(Value::as_str)
            .map(str::to_string)
            .collect(),
        _ => Vec::new(),
    }
}

/// Builds a safe Telegram HTML link to a Commons file named in an upload warning.
fn warning_file_link(name: &str) -> String {
    let title = name.strip_prefix("File:").unwrap_or(name);
    let url = file_page_url(title);
    format!(
        "<a href=\"{}\">{}</a>",
        html_attribute(&url),
        html_text(name)
    )
}

/// Escapes text for a Telegram HTML text node.
fn html_text(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// Escapes text for a Telegram HTML quoted attribute.
fn html_attribute(text: &str) -> String {
    html_text(text).replace('"', "&quot;")
}

/// Turns a Commons API error code/info into a clear, actionable message for the user.
fn friendly_error(code: &str, info: &str) -> String {
    match code {
        "blocked" | "autoblocked" | "globalblocking-ipblocked"
        | "globalblocking-ipblocked-range" => "❌ Wikimedia refused the upload because the \
             server's IP is in a range Wikimedia blocks (AWS data-centre IPs are blocked as \
             \"open proxy/webhost\").\n\nThis is a limitation of the bot running on AWS — not your \
             account. Please message the author @vitaly_zdanevich (the bot needs a clean upload \
             route).\n\nAdvanced — request an IP-block exemption for your account: \
             https://commons.wikimedia.org/wiki/Commons:IP_block_exemption (for global blocks: \
             https://meta.wikimedia.org/wiki/Steward_requests/Global_permissions )"
            .to_string(),
        "permissiondenied" | "badaccess-groups" | "writeapidenied" | "cantcreate-anon"
        | "cantcreate" => "❌ Your bot password is missing a permission. Open \
             https://commons.wikimedia.org/wiki/Special:BotPasswords , edit your bot password, and \
             tick both \"Upload new files\" and \"Create, edit, and move pages\", then resend."
            .to_string(),
        "ratelimited" => {
            "❌ You're uploading too fast for Commons. Please wait a minute and try again."
                .to_string()
        }
        "mustbeloggedin" | "assertuserfailed" | "badtoken" => "❌ Your Commons session expired — \
             just resend and the bot will log in again. If it keeps failing, run /start to re-enter \
             your bot password."
            .to_string(),
        "abusefilter-disallowed" | "abusefilter-warning" => {
            format!(
                "❌ A Commons edit filter blocked this upload: {}",
                strip_wikitext(info)
            )
        }
        _ => format!(
            "❌ Commons rejected the upload ({code}): {}",
            strip_wikitext(info)
        ),
    }
}

/// Strips the most common wikitext markup so API error text reads cleanly in Telegram.
fn strip_wikitext(text: &str) -> String {
    let mut output = text.replace("'''", "").replace("''", "");
    while let Some(start) = output.find("[[") {
        let Some(rel_end) = output[start..].find("]]") else {
            break;
        };
        let end = start + rel_end;
        let inner = &output[start + 2..end];
        let label = inner.rsplit('|').next().unwrap_or(inner).to_string();
        output.replace_range(start..end + 2, &label);
    }
    output.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Builds a Commons file-page URL from a canonical title.
fn file_page_url(title: &str) -> String {
    let title = title.strip_prefix("File:").unwrap_or(title);
    commons_page_url("File", title)
}

/// Builds a Commons page URL, percent-encoding the wiki title path segment.
fn commons_page_url(namespace: &str, title: &str) -> String {
    let prefix = format!("{namespace}:");
    let title = title.strip_prefix(&prefix).unwrap_or(title);
    let title = title.replace(' ', "_");
    format!(
        "https://commons.wikimedia.org/wiki/{namespace}:{}",
        urlencoding::encode(&title)
    )
}

/// Builds a Commons category-page URL from a category name.
pub fn category_url(name: &str) -> String {
    commons_page_url("Category", name)
}

/// A caption split into a description and category names.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ParsedCaption {
    /// Description text (caption with directive lines removed).
    pub description: String,
    /// Category names (without the `Category:` prefix).
    pub categories: Vec<String>,
    /// Optional `Source:` override (e.g. an external URL).
    pub source: Option<String>,
    /// Optional `Author:` override.
    pub author: Option<String>,
    /// Optional `Date:` override (e.g. `2009-12-03`).
    pub date: Option<String>,
    /// Optional coordinates from a `Coord:` directive (latitude, longitude).
    pub coordinates: Option<(f64, f64)>,
}

/// Parses a caption, extracting `Categories:`, `Source:`, and `Author:` directive lines.
pub fn parse_caption(caption: &str) -> ParsedCaption {
    let mut description_lines = Vec::new();
    let mut categories = Vec::new();
    let mut source = None;
    let mut author = None;
    let mut date = None;
    let mut coordinates = None;
    for line in caption.lines() {
        let trimmed = line.trim();
        if let Some(rest) = strip_prefix_ci(trimmed, &["categories:", "category:", "c:"]) {
            for raw in rest.split(',') {
                let category = clean_category(raw);
                if !category.is_empty() {
                    categories.push(category);
                }
            }
        } else if let Some(rest) = strip_prefix_ci(trimmed, &["source:"]) {
            if !rest.is_empty() {
                source = Some(rest.to_string());
            }
        } else if let Some(rest) = strip_prefix_ci(trimmed, &["author:", "a:"]) {
            if !rest.is_empty() {
                author = Some(rest.to_string());
            }
        } else if let Some(rest) = strip_prefix_ci(trimmed, &["date:"]) {
            if !rest.is_empty() {
                date = Some(rest.to_string());
            }
        } else if let Some(rest) =
            strip_prefix_ci(trimmed, &["coordinates:", "coord:", "location:", "gps:"])
        {
            if let Some(coords) = crate::geo::parse_coordinates(rest) {
                coordinates = Some(coords);
            }
        } else {
            description_lines.push(line);
        }
    }
    ParsedCaption {
        description: description_lines.join("\n").trim().to_string(),
        categories,
        source,
        author,
        date,
        coordinates,
    }
}

/// A standalone "set defaults" command parsed from a chat message.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct SettingsCommand {
    /// Categories to add to the user's defaults.
    pub categories: Vec<String>,
    /// New default author, if set.
    pub author: Option<String>,
    /// New filename prefix, if set.
    pub prefix: Option<String>,
    /// New default description, if set.
    pub description: Option<String>,
    /// New default description language code, if set.
    pub lang: Option<String>,
    /// New custom license, if set.
    pub license: Option<String>,
}

impl SettingsCommand {
    /// Returns true when no directive matched.
    pub fn is_empty(&self) -> bool {
        self.categories.is_empty()
            && self.author.is_none()
            && self.prefix.is_none()
            && self.description.is_none()
            && self.lang.is_none()
            && self.license.is_none()
    }
}

/// Parses a standalone settings message. Each line may set a directive, with or without a
/// colon and via a short alias: `Category X`/`c X`, `Author X`/`a X`, `Prefix X`/`p X`.
pub fn parse_settings_command(text: &str) -> SettingsCommand {
    let mut command = SettingsCommand::default();
    for line in text.lines() {
        let trimmed = line.trim();
        if let Some(rest) = directive_value(trimmed, &["categories", "category", "c"]) {
            for raw in rest.split(',') {
                let category = clean_category(raw);
                if !category.is_empty() {
                    command.categories.push(category);
                }
            }
        } else if let Some(rest) = directive_value(trimmed, &["author", "a"]) {
            if !rest.is_empty() {
                command.author = Some(rest.to_string());
            }
        } else if let Some(rest) = directive_value(trimmed, &["prefix", "p"]) {
            command.prefix = Some(rest.to_string());
        } else if let Some(rest) = directive_value(trimmed, &["description", "caption", "cap", "d"])
        {
            if !rest.is_empty() {
                command.description = Some(rest.to_string());
            }
        } else if let Some(rest) = directive_value(trimmed, &["language", "lang"]) {
            if !rest.is_empty() {
                command.lang = Some(rest.to_string());
            }
        } else if let Some(rest) = directive_value(trimmed, &["license", "l"])
            && !rest.is_empty()
        {
            command.license = Some(rest.to_string());
        }
    }
    command
}

/// Returns a directive's value when the line starts with one of `keywords` followed by a
/// colon or whitespace (colon optional). Keywords must be lower-case, longest first.
fn directive_value<'a>(line: &'a str, keywords: &[&str]) -> Option<&'a str> {
    let lower = line.to_ascii_lowercase();
    for keyword in keywords {
        if let Some(rest) = lower.strip_prefix(keyword)
            && (rest.is_empty() || rest.starts_with(':') || rest.starts_with(char::is_whitespace))
        {
            let after = line[keyword.len()..].trim_start();
            let after = after.strip_prefix(':').unwrap_or(after);
            return Some(after.trim());
        }
    }
    None
}

/// Returns the text after any of the case-insensitive prefixes, if the line has one.
fn strip_prefix_ci<'a>(line: &'a str, prefixes: &[&str]) -> Option<&'a str> {
    let lower = line.to_ascii_lowercase();
    for prefix in prefixes {
        if lower.starts_with(prefix) {
            return Some(line[prefix.len()..].trim());
        }
    }
    None
}

/// Normalises one category name (strips brackets, a `Category:` prefix, illegal chars).
fn clean_category(raw: &str) -> String {
    let trimmed = raw
        .trim()
        .trim_start_matches("[[")
        .trim_end_matches("]]")
        .trim();
    let without_prefix = trimmed
        .strip_prefix("Category:")
        .or_else(|| trimmed.strip_prefix("category:"))
        .unwrap_or(trimmed);
    sanitize_title(without_prefix)
}

/// Replaces characters illegal in Commons page titles and collapses whitespace.
pub fn sanitize_title(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for ch in input.chars() {
        match ch {
            '#' | '<' | '>' | '[' | ']' | '|' | '{' | '}' | '/' | '\\' | ':' | '~' | '\n'
            | '\r' | '\t' => out.push(' '),
            _ => out.push(ch),
        }
    }
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Returns true for emoji / pictographic symbol code points (dropped from filenames).
fn is_emoji(ch: char) -> bool {
    let code = ch as u32;
    matches!(
        code,
        0x200D                  // zero-width joiner
        | 0x2190..=0x21FF       // arrows
        | 0x2300..=0x23FF       // misc technical (⌚ ⏰ …)
        | 0x2460..=0x24FF       // enclosed alphanumerics (① …)
        | 0x2500..=0x27BF       // shapes, misc symbols, dingbats
        | 0x2B00..=0x2BFF       // misc symbols and arrows (⭐ …)
        | 0xFE00..=0xFE0F       // variation selectors
        | 0x1F000..=0x1FAFF     // emoji blocks (📍 included)
    )
}

/// Sanitizes one filename component: drops emoji, then applies page-title rules.
fn sanitize_filename_part(input: &str) -> String {
    let without_emoji: String = input.chars().filter(|&ch| !is_emoji(ch)).collect();
    sanitize_title(&without_emoji)
}

/// Builds a sanitized Commons filename from prefix, caption, and original stem.
///
/// The caption text becomes a descriptive prefix so generic camera names like
/// `IMG_5638` are acceptable and never collide within an album, while the original stem
/// keeps each file unique. A trailing `_` or `-` in the configured prefix is treated as
/// an explicit separator, so `Belarus_2014_` becomes `Belarus_2014_IMG_5638.jpg`.
/// Newlines collapse to spaces and emoji are dropped. When there is no usable stem,
/// `unique_token` is appended so files stay unique.
pub fn build_filename(
    prefix: &str,
    caption: &str,
    original_stem: &str,
    extension: &str,
    unique_token: &str,
) -> String {
    let mut parts = Vec::new();
    let prefix = sanitize_filename_part(prefix);
    if !prefix.is_empty() {
        parts.push(prefix);
    }
    let caption = sanitize_filename_part(caption);
    if !caption.is_empty() {
        parts.push(caption);
    }
    let stem = sanitize_filename_part(original_stem);
    let has_stem = !stem.is_empty();
    if has_stem {
        parts.push(stem);
    }
    if parts.is_empty() {
        parts.push("image".to_string());
    }
    if !has_stem {
        let token = sanitize_filename_part(unique_token);
        if !token.is_empty() {
            parts.push(token);
        }
    }
    let stem = truncate_bytes(&join_filename_parts(&parts), MAX_FILENAME_STEM_BYTES);
    format!("{stem}.{}", extension.to_ascii_lowercase())
}

fn join_filename_parts(parts: &[String]) -> String {
    let mut result = String::new();
    for part in parts {
        if part.is_empty() {
            continue;
        }
        if !result.is_empty()
            && !result.ends_with(' ')
            && !result.ends_with('_')
            && !result.ends_with('-')
        {
            result.push(' ');
        }
        result.push_str(part);
    }
    result
}

/// Truncates a string to at most `max_bytes`, never splitting a UTF-8 char.
fn truncate_bytes(input: &str, max_bytes: usize) -> String {
    if input.len() <= max_bytes {
        return input.to_string();
    }
    let mut end = max_bytes;
    while end > 0 && !input.is_char_boundary(end) {
        end -= 1;
    }
    input[..end].trim_end().to_string()
}

/// Parameters for building a Commons file-description page.
pub struct DescriptionParams<'a> {
    /// Description text.
    pub description: &'a str,
    /// Bot-password username (`Account@label`); only the account part is shown.
    pub author_username: &'a str,
    /// Optional author override from the caption (defaults to the account link).
    pub author_override: Option<&'a str>,
    /// Optional source override from the caption (defaults to `{{own}}`).
    pub source: Option<&'a str>,
    /// Chosen license (used when no custom override is set).
    pub license: License,
    /// Custom license wikitext/template overriding `license`.
    pub license_override: Option<&'a str>,
    /// Optional description language code (wraps the description).
    pub lang: Option<&'a str>,
    /// Category names to add.
    pub categories: &'a [String],
    /// Date string for the `{{Information}}` `date` field (e.g. `2026-06-20`).
    pub date: &'a str,
    /// Optional GPS latitude in decimal degrees (from EXIF).
    pub latitude: Option<f64>,
    /// Optional GPS longitude in decimal degrees (from EXIF).
    pub longitude: Option<f64>,
    /// Provenance (original filename and, for conversions, original hashes).
    pub provenance: &'a UploadProvenance,
}

/// Builds the `{{Information}}` + license + categories wikitext for a file page.
pub fn build_wikitext(params: &DescriptionParams) -> String {
    let description = if params.description.trim().is_empty() {
        String::new()
    } else {
        wikitext_value(params.description)
    };
    let description = match params.lang.map(str::trim).filter(|lang| !lang.is_empty()) {
        Some(lang) if !description.is_empty() => format!("{{{{{lang}|1={description}}}}}"),
        None => description,
        _ => description,
    };
    let source = match params
        .source
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        Some(value) => wikitext_value(value),
        None => "{{own}}".to_string(),
    };
    let author = match params.author_override.map(str::trim) {
        Some(override_author) if !override_author.is_empty() => wikitext_value(override_author),
        _ => {
            let account = account_name(params.author_username);
            format!("[[User:{account}|{account}]]")
        }
    };

    let mut wikitext = String::new();
    wikitext.push_str("=={{int:filedesc}}==\n{{Information\n");
    wikitext.push_str(&format!("|description={description}\n"));
    wikitext.push_str(&format!("|date={}\n", params.date));
    wikitext.push_str(&format!("|source={source}\n"));
    wikitext.push_str(&format!("|author={author}\n"));
    let other_fields = build_other_fields(params.provenance);
    if !other_fields.is_empty() {
        wikitext.push_str(&format!("|other fields={other_fields}\n"));
    }
    wikitext.push_str("}}\n\n");

    wikitext.push_str("=={{int:license-header}}==\n");
    wikitext.push_str(&format!(
        "{}\n\n",
        render_license(params.license_override, params.license)
    ));

    if let (Some(latitude), Some(longitude)) = (params.latitude, params.longitude) {
        wikitext.push_str(&format!(
            "{{{{Location dec|{latitude:.6}|{longitude:.6}}}}}\n\n"
        ));
    }

    for category in params.categories {
        wikitext.push_str(&format!("[[Category:{category}]]\n"));
    }
    wikitext.push_str(&format!("[[Category:{BOT_CATEGORY}]]\n"));
    wikitext
}

/// Escapes characters that would break or inject into the `{{Information}}` template.
fn wikitext_value(text: &str) -> String {
    text.replace('{', "&#123;")
        .replace('}', "&#125;")
        .replace('|', "&#124;")
}

/// Renders the license wikitext from an optional custom override or the picked license.
fn render_license(override_license: Option<&str>, license: License) -> String {
    match override_license
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        Some(value) if value.starts_with("{{") => value.to_string(),
        Some(value) => match License::parse(value) {
            Some(parsed) => parsed.wikitext(),
            None if value.contains(char::is_whitespace) => value.to_string(),
            None => format!("{{{{{value}}}}}"),
        },
        None => license.wikitext(),
    }
}

/// Builds `{{Information field}}` provenance entries for the original file.
fn build_other_fields(provenance: &UploadProvenance) -> String {
    let mut fields = String::new();
    if !provenance.original_filename.is_empty() {
        fields.push_str(&format!(
            "{{{{Information field|name=Original file|value={}}}}}",
            wikitext_value(&provenance.original_filename)
        ));
    }
    if let Some(sha1) = &provenance.original_sha1 {
        fields.push_str(&format!(
            "{{{{Information field|name=Original SHA1|value={sha1}}}}}"
        ));
    }
    if let Some(md5) = &provenance.original_md5 {
        fields.push_str(&format!(
            "{{{{Information field|name=Original MD5|value={md5}}}}}"
        ));
    }
    fields
}

/// Returns the Commons account name from a bot-password username (`Account@label`).
fn account_name(username: &str) -> &str {
    username.split('@').next().unwrap_or(username)
}

#[cfg(test)]
mod tests {
    use super::{
        DescriptionParams, ParsedCaption, account_name, build_filename, build_wikitext,
        coordinate_statement, copyright_status_qid, describe_warnings, file_title,
        interpret_upload_response, item_statement, license_override_key, license_qid,
        parse_caption, resolved_structured_data_license, sanitize_title, truncate_bytes,
        valid_coordinates,
    };
    use crate::models::{License, UploadProvenance};
    use serde_json::json;
    use std::io::{Read, Write};

    #[test]
    fn parses_categories_and_description() {
        let parsed =
            parse_caption("A nice photo\nCategories: Minsk, [[Category:Architecture]], Belarus");
        assert_eq!(
            parsed,
            ParsedCaption {
                description: "A nice photo".to_string(),
                categories: vec!["Minsk".into(), "Architecture".into(), "Belarus".into()],
                source: None,
                author: None,
                date: None,
                coordinates: None,
            }
        );
    }

    #[test]
    fn caption_without_categories_keeps_full_description() {
        let parsed = parse_caption("Just a description");
        assert_eq!(parsed.description, "Just a description");
        assert!(parsed.categories.is_empty());
    }

    #[test]
    fn parses_source_author_and_date_directives() {
        let parsed = parse_caption(
            "A cat\nAuthor: Somebody\nCategory: Blabla\nDate: 2009-12-03\nSource: https://example.com/cat/",
        );
        assert_eq!(parsed.description, "A cat");
        assert_eq!(parsed.author.as_deref(), Some("Somebody"));
        assert_eq!(parsed.categories, vec!["Blabla"]);
        assert_eq!(parsed.date.as_deref(), Some("2009-12-03"));
        assert_eq!(parsed.source.as_deref(), Some("https://example.com/cat/"));
    }

    #[test]
    fn sanitizes_illegal_title_characters() {
        assert_eq!(sanitize_title("a/b:c[d]  e"), "a b c d e");
    }

    #[test]
    fn filename_uses_caption_prefix_and_original_stem() {
        let name = build_filename("Minsk", "Old town", "IMG_5638", "webp", "AgAD");
        assert_eq!(name, "Minsk Old town IMG_5638.webp");
    }

    #[test]
    fn filename_does_not_add_space_after_separator_suffix_prefix() {
        let name = build_filename("Беларусь_2014_", "", "IMG_3955", "jpg", "AgAD");
        assert_eq!(name, "Беларусь_2014_IMG_3955.jpg");
    }

    #[test]
    fn filename_collapses_multiline_caption_and_drops_emoji() {
        let name = build_filename(
            "",
            "Храм Вознесения Господня\n📍Ждановичи",
            "IMG_5638",
            "webp",
            "x",
        );
        assert_eq!(name, "Храм Вознесения Господня Ждановичи IMG_5638.webp");
    }

    #[test]
    fn filename_appends_unique_token_when_no_stem() {
        let name = build_filename("", "Old town", "", "webp", "AgAD42");
        assert_eq!(name, "Old town AgAD42.webp");
    }

    #[test]
    fn filename_falls_back_to_image_when_empty() {
        let name = build_filename("", "", "", "jpg", "uniq");
        assert_eq!(name, "image uniq.jpg");
    }

    #[test]
    fn truncates_on_char_boundaries() {
        assert_eq!(truncate_bytes("héllo", 2), "h");
    }

    #[test]
    fn account_name_strips_bot_password_label() {
        assert_eq!(account_name("Example@uploader"), "Example");
        assert_eq!(account_name("Example"), "Example");
    }

    #[test]
    fn structured_data_license_mapping_handles_known_templates() {
        assert_eq!(
            license_override_key("{{self|cc-by-sa-4.0|migration=relicense}}"),
            "cc-by-sa-4.0"
        );
        assert_eq!(
            resolved_structured_data_license(License::CcBy40, Some("{{self|cc-by-sa-4.0}}")),
            Some(License::CcBySa40)
        );
        assert_eq!(
            resolved_structured_data_license(License::CcBy40, Some("{{PD-Russia}}")),
            Some(License::PdRussia)
        );
        assert_eq!(
            resolved_structured_data_license(License::CcBy40, Some("{{PD-RU-exempt}}")),
            None
        );
    }

    #[test]
    fn structured_data_license_status_mapping_is_conservative() {
        assert_eq!(license_qid(License::CcBy40), Some("Q20007257"));
        assert_eq!(license_qid(License::CcBySa40), Some("Q18199165"));
        assert_eq!(license_qid(License::Cc0), Some("Q6938433"));
        assert_eq!(license_qid(License::PdRussia), None);
        assert_eq!(copyright_status_qid(License::CcBy40), "Q50423863");
        assert_eq!(copyright_status_qid(License::PdRussia), "Q19652");
    }

    #[test]
    fn structured_data_statement_builders_match_wikibase_json_shape() {
        let item = item_statement("P275", "Q20007257");
        assert_eq!(
            item["mainsnak"]["datavalue"]["value"]["numeric-id"],
            json!(20007257)
        );
        assert_eq!(
            item["mainsnak"]["datavalue"]["type"],
            json!("wikibase-entityid")
        );

        let coordinate = coordinate_statement("P1259", 50.45, 30.523333);
        assert_eq!(
            coordinate["mainsnak"]["datavalue"]["type"],
            json!("globecoordinate")
        );
        assert_eq!(
            coordinate["mainsnak"]["datavalue"]["value"]["globe"],
            json!("http://www.wikidata.org/entity/Q2")
        );
    }

    #[test]
    fn structured_data_helpers_normalize_titles_and_coordinates() {
        assert_eq!(file_title("Example.jpg"), "File:Example.jpg");
        assert_eq!(file_title("File:Example.jpg"), "File:Example.jpg");
        assert!(valid_coordinates(50.45, 30.52));
        assert!(!valid_coordinates(91.0, 30.52));
        assert!(!valid_coordinates(50.45, f64::NAN));
    }

    #[test]
    fn wikitext_includes_license_categories_and_provenance() {
        let provenance = UploadProvenance {
            original_filename: "IMG_0001.dng".into(),
            original_sha1: Some("abc123".into()),
            original_md5: Some("def456".into()),
        };
        let categories = vec!["Minsk".to_string()];
        let wikitext = build_wikitext(&DescriptionParams {
            description: "Old town",
            author_username: "Example@uploader",
            license: License::CcBySa40,
            license_override: None,
            lang: None,
            categories: &categories,
            date: "2026-06-20",
            latitude: Some(50.45),
            longitude: Some(30.523333),
            source: None,
            author_override: None,
            provenance: &provenance,
        });
        assert!(wikitext.contains("{{self|cc-by-sa-4.0}}"));
        assert!(wikitext.contains("{{Location dec|50.450000|30.523333}}"));
        assert!(wikitext.contains("[[User:Example|Example]]"));
        assert!(wikitext.contains("[[Category:Minsk]]"));
        assert!(wikitext.contains(
            "Uploaded with Telegram bot @wikimedia_commons_uploader_bot by Vitaly Zdanevich"
        ));
        assert!(wikitext.contains("Original SHA1|value=abc123"));
        assert!(wikitext.contains("Original file|value=IMG_0001.dng"));
    }

    #[test]
    fn wikitext_uses_source_and_author_overrides() {
        let provenance = UploadProvenance::default();
        let wikitext = build_wikitext(&DescriptionParams {
            description: "A cat",
            author_username: "Example@uploader",
            author_override: Some("John Doe"),
            source: Some("https://example.com/cat/"),
            license: License::CcBy40,
            license_override: None,
            lang: None,
            categories: &[],
            date: "2026-06-20",
            latitude: None,
            longitude: None,
            provenance: &provenance,
        });
        assert!(wikitext.contains("|source=https://example.com/cat/"));
        assert!(wikitext.contains("|author=John Doe"));
        assert!(!wikitext.contains("{{own}}"));
    }

    #[test]
    fn wikitext_renders_public_domain_license_templates_directly() {
        let provenance = UploadProvenance::default();
        let wikitext = build_wikitext(&DescriptionParams {
            description: "Old photo",
            author_username: "Example@uploader",
            author_override: None,
            source: None,
            license: License::PdRussiaExpired,
            license_override: None,
            lang: None,
            categories: &[],
            date: "2026-06-20",
            latitude: None,
            longitude: None,
            provenance: &provenance,
        });

        assert!(wikitext.contains("{{PD-Russia-expired}}"));
        assert!(!wikitext.contains("{{self|PD-Russia-expired}}"));
    }

    #[test]
    fn wikitext_keeps_empty_description_empty() {
        let provenance = UploadProvenance::default();
        let wikitext = build_wikitext(&DescriptionParams {
            description: "",
            author_username: "Example@uploader",
            author_override: None,
            source: None,
            license: License::CcBy40,
            license_override: None,
            lang: Some("ru"),
            categories: &[],
            date: "2026-06-20",
            latitude: None,
            longitude: None,
            provenance: &provenance,
        });
        assert!(wikitext.contains("|description=\n"));
        assert!(!wikitext.contains("Uploaded via Telegram"));
        assert!(!wikitext.contains("{{ru|1=}}"));
    }

    #[test]
    fn interprets_successful_upload() {
        let response = json!({"upload": {"result": "Success", "filename": "Minsk_old_town.webp"}});
        let outcome = interpret_upload_response(&response, "fallback.webp");
        match outcome {
            super::UploadOutcome::Success { title, url } => {
                assert_eq!(title, "Minsk_old_town.webp");
                assert!(url.ends_with("File:Minsk_old_town.webp"));
            }
            other => panic!("expected success, got {other:?}"),
        }
    }

    #[test]
    fn commons_page_urls_percent_encode_titles_for_telegram_links() {
        let file_url =
            super::file_page_url("Родник «Серебрянка» 55.859992, 28.441198 IMG_0748.jpg");
        let category = super::category_url("Родники Беларуси");

        assert!(!file_url.contains(' '));
        assert!(!file_url.contains('«'));
        assert!(file_url.contains("%D0%A0%D0%BE%D0%B4%D0%BD%D0%B8%D0%BA"));
        assert!(
            file_url.contains(
                "%C2%AB%D0%A1%D0%B5%D1%80%D0%B5%D0%B1%D1%80%D1%8F%D0%BD%D0%BA%D0%B0%C2%BB"
            )
        );
        assert_eq!(
            category,
            "https://commons.wikimedia.org/wiki/Category:%D0%A0%D0%BE%D0%B4%D0%BD%D0%B8%D0%BA%D0%B8_%D0%91%D0%B5%D0%BB%D0%B0%D1%80%D1%83%D1%81%D0%B8"
        );
    }

    #[test]
    fn reports_existing_filename_as_failure() {
        let response =
            json!({"upload": {"result": "Warning", "warnings": {"exists": "Minsk.webp"}}});
        let outcome = interpret_upload_response(&response, "Minsk.webp");
        match outcome {
            super::UploadOutcome::Failed { message, .. } => {
                assert!(message.contains("already exists"));
                assert!(message.contains("Minsk.webp"));
            }
            other => panic!("expected failure, got {other:?}"),
        }
    }

    #[test]
    fn describes_normalized_existing_filename_warning() {
        let warnings = json!({"exists-normalized": ["Minsk old town.webp"]});
        let message = describe_warnings(Some(&warnings));

        assert!(message.contains("normalized Commons filename"));
        assert!(message.contains("Minsk old town.webp"));
        assert!(message.contains("<a href=\"https://commons.wikimedia.org/wiki/File:Minsk_old_town.webp\">Minsk old town.webp</a>"));
    }

    #[test]
    fn reports_api_error_as_failure() {
        let response = json!({"error": {"code": "ratelimited", "info": "slow down"}});
        match interpret_upload_response(&response, "x.webp") {
            super::UploadOutcome::Failed { message, .. } => {
                assert!(message.contains("too fast"));
            }
            other => panic!("expected failure, got {other:?}"),
        }
    }

    #[test]
    fn describes_duplicate_warning() {
        let warnings = json!({"duplicate": ["Existing1.jpg", "Existing2.jpg"]});
        let message = describe_warnings(Some(&warnings));
        assert!(message.contains("duplicate of:"));
        assert!(message.contains(
            "<a href=\"https://commons.wikimedia.org/wiki/File:Existing1.jpg\">Existing1.jpg</a>"
        ));
        assert!(message.contains(
            "<a href=\"https://commons.wikimedia.org/wiki/File:Existing2.jpg\">Existing2.jpg</a>"
        ));
    }

    #[test]
    fn only_exists_normalized_warning_is_auto_retryable() {
        let retryable =
            json!({"upload": {"result": "Warning", "warnings": {"exists-normalized": ["A.jpg"]}}});
        let mixed = json!({"upload": {"result": "Warning", "warnings": {"exists-normalized": ["A.jpg"], "duplicate": ["B.jpg"]}}});
        let existing = json!({"upload": {"result": "Warning", "warnings": {"exists": "A.jpg"}}});

        assert!(super::is_only_exists_normalized_warning(&retryable));
        assert!(!super::is_only_exists_normalized_warning(&mixed));
        assert!(!super::is_only_exists_normalized_warning(&existing));
    }

    #[test]
    fn friendly_error_gives_actionable_messages() {
        assert!(super::friendly_error("blocked", "blah").contains("IP"));
        assert!(
            super::friendly_error("permissiondenied", "x").contains("Create, edit, and move pages")
        );
    }

    #[test]
    fn login_failure_message_guides_reconnect() {
        let message = super::login_failure_message("WrongPass");
        assert!(message.contains("WrongPass"));
        assert!(message.contains("/start"));
        assert!(message.contains("Special:BotPasswords"));
    }

    #[test]
    fn transient_login_failure_message_says_retry() {
        let reason = "Unable to continue login. Your session most likely timed out.";
        let message = super::login_failure_message(reason);
        assert!(super::is_transient_login_failure(reason));
        assert!(message.contains("usually transient"));
        assert!(message.contains("Please retry"));
        assert!(!message.contains("bot password may be wrong"));
    }

    #[test]
    fn strip_wikitext_cleans_markup() {
        assert_eq!(
            super::strip_wikitext("'''Bold''' and [[m:Help|help page]] here"),
            "Bold and help page here"
        );
    }

    #[tokio::test]
    async fn upload_count_by_user_follows_commons_continue_token() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let api_url = format!("http://{}", listener.local_addr().unwrap());
        let server = std::thread::spawn(move || {
            let mut targets = Vec::new();
            for index in 0..2 {
                let (mut stream, _) = listener.accept().unwrap();
                let mut buffer = Vec::new();
                let mut chunk = [0_u8; 1024];
                loop {
                    let read = stream.read(&mut chunk).unwrap();
                    assert!(read > 0, "client closed before request headers");
                    buffer.extend_from_slice(&chunk[..read]);
                    if buffer.windows(4).any(|window| window == b"\r\n\r\n") {
                        break;
                    }
                }
                let request = String::from_utf8(buffer).unwrap();
                let target = request
                    .lines()
                    .next()
                    .unwrap()
                    .split_whitespace()
                    .nth(1)
                    .unwrap()
                    .to_string();
                targets.push(target);

                let body = if index == 0 {
                    json!({
                        "query": {"allimages": [{"name": "A.jpg"}, {"name": "B.jpg"}]},
                        "continue": {"aicontinue": "next-token"}
                    })
                } else {
                    json!({"query": {"allimages": [{"name": "C.jpg"}]}})
                }
                .to_string();
                write!(
                    stream,
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                )
                .unwrap();
            }
            targets
        });

        let client = super::CommonsClient::new(api_url, "test-agent", None, None, false);
        let count = client.upload_count_by_user("Example User").await.unwrap();
        assert_eq!(count, 3);

        let targets = server.join().unwrap();
        assert_eq!(targets.len(), 2);
        let first = url::Url::parse(&format!("http://localhost{}", targets[0])).unwrap();
        assert_eq!(
            first
                .query_pairs()
                .find(|(key, _)| key == "aiuser")
                .map(|(_, value)| value.into_owned())
                .as_deref(),
            Some("Example User")
        );
        assert!(
            first
                .query_pairs()
                .all(|(key, _)| key.as_ref() != "aicontinue")
        );
        let second = url::Url::parse(&format!("http://localhost{}", targets[1])).unwrap();
        assert_eq!(
            second
                .query_pairs()
                .find(|(key, _)| key == "aicontinue")
                .map(|(_, value)| value.into_owned())
                .as_deref(),
            Some("next-token")
        );
    }
}
