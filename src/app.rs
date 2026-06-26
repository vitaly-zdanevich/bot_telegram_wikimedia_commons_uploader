use crate::commons::{
    CommonsBotPasswordSession, CommonsClient, DescriptionParams, UploadAuth, UploadData,
    UploadOutcome, UploadRequest, build_filename, build_wikitext, category_url, parse_caption,
};
use crate::config::Config;
use crate::convert;
use crate::crypto::Cipher;
use crate::metadata;
use crate::models::{
    CallbackQuery, License, Message, OnboardingStep, Profile, Update, UploadProvenance,
};
use crate::oauth::{Consumer, OAuthClient, OAuthEndpoints};
use crate::store::Store;
use crate::telegram::{
    InlineKeyboardButton, InlineKeyboardMarkup, TelegramClient, TelegramFile, escape_html,
    license_keyboard,
};
use anyhow::{Context, Result};
use bytes::Bytes;
use http::{HeaderMap, Method, StatusCode};
use http_body_util::{BodyExt, Full};
use hyper::body::Incoming;
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Request as HyperRequest, Response as HyperResponse};
use hyper_util::rt::TokioIo;
use lambda_http::{Body, Request as LambdaRequest, Response as LambdaResponse};
use std::convert::Infallible;
use std::net::SocketAddr;
use std::path::Path;
#[cfg(feature = "archive")]
use std::path::PathBuf;
use tokio::net::TcpListener;

/// Telegram handle of the author, shown in `/help`.
const CONTACT: &str = "@vitaly_zdanevich";
/// Telegram handle of this bot.
const BOT_USERNAME: &str = "@wikimedia_commons_uploader_bot";
/// Author's Telegram bot for browsing/reading Commons media.
const RELATED_BROWSE_BOT: &str =
    "https://github.com/vitaly-zdanevich/bot_telegram_wikimedia_commons";
/// Author's gThumb extension for Commons.
const RELATED_GTHUMB: &str =
    "https://gitlab.com/vitaly_zdanevich_wikimedia/gthumb-wikimedia-commons-extension";
/// Author's browser extension that uploads to Commons.
const RELATED_WEB_EXTENSION: &str =
    "https://gitlab.com/vitaly-zdanevich-extensions/uploading-to-wikimedia-commons";
/// Author's dark Wikipedia userstyle.
const RELATED_DARK_THEME: &str =
    "https://github.com/vitaly-zdanevich/wikipedia-userstyle-dark-minimum";
/// Author's Wikipedia-to-man-page converter.
const RELATED_WIKI2MAN: &str = "https://gitlab.com/vitaly_zdanevich_wikimedia/wiki2man_on_rust";
/// Author's Pywikibot-wrapper CLI for simpler Commons uploads.
const RELATED_CLI: &str =
    "https://gitlab.com/vitaly_zdanevich_wikimedia/pwb_wrapper_for_simpler_uploading_to_commons";
/// How long a processed Telegram update id is remembered (suppresses webhook retries).
const UPDATE_IDEMPOTENCY_SECONDS: i64 = 24 * 60 * 60;
/// Message shown once onboarding is complete.
const ONBOARDING_DONE_MSG: &str = "✅ All set! Send me a photo or file and I'll upload it to Wikimedia Commons. Tip: a caption becomes the file's <b>description</b> and its <b>filename prefix</b>; add a line like <code>Categories: Minsk, Belarus</code> to set categories.";
/// Bytes needed to identify HEIC/BMP/archive magic without loading the full file.
const FILE_SNIFF_BYTES: usize = 512;

/// Keeps Telegram's chat action visible while a long operation is running.
#[cfg(feature = "archive")]
struct ChatActionGuard {
    stop: std::sync::mpsc::Sender<()>,
}

#[cfg(feature = "archive")]
impl ChatActionGuard {
    fn start(telegram: TelegramClient, chat_id: i64, action: &'static str) -> Self {
        let (stop, stopped) = std::sync::mpsc::channel();
        std::thread::Builder::new()
            .name("telegram-chat-action".to_string())
            .spawn(move || {
                let runtime = match tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                {
                    Ok(runtime) => runtime,
                    Err(error) => {
                        tracing::warn!(error = %format!("{error:#}"), "failed to start chat action runtime");
                        return;
                    }
                };
                loop {
                    if let Err(error) = runtime.block_on(telegram.send_chat_action(chat_id, action))
                    {
                        tracing::warn!(
                            chat_id,
                            action,
                            error = %format!("{error:#}"),
                            "failed to send Telegram chat action"
                        );
                    }
                    if stopped
                        .recv_timeout(std::time::Duration::from_secs(4))
                        .is_ok()
                    {
                        break;
                    }
                }
            })
            .expect("chat action helper thread should start");
        Self { stop }
    }
}

#[cfg(feature = "archive")]
impl Drop for ChatActionGuard {
    fn drop(&mut self) {
        let _ = self.stop.send(());
    }
}

/// Logs and suppresses failures when sending a best-effort Telegram chat action.
#[cfg(feature = "archive")]
async fn send_chat_action_best_effort(telegram: &TelegramClient, chat_id: i64, action: &str) {
    if let Err(error) = telegram.send_chat_action(chat_id, action).await {
        tracing::warn!(
            chat_id,
            action,
            error = %format!("{error:#}"),
            "failed to send Telegram chat action"
        );
    }
}

#[cfg(feature = "archive")]
fn spawn_archive_upload(
    config: Config,
    chat_id: i64,
    user_id: i64,
    caption: String,
    extra_categories: Vec<String>,
    entries: Vec<crate::archive::ArchiveEntry>,
) {
    tokio::spawn(async move {
        let bot = Bot::from_config(config);
        let mut profile = bot.store.get_profile(user_id).await;
        if let Err(error) = bot
            .upload_entries(
                chat_id,
                user_id,
                &mut profile,
                &caption,
                &extra_categories,
                entries,
            )
            .await
        {
            tracing::error!(
                user_id,
                chat_id,
                error = %format!("{error:#}"),
                "archive upload task failed"
            );
            bot.telegram
                .send_message(
                    chat_id,
                    &format!(
                        "❌ Archive upload failed: {}",
                        escape_html(&format!("{error}"))
                    ),
                    None,
                )
                .await
                .ok();
        }
    });
}

#[cfg(feature = "archive")]
enum ArchivePreviewFollowup {
    Confirm {
        count: usize,
        archive_file_name: Option<String>,
    },
    Prefix {
        count: usize,
        sample_name: Option<String>,
        archive_file_name: Option<String>,
    },
}

#[cfg(feature = "archive")]
enum ArchiveConfirmAction {
    Start,
    ArchiveNamePrefix { include_category: bool },
    Cancel,
}

#[cfg(feature = "archive")]
fn archive_confirm_action_name(action: &ArchiveConfirmAction) -> &'static str {
    match action {
        ArchiveConfirmAction::Start => "start",
        ArchiveConfirmAction::ArchiveNamePrefix {
            include_category: false,
        } => "archive_name_prefix",
        ArchiveConfirmAction::ArchiveNamePrefix {
            include_category: true,
        } => "archive_name_prefix_and_category",
        ArchiveConfirmAction::Cancel => "cancel",
    }
}

#[cfg(feature = "archive")]
fn spawn_archive_thumbnail_preview(
    config: Config,
    chat_id: i64,
    user_id: i64,
    token: String,
    followup: ArchivePreviewFollowup,
) {
    tokio::spawn(async move {
        let resize = config.archive_thumbnail_resize;
        let bot = Bot::from_config(config);
        let _typing = ChatActionGuard::start(bot.telegram.clone(), chat_id, "typing");
        tracing::info!(
            user_id,
            chat_id,
            token,
            resize,
            "starting archive thumbnail preview"
        );
        let completed =
            match send_archive_thumbnail_preview(&bot.telegram, chat_id, &token, resize).await {
                Ok(completed) => completed,
                Err(error) => {
                    tracing::warn!(
                        user_id,
                        chat_id,
                        token,
                        error = %format!("{error:#}"),
                        "archive thumbnail preview failed"
                    );
                    false
                }
            };
        if !completed || !pending_archive_manifest_path(&token).exists() {
            return;
        }

        match followup {
            ArchivePreviewFollowup::Confirm {
                count,
                archive_file_name,
            } => {
                bot.send_archive_confirmation(
                    chat_id,
                    &token,
                    count,
                    None,
                    archive_file_name.as_deref(),
                )
                .await
                .ok();
            }
            ArchivePreviewFollowup::Prefix {
                count,
                sample_name,
                archive_file_name,
            } => {
                let profile = bot.store.get_profile(user_id).await;
                if profile.onboarding_step == OnboardingStep::AwaitingArchivePrefix {
                    bot.prompt_archive_prefix(
                        chat_id,
                        count,
                        sample_name.as_deref(),
                        Some(&token),
                        archive_file_name.as_deref(),
                    )
                    .await
                    .ok();
                }
            }
        }
        tracing::info!(
            user_id,
            chat_id,
            token,
            completed,
            "finished archive thumbnail preview"
        );
    });
}

#[cfg(feature = "archive")]
async fn send_archive_thumbnail_preview(
    telegram: &TelegramClient,
    chat_id: i64,
    token: &str,
    resize: bool,
) -> Result<bool> {
    if !pending_archive_manifest_path(token).exists() {
        return Ok(false);
    }
    let manifest = read_pending_archive_manifest(token)?;
    let dir = pending_archive_token_dir(token);
    for entry in manifest.entries {
        let path = dir.join(&entry.file_name);
        let bytes = match std::fs::read(&path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("failed to read archive preview {}", path.display()));
            }
        };
        if resize {
            let preview =
                convert::make_thumbnail(&bytes, 320).map(|thumb| (thumb, "thumb.jpg".to_string()));
            let Some((photo, file_name)) = preview else {
                continue;
            };
            if let Err(error) = telegram
                .send_photo(chat_id, photo, &file_name, Some(&entry.name), None)
                .await
            {
                tracing::warn!(
                    chat_id,
                    token,
                    name = %entry.name,
                    resize,
                    error = %format!("{error:#}"),
                    "failed to send archive thumbnail"
                );
            }
        } else {
            match telegram
                .send_photo(chat_id, bytes.clone(), &entry.name, Some(&entry.name), None)
                .await
            {
                Ok(()) => {}
                Err(error) => {
                    tracing::warn!(
                        chat_id,
                        token,
                        name = %entry.name,
                        error = %format!("{error:#}"),
                        "failed to send original archive preview; trying resized thumbnail"
                    );
                    let Some(thumb) = convert::make_thumbnail(&bytes, 320) else {
                        continue;
                    };
                    if let Err(error) = telegram
                        .send_photo(chat_id, thumb, "thumb.jpg", Some(&entry.name), None)
                        .await
                    {
                        tracing::warn!(
                            chat_id,
                            token,
                            name = %entry.name,
                            error = %format!("{error:#}"),
                            "failed to send resized archive thumbnail fallback"
                        );
                    }
                }
            }
        }
    }
    Ok(true)
}

/// Handles one AWS Lambda HTTP request from the Telegram webhook.
pub async fn handle_lambda_request(request: LambdaRequest) -> Result<LambdaResponse<Body>> {
    handle_webhook_payload(request.headers(), request.body().as_ref()).await?;
    ok_response()
}

/// Runs a standalone HTTP server for Telegram webhooks.
///
/// Toolforge's webservice proxy forwards requests to `$PORT` (currently 8000). This mode
/// accepts Telegram POSTs on `/` and `/telegram`, and exposes `/healthz` for checks.
pub async fn run_webhook_server() -> Result<()> {
    let port = std::env::var("PORT")
        .ok()
        .and_then(|value| value.parse::<u16>().ok())
        .unwrap_or(8000);
    let addr = SocketAddr::from(([0, 0, 0, 0], port));
    let listener = TcpListener::bind(addr)
        .await
        .with_context(|| format!("failed to bind HTTP server on {addr}"))?;

    tracing::info!(%addr, "starting Telegram webhook server");
    loop {
        let (stream, peer_addr) = listener.accept().await?;
        tokio::task::spawn(async move {
            let io = TokioIo::new(stream);
            let service = service_fn(handle_http_request);
            if let Err(error) = http1::Builder::new().serve_connection(io, service).await {
                tracing::warn!(%peer_addr, error = %format!("{error:#}"), "HTTP connection failed");
            }
        });
    }
}

async fn handle_http_request(
    request: HyperRequest<Incoming>,
) -> std::result::Result<HyperResponse<Full<Bytes>>, Infallible> {
    let response = match (request.method(), request.uri().path()) {
        (&Method::GET, "/healthz") => text_response(StatusCode::OK, "ok"),
        (&Method::POST, "/") | (&Method::POST, "/telegram") => {
            let headers = request.headers().clone();
            match request.into_body().collect().await {
                Ok(collected) => {
                    match handle_webhook_payload(&headers, &collected.to_bytes()).await {
                        Ok(()) => text_response(StatusCode::OK, "ok"),
                        Err(error) => {
                            let status = status_for_webhook_error(&error);
                            tracing::warn!(status = %status, error = %format!("{error:#}"), "webhook request rejected");
                            text_response(status, status.canonical_reason().unwrap_or("error"))
                        }
                    }
                }
                Err(error) => {
                    tracing::warn!(error = %format!("{error:#}"), "failed to read webhook request body");
                    text_response(StatusCode::BAD_REQUEST, "bad request")
                }
            }
        }
        _ => text_response(StatusCode::NOT_FOUND, "not found"),
    };
    Ok(response)
}

fn text_response(status: StatusCode, text: &str) -> HyperResponse<Full<Bytes>> {
    HyperResponse::builder()
        .status(status)
        .header("content-type", "text/plain; charset=utf-8")
        .body(Full::new(Bytes::copy_from_slice(text.as_bytes())))
        .expect("valid HTTP response")
}

fn status_for_webhook_error(error: &anyhow::Error) -> StatusCode {
    let text = format!("{error:#}");
    if text.contains("invalid Telegram webhook secret") {
        StatusCode::UNAUTHORIZED
    } else if text.contains("invalid Telegram update JSON") {
        StatusCode::BAD_REQUEST
    } else {
        StatusCode::INTERNAL_SERVER_ERROR
    }
}

async fn handle_webhook_payload(headers: &HeaderMap, body: &[u8]) -> Result<()> {
    let config = Config::from_env();
    verify_telegram_secret(&config, headers)?;
    let update: Update = serde_json::from_slice(body).context("invalid Telegram update JSON")?;

    let bot = Bot::from_config(config);
    if let Some(update_id) = update.update_id {
        match bot
            .store
            .reserve_idempotency(
                &format!("TELEGRAM_UPDATE#{update_id}"),
                UPDATE_IDEMPOTENCY_SECONDS,
            )
            .await
        {
            Ok(false) => {
                tracing::info!(update_id, "skipping duplicate Telegram update");
                return Ok(());
            }
            Ok(true) => {}
            Err(error) => {
                tracing::warn!(error = %format!("{error:#}"), "idempotency reservation failed");
            }
        }
    }

    if let Err(error) = bot.handle_update(update).await {
        tracing::error!(error = %format!("{error:#}"), "failed to handle Telegram update");
    }
    Ok(())
}

/// Runs the bot as a long-living server using Telegram long polling.
///
/// Used for non-Lambda deployments (e.g. Toolforge / Cloud VPS). Paired with a self-hosted
/// Telegram Bot API server, the file limit rises from 20 MB to ~2 GB, and the clean server
/// IP avoids Wikimedia's data-centre IP blocks.
pub async fn run_polling() -> Result<()> {
    let config = Config::from_env();
    let bot = Bot::from_config(config);
    tracing::info!("starting Telegram long-polling loop");
    let mut offset = 0i64;
    loop {
        match bot.telegram.get_updates(offset, 50).await {
            Ok(updates) => {
                for update in updates {
                    if let Some(update_id) = update.update_id {
                        offset = update_id + 1;
                    }
                    if let Err(error) = bot.handle_update(update).await {
                        tracing::error!(error = %format!("{error:#}"), "failed to handle update");
                    }
                }
            }
            Err(error) => {
                tracing::error!(error = %format!("{error:#}"), "getUpdates failed");
                tokio::time::sleep(std::time::Duration::from_secs(3)).await;
            }
        }
    }
}

/// Returns the standard Telegram webhook success response.
fn ok_response() -> Result<LambdaResponse<Body>> {
    Ok(LambdaResponse::builder()
        .status(200)
        .body(Body::Text("ok".into()))?)
}

/// Verifies the Telegram webhook secret header when configured.
fn verify_telegram_secret(config: &Config, headers: &HeaderMap) -> Result<()> {
    let Some(expected) = &config.telegram_webhook_secret else {
        return Ok(());
    };
    let actual = headers
        .get("x-telegram-bot-api-secret-token")
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default();
    anyhow::ensure!(actual == expected, "invalid Telegram webhook secret");
    Ok(())
}

/// Bundles the clients and configuration used to service one update.
struct Bot {
    config: Config,
    telegram: TelegramClient,
    store: Store,
    commons: CommonsClient,
    cipher: Option<Cipher>,
    oauth: Option<OAuthClient>,
}

/// A file attachment extracted from a Telegram message.
struct FileRef {
    file_id: String,
    file_unique_id: String,
    file_name: Option<String>,
    mime: Option<String>,
    file_size: Option<u64>,
    compressed_photo: bool,
}

/// Structured outcome of running one file through the convert + upload pipeline.
///
/// Reporting is left to the caller, so single files and archive members can be
/// summarised differently (one detailed reply vs. an aggregate count).
enum FileResult {
    /// Uploaded successfully.
    Uploaded {
        filename: String,
        url: String,
        categories: Vec<String>,
    },
    /// An identical file already exists on Commons (matched by SHA-1).
    Duplicate { titles: Vec<String> },
    /// The file could not be converted or its type is not accepted.
    Rejected { reason: String },
    /// Commons refused the upload (e.g. blocked, permission denied).
    Failed { message: String },
}

impl Bot {
    /// Builds a bot from runtime configuration.
    fn from_config(config: Config) -> Self {
        let telegram = TelegramClient::new(
            config.telegram_bot_token.clone().unwrap_or_default(),
            config.telegram_api_base.clone(),
        );
        let store = Store::new(&config);
        let oauth = build_oauth_client(&config);
        let commons = CommonsClient::new(
            config.commons_api_url.clone(),
            config.user_agent.clone(),
            config.commons_proxy.clone(),
            oauth.clone(),
        );
        let cipher = config
            .credential_enc_key
            .as_deref()
            .and_then(|key| Cipher::from_base64_key(key).ok());
        Self {
            config,
            telegram,
            store,
            commons,
            cipher,
            oauth,
        }
    }

    /// Routes an update to the message or callback handler.
    async fn handle_update(&self, update: Update) -> Result<()> {
        if let Some(callback) = update.callback_query {
            return self.handle_callback(callback).await;
        }
        if let Some(message) = update.message {
            return self.handle_message(message).await;
        }
        Ok(())
    }

    /// Handles an incoming message: media upload, command, or onboarding answer.
    async fn handle_message(&self, message: Message) -> Result<()> {
        let chat_id = message.chat.id;
        let Some(user) = message.from.as_ref() else {
            return Ok(());
        };
        let user_id = user.id;

        if extract_file(&message).is_some() {
            return self.handle_upload(chat_id, user_id, &message).await;
        }

        let text = message.text.clone().unwrap_or_default();
        let trimmed = text.trim().to_string();
        if trimmed.starts_with('/') {
            return self.handle_command(chat_id, user_id, &trimmed).await;
        }
        self.handle_onboarding_text(chat_id, user_id, &message, &trimmed)
            .await
    }

    /// Dispatches a slash command.
    async fn handle_command(&self, chat_id: i64, user_id: i64, text: &str) -> Result<()> {
        let mut parts = text.splitn(2, char::is_whitespace);
        let command = parts.next().unwrap_or("");
        let argument = parts.next().unwrap_or("").trim();
        let command = command.split('@').next().unwrap_or(command);
        match command {
            "/start" => self.cmd_start(chat_id, user_id).await,
            "/help" => self.send_help(chat_id, user_id).await,
            "/stat" | "/stats" => self.cmd_stat(chat_id, user_id).await,
            "/settings" | "/prefs" | "/preferences" => {
                self.cmd_settings(chat_id, user_id, argument).await
            }
            "/forget" => self.cmd_forget(chat_id, user_id).await,
            "/cancel" => self.cmd_cancel(chat_id, user_id).await,
            _ => {
                self.telegram
                    .send_message(chat_id, "Unknown command. Try /help.", None)
                    .await
            }
        }
    }

    /// Starts or resumes onboarding, or shows status when already configured.
    async fn cmd_start(&self, chat_id: i64, user_id: i64) -> Result<()> {
        let mut profile = self.store.get_profile(user_id).await;
        if profile.is_ready() {
            let account = profile.commons_username.clone().unwrap_or_default();
            let text = format!(
                "✅ You're set up as <code>{}</code>.\n\nSend me a photo or file to upload it to Wikimedia Commons. Use /settings to change options, /forget to remove your credentials, or /help.",
                escape_html(&account)
            );
            return self.telegram.send_message(chat_id, &text, None).await;
        }
        if profile.onboarding_step == OnboardingStep::Done {
            profile.onboarding_step = OnboardingStep::AwaitingUsername;
            touch(&mut profile);
            self.store.put_profile(user_id, &profile).await?;
        }
        #[cfg(feature = "archive")]
        if profile.onboarding_step == OnboardingStep::AwaitingArchivePrefix {
            return self.prompt_current_archive_prefix(chat_id, user_id).await;
        }
        self.prompt_step(chat_id, profile.onboarding_step).await
    }

    /// Sends the prompt for the current onboarding step.
    async fn prompt_step(&self, chat_id: i64, step: OnboardingStep) -> Result<()> {
        match step {
            OnboardingStep::AwaitingUsername => {
                if self.oauth.is_some() {
                    let text = "👋 I upload your photos and files to <b>Wikimedia Commons</b> under <b>your</b> account.\n\nChoose how to connect your account:\n• <b>OAuth</b> (recommended) — authorize on the wiki and paste back a short code; you never share a password.\n• <b>Bot password</b> — create a scoped token yourself.";
                    self.telegram
                        .send_message(chat_id, text, Some(connect_method_keyboard()))
                        .await
                } else {
                    self.send_botpassword_prompt(chat_id).await
                }
            }
            OnboardingStep::AwaitingOAuthVerifier => {
                self.telegram
                    .send_message(
                        chat_id,
                        "After authorizing on the wiki, paste the <b>verification code</b> it shows here.",
                        None,
                    )
                    .await
            }
            OnboardingStep::AwaitingPassword => {
                self.telegram
                    .send_message(
                        chat_id,
                        "Now send the <b>bot password</b> (the generated password from that page). I delete your message immediately and store it encrypted.",
                        Some(change_username_keyboard()),
                    )
                    .await
            }
            OnboardingStep::AwaitingLicense => {
                self.telegram
                    .send_message(
                        chat_id,
                        "Choose a license for your uploads (default is CC BY 4.0):",
                        Some(license_keyboard()),
                    )
                    .await
            }
            OnboardingStep::AwaitingPrefix => {
                self.telegram
                    .send_message(
                        chat_id,
                        "Send a <b>filename prefix</b> for your uploads, or tap Skip for none.",
                        Some(skip_prefix_keyboard()),
                    )
                    .await
            }
            OnboardingStep::AwaitingArchivePrefix => {
                #[cfg(feature = "archive")]
                {
                    self.prompt_archive_prefix(chat_id, 0, None, None, None)
                        .await
                }
                #[cfg(not(feature = "archive"))]
                {
                    self.telegram
                        .send_message(chat_id, "Archive support is not enabled here.", None)
                        .await
                }
            }
            OnboardingStep::Done => {
                self.telegram
                    .send_message(chat_id, "✅ All set! Send me a photo or file.", None)
                    .await
            }
        }
    }

    /// Sends the bot-password username prompt (the bot-password onboarding path).
    async fn send_botpassword_prompt(&self, chat_id: i64) -> Result<()> {
        let text = "🔑 Create a <b>Bot Password</b> so you never share your real password:\n1. Open https://commons.wikimedia.org/wiki/Special:BotPasswords\n2. Use a label like <code>telegram</code> and tick <b>Upload new files</b> and <b>Create, edit, and move pages</b> (needed to write each file's page).\n3. You'll get a username like <code>YourName@telegram</code> and a password.\n\nNow send me your bot-password <b>username</b> (e.g. <code>YourName@telegram</code>).";
        self.telegram.send_message(chat_id, text, None).await
    }

    /// Starts the OAuth out-of-band flow: gets a request token and sends the authorize link.
    async fn start_oauth(&self, chat_id: i64, user_id: i64) -> Result<()> {
        let Some(oauth) = &self.oauth else {
            return self.send_botpassword_prompt(chat_id).await;
        };
        let Some(cipher) = &self.cipher else {
            return self
                .telegram
                .send_message(chat_id, "⚠️ The bot is missing its encryption key.", None)
                .await;
        };
        self.telegram.send_chat_action(chat_id, "typing").await.ok();
        let (request_token, request_secret) = match oauth.initiate().await {
            Ok(tokens) => tokens,
            Err(error) => {
                let text = format!(
                    "❌ Couldn't start OAuth: {}\n\nYou can use a bot password instead — /start.",
                    escape_html(&format!("{error}"))
                );
                return self.telegram.send_message(chat_id, &text, None).await;
            }
        };
        let mut profile = self.store.get_profile(user_id).await;
        profile.oauth_pending_ciphertext =
            Some(cipher.encrypt(&format!("{request_token}\n{request_secret}"))?);
        profile.onboarding_step = OnboardingStep::AwaitingOAuthVerifier;
        touch(&mut profile);
        self.store.put_profile(user_id, &profile).await?;

        let text = format!(
            "🔐 Open this link, authorize the app, then paste the <b>verification code</b> it shows back here:\n\n{}",
            oauth.authorize_url(&request_token)
        );
        self.telegram.send_message(chat_id, &text, None).await
    }

    /// Completes the OAuth flow with the pasted verifier, storing the access token.
    async fn finish_oauth(&self, chat_id: i64, user_id: i64, verifier: &str) -> Result<()> {
        let (Some(oauth), Some(cipher)) = (&self.oauth, &self.cipher) else {
            return self
                .telegram
                .send_message(chat_id, "⚠️ OAuth is not available right now.", None)
                .await;
        };
        let mut profile = self.store.get_profile(user_id).await;
        let Some(pending) = profile.oauth_pending_ciphertext.as_deref() else {
            profile.onboarding_step = OnboardingStep::AwaitingUsername;
            self.store.put_profile(user_id, &profile).await?;
            return self
                .telegram
                .send_message(
                    chat_id,
                    "That request expired — let's start over with /start.",
                    None,
                )
                .await;
        };
        let decoded = cipher
            .decrypt(pending)
            .context("failed to read pending OAuth token")?;
        let (request_token, request_secret) = decoded
            .split_once('\n')
            .context("pending OAuth token is malformed")?;

        self.telegram.send_chat_action(chat_id, "typing").await.ok();
        let (access_token, access_secret) = match oauth
            .exchange(request_token, request_secret, verifier.trim())
            .await
        {
            Ok(tokens) => tokens,
            Err(error) => {
                let text = format!(
                    "❌ That code didn't work: {}\n\nOpen the link again and paste the new code, or use a bot password with /start.",
                    escape_html(&format!("{error}"))
                );
                return self.telegram.send_message(chat_id, &text, None).await;
            }
        };
        let username = self
            .commons
            .oauth_username(&access_token, &access_secret)
            .await
            .unwrap_or_default();

        profile.oauth_ciphertext =
            Some(cipher.encrypt(&format!("{access_token}\n{access_secret}"))?);
        profile.oauth_pending_ciphertext = None;
        if !username.is_empty() {
            profile.commons_username = Some(username);
        }
        profile.onboarding_step = OnboardingStep::AwaitingLicense;
        touch(&mut profile);
        self.store.put_profile(user_id, &profile).await?;
        self.prompt_step(chat_id, OnboardingStep::AwaitingLicense)
            .await
    }

    /// Handles a free-text message as an answer to the current onboarding step.
    async fn handle_onboarding_text(
        &self,
        chat_id: i64,
        user_id: i64,
        message: &Message,
        text: &str,
    ) -> Result<()> {
        let mut profile = self.store.get_profile(user_id).await;
        match profile.onboarding_step {
            OnboardingStep::AwaitingUsername => {
                if text.is_empty() {
                    return self
                        .prompt_step(chat_id, OnboardingStep::AwaitingUsername)
                        .await;
                }
                profile.commons_username = Some(text.to_string());
                profile.onboarding_step = OnboardingStep::AwaitingPassword;
                touch(&mut profile);
                self.store.put_profile(user_id, &profile).await?;
                self.prompt_step(chat_id, OnboardingStep::AwaitingPassword)
                    .await
            }
            OnboardingStep::AwaitingPassword => {
                if let Some(message_id) = message.message_id {
                    self.telegram.delete_message(chat_id, message_id).await.ok();
                }
                let Some(cipher) = &self.cipher else {
                    return self
                        .telegram
                        .send_message(
                            chat_id,
                            "⚠️ The bot operator has not configured an encryption key, so credentials cannot be stored securely. Please contact the operator.",
                            None,
                        )
                        .await;
                };
                let username = profile.commons_username.clone().unwrap_or_default();
                self.telegram.send_chat_action(chat_id, "typing").await.ok();
                match self.commons.validate_credentials(&username, text).await {
                    Ok(()) => {
                        profile.credential_ciphertext = Some(cipher.encrypt(text)?);
                        profile.onboarding_step = OnboardingStep::AwaitingLicense;
                        touch(&mut profile);
                        self.store.put_profile(user_id, &profile).await?;
                        self.prompt_step(chat_id, OnboardingStep::AwaitingLicense)
                            .await
                    }
                    Err(error) => {
                        let text = format!(
                            "❌ Login failed: {}\n\nThe username or bot password was wrong. Re-send the <b>bot password</b> to retry, or tap the button below to change the username.",
                            escape_html(&format!("{error}"))
                        );
                        self.telegram
                            .send_message(chat_id, &text, Some(change_username_keyboard()))
                            .await
                    }
                }
            }
            OnboardingStep::AwaitingOAuthVerifier => {
                if text.is_empty() {
                    return self
                        .prompt_step(chat_id, OnboardingStep::AwaitingOAuthVerifier)
                        .await;
                }
                self.finish_oauth(chat_id, user_id, text).await
            }
            OnboardingStep::AwaitingLicense => {
                self.telegram
                    .send_message(
                        chat_id,
                        "Please pick a license using the buttons.",
                        Some(license_keyboard()),
                    )
                    .await
            }
            OnboardingStep::AwaitingPrefix => {
                let prefix = if text.eq_ignore_ascii_case("skip") {
                    String::new()
                } else {
                    text.to_string()
                };
                profile.filename_prefix = prefix;
                profile.onboarding_step = OnboardingStep::Done;
                touch(&mut profile);
                self.store.put_profile(user_id, &profile).await?;
                self.telegram
                    .send_message(chat_id, ONBOARDING_DONE_MSG, None)
                    .await
            }
            #[cfg(feature = "archive")]
            OnboardingStep::AwaitingArchivePrefix => {
                let pending = pending_archive_summary_for_user(user_id);
                if text.is_empty() || text.eq_ignore_ascii_case("skip") {
                    return self.prompt_current_archive_prefix(chat_id, user_id).await;
                }

                let Some(pending) = pending else {
                    profile.filename_prefix = text.to_string();
                    profile.onboarding_step = OnboardingStep::Done;
                    touch(&mut profile);
                    self.store.put_profile(user_id, &profile).await?;
                    let text = format!(
                        "Filename prefix set to <code>{}</code>, but I no longer have the pending archive. Please resend the archive.",
                        escape_html(&profile.filename_prefix)
                    );
                    return self.telegram.send_message(chat_id, &text, None).await;
                };

                profile.filename_prefix = text.to_string();
                profile.onboarding_step = OnboardingStep::Done;
                touch(&mut profile);
                self.store.put_profile(user_id, &profile).await?;

                if pending.confirm_before_upload {
                    return self
                        .send_archive_confirmation(
                            chat_id,
                            &pending.token,
                            pending.count,
                            Some(&profile.filename_prefix),
                            pending.archive_file_name.as_deref(),
                        )
                        .await;
                }

                let pending_archive = take_pending_archive(&pending.token);
                if let Some(pending_archive) = pending_archive {
                    let text = format!(
                        "Filename prefix set to <code>{}</code>. Uploading archive…",
                        escape_html(&profile.filename_prefix)
                    );
                    self.telegram.send_message(chat_id, &text, None).await.ok();
                    return self
                        .upload_entries(
                            chat_id,
                            user_id,
                            &mut profile,
                            &pending_archive.caption,
                            &[],
                            pending_archive.entries,
                        )
                        .await;
                }

                self.telegram
                    .send_message(
                        chat_id,
                        "I no longer have the pending archive. Please resend the archive.",
                        None,
                    )
                    .await
            }
            #[cfg(not(feature = "archive"))]
            OnboardingStep::AwaitingArchivePrefix => {
                profile.onboarding_step = OnboardingStep::Done;
                touch(&mut profile);
                self.store.put_profile(user_id, &profile).await?;
                self.telegram
                    .send_message(chat_id, "Archive support is not enabled here.", None)
                    .await
            }
            OnboardingStep::Done => {
                let command = crate::commons::parse_settings_command(text);
                if !command.is_empty() {
                    if !command.categories.is_empty() {
                        profile.default_categories =
                            merge_categories(&profile.default_categories, &command.categories);
                    }
                    if let Some(author) = command.author {
                        profile.default_author = Some(author);
                    }
                    if let Some(prefix) = command.prefix {
                        profile.filename_prefix = prefix;
                    }
                    if let Some(description) = command.description {
                        profile.default_description = Some(description);
                    }
                    if let Some(lang) = command.lang {
                        profile.default_lang = Some(lang);
                    }
                    if let Some(license) = command.license {
                        profile.license_override = Some(license);
                    }
                    touch(&mut profile);
                    self.store.put_profile(user_id, &profile).await?;
                    return self
                        .telegram
                        .send_message(chat_id, &defaults_summary(&profile), None)
                        .await;
                }
                self.telegram
                    .send_message(
                        chat_id,
                        "I upload files <b>to</b> Wikimedia Commons. To <b>get/search</b> files <b>from</b> Commons, use @wikimedia_commons_bot.\n\nSend me a photo or file to upload it, or /help.",
                        None,
                    )
                    .await
            }
        }
    }

    /// Handles inline-keyboard presses (license choice and setting toggles).
    async fn handle_callback(&self, callback: CallbackQuery) -> Result<()> {
        let data = callback.data.clone().unwrap_or_default();
        let user_id = callback.from.id;
        self.telegram
            .answer_callback_query(&callback.id, None)
            .await
            .ok();
        let Some(chat_id) = callback.message.as_ref().map(|message| message.chat.id) else {
            tracing::warn!(
                user_id,
                callback_data = %data,
                "callback query has no message context"
            );
            return Ok(());
        };
        tracing::info!(
            user_id,
            chat_id,
            callback_data = %data,
            "handling Telegram callback"
        );

        #[cfg(feature = "archive")]
        if let Some(token) = data.strip_prefix("arc:ok:") {
            return self
                .confirm_archive(chat_id, user_id, token, ArchiveConfirmAction::Start)
                .await;
        }
        #[cfg(feature = "archive")]
        if let Some(token) = data.strip_prefix("arc:name:") {
            return self
                .confirm_archive(
                    chat_id,
                    user_id,
                    token,
                    ArchiveConfirmAction::ArchiveNamePrefix {
                        include_category: false,
                    },
                )
                .await;
        }
        #[cfg(feature = "archive")]
        if let Some(token) = data.strip_prefix("arc:namecat:") {
            return self
                .confirm_archive(
                    chat_id,
                    user_id,
                    token,
                    ArchiveConfirmAction::ArchiveNamePrefix {
                        include_category: true,
                    },
                )
                .await;
        }
        #[cfg(feature = "archive")]
        if let Some(token) = data.strip_prefix("arc:no:") {
            return self
                .confirm_archive(chat_id, user_id, token, ArchiveConfirmAction::Cancel)
                .await;
        }

        let mut profile = self.store.get_profile(user_id).await;

        if let Some(key) = data.strip_prefix(crate::telegram::LICENSE_CALLBACK_PREFIX) {
            let Some(license) = License::parse(key) else {
                return Ok(());
            };
            profile.license = license;
            if profile.onboarding_step == OnboardingStep::AwaitingLicense {
                profile.onboarding_step = OnboardingStep::AwaitingPrefix;
                touch(&mut profile);
                self.store.put_profile(user_id, &profile).await?;
                return self
                    .prompt_step(chat_id, OnboardingStep::AwaitingPrefix)
                    .await;
            }
            touch(&mut profile);
            self.store.put_profile(user_id, &profile).await?;
            let text = format!("License set to <b>{}</b>.", escape_html(license.label()));
            return self.telegram.send_message(chat_id, &text, None).await;
        }

        if data == "onb:oauth" {
            return self.start_oauth(chat_id, user_id).await;
        }

        if data == "onb:botpass" {
            profile.onboarding_step = OnboardingStep::AwaitingUsername;
            touch(&mut profile);
            self.store.put_profile(user_id, &profile).await?;
            return self.send_botpassword_prompt(chat_id).await;
        }

        if data == "onb:username" {
            profile.onboarding_step = OnboardingStep::AwaitingUsername;
            touch(&mut profile);
            self.store.put_profile(user_id, &profile).await?;
            return self.send_botpassword_prompt(chat_id).await;
        }

        if data == "onb:skipprefix" {
            #[cfg(feature = "archive")]
            if profile.onboarding_step == OnboardingStep::AwaitingArchivePrefix {
                return self.prompt_current_archive_prefix(chat_id, user_id).await;
            }
            profile.filename_prefix = String::new();
            profile.onboarding_step = OnboardingStep::Done;
            touch(&mut profile);
            self.store.put_profile(user_id, &profile).await?;
            return self
                .telegram
                .send_message(chat_id, ONBOARDING_DONE_MSG, None)
                .await;
        }

        let (label, value) = match data.as_str() {
            "set:links" => {
                profile.return_upload_links = !profile.return_upload_links;
                ("Return upload links", profile.return_upload_links)
            }
            "set:catlinks" => {
                profile.return_category_links = !profile.return_category_links;
                ("Return category links", profile.return_category_links)
            }
            "set:misscat" => {
                profile.return_missing_category_links = !profile.return_missing_category_links;
                (
                    "Return non-existing category links",
                    profile.return_missing_category_links,
                )
            }
            #[cfg(feature = "archive")]
            "set:arclist" => {
                profile.return_archive_file_list = !profile.return_archive_file_list;
                ("Return archive file list", profile.return_archive_file_list)
            }
            #[cfg(feature = "archive")]
            "set:arcconfirm" => {
                profile.archive_confirm = !profile.archive_confirm;
                ("Confirm archive before upload", profile.archive_confirm)
            }
            _ => return Ok(()),
        };
        touch(&mut profile);
        self.store.put_profile(user_id, &profile).await?;
        let text = format!("{label}: <b>{}</b>", on_off(value));
        self.telegram
            .send_message(chat_id, &text, Some(settings_keyboard(&profile)))
            .await
    }

    /// Shows or updates settings.
    async fn cmd_settings(&self, chat_id: i64, user_id: i64, argument: &str) -> Result<()> {
        let mut profile = self.store.get_profile(user_id).await;
        if argument.is_empty() {
            return self
                .telegram
                .send_message(
                    chat_id,
                    &settings_overview(&profile),
                    Some(settings_keyboard(&profile)),
                )
                .await;
        }
        let mut parts = argument.splitn(2, char::is_whitespace);
        let key = parts.next().unwrap_or("");
        let rest = parts.next().unwrap_or("").trim();
        match key {
            "prefix" => {
                profile.filename_prefix = rest.to_string();
                touch(&mut profile);
                self.store.put_profile(user_id, &profile).await?;
                let text = format!("Filename prefix set to <code>{}</code>.", escape_html(rest));
                self.telegram.send_message(chat_id, &text, None).await
            }
            "categories" => {
                profile.default_categories = parse_category_list(rest);
                touch(&mut profile);
                self.store.put_profile(user_id, &profile).await?;
                let text = format!(
                    "Default categories set to: {}",
                    if profile.default_categories.is_empty() {
                        "(none)".to_string()
                    } else {
                        escape_html(&profile.default_categories.join(", "))
                    }
                );
                self.telegram.send_message(chat_id, &text, None).await
            }
            "license" => {
                if let Some(license) = License::parse(rest) {
                    profile.license = license;
                    touch(&mut profile);
                    self.store.put_profile(user_id, &profile).await?;
                    let text = format!("License set to <b>{}</b>.", escape_html(license.label()));
                    self.telegram.send_message(chat_id, &text, None).await
                } else {
                    self.telegram
                        .send_message(
                            chat_id,
                            "Unknown license. Use one of: cc-by-4.0, cc-by-sa-4.0, cc-zero.",
                            None,
                        )
                        .await
                }
            }
            _ => {
                self.telegram
                    .send_message(
                        chat_id,
                        &settings_overview(&profile),
                        Some(settings_keyboard(&profile)),
                    )
                    .await
            }
        }
    }

    /// Deletes the user's stored credentials and profile.
    async fn cmd_forget(&self, chat_id: i64, user_id: i64) -> Result<()> {
        self.store.delete_profile(user_id).await?;
        self.telegram
            .send_message(
                chat_id,
                "🗑 Your stored credentials and settings were deleted. You can also revoke the bot password at https://commons.wikimedia.org/wiki/Special:BotPasswords",
                None,
            )
            .await
    }

    /// Cancels an in-progress onboarding step.
    async fn cmd_cancel(&self, chat_id: i64, user_id: i64) -> Result<()> {
        let mut profile = self.store.get_profile(user_id).await;
        #[cfg(feature = "archive")]
        if profile.onboarding_step == OnboardingStep::AwaitingArchivePrefix {
            remove_pending_archives_for_user(user_id);
            profile.onboarding_step = OnboardingStep::Done;
            touch(&mut profile);
            self.store.put_profile(user_id, &profile).await?;
            return self
                .telegram
                .send_message(chat_id, "✖ Archive upload cancelled.", None)
                .await;
        }
        if profile.is_ready() {
            profile.onboarding_step = OnboardingStep::Done;
            touch(&mut profile);
            self.store.put_profile(user_id, &profile).await?;
        }
        self.telegram
            .send_message(chat_id, "Cancelled. Use /start or /settings.", None)
            .await
    }

    /// Shows aggregate stats to administrators.
    async fn cmd_stat(&self, chat_id: i64, user_id: i64) -> Result<()> {
        if !self.config.is_admin(user_id) {
            return self
                .telegram
                .send_message(chat_id, "This command is for administrators only.", None)
                .await;
        }
        self.telegram.send_chat_action(chat_id, "typing").await.ok();
        let stats = self.store.aggregate_stats().await.unwrap_or_default();
        let text = format!(
            "📊 <b>Stats</b>\nUsers: <b>{}</b>\nTotal uploads: <b>{}</b>",
            stats.users, stats.uploads
        );
        self.telegram.send_message(chat_id, &text, None).await
    }

    /// Sends the help message with usage, limits, contact, and related projects.
    async fn send_help(&self, chat_id: i64, user_id: i64) -> Result<()> {
        let profile = self.store.get_profile(user_id).await;
        let uploads_line = match &profile.commons_username {
            Some(username) => {
                let account = username
                    .split('@')
                    .next()
                    .unwrap_or(username)
                    .replace(' ', "_");
                format!(
                    "\n\n📂 Your uploads: https://commons.wikimedia.org/wiki/Special:ListFiles/{account}"
                )
            }
            None => String::new(),
        };
        let max_upload_size = format_size_limit(self.config.max_file_bytes);
        let conversion_limit = format_size_limit(self.config.max_conversion_file_bytes);
        let archive_limit = format_size_limit(self.config.max_archive_file_bytes);
        let text = format!(
            "🖼 <b>Wikimedia Commons uploader</b> ({BOT_USERNAME})\n\nSend me a photo or file and I upload it to <b>Wikimedia Commons</b> under your own account.\n\n📎 <b>Send images as files</b> (attach → File), not as compressed photos, to preserve the original quality.\n\n⚠️ <b>Uploads are public</b> and reusable, even commercially; storage is unlimited, but files you may not share get deleted.\n• ✅ Best: <b>your own</b> photos (nature, animals, food, events) and your own art or scans.\n• ❌ Files from other sites/social media, screenshots, posters, most logos/covers — <b>usually</b> copyrighted (a few exceptions).\n• ✅ Others' work only under a free license: CC BY, CC BY-SA, CC0 or public domain — <b>not</b> NC (Non-Commercial).\n• 📚 Public domain when old: ~<b>70 years after the author's death</b> (<b>50</b> in Belarus), varies by country; photos of buildings/statues also need Freedom of Panorama.\nWhat may be uploaded: https://commons.wikimedia.org/wiki/Commons:Licensing\n\n<b>Set up</b>: run /start, then connect with <b>OAuth</b> (recommended) or a <b>bot password</b> (tick Upload new files + Create, edit, and move pages at https://commons.wikimedia.org/wiki/Special:BotPasswords).\n\n<b>In a caption</b> (per file, whole album too): <code>Categories: A, B</code>, <code>Source: …</code>, <code>Author: …</code>, <code>Date: 2009-12-03</code>, <code>Coord: &lt;map link or lat,lon&gt;</code>.\n\n<b>Set your defaults</b> any time (for future uploads): <code>category …</code>, <code>author …</code>, <code>prefix …</code>, <code>description …</code>, <code>lang ru</code>, <code>license {{PD-RU-exempt}}</code> — colon optional; short aliases <code>c/a/p/d/l</code>.\n\n<b>Accepted</b>: JPEG, PNG, GIF, SVG, TIFF, WebP, PDF, DjVu, audio (WAV, MP3, OGG, Opus, FLAC), video (WebM, OGV). DNG, HEIC and BMP are converted to WebP automatically (HEIC→WebP; DNG is developed from raw, or its embedded full-resolution JPEG is extracted).\n<b>Max size</b>: {max_upload_size} for accepted files; conversions are limited to {conversion_limit}; archives are limited to {archive_limit}.\n\n<b>Commands</b>: /start, /settings, /forget, /help\n\nMade by {CONTACT} — message me for help or uploading assistance.\n\n<b>Related projects</b>:\n• Browse Commons in Telegram: {RELATED_BROWSE_BOT}\n• gThumb extension: {RELATED_GTHUMB}\n• Browser upload extension: {RELATED_WEB_EXTENSION}\n• CLI upload tool: {RELATED_CLI}\n• Dark Wikipedia theme: {RELATED_DARK_THEME}\n• Wikipedia → man pages: {RELATED_WIKI2MAN}\n\nSource: {}{uploads_line}",
            self.config.github_url
        );
        #[cfg(feature = "archive")]
        let text = format!(
            "{text}\n\n📦 <b>Archives</b>: send a <b>.zip</b> (or .rar) and I upload the images inside under one caption/categories. In /settings you can show the archive's file list and require a thumbnail + <b>Confirm</b> step before uploading."
        );
        self.telegram.send_message(chat_id, &text, None).await
    }

    /// Resolves how to authenticate uploads for a profile, plus the author username.
    ///
    /// Prefers a stored OAuth token; falls back to the bot-password credentials.
    fn resolve_auth(&self, profile: &Profile) -> Result<(UploadAuth, String)> {
        let cipher = self
            .cipher
            .as_ref()
            .context("the bot is missing its encryption key; cannot read your credentials")?;
        if let Some(ciphertext) = &profile.oauth_ciphertext {
            let decoded = cipher
                .decrypt(ciphertext)
                .context("failed to decrypt stored OAuth token")?;
            let (token, secret) = decoded
                .split_once('\n')
                .context("stored OAuth token is malformed")?;
            let author = profile.commons_username.clone().unwrap_or_default();
            return Ok((
                UploadAuth::OAuth {
                    token: token.to_string(),
                    secret: secret.to_string(),
                },
                author,
            ));
        }
        let username = profile
            .commons_username
            .clone()
            .context("no Commons account is connected — run /start")?;
        let ciphertext = profile
            .credential_ciphertext
            .as_deref()
            .context("no credentials stored — run /start")?;
        let password = cipher
            .decrypt(ciphertext)
            .context("failed to decrypt stored credentials")?;
        Ok((
            UploadAuth::BotPassword {
                username: username.clone(),
                password,
            },
            username,
        ))
    }

    /// Runs one file through convert/dup-check/build/upload and returns a structured result.
    ///
    /// Performs no user messaging; callers translate the [`FileResult`] into replies.
    #[allow(clippy::too_many_arguments)]
    async fn process_one_file(
        &self,
        profile: &Profile,
        caption: &str,
        extra_categories: &[String],
        original: TelegramFile,
        file_name: Option<&str>,
        mime: Option<&str>,
        unique_id: &str,
        auth: &UploadAuth,
        bot_password_session: Option<&CommonsBotPasswordSession>,
        author_username: &str,
    ) -> Result<FileResult> {
        let parsed = parse_caption(caption);
        let categories = upload_categories(
            &parsed.categories,
            extra_categories,
            &profile.default_categories,
        );
        let metadata = metadata_from_telegram_file(&original);

        // Convert DNG/HEIC/BMP to WebP; pass everything else through if Commons accepts it.
        let sniff = original.read_prefix(FILE_SNIFF_BYTES)?;
        let format = convert::classify(file_name, mime, &sniff);
        let mut provenance = UploadProvenance {
            original_filename: file_name
                .map(str::to_string)
                .unwrap_or_else(|| format!("telegram_{unique_id}")),
            ..UploadProvenance::default()
        };
        let (upload_data, extension) = if format.needs_conversion() {
            if original.len() > self.config.max_conversion_file_bytes {
                let limit = format_size_limit(self.config.max_conversion_file_bytes);
                return Ok(FileResult::Rejected {
                    reason: format!(
                        "This format needs conversion, and conversion is currently limited to {limit}"
                    ),
                });
            }
            let original = original.into_bytes()?;
            provenance.original_sha1 = Some(sha1_hex(&original));
            provenance.original_md5 = Some(md5_hex(&original));
            match convert::convert(&original, format, self.config.webp_quality) {
                Ok((bytes, ext)) => (UploadData::Bytes(bytes), ext.to_string()),
                Err(error) => {
                    return Ok(FileResult::Rejected {
                        reason: format!("Couldn't convert this file: {error}"),
                    });
                }
            }
        } else {
            let extension = convert::passthrough_extension(file_name, mime);
            if !convert::is_commons_accepted(&extension) {
                return Ok(FileResult::Rejected {
                    reason: format!("Commons does not accept .{extension} files"),
                });
            }
            (upload_data_from_telegram_file(original), extension)
        };

        // Duplicate pre-check by content hash.
        let upload_sha1 = sha1_hex_upload_data(&upload_data)?;
        if let Ok(existing) = self.commons.find_by_sha1(&upload_sha1).await
            && !existing.is_empty()
        {
            return Ok(FileResult::Duplicate { titles: existing });
        }

        // Build the filename: caption text as a descriptive prefix and the original stem
        // for per-file uniqueness (emoji dropped, newlines collapsed by build_filename).
        let filename = build_filename(
            &profile.filename_prefix,
            &parsed.description,
            file_stem(file_name),
            &extension,
            unique_id,
        );
        let (latitude, longitude) = match parsed.coordinates.or_else(|| metadata.coordinates()) {
            Some((latitude, longitude)) => (Some(latitude), Some(longitude)),
            None => (None, None),
        };
        let date = parsed
            .date
            .clone()
            .or_else(|| metadata.date.clone())
            .unwrap_or_else(today_iso);
        let description = if parsed.description.trim().is_empty() {
            profile.default_description.clone().unwrap_or_default()
        } else {
            parsed.description.clone()
        };
        let wikitext = build_wikitext(&DescriptionParams {
            description: &description,
            author_username,
            author_override: parsed
                .author
                .as_deref()
                .or(profile.default_author.as_deref()),
            source: parsed.source.as_deref(),
            license: profile.license,
            license_override: profile.license_override.as_deref(),
            lang: profile.default_lang.as_deref(),
            categories: &categories,
            date: &date,
            latitude,
            longitude,
            provenance: &provenance,
        });

        let request = UploadRequest {
            auth: auth.clone(),
            filename: filename.clone(),
            data: upload_data,
            wikitext,
            comment: format!("Uploaded via Telegram bot {BOT_USERNAME}"),
        };
        let outcome = if let Some(session) = bot_password_session {
            self.commons
                .upload_with_bot_password_session(session, &request)
                .await?
        } else {
            self.commons.upload(&request).await?
        };

        Ok(match outcome {
            UploadOutcome::Success { url, .. } => FileResult::Uploaded {
                filename,
                url,
                categories,
            },
            UploadOutcome::Failed { message } => FileResult::Failed { message },
        })
    }

    /// Downloads, converts if needed, and uploads one file to Commons.
    async fn handle_upload(&self, chat_id: i64, user_id: i64, message: &Message) -> Result<()> {
        let mut profile = self.store.get_profile(user_id).await;
        if !profile.is_ready() {
            // For an album, nudge once instead of for every photo.
            let should_nudge = match &message.media_group_id {
                Some(group) => self
                    .store
                    .reserve_idempotency(&format!("ONBOARD_NUDGE#{chat_id}#{group}"), 300)
                    .await
                    .unwrap_or(true),
                None => true,
            };
            if !should_nudge {
                return Ok(());
            }
            let text = if profile.onboarding_step == OnboardingStep::AwaitingArchivePrefix {
                "Send a filename prefix for the pending archive first 👇"
            } else {
                "Let's finish connecting your Commons account first 👇"
            };
            self.telegram.send_message(chat_id, text, None).await.ok();
            let step = if profile.onboarding_step == OnboardingStep::Done {
                OnboardingStep::AwaitingUsername
            } else {
                profile.onboarding_step
            };
            #[cfg(feature = "archive")]
            if step == OnboardingStep::AwaitingArchivePrefix {
                return self.prompt_current_archive_prefix(chat_id, user_id).await;
            }
            return self.prompt_step(chat_id, step).await;
        }
        let Some(file) = extract_file(message) else {
            return Ok(());
        };
        if file
            .file_size
            .is_some_and(|size| size > self.config.max_file_bytes)
        {
            return self.reject_too_large(chat_id).await;
        }

        #[cfg(feature = "archive")]
        let mut archive_chat_action = if crate::archive::is_archive(file.file_name.as_deref(), &[])
        {
            self.telegram.send_chat_action(chat_id, "typing").await.ok();
            Some(ChatActionGuard::start(
                self.telegram.clone(),
                chat_id,
                "typing",
            ))
        } else {
            None
        };

        #[cfg(feature = "archive")]
        if archive_chat_action.is_none() {
            self.telegram
                .send_chat_action(chat_id, "upload_document")
                .await
                .ok();
        }
        #[cfg(not(feature = "archive"))]
        self.telegram
            .send_chat_action(chat_id, "upload_document")
            .await
            .ok();
        let original = match self
            .telegram
            .resolve_by_file_id(&file.file_id, self.config.max_file_bytes)
            .await
        {
            Ok(file) => file,
            Err(error) => {
                tracing::warn!(error = %format!("{error:#}"), "download failed");
                return self.reject_too_large(chat_id).await;
            }
        };

        // Archives (zip/rar) are expanded and each member uploaded — VM-only feature.
        #[cfg(feature = "archive")]
        {
            let sniff = match original.read_prefix(FILE_SNIFF_BYTES) {
                Ok(sniff) => sniff,
                Err(error) => {
                    tracing::warn!(error = %format!("{error:#}"), "failed to inspect Telegram file");
                    return self
                        .telegram
                        .send_message(
                            chat_id,
                            "❌ Couldn't read this Telegram file. Please resend it.",
                            None,
                        )
                        .await;
                }
            };
            if crate::archive::is_archive(file.file_name.as_deref(), &sniff) {
                if archive_chat_action.is_none() {
                    self.telegram.send_chat_action(chat_id, "typing").await.ok();
                    archive_chat_action.get_or_insert_with(|| {
                        ChatActionGuard::start(self.telegram.clone(), chat_id, "typing")
                    });
                }
                if original.len() > self.config.max_archive_file_bytes {
                    return self.reject_archive_too_large(chat_id).await;
                }
                let original = match original.into_bytes() {
                    Ok(bytes) => bytes,
                    Err(error) => {
                        tracing::warn!(error = %format!("{error:#}"), "failed to read archive");
                        return self
                            .telegram
                            .send_message(
                                chat_id,
                                "❌ Couldn't read this Telegram archive. Please resend it.",
                                None,
                            )
                            .await;
                    }
                };
                return self
                    .handle_archive(chat_id, user_id, &mut profile, message, original)
                    .await;
            }
        }

        let (auth, author_username) = match self.resolve_auth(&profile) {
            Ok(resolved) => resolved,
            Err(error) => {
                return self
                    .telegram
                    .send_message(chat_id, &escape_html(&format!("{error}")), None)
                    .await;
            }
        };
        // Resolve the description and categories, sharing an album's caption across photos.
        let caption = self.resolve_caption(message).await;

        self.telegram
            .send_chat_action(chat_id, "upload_document")
            .await
            .ok();
        let result = self
            .process_one_file(
                &profile,
                &caption,
                &[],
                original,
                file.file_name.as_deref(),
                file.mime.as_deref(),
                &file.file_unique_id,
                &auth,
                None,
                &author_username,
            )
            .await?;

        match result {
            FileResult::Uploaded {
                filename,
                url,
                categories,
            } => {
                profile.uploads_count = profile.uploads_count.saturating_add(1);
                touch(&mut profile);
                self.store.put_profile(user_id, &profile).await.ok();
                self.send_success(
                    chat_id,
                    &filename,
                    &url,
                    &categories,
                    &profile,
                    file.compressed_photo,
                )
                .await
            }
            FileResult::Duplicate { titles } => {
                let links = titles
                    .iter()
                    .map(|title| format!("• {}", commons_title_url(title)))
                    .collect::<Vec<_>>()
                    .join("\n");
                let text =
                    format!("⚠️ This exact file already exists on Commons:\n{links}\n\nSkipped.");
                self.telegram.send_message(chat_id, &text, None).await
            }
            FileResult::Rejected { reason } => {
                let text = format!(
                    "❌ {}.\n\nAccepted: JPEG, PNG, GIF, SVG, TIFF, WebP, PDF, DjVu, audio (WAV/MP3/OGG/Opus/FLAC), video (WebM/OGV). DNG, HEIC and BMP are converted automatically.",
                    escape_html(&reason)
                );
                self.telegram.send_message(chat_id, &text, None).await
            }
            FileResult::Failed { message } => {
                self.telegram
                    .send_message(chat_id, &escape_html(&message), None)
                    .await
            }
        }
    }

    /// Resolves the caption for a message, sharing an album's caption across its photos.
    async fn resolve_caption(&self, message: &Message) -> String {
        if let Some(group_id) = &message.media_group_id {
            if let Some(caption) = message.caption.as_ref() {
                self.store.put_group_caption(group_id, caption).await.ok();
                return caption.clone();
            }
            return self
                .store
                .get_group_caption(group_id)
                .await
                .unwrap_or_default();
        }
        message.caption.clone().unwrap_or_default()
    }

    /// Sends the post-upload confirmation, honoring the user's link settings.
    async fn send_success(
        &self,
        chat_id: i64,
        filename: &str,
        url: &str,
        categories: &[String],
        profile: &Profile,
        compressed_photo: bool,
    ) -> Result<()> {
        let mut text = if profile.return_upload_links {
            format!(
                "✅ Uploaded: <a href=\"{url}\">{}</a>",
                escape_html(filename)
            )
        } else {
            format!("✅ Uploaded <code>{}</code>", escape_html(filename))
        };
        if compressed_photo {
            text.push_str("\nℹ️ This was a compressed photo; send it as a file for full quality.");
        }
        if profile.return_category_links && !categories.is_empty() {
            text.push_str("\n\n<b>Categories</b>:");
            for category in categories {
                text.push_str(&format!(
                    "\n• <a href=\"{}\">{}</a>",
                    category_url(category),
                    escape_html(category)
                ));
            }
        }
        if profile.return_missing_category_links
            && !categories.is_empty()
            && let Ok(missing) = self.commons.missing_categories(categories).await
            && !missing.is_empty()
        {
            text.push_str("\n\n<b>These categories don't exist yet</b> (create them?):");
            for category in missing {
                text.push_str(&format!(
                    "\n• <a href=\"{}\">{}</a>",
                    category_url(&category),
                    escape_html(&category)
                ));
            }
        }
        self.telegram.send_message(chat_id, &text, None).await
    }

    /// Tells the user a file is over the configured upload limit.
    async fn reject_too_large(&self, chat_id: i64) -> Result<()> {
        let limit = format_size_limit(self.config.max_file_bytes);
        let text = format!(
            "❌ That file is too large. This bot is configured to accept files up to {limit}. Please send a smaller export."
        );
        self.telegram.send_message(chat_id, &text, None).await
    }

    /// Tells the user an archive is over the configured archive processing limit.
    #[cfg(feature = "archive")]
    async fn reject_archive_too_large(&self, chat_id: i64) -> Result<()> {
        let archive_limit = format_size_limit(self.config.max_archive_file_bytes);
        let upload_limit = format_size_limit(self.config.max_file_bytes);
        let text = format!(
            "❌ Archives larger than {archive_limit} are not supported yet because they must be extracted before upload. Accepted files that do not need conversion can be uploaded up to {upload_limit}."
        );
        self.telegram.send_message(chat_id, &text, None).await
    }
}

/// Archive (zip/rar) handling — built only with the `archive` feature (VM-only).
#[cfg(feature = "archive")]
impl Bot {
    /// Expands an archive and either previews it (confirm flow) or uploads every image.
    async fn handle_archive(
        &self,
        chat_id: i64,
        user_id: i64,
        profile: &mut Profile,
        message: &Message,
        original: Vec<u8>,
    ) -> Result<()> {
        let file_name = message
            .document
            .as_ref()
            .and_then(|document| document.file_name.clone());
        // Evict expired or memory-pressuring staged archives before unpacking another.
        prune_pending(original.len());
        let entries = match crate::archive::extract_images_with_limit(
            &original,
            file_name.as_deref(),
            self.config.max_archive_file_bytes,
        ) {
            Ok(entries) => entries,
            Err(error) => {
                let text = format!(
                    "❌ Couldn't read the archive: {}",
                    escape_html(&format!("{error}"))
                );
                return self.telegram.send_message(chat_id, &text, None).await;
            }
        };

        if profile.return_archive_file_list {
            let list = entries
                .iter()
                .map(|entry| format!("• {}", escape_html(&entry.name)))
                .collect::<Vec<_>>()
                .join("\n");
            let count = entries.len();
            let text = format!(
                "📦 {count} {} inside the archive:\n{list}",
                image_count_label(count)
            );
            self.telegram.send_message(chat_id, &text, None).await.ok();
        }

        let caption = self.resolve_caption(message).await;
        let needs_prefix = archive_needs_filename_prefix(&entries);
        tracing::info!(
            user_id,
            chat_id,
            count = entries.len(),
            archive_file_name = file_name.as_deref().unwrap_or(""),
            needs_prefix,
            confirm_before_upload = profile.archive_confirm,
            "archive extracted"
        );

        if profile.archive_confirm || needs_prefix {
            let token = new_token();
            let count = entries.len();
            let sample_name = if needs_prefix {
                entries
                    .iter()
                    .find(|entry| archive_entry_needs_filename_prefix(&entry.name))
                    .map(|entry| entry.name.clone())
            } else {
                None
            };
            let pending = PendingArchive {
                user_id,
                caption,
                archive_file_name: file_name.clone(),
                entries,
                confirm_before_upload: profile.archive_confirm,
                created_at: now_ts(),
            };
            let persisted = match persist_pending_archive(&token, &pending) {
                Ok(()) => true,
                Err(error) => {
                    tracing::warn!(
                        token,
                        error = %format!("{error:#}"),
                        "failed to persist pending archive"
                    );
                    false
                }
            };
            archive_pending()
                .lock()
                .unwrap()
                .insert(token.clone(), pending);
            tracing::info!(
                user_id,
                chat_id,
                token,
                count,
                archive_file_name = file_name.as_deref().unwrap_or(""),
                needs_prefix,
                confirm_before_upload = profile.archive_confirm,
                "archive staged"
            );
            if needs_prefix {
                profile.onboarding_step = OnboardingStep::AwaitingArchivePrefix;
                touch(profile);
                self.store.put_profile(user_id, profile).await?;
                if profile.archive_confirm && persisted {
                    spawn_archive_thumbnail_preview(
                        self.config.clone(),
                        chat_id,
                        user_id,
                        token,
                        ArchivePreviewFollowup::Prefix {
                            count,
                            sample_name,
                            archive_file_name: file_name,
                        },
                    );
                    return Ok(());
                }
                return self
                    .prompt_archive_prefix(
                        chat_id,
                        count,
                        sample_name.as_deref(),
                        Some(&token),
                        file_name.as_deref(),
                    )
                    .await;
            }
            if profile.archive_confirm && persisted {
                spawn_archive_thumbnail_preview(
                    self.config.clone(),
                    chat_id,
                    user_id,
                    token,
                    ArchivePreviewFollowup::Confirm {
                        count,
                        archive_file_name: file_name,
                    },
                );
                return Ok(());
            }
            return self
                .send_archive_confirmation(chat_id, &token, count, None, file_name.as_deref())
                .await;
        }

        self.upload_entries(chat_id, user_id, profile, &caption, &[], entries)
            .await
    }

    /// Re-prompts for the current user's staged archive prefix, including any archive-name actions.
    #[cfg(feature = "archive")]
    async fn prompt_current_archive_prefix(&self, chat_id: i64, user_id: i64) -> Result<()> {
        let Some(pending) = pending_archive_summary_for_user(user_id) else {
            self.clear_archive_prefix_step(user_id).await?;
            return self
                .telegram
                .send_message(
                    chat_id,
                    "I no longer have the pending archive. Please resend the archive.",
                    None,
                )
                .await;
        };
        self.prompt_archive_prefix(
            chat_id,
            pending.count,
            pending.sample_name.as_deref(),
            Some(&pending.token),
            pending.archive_file_name.as_deref(),
        )
        .await
    }

    /// Leaves the archive-prefix prompt state if the staged archive flow is being resolved.
    #[cfg(feature = "archive")]
    async fn clear_archive_prefix_step(&self, user_id: i64) -> Result<()> {
        let mut profile = self.store.get_profile(user_id).await;
        if profile.onboarding_step == OnboardingStep::AwaitingArchivePrefix {
            profile.onboarding_step = OnboardingStep::Done;
            touch(&mut profile);
            self.store.put_profile(user_id, &profile).await?;
        }
        Ok(())
    }

    /// Asks for a required filename prefix before continuing an archive upload.
    async fn prompt_archive_prefix(
        &self,
        chat_id: i64,
        count: usize,
        sample_name: Option<&str>,
        token: Option<&str>,
        archive_file_name: Option<&str>,
    ) -> Result<()> {
        let sample = sample_name.unwrap_or("IMG_...");
        let image_label = image_count_label(count);
        let text = format!(
            "📦 Found <b>{count}</b> {image_label}. At least one filename starts with <code>IMG_</code>{example}, so send a short <b>filename prefix</b> before upload.\n\nExample: <code>Minsk trip</code>",
            example = if sample == "IMG_..." {
                String::new()
            } else {
                format!(" (for example <code>{}</code>)", escape_html(sample))
            }
        );
        let keyboard = token.map(|token| archive_prefix_keyboard(token, archive_file_name));
        self.telegram.send_message(chat_id, &text, keyboard).await
    }

    /// Sends the final archive upload confirmation.
    async fn send_archive_confirmation(
        &self,
        chat_id: i64,
        token: &str,
        count: usize,
        prefix: Option<&str>,
        archive_file_name: Option<&str>,
    ) -> Result<()> {
        let keyboard = InlineKeyboardMarkup {
            inline_keyboard: archive_confirmation_buttons(token, archive_file_name),
        };
        let image_label = image_count_label(count);
        let text = match prefix {
            Some(prefix) => format!(
                "Filename prefix set to <code>{}</code>.\n\n📦 Found <b>{count}</b> {image_label}. Confirm uploading to Wikimedia Commons?",
                escape_html(prefix)
            ),
            None => {
                format!(
                    "📦 Found <b>{count}</b> {image_label}. Confirm uploading to Wikimedia Commons?"
                )
            }
        };
        self.telegram
            .send_message(chat_id, &text, Some(keyboard))
            .await
    }

    /// Resolves a pending archive by token and uploads it (or cancels it).
    async fn confirm_archive(
        &self,
        chat_id: i64,
        user_id: i64,
        token: &str,
        action: ArchiveConfirmAction,
    ) -> Result<()> {
        let pending = take_pending_archive(token);
        let Some(pending) = pending else {
            tracing::warn!(
                user_id,
                chat_id,
                token,
                action = archive_confirm_action_name(&action),
                "pending archive not found during confirmation"
            );
            self.clear_archive_prefix_step(user_id).await?;
            return self
                .telegram
                .send_message(
                    chat_id,
                    "I no longer have the pending archive. Please resend the archive.",
                    None,
                )
                .await;
        };
        if pending.user_id != user_id {
            tracing::warn!(
                user_id,
                pending_user_id = pending.user_id,
                chat_id,
                token,
                "archive confirmation clicked by a different user"
            );
            return self
                .telegram
                .send_message(
                    chat_id,
                    "This archive confirmation belongs to another Telegram user.",
                    None,
                )
                .await;
        }
        let count = pending.entries.len();
        tracing::info!(
            user_id,
            chat_id,
            token,
            action = archive_confirm_action_name(&action),
            count,
            "archive confirmation received"
        );
        if matches!(action, ArchiveConfirmAction::Cancel) {
            self.clear_archive_prefix_step(user_id).await?;
            return self
                .telegram
                .send_message(chat_id, "✖ Archive upload cancelled.", None)
                .await;
        }
        let caption = pending.caption;
        let mut extra_categories = Vec::new();
        let mut prefix_message = String::new();
        match action {
            ArchiveConfirmAction::ArchiveNamePrefix { include_category } => {
                let Some(prefix) = archive_name_prefix(pending.archive_file_name.as_deref()) else {
                    return self
                        .telegram
                        .send_message(
                            chat_id,
                            "This archive does not have a usable filename for a prefix. Send a prefix manually.",
                            None,
                        )
                        .await;
                };

                let mut profile = self.store.get_profile(user_id).await;
                profile.filename_prefix = prefix.clone();
                if profile.onboarding_step == OnboardingStep::AwaitingArchivePrefix {
                    profile.onboarding_step = OnboardingStep::Done;
                }
                touch(&mut profile);
                self.store.put_profile(user_id, &profile).await?;

                if include_category
                    && let Some(category) =
                        archive_name_category(pending.archive_file_name.as_deref())
                {
                    extra_categories.push(category.clone());
                    prefix_message = format!(
                        "Using archive filename prefix <code>{}</code> and category <code>{}</code>.\n",
                        escape_html(&prefix),
                        escape_html(&category)
                    );
                } else {
                    prefix_message = format!(
                        "Using archive filename prefix <code>{}</code>.\n",
                        escape_html(&prefix)
                    );
                }
            }
            ArchiveConfirmAction::Start => {
                self.clear_archive_prefix_step(user_id).await?;
            }
            ArchiveConfirmAction::Cancel => {
                unreachable!("cancel action is handled before upload starts");
            }
        }
        let image_label = image_count_label(count);
        send_chat_action_best_effort(&self.telegram, chat_id, "typing").await;
        self.telegram
            .send_message(
                chat_id,
                &format!("{prefix_message}Uploading {count} {image_label} to Wikimedia Commons…"),
                None,
            )
            .await
            .ok();
        spawn_archive_upload(
            self.config.clone(),
            chat_id,
            user_id,
            caption,
            extra_categories,
            pending.entries,
        );
        Ok(())
    }

    /// Uploads every extracted image, replying with an aggregate summary.
    async fn upload_entries(
        &self,
        chat_id: i64,
        user_id: i64,
        profile: &mut Profile,
        caption: &str,
        extra_categories: &[String],
        entries: Vec<crate::archive::ArchiveEntry>,
    ) -> Result<()> {
        let entry_count = entries.len();
        let (auth, author_username) = match self.resolve_auth(profile) {
            Ok(resolved) => resolved,
            Err(error) => {
                return self
                    .telegram
                    .send_message(chat_id, &escape_html(&format!("{error}")), None)
                    .await;
            }
        };
        let bot_password_session = match &auth {
            UploadAuth::BotPassword { username, password } => {
                match self.commons.bot_password_session(username, password).await {
                    Ok(session) => Some(session),
                    Err(error) => {
                        return self
                            .telegram
                            .send_message(chat_id, &escape_html(&format!("{error}")), None)
                            .await;
                    }
                }
            }
            UploadAuth::OAuth { .. } => None,
        };

        send_chat_action_best_effort(&self.telegram, chat_id, "typing").await;
        let _typing = ChatActionGuard::start(self.telegram.clone(), chat_id, "typing");
        tracing::info!(
            user_id,
            chat_id,
            count = entry_count,
            "starting archive upload"
        );
        let (mut uploaded, mut duplicate, mut rejected, mut failed) = (0u32, 0u32, 0u32, 0u32);
        let mut rejected_reasons: Vec<(String, u32)> = Vec::new();
        let mut failed_reasons: Vec<(String, u32)> = Vec::new();
        for (index, entry) in entries.into_iter().enumerate() {
            let member_index = index + 1;
            let entry_name = entry.name.clone();
            tracing::info!(
                user_id,
                chat_id,
                member_index,
                count = entry_count,
                name = %entry_name,
                "uploading archive member"
            );
            send_chat_action_best_effort(&self.telegram, chat_id, "typing").await;
            let unique = short_id(&entry.bytes);
            match self
                .process_one_file(
                    profile,
                    caption,
                    extra_categories,
                    TelegramFile::Bytes(entry.bytes),
                    Some(&entry.name),
                    None,
                    &unique,
                    &auth,
                    bot_password_session.as_ref(),
                    &author_username,
                )
                .await
            {
                Ok(FileResult::Uploaded { filename, url, .. }) => {
                    uploaded += 1;
                    profile.uploads_count = profile.uploads_count.saturating_add(1);
                    if profile.return_upload_links {
                        let text = format!(
                            "✅ Uploaded {member_index}/{entry_count}: <a href=\"{url}\">{}</a>",
                            escape_html(&filename)
                        );
                        if let Err(error) = self.telegram.send_message(chat_id, &text, None).await {
                            tracing::warn!(
                                error = %format!("{error:#}"),
                                member_index,
                                count = entry_count,
                                "failed to send archive upload link"
                            );
                        }
                    }
                }
                Ok(FileResult::Duplicate { .. }) => duplicate += 1,
                Ok(FileResult::Rejected { reason }) => {
                    rejected += 1;
                    tracing::warn!(
                        user_id,
                        chat_id,
                        member_index,
                        count = entry_count,
                        name = %entry_name,
                        reason = %reason,
                        "archive member rejected"
                    );
                    record_archive_reason(&mut rejected_reasons, reason);
                }
                Ok(FileResult::Failed { message }) => {
                    failed += 1;
                    tracing::warn!(
                        user_id,
                        chat_id,
                        member_index,
                        count = entry_count,
                        name = %entry_name,
                        message = %message,
                        "archive member upload failed"
                    );
                    record_archive_reason(&mut failed_reasons, message);
                }
                Err(error) => {
                    failed += 1;
                    let message = format!("{error:#}");
                    tracing::warn!(
                        user_id,
                        chat_id,
                        member_index,
                        count = entry_count,
                        name = %entry_name,
                        error = %message,
                        "archive member upload failed"
                    );
                    record_archive_reason(&mut failed_reasons, message);
                }
            }
        }
        touch(profile);
        self.store.put_profile(user_id, profile).await.ok();

        let mut text = format!("📦 Archive done — ✅ {uploaded} uploaded");
        if duplicate > 0 {
            text.push_str(&format!(", ⚠️ {duplicate} duplicate"));
        }
        if rejected > 0 {
            text.push_str(&format!(", ⛔ {rejected} skipped"));
        }
        if failed > 0 {
            text.push_str(&format!(", ❌ {failed} failed"));
        }
        append_archive_reasons(&mut text, "Skipped reasons", &rejected_reasons);
        append_archive_reasons(&mut text, "Failed reasons", &failed_reasons);
        tracing::info!(
            user_id,
            chat_id,
            count = entry_count,
            uploaded,
            duplicate,
            rejected,
            failed,
            "finished archive upload"
        );
        self.telegram.send_message(chat_id, &text, None).await
    }
}

/// A staged archive awaiting the user's upload confirmation (in-memory; the VM is long-living).
#[cfg(feature = "archive")]
struct PendingArchive {
    user_id: i64,
    caption: String,
    archive_file_name: Option<String>,
    entries: Vec<crate::archive::ArchiveEntry>,
    confirm_before_upload: bool,
    /// Unix timestamp when staged, for TTL eviction.
    created_at: i64,
}

#[cfg(feature = "archive")]
struct PendingArchiveSummary {
    token: String,
    count: usize,
    archive_file_name: Option<String>,
    sample_name: Option<String>,
    confirm_before_upload: bool,
    created_at: i64,
}

#[cfg(feature = "archive")]
#[derive(serde::Deserialize, serde::Serialize)]
struct PendingArchiveManifest {
    user_id: i64,
    caption: String,
    #[serde(default)]
    archive_file_name: Option<String>,
    confirm_before_upload: bool,
    created_at: i64,
    entries: Vec<PendingArchiveManifestEntry>,
}

#[cfg(feature = "archive")]
#[derive(serde::Deserialize, serde::Serialize)]
struct PendingArchiveManifestEntry {
    name: String,
    file_name: String,
}

/// How long a staged-but-unconfirmed archive is kept before eviction (30 days).
#[cfg(feature = "archive")]
const PENDING_TTL_SECS: i64 = 30 * 24 * 60 * 60;

/// Free-memory headroom (bytes) required on top of the incoming archive before staging.
#[cfg(feature = "archive")]
const LOW_MEMORY_MARGIN: u64 = 256 * 1024 * 1024;

/// Returns true once a staged archive is older than [`PENDING_TTL_SECS`].
#[cfg(feature = "archive")]
fn pending_is_expired(created_at: i64, now: i64) -> bool {
    now.saturating_sub(created_at) >= PENDING_TTL_SECS
}

#[cfg(feature = "archive")]
fn archive_needs_filename_prefix(entries: &[crate::archive::ArchiveEntry]) -> bool {
    entries
        .iter()
        .any(|entry| archive_entry_needs_filename_prefix(&entry.name))
}

#[cfg(feature = "archive")]
fn archive_entry_needs_filename_prefix(name: &str) -> bool {
    name.starts_with("IMG_")
}

#[cfg(feature = "archive")]
fn pending_archive_summary_for_user(user_id: i64) -> Option<PendingArchiveSummary> {
    let now = now_ts();
    let mut map = archive_pending().lock().unwrap();
    map.retain(|_, pending| !pending_is_expired(pending.created_at, now));
    let mut summaries = map
        .iter()
        .filter(|(_, pending)| pending.user_id == user_id)
        .map(|(token, pending)| PendingArchiveSummary {
            token: token.clone(),
            count: pending.entries.len(),
            archive_file_name: pending.archive_file_name.clone(),
            sample_name: pending
                .entries
                .iter()
                .find(|entry| archive_entry_needs_filename_prefix(&entry.name))
                .map(|entry| entry.name.clone()),
            confirm_before_upload: pending.confirm_before_upload,
            created_at: pending.created_at,
        })
        .collect::<Vec<_>>();
    drop(map);

    summaries.extend(disk_pending_archive_summaries_for_user(user_id, now));
    summaries
        .into_iter()
        .max_by_key(|summary| summary.created_at)
}

#[cfg(feature = "archive")]
fn remove_pending_archives_for_user(user_id: i64) -> usize {
    let mut map = archive_pending().lock().unwrap();
    let before = map.len();
    map.retain(|_, pending| pending.user_id != user_id);
    let removed = before.saturating_sub(map.len());
    drop(map);
    removed + remove_disk_pending_archives_for_user(user_id)
}

#[cfg(feature = "archive")]
fn take_pending_archive(token: &str) -> Option<PendingArchive> {
    let pending = archive_pending().lock().unwrap().remove(token);
    let pending = pending.or_else(|| match load_pending_archive(token) {
        Ok(pending) => Some(pending),
        Err(error) => {
            tracing::warn!(
                token,
                error = %format!("{error:#}"),
                "failed to load persisted pending archive"
            );
            None
        }
    });
    if pending.is_some()
        && let Err(error) = remove_pending_archive_files(token)
    {
        tracing::warn!(
            token,
            error = %format!("{error:#}"),
            "failed to remove pending archive files"
        );
    }
    pending
}

#[cfg(feature = "archive")]
fn persist_pending_archive(token: &str, pending: &PendingArchive) -> Result<()> {
    let dir = pending_archive_token_dir(token);
    if dir.exists() {
        std::fs::remove_dir_all(&dir)
            .with_context(|| format!("failed to replace pending archive {}", dir.display()))?;
    }
    std::fs::create_dir_all(&dir)
        .with_context(|| format!("failed to create pending archive dir {}", dir.display()))?;

    let mut manifest_entries = Vec::with_capacity(pending.entries.len());
    for (index, entry) in pending.entries.iter().enumerate() {
        let file_name = format!("entry-{index:06}");
        let path = dir.join(&file_name);
        std::fs::write(&path, &entry.bytes)
            .with_context(|| format!("failed to write pending archive entry {}", path.display()))?;
        manifest_entries.push(PendingArchiveManifestEntry {
            name: entry.name.clone(),
            file_name,
        });
    }

    let manifest = PendingArchiveManifest {
        user_id: pending.user_id,
        caption: pending.caption.clone(),
        archive_file_name: pending.archive_file_name.clone(),
        confirm_before_upload: pending.confirm_before_upload,
        created_at: pending.created_at,
        entries: manifest_entries,
    };
    let manifest_path = pending_archive_manifest_path(token);
    let manifest_bytes =
        serde_json::to_vec(&manifest).context("failed to serialize pending archive manifest")?;
    std::fs::write(&manifest_path, manifest_bytes).with_context(|| {
        format!(
            "failed to write pending archive manifest {}",
            manifest_path.display()
        )
    })?;
    Ok(())
}

#[cfg(feature = "archive")]
fn load_pending_archive(token: &str) -> Result<PendingArchive> {
    let manifest = read_pending_archive_manifest(token)?;
    let now = now_ts();
    if pending_is_expired(manifest.created_at, now) {
        remove_pending_archive_files(token).ok();
        anyhow::bail!("pending archive expired");
    }
    let dir = pending_archive_token_dir(token);
    let mut entries = Vec::with_capacity(manifest.entries.len());
    for entry in manifest.entries {
        let path = dir.join(&entry.file_name);
        let bytes = std::fs::read(&path)
            .with_context(|| format!("failed to read pending archive entry {}", path.display()))?;
        entries.push(crate::archive::ArchiveEntry {
            name: entry.name,
            bytes,
        });
    }
    Ok(PendingArchive {
        user_id: manifest.user_id,
        caption: manifest.caption,
        archive_file_name: manifest.archive_file_name,
        entries,
        confirm_before_upload: manifest.confirm_before_upload,
        created_at: manifest.created_at,
    })
}

#[cfg(feature = "archive")]
fn read_pending_archive_manifest(token: &str) -> Result<PendingArchiveManifest> {
    let path = pending_archive_manifest_path(token);
    let bytes = std::fs::read(&path)
        .with_context(|| format!("failed to read pending archive manifest {}", path.display()))?;
    serde_json::from_slice(&bytes).with_context(|| {
        format!(
            "failed to parse pending archive manifest {}",
            path.display()
        )
    })
}

#[cfg(feature = "archive")]
fn disk_pending_archive_summaries_for_user(user_id: i64, now: i64) -> Vec<PendingArchiveSummary> {
    let Ok(dirs) = std::fs::read_dir(pending_archive_dir()) else {
        return Vec::new();
    };
    let mut summaries = Vec::new();
    for dir in dirs.flatten() {
        let Ok(file_type) = dir.file_type() else {
            continue;
        };
        if !file_type.is_dir() {
            continue;
        }
        let token = dir.file_name().to_string_lossy().to_string();
        let manifest = match read_pending_archive_manifest(&token) {
            Ok(manifest) => manifest,
            Err(error) => {
                tracing::warn!(
                    token,
                    error = %format!("{error:#}"),
                    "failed to read pending archive manifest"
                );
                continue;
            }
        };
        if pending_is_expired(manifest.created_at, now) {
            remove_pending_archive_files(&token).ok();
            continue;
        }
        if manifest.user_id != user_id {
            continue;
        }
        summaries.push(PendingArchiveSummary {
            token,
            count: manifest.entries.len(),
            archive_file_name: manifest.archive_file_name.clone(),
            sample_name: manifest
                .entries
                .iter()
                .find(|entry| archive_entry_needs_filename_prefix(&entry.name))
                .map(|entry| entry.name.clone()),
            confirm_before_upload: manifest.confirm_before_upload,
            created_at: manifest.created_at,
        });
    }
    summaries
}

#[cfg(feature = "archive")]
fn remove_disk_pending_archives_for_user(user_id: i64) -> usize {
    let Ok(dirs) = std::fs::read_dir(pending_archive_dir()) else {
        return 0;
    };
    let mut removed = 0;
    for dir in dirs.flatten() {
        let Ok(file_type) = dir.file_type() else {
            continue;
        };
        if !file_type.is_dir() {
            continue;
        }
        let token = dir.file_name().to_string_lossy().to_string();
        let Ok(manifest) = read_pending_archive_manifest(&token) else {
            continue;
        };
        if manifest.user_id == user_id && remove_pending_archive_files(&token).is_ok() {
            removed += 1;
        }
    }
    removed
}

#[cfg(feature = "archive")]
fn remove_pending_archive_files(token: &str) -> Result<()> {
    let dir = pending_archive_token_dir(token);
    if dir.exists() {
        std::fs::remove_dir_all(&dir)
            .with_context(|| format!("failed to remove pending archive dir {}", dir.display()))?;
    }
    Ok(())
}

#[cfg(feature = "archive")]
fn pending_archive_manifest_path(token: &str) -> PathBuf {
    pending_archive_token_dir(token).join("manifest.json")
}

#[cfg(feature = "archive")]
fn pending_archive_token_dir(token: &str) -> PathBuf {
    pending_archive_dir().join(token)
}

#[cfg(feature = "archive")]
fn pending_archive_dir() -> PathBuf {
    if let Ok(dir) = std::env::var("PENDING_ARCHIVE_DIR")
        && !dir.trim().is_empty()
    {
        return PathBuf::from(dir);
    }
    if let Ok(dir) = std::env::var("TOOL_DATA_DIR")
        && !dir.trim().is_empty()
    {
        return PathBuf::from(dir).join("pending-archives");
    }
    if let Ok(tool) = std::env::var("TOOLFORGE_TOOL")
        && !tool.trim().is_empty()
    {
        return PathBuf::from("/data/project")
            .join(tool)
            .join("pending-archives");
    }
    std::env::temp_dir().join("telegram-wikimedia-commons-uploader-pending-archives")
}

/// Parses `MemAvailable` (in kB) out of `/proc/meminfo` contents.
#[cfg(feature = "archive")]
fn parse_mem_available_kb(meminfo: &str) -> Option<u64> {
    meminfo.lines().find_map(|line| {
        let rest = line.strip_prefix("MemAvailable:")?;
        rest.split_whitespace().next()?.parse::<u64>().ok()
    })
}

/// Reads currently-available memory in bytes (Linux only); `None` if it can't be determined.
#[cfg(feature = "archive")]
fn available_memory_bytes() -> Option<u64> {
    let meminfo = std::fs::read_to_string("/proc/meminfo").ok()?;
    parse_mem_available_kb(&meminfo).map(|kb| kb.saturating_mul(1024))
}

/// True when staging an archive of `incoming_len` bytes risks exhausting RAM.
///
/// When available memory is unknown (`None`), returns false so we never evict blindly.
#[cfg(feature = "archive")]
fn is_low_memory(available: Option<u64>, incoming_len: usize) -> bool {
    match available {
        Some(available) => {
            let needed = (incoming_len as u64)
                .saturating_mul(3)
                .saturating_add(LOW_MEMORY_MARGIN);
            available < needed
        }
        None => false,
    }
}

/// Evicts expired staged archives, and — if memory is tight before unpacking another
/// archive of `incoming_len` bytes — drops all staged archives to reclaim RAM.
#[cfg(feature = "archive")]
fn prune_pending(incoming_len: usize) {
    let now = now_ts();
    let mut map = archive_pending().lock().unwrap();
    map.retain(|_, pending| !pending_is_expired(pending.created_at, now));
    if is_low_memory(available_memory_bytes(), incoming_len) && !map.is_empty() {
        let freed = map.len();
        map.clear();
        tracing::warn!(
            freed,
            "cleared staged archives to free memory before unpacking"
        );
    }
}

/// Process-wide store of archives awaiting confirmation, keyed by a short token.
#[cfg(feature = "archive")]
fn archive_pending() -> &'static std::sync::Mutex<std::collections::HashMap<String, PendingArchive>>
{
    static PENDING: std::sync::OnceLock<
        std::sync::Mutex<std::collections::HashMap<String, PendingArchive>>,
    > = std::sync::OnceLock::new();
    PENDING.get_or_init(|| std::sync::Mutex::new(std::collections::HashMap::new()))
}

/// Returns a short random token for a pending-archive confirmation.
#[cfg(feature = "archive")]
fn new_token() -> String {
    let value: u64 = rand::random();
    format!("{value:x}")
}

/// Derives a short, content-based id from file bytes (keeps archive filenames unique
/// even when two archives contain different files with the same name).
#[cfg(feature = "archive")]
fn short_id(data: &[u8]) -> String {
    sha1_hex(data)[..10].to_string()
}

/// Extracts the best file attachment from a message, if any.
fn extract_file(message: &Message) -> Option<FileRef> {
    if let Some(document) = &message.document {
        return Some(FileRef {
            file_id: document.file_id.clone(),
            file_unique_id: document.file_unique_id.clone(),
            file_name: document.file_name.clone(),
            mime: document.mime_type.clone(),
            file_size: document.file_size,
            compressed_photo: false,
        });
    }
    if let Some(audio) = &message.audio {
        return Some(FileRef {
            file_id: audio.file_id.clone(),
            file_unique_id: audio.file_unique_id.clone(),
            file_name: audio.file_name.clone(),
            mime: audio.mime_type.clone(),
            file_size: audio.file_size,
            compressed_photo: false,
        });
    }
    if let Some(voice) = &message.voice {
        return Some(FileRef {
            file_id: voice.file_id.clone(),
            file_unique_id: voice.file_unique_id.clone(),
            file_name: None,
            mime: voice
                .mime_type
                .clone()
                .or_else(|| Some("audio/ogg".to_string())),
            file_size: voice.file_size,
            compressed_photo: false,
        });
    }
    if let Some(video) = &message.video {
        return Some(FileRef {
            file_id: video.file_id.clone(),
            file_unique_id: video.file_unique_id.clone(),
            file_name: video.file_name.clone(),
            mime: video.mime_type.clone(),
            file_size: video.file_size,
            compressed_photo: false,
        });
    }
    if let Some(photo) = message.photo.as_ref().and_then(|sizes| sizes.last()) {
        return Some(FileRef {
            file_id: photo.file_id.clone(),
            file_unique_id: photo.file_unique_id.clone(),
            file_name: None,
            mime: Some("image/jpeg".to_string()),
            file_size: photo.file_size,
            compressed_photo: true,
        });
    }
    None
}

/// Merges caption categories with the user's default categories, de-duplicating.
fn merge_categories(caption_categories: &[String], default_categories: &[String]) -> Vec<String> {
    let mut merged = Vec::new();
    for category in caption_categories.iter().chain(default_categories.iter()) {
        if !category.is_empty() && !merged.contains(category) {
            merged.push(category.clone());
        }
    }
    merged
}

fn upload_categories(
    caption_categories: &[String],
    extra_categories: &[String],
    default_categories: &[String],
) -> Vec<String> {
    let mut explicit_categories = Vec::new();
    for category in caption_categories.iter().chain(extra_categories.iter()) {
        if !category.is_empty() && !explicit_categories.contains(category) {
            explicit_categories.push(category.clone());
        }
    }
    merge_categories(&explicit_categories, default_categories)
}

/// Parses a comma-separated category list from a settings command.
fn parse_category_list(value: &str) -> Vec<String> {
    value
        .split(',')
        .map(crate::commons::sanitize_title)
        .filter(|category| !category.is_empty())
        .collect()
}

/// Builds the settings overview message.
fn settings_overview(profile: &Profile) -> String {
    let account = profile
        .commons_username
        .clone()
        .unwrap_or_else(|| "(not set)".to_string());
    let prefix = if profile.filename_prefix.is_empty() {
        "(none)".to_string()
    } else {
        profile.filename_prefix.clone()
    };
    let categories = if profile.default_categories.is_empty() {
        "(none)".to_string()
    } else {
        profile.default_categories.join(", ")
    };
    let mut text = format!(
        "⚙️ <b>Settings</b>\nCommons account: <code>{}</code>\nLicense: <b>{}</b>\nFilename prefix: <code>{}</code>\nDefault categories: {}\nReturn upload links: <b>{}</b>\nReturn category links: <b>{}</b>\nReturn non-existing category links: <b>{}</b>",
        escape_html(&account),
        escape_html(profile.license.label()),
        escape_html(&prefix),
        escape_html(&categories),
        on_off(profile.return_upload_links),
        on_off(profile.return_category_links),
        on_off(profile.return_missing_category_links),
    );
    #[cfg(feature = "archive")]
    {
        text.push_str(&format!(
            "\nArchive — return file list: <b>{}</b>\nArchive — confirm before upload: <b>{}</b>",
            on_off(profile.return_archive_file_list),
            on_off(profile.archive_confirm),
        ));
    }
    text.push_str(
        "\n\nButtons below toggle options and the license.\nText commands:\n<code>/settings prefix Your Prefix</code>\n<code>/settings categories Cat A, Cat B</code>\n<code>/settings license cc-by-4.0</code>",
    );
    text
}

/// Builds the settings inline keyboard (toggles plus license picker).
fn settings_keyboard(profile: &Profile) -> InlineKeyboardMarkup {
    let mut rows = vec![
        vec![toggle_button(
            "Upload links",
            "set:links",
            profile.return_upload_links,
        )],
        vec![toggle_button(
            "Category links",
            "set:catlinks",
            profile.return_category_links,
        )],
        vec![toggle_button(
            "Missing-category links",
            "set:misscat",
            profile.return_missing_category_links,
        )],
    ];
    #[cfg(feature = "archive")]
    {
        rows.push(vec![toggle_button(
            "Archive: list files",
            "set:arclist",
            profile.return_archive_file_list,
        )]);
        rows.push(vec![toggle_button(
            "Archive: confirm first",
            "set:arcconfirm",
            profile.archive_confirm,
        )]);
    }
    rows.extend(license_keyboard().inline_keyboard);
    InlineKeyboardMarkup {
        inline_keyboard: rows,
    }
}

/// Builds one toggle button labelled with its current state.
fn toggle_button(label: &str, data: &str, value: bool) -> InlineKeyboardButton {
    InlineKeyboardButton {
        text: format!("{label}: {}", on_off(value)),
        callback_data: Some(data.to_string()),
        url: None,
    }
}

/// Builds the keyboard offering OAuth or bot-password onboarding.
fn connect_method_keyboard() -> InlineKeyboardMarkup {
    InlineKeyboardMarkup {
        inline_keyboard: vec![
            vec![InlineKeyboardButton {
                text: "🔐 Connect with OAuth (recommended)".to_string(),
                callback_data: Some("onb:oauth".to_string()),
                url: None,
            }],
            vec![InlineKeyboardButton {
                text: "🔑 Use a bot password".to_string(),
                callback_data: Some("onb:botpass".to_string()),
                url: None,
            }],
        ],
    }
}

/// Builds a keyboard with a single button to restart username entry.
fn change_username_keyboard() -> InlineKeyboardMarkup {
    InlineKeyboardMarkup {
        inline_keyboard: vec![vec![InlineKeyboardButton {
            text: "✏️ Change username".to_string(),
            callback_data: Some("onb:username".to_string()),
            url: None,
        }]],
    }
}

/// Builds a keyboard with a single button to skip the filename prefix.
fn skip_prefix_keyboard() -> InlineKeyboardMarkup {
    InlineKeyboardMarkup {
        inline_keyboard: vec![vec![InlineKeyboardButton {
            text: "⏭ Skip (no prefix)".to_string(),
            callback_data: Some("onb:skipprefix".to_string()),
            url: None,
        }]],
    }
}

#[cfg(feature = "archive")]
fn archive_prefix_keyboard(token: &str, archive_file_name: Option<&str>) -> InlineKeyboardMarkup {
    let mut rows = Vec::new();
    if archive_name_prefix(archive_file_name).is_some() {
        rows.push(vec![InlineKeyboardButton {
            text: "Upload with prefix from archive name".to_string(),
            callback_data: Some(format!("arc:name:{token}")),
            url: None,
        }]);
        rows.push(vec![InlineKeyboardButton {
            text: "Upload with prefix and category from archive name".to_string(),
            callback_data: Some(format!("arc:namecat:{token}")),
            url: None,
        }]);
    }
    rows.push(vec![InlineKeyboardButton {
        text: "✖ Cancel".to_string(),
        callback_data: Some(format!("arc:no:{token}")),
        url: None,
    }]);
    InlineKeyboardMarkup {
        inline_keyboard: rows,
    }
}

#[cfg(feature = "archive")]
fn archive_confirmation_buttons(
    token: &str,
    archive_file_name: Option<&str>,
) -> Vec<Vec<InlineKeyboardButton>> {
    let mut rows = vec![vec![InlineKeyboardButton {
        text: "Start upload".to_string(),
        callback_data: Some(format!("arc:ok:{token}")),
        url: None,
    }]];
    if archive_name_prefix(archive_file_name).is_some() {
        rows.push(vec![InlineKeyboardButton {
            text: "Upload with prefix from archive name".to_string(),
            callback_data: Some(format!("arc:name:{token}")),
            url: None,
        }]);
        rows.push(vec![InlineKeyboardButton {
            text: "Upload with prefix and category from archive name".to_string(),
            callback_data: Some(format!("arc:namecat:{token}")),
            url: None,
        }]);
    }
    rows.push(vec![InlineKeyboardButton {
        text: "✖ Cancel".to_string(),
        callback_data: Some(format!("arc:no:{token}")),
        url: None,
    }]);
    rows
}

/// Renders a boolean as a human on/off label.
fn on_off(value: bool) -> &'static str {
    if value { "on" } else { "off" }
}

/// Adds a grouped archive failure/rejection reason while preserving first-seen order.
#[cfg(feature = "archive")]
fn record_archive_reason(reasons: &mut Vec<(String, u32)>, reason: String) {
    let reason = reason.trim().to_string();
    if reason.is_empty() {
        return;
    }
    if let Some((_, count)) = reasons
        .iter_mut()
        .find(|(existing, _)| existing.as_str() == reason.as_str())
    {
        *count = count.saturating_add(1);
        return;
    }
    reasons.push((reason, 1));
}

/// Appends a short grouped reason list to an archive summary.
#[cfg(feature = "archive")]
fn append_archive_reasons(text: &mut String, heading: &str, reasons: &[(String, u32)]) {
    if reasons.is_empty() {
        return;
    }
    const MAX_REASONS: usize = 5;
    text.push_str(&format!("\n\n<b>{heading}</b>:"));
    for (reason, count) in reasons.iter().take(MAX_REASONS) {
        if *count > 1 {
            text.push_str(&format!(
                "\n• {count} files: {}",
                escape_html(&truncate_reason(reason))
            ));
        } else {
            text.push_str(&format!("\n• {}", escape_html(&truncate_reason(reason))));
        }
    }
    if reasons.len() > MAX_REASONS {
        text.push_str(&format!(
            "\n… and {} more reason(s).",
            reasons.len() - MAX_REASONS
        ));
    }
}

/// Keeps a single reason compact enough for Telegram summary messages.
#[cfg(feature = "archive")]
fn truncate_reason(reason: &str) -> String {
    const MAX_CHARS: usize = 700;
    let mut chars = reason.chars();
    let truncated: String = chars.by_ref().take(MAX_CHARS).collect();
    if chars.next().is_some() {
        format!("{truncated}…")
    } else {
        truncated
    }
}

/// Returns a natural label for an image count.
#[cfg(feature = "archive")]
fn image_count_label(count: usize) -> &'static str {
    if count == 1 { "image" } else { "images" }
}

/// Summarizes a user's upload defaults after a settings directive.
fn defaults_summary(profile: &Profile) -> String {
    let mut parts = Vec::new();
    if !profile.default_categories.is_empty() {
        parts.push(format!(
            "categories: {}",
            profile.default_categories.join(", ")
        ));
    }
    if let Some(author) = &profile.default_author {
        parts.push(format!("author: {author}"));
    }
    if !profile.filename_prefix.is_empty() {
        parts.push(format!("filename prefix: {}", profile.filename_prefix));
    }
    if let Some(description) = &profile.default_description {
        parts.push(format!("description: {description}"));
    }
    if let Some(lang) = &profile.default_lang {
        parts.push(format!("language: {lang}"));
    }
    if let Some(license) = &profile.license_override {
        parts.push(format!("license: {license}"));
    }
    if parts.is_empty() {
        "✅ Upload defaults cleared.".to_string()
    } else {
        format!(
            "✅ Your upload defaults — {}",
            escape_html(&parts.join("; "))
        )
    }
}

/// Builds the OAuth client when a consumer is configured (otherwise OAuth is disabled).
fn build_oauth_client(config: &Config) -> Option<OAuthClient> {
    let key = config.oauth_consumer_key.clone()?;
    let secret = config.oauth_consumer_secret.clone()?;
    OAuthClient::new(
        Consumer { key, secret },
        OAuthEndpoints::wikimedia(),
        &config.user_agent,
    )
    .ok()
}

/// Updates the profile timestamps before a save.
fn touch(profile: &mut Profile) {
    let now = now_ts();
    if profile.created_at == 0 {
        profile.created_at = now;
    }
    profile.updated_at = now;
}

/// Returns the current unix timestamp in seconds.
fn now_ts() -> i64 {
    time::OffsetDateTime::now_utc().unix_timestamp()
}

/// Returns today's date as `YYYY-MM-DD` (UTC).
fn today_iso() -> String {
    time::OffsetDateTime::now_utc()
        .format(&time::macros::format_description!("[year]-[month]-[day]"))
        .unwrap_or_default()
}

/// Returns lower-case SHA-1 hex of the bytes.
fn sha1_hex(bytes: &[u8]) -> String {
    use sha1::{Digest, Sha1};
    hex::encode(Sha1::digest(bytes))
}

/// Returns lower-case SHA-1 hex of the upload data without loading disk-backed files.
fn sha1_hex_upload_data(data: &UploadData) -> Result<String> {
    match data {
        UploadData::Bytes(bytes) => Ok(sha1_hex(bytes)),
        UploadData::File { path, .. } => sha1_hex_path(path),
    }
}

/// Returns lower-case SHA-1 hex of a file, streaming it in bounded chunks.
fn sha1_hex_path(path: &Path) -> Result<String> {
    use sha1::{Digest, Sha1};
    use std::io::Read;

    let mut file = std::fs::File::open(path)
        .with_context(|| format!("failed to open upload file {}", path.display()))?;
    let mut hasher = Sha1::new();
    let mut buffer = [0u8; 1024 * 1024];
    loop {
        let read = file
            .read(&mut buffer)
            .with_context(|| format!("failed to read upload file {}", path.display()))?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(hex::encode(hasher.finalize()))
}

/// Returns lower-case MD5 hex of the bytes.
fn md5_hex(bytes: &[u8]) -> String {
    use md5::{Digest, Md5};
    hex::encode(Md5::digest(bytes))
}

/// Extracts metadata from either in-memory Telegram bytes or a local Bot API file path.
fn metadata_from_telegram_file(file: &TelegramFile) -> metadata::ImageMetadata {
    match file {
        TelegramFile::Bytes(bytes) => metadata::extract(bytes),
        TelegramFile::LocalPath { path, .. } => metadata::extract_path(path),
    }
}

/// Converts a resolved Telegram file into Commons upload data.
fn upload_data_from_telegram_file(file: TelegramFile) -> UploadData {
    match file {
        TelegramFile::Bytes(bytes) => UploadData::Bytes(bytes),
        TelegramFile::LocalPath { path, size } => UploadData::File { path, len: size },
    }
}

/// Formats a byte limit for user-facing messages.
fn format_size_limit(bytes: u64) -> String {
    let mb = bytes / (1024 * 1024);
    if mb >= 1024 {
        format!("{:.1} GB ({mb} MB)", mb as f64 / 1024.0)
    } else {
        format!("{mb} MB")
    }
}

/// Returns the filename stem (without extension), if a filename is present.
fn file_stem(file_name: Option<&str>) -> &str {
    file_name
        .map(|name| name.rsplit_once('.').map(|(stem, _)| stem).unwrap_or(name))
        .unwrap_or("")
}

#[cfg(feature = "archive")]
fn archive_name_stem(file_name: Option<&str>) -> Option<String> {
    let stem = file_stem(file_name).trim();
    if stem.is_empty() {
        return None;
    }
    let stem = stem.split_whitespace().collect::<Vec<_>>().join(" ");
    if stem.is_empty() { None } else { Some(stem) }
}

#[cfg(feature = "archive")]
fn archive_name_prefix(file_name: Option<&str>) -> Option<String> {
    let mut prefix = archive_name_stem(file_name)?;
    if !prefix.ends_with('_') {
        prefix.push('_');
    }
    Some(prefix)
}

#[cfg(feature = "archive")]
fn archive_name_category(file_name: Option<&str>) -> Option<String> {
    let category = crate::commons::sanitize_title(&archive_name_stem(file_name)?);
    if category.is_empty() {
        None
    } else {
        Some(category)
    }
}

/// Builds a Commons URL from a canonical `File:`/`Category:` title.
fn commons_title_url(title: &str) -> String {
    format!(
        "https://commons.wikimedia.org/wiki/{}",
        title.replace(' ', "_")
    )
}

#[cfg(test)]
mod tests {
    use super::{merge_categories, parse_category_list};

    #[test]
    fn merges_and_deduplicates_categories() {
        let merged = merge_categories(
            &["Minsk".to_string(), "Belarus".to_string()],
            &["Belarus".to_string(), "Travel".to_string()],
        );
        assert_eq!(merged, vec!["Minsk", "Belarus", "Travel"]);
    }

    #[test]
    fn parses_category_list_from_settings() {
        assert_eq!(
            parse_category_list("Minsk, Old town , , Belarus"),
            vec!["Minsk", "Old town", "Belarus"]
        );
    }
}

#[cfg(all(test, feature = "archive"))]
mod archive_tests {
    use super::{
        PENDING_TTL_SECS, PendingArchiveManifest, archive_confirmation_buttons,
        archive_entry_needs_filename_prefix, archive_name_category, archive_name_prefix,
        archive_needs_filename_prefix, archive_prefix_keyboard, is_low_memory,
        parse_mem_available_kb, pending_is_expired, short_id, upload_categories,
    };

    #[test]
    fn short_id_is_content_based_and_short() {
        let a = short_id(b"hello");
        assert_eq!(a.len(), 10);
        assert_eq!(a, short_id(b"hello"));
        assert_ne!(a, short_id(b"world"));
    }

    #[test]
    fn parses_mem_available_line() {
        let sample = "MemTotal:       16314072 kB\nMemFree:  123 kB\nMemAvailable:    8157036 kB\n";
        assert_eq!(parse_mem_available_kb(sample), Some(8_157_036));
        assert_eq!(parse_mem_available_kb("MemTotal: 1 kB\n"), None);
    }

    #[test]
    fn low_memory_needs_headroom_over_incoming() {
        let mb = 1024 * 1024;
        // 100 MB free, 50 MB incoming → needs 150 MB + 256 MB margin → low.
        assert!(is_low_memory(Some(100 * mb), 50 * mb as usize));
        // 2 GB free, 50 MB incoming → plenty.
        assert!(!is_low_memory(Some(2048 * mb), 50 * mb as usize));
        // Unknown availability → never evict.
        assert!(!is_low_memory(None, 999 * mb as usize));
    }

    #[test]
    fn pending_expiry_uses_ttl() {
        assert!(!pending_is_expired(1000, 1000));
        assert!(!pending_is_expired(1000, 1000 + PENDING_TTL_SECS - 1));
        assert!(pending_is_expired(1000, 1000 + PENDING_TTL_SECS));
    }

    #[test]
    fn archive_prefix_is_required_for_img_names() {
        assert!(archive_entry_needs_filename_prefix("IMG_5638.jpg"));
        assert!(!archive_entry_needs_filename_prefix("Minsk IMG_5638.jpg"));

        let entries = vec![
            crate::archive::ArchiveEntry {
                name: "DSC_0001.jpg".into(),
                bytes: Vec::new(),
            },
            crate::archive::ArchiveEntry {
                name: "IMG_0002.jpg".into(),
                bytes: Vec::new(),
            },
        ];
        assert!(archive_needs_filename_prefix(&entries));
    }

    #[test]
    fn archive_confirmation_buttons_include_archive_name_actions_between_start_and_cancel() {
        let rows = archive_confirmation_buttons("abc123", Some("Беларусь 2014.rar"));
        let labels = rows
            .iter()
            .map(|row| row[0].text.as_str())
            .collect::<Vec<_>>();
        assert_eq!(
            labels,
            vec![
                "Start upload",
                "Upload with prefix from archive name",
                "Upload with prefix and category from archive name",
                "✖ Cancel",
            ]
        );
        assert_eq!(rows[1][0].callback_data.as_deref(), Some("arc:name:abc123"));
        assert_eq!(
            rows[2][0].callback_data.as_deref(),
            Some("arc:namecat:abc123")
        );
    }

    #[test]
    fn archive_confirmation_buttons_skip_archive_name_actions_without_name() {
        let rows = archive_confirmation_buttons("abc123", None);
        let labels = rows
            .iter()
            .map(|row| row[0].text.as_str())
            .collect::<Vec<_>>();
        assert_eq!(labels, vec!["Start upload", "✖ Cancel"]);
    }

    #[test]
    fn archive_prefix_keyboard_shows_archive_name_actions_and_cancel_without_start() {
        let keyboard = archive_prefix_keyboard("abc123", Some("Беларусь 2014.rar"));
        let labels = keyboard
            .inline_keyboard
            .iter()
            .map(|row| row[0].text.as_str())
            .collect::<Vec<_>>();
        assert_eq!(
            labels,
            vec![
                "Upload with prefix from archive name",
                "Upload with prefix and category from archive name",
                "✖ Cancel",
            ]
        );
        assert_eq!(
            keyboard.inline_keyboard[0][0].callback_data.as_deref(),
            Some("arc:name:abc123")
        );
        assert_eq!(
            keyboard.inline_keyboard[1][0].callback_data.as_deref(),
            Some("arc:namecat:abc123")
        );
    }

    #[test]
    fn archive_prefix_keyboard_without_archive_name_still_allows_cancel() {
        let keyboard = archive_prefix_keyboard("abc123", None);
        let labels = keyboard
            .inline_keyboard
            .iter()
            .map(|row| row[0].text.as_str())
            .collect::<Vec<_>>();
        assert_eq!(labels, vec!["✖ Cancel"]);
    }

    #[test]
    fn archive_name_prefix_ends_with_one_underscore() {
        assert_eq!(
            archive_name_prefix(Some("Беларусь 2014.rar")).as_deref(),
            Some("Беларусь 2014_")
        );
        assert_eq!(
            archive_name_prefix(Some("Беларусь_2014_.zip")).as_deref(),
            Some("Беларусь_2014_")
        );
        assert_eq!(archive_name_prefix(Some("  .zip")), None);
    }

    #[test]
    fn archive_name_category_uses_archive_stem() {
        assert_eq!(
            archive_name_category(Some("Беларусь 2014.rar")).as_deref(),
            Some("Беларусь 2014")
        );
    }

    #[test]
    fn archive_name_category_with_commas_stays_single_category() {
        let category =
            archive_name_category(Some("2014,_Минск,Боруны,_Гольшаны,_и_еще_что_то.rar")).unwrap();
        let parsed = crate::commons::parse_caption("A caption\nCategories: Existing, Another");
        let categories =
            upload_categories(&parsed.categories, std::slice::from_ref(&category), &[]);
        assert_eq!(
            categories,
            vec![
                "Existing".to_string(),
                "Another".to_string(),
                "2014,_Минск,Боруны,_Гольшаны,_и_еще_что_то".to_string(),
            ]
        );
    }

    #[test]
    fn old_pending_archive_manifest_without_archive_file_name_still_loads() {
        let manifest: PendingArchiveManifest = serde_json::from_str(
            r#"{"user_id":1,"caption":"","confirm_before_upload":true,"created_at":1000,"entries":[]}"#,
        )
        .unwrap();
        assert_eq!(manifest.archive_file_name, None);
    }
}
