use crate::commons::{
    CommonsBotPasswordSession, CommonsClient, DescriptionParams, UploadAuth, UploadData,
    UploadOutcome, UploadRequest, build_filename, build_wikitext, category_url, parse_caption,
};
use crate::config::Config;
use crate::convert;
use crate::crypto::Cipher;
use crate::metadata;
use crate::models::{
    CallbackQuery, DngMode, License, Message, OnboardingStep, Profile, Update, UploadProvenance,
};
use crate::oauth::{Consumer, OAuthClient, OAuthEndpoints};
use crate::store::Store;
use crate::telegram::{
    InlineKeyboardButton, InlineKeyboardMarkup, TelegramClient, TelegramFile, escape_html,
    license_keyboard,
};
use anyhow::{Context, Result, bail};
use bytes::Bytes;
use http::{HeaderMap, Method, StatusCode};
use http_body_util::{BodyExt, Full};
use hyper::body::Incoming;
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Request as HyperRequest, Response as HyperResponse};
use hyper_util::rt::TokioIo;
use lambda_http::{Body, Request as LambdaRequest, Response as LambdaResponse};
use once_cell::sync::Lazy;
use std::collections::HashMap;
use std::convert::Infallible;
use std::ffi::OsString;
use std::io::Write;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use tokio::net::TcpListener;
use tokio::process::Command;
use tokio::sync::{RwLock, Semaphore};
use url::Url;

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
/// How long an in-flight webhook update can block duplicate delivery attempts.
const UPDATE_IN_PROGRESS_SECONDS: i64 = 10 * 60;
/// Error marker used to make duplicate in-flight webhooks retryable by Telegram.
const UPDATE_ALREADY_IN_PROGRESS_ERROR: &str = "telegram update is already being processed";
/// Message shown once onboarding is complete.
const ONBOARDING_DONE_MSG: &str = "✅ All set! Send me a photo or file and I'll upload it to Wikimedia Commons. Tip: a caption becomes the file's <b>description</b> and its <b>filename prefix</b>; add a line like <code>Categories: Minsk, Belarus</code> to set categories.";
/// Bytes needed to identify HEIC/BMP/archive magic without loading the full file.
const FILE_SNIFF_BYTES: usize = 512;
/// Telegram `sendPhoto` rejects uploaded photos larger than 10 MiB.
#[cfg(feature = "archive")]
const TELEGRAM_PHOTO_PREVIEW_MAX_BYTES: usize = 10 * 1024 * 1024;
/// Archive preview size where a visible progress message is useful before thumbnails.
#[cfg(feature = "archive")]
const ARCHIVE_PREVIEW_PROGRESS_MIN_IMAGES: usize = 10;
/// How many times an album item without a caption waits for another item's caption.
const MEDIA_GROUP_CAPTION_WAIT_ATTEMPTS: usize = 10;
/// Delay between album-caption checks.
const MEDIA_GROUP_CAPTION_WAIT_MS: u64 = 300;
/// Delay before uploading an album item after acknowledging its webhook update.
const MEDIA_GROUP_UPLOAD_DELAY_MS: u64 = 5_000;
/// Keeps deferred album uploads from starting all at once.
static MEDIA_GROUP_UPLOADS: Lazy<Semaphore> = Lazy::new(|| Semaphore::new(1));
/// How long an incomplete album progress counter is kept after its last update.
const MEDIA_GROUP_PROGRESS_TTL_SECONDS: i64 = 10 * 60;
/// Album upload progress keyed by `(chat_id, media_group_id)`.
static MEDIA_GROUP_PROGRESS: Lazy<std::sync::Mutex<HashMap<MediaGroupKey, MediaGroupProgress>>> =
    Lazy::new(|| std::sync::Mutex::new(HashMap::new()));
/// How long a text-only message can be reused as nearby upload caption context.
const TEXT_CONTEXT_TTL_SECONDS: i64 = 2 * 60;
/// How many times a generic single-file upload waits for adjacent text context.
const TEXT_CONTEXT_WAIT_ATTEMPTS: usize = 20;
/// Delay between adjacent-text context checks.
const TEXT_CONTEXT_WAIT_MS: u64 = 300;
/// Recently seen text-only messages keyed by `(chat_id, user_id)`.
static TEXT_CONTEXTS: Lazy<RwLock<HashMap<(i64, i64), TextContext>>> =
    Lazy::new(|| RwLock::new(HashMap::new()));
/// Maximum time spent downloading a direct URL.
const DIRECT_URL_DOWNLOAD_TIMEOUT_SECS: u64 = 20 * 60;
/// Maximum time spent downloading with yt-dlp.
const YTDLP_DOWNLOAD_TIMEOUT_SECS: u64 = 45 * 60;
/// Maximum time spent transcoding unsupported media.
const FFMPEG_TRANSCODE_TIMEOUT_SECS: u64 = 60 * 60;
/// Maximum single-file upload size currently documented by Wikimedia Commons.
const COMMONS_MAX_FILE_BYTES: u64 = 5 * 1024 * 1024 * 1024;
/// Documentation for Wikimedia Commons' maximum upload size.
const COMMONS_MAX_FILE_SIZE_DOC: &str =
    "https://commons.wikimedia.org/wiki/Commons:Maximum_file_size";
/// yt-dlp video selector: prefer AV1+Opus, but still download a file if a site has no AV1.
const YTDLP_VIDEO_FORMAT_SELECTOR: &str = "bestvideo[ext=webm][vcodec^=av01]+bestaudio[ext=webm][acodec=opus]/bestvideo[vcodec^=av01]+bestaudio[acodec=opus]/bestvideo+bestaudio/best";
/// yt-dlp audio selector for podcast-style links.
const YTDLP_AUDIO_FORMAT_SELECTOR: &str = "bestaudio/best";

/// Keeps Telegram's chat action visible while a long operation is running.
struct ChatActionGuard {
    stop: std::sync::mpsc::Sender<()>,
}

#[derive(Clone, Debug)]
struct TextContext {
    text: String,
    expires_at: i64,
}

/// Stable key for one Telegram media group inside a chat.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
struct MediaGroupKey {
    chat_id: i64,
    group_id: String,
}

/// In-memory upload counter for one deferred Telegram album.
#[derive(Clone, Debug)]
struct MediaGroupProgress {
    total: usize,
    completed: usize,
    updated_at: i64,
}

/// Position of a file inside a multi-file upload.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct UploadProgress {
    current: usize,
    total: usize,
}

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

impl Drop for ChatActionGuard {
    fn drop(&mut self) {
        let _ = self.stop.send(());
    }
}

/// Logs and suppresses failures when sending a best-effort Telegram chat action.
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

/// Returns true when updates can be acknowledged before deferred upload work starts.
fn defer_uploads_after_ack() -> bool {
    std::env::var("AWS_LAMBDA_RUNTIME_API").is_err()
}

/// Returns true when a single generic camera file should wait for adjacent text context.
fn should_defer_single_upload_for_text_context(message: &Message, file: &FileRef) -> bool {
    message.media_group_id.is_none()
        && message
            .caption
            .as_ref()
            .is_none_or(|caption| caption.trim().is_empty())
        && file
            .file_name
            .as_deref()
            .is_some_and(|file_name| file_stem(Some(file_name)).starts_with("IMG_"))
}

/// Uploads one media item after a short delay, giving Telegram time to deliver adjacent text.
fn spawn_deferred_upload(config: Config, chat_id: i64, user_id: i64, message: Message) {
    tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(
            MEDIA_GROUP_UPLOAD_DELAY_MS,
        ))
        .await;
        let Ok(_permit) = MEDIA_GROUP_UPLOADS.acquire().await else {
            tracing::warn!(user_id, chat_id, "media-group upload semaphore was closed");
            return;
        };
        let bot = Bot::from_config(config);
        if let Err(error) = bot.handle_upload(chat_id, user_id, &message).await {
            tracing::error!(
                user_id,
                chat_id,
                error = %format!("{error:#}"),
                "media-group upload task failed"
            );
            bot.telegram
                .send_message(
                    chat_id,
                    &format!("❌ Upload failed: {}", escape_html(&format!("{error}"))),
                    None,
                )
                .await
                .ok();
        }
    });
}

/// Registers one incoming Telegram album item before the deferred upload starts.
fn register_media_group_upload(chat_id: i64, group_id: Option<&str>) {
    let Some(group_id) = group_id.filter(|group_id| !group_id.trim().is_empty()) else {
        return;
    };
    let now = now_ts();
    let mut groups = MEDIA_GROUP_PROGRESS.lock().unwrap();
    retain_fresh_media_group_progress(&mut groups, now);
    let progress = groups
        .entry(MediaGroupKey {
            chat_id,
            group_id: group_id.to_string(),
        })
        .or_insert(MediaGroupProgress {
            total: 0,
            completed: 0,
            updated_at: now,
        });
    progress.total = progress.total.saturating_add(1);
    progress.updated_at = now;
}

/// Returns the next progress position for a Telegram album upload item.
fn media_group_upload_progress(chat_id: i64, group_id: Option<&str>) -> Option<UploadProgress> {
    let group_id = group_id.filter(|group_id| !group_id.trim().is_empty())?;
    let now = now_ts();
    let key = MediaGroupKey {
        chat_id,
        group_id: group_id.to_string(),
    };
    let mut groups = MEDIA_GROUP_PROGRESS.lock().unwrap();
    retain_fresh_media_group_progress(&mut groups, now);
    let progress = groups.get_mut(&key)?;
    progress.completed = progress.completed.saturating_add(1);
    progress.updated_at = now;
    let current = progress.completed;
    let total = progress.total.max(current);
    if current >= total {
        groups.remove(&key);
    }
    (total > 1).then_some(UploadProgress { current, total })
}

/// Drops stale Telegram album progress counters.
fn retain_fresh_media_group_progress(
    groups: &mut HashMap<MediaGroupKey, MediaGroupProgress>,
    now: i64,
) {
    groups.retain(|_, progress| {
        now.saturating_sub(progress.updated_at) <= MEDIA_GROUP_PROGRESS_TTL_SECONDS
    });
}

#[cfg(feature = "archive")]
fn spawn_archive_upload(
    config: Config,
    chat_id: i64,
    user_id: i64,
    caption: String,
    filename_prefix: Option<String>,
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
                filename_prefix.as_deref(),
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
fn archive_preview_followup_count(followup: &ArchivePreviewFollowup) -> usize {
    match followup {
        ArchivePreviewFollowup::Confirm { count, .. }
        | ArchivePreviewFollowup::Prefix { count, .. } => *count,
    }
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
        let count = archive_preview_followup_count(&followup);
        if count >= ARCHIVE_PREVIEW_PROGRESS_MIN_IMAGES {
            let image_label = image_count_label(count);
            bot.telegram
                .send_message(
                    chat_id,
                    &format!(
                        "Preparing previews for <b>{count}</b> {image_label}. Upload buttons will appear after the previews."
                    ),
                    None,
                )
                .await
                .ok();
        }
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
        if !should_send_original_archive_preview(resize, bytes.len()) {
            if !resize {
                tracing::info!(
                    chat_id,
                    token,
                    name = %entry.name,
                    bytes = bytes.len(),
                    max_bytes = TELEGRAM_PHOTO_PREVIEW_MAX_BYTES,
                    "sending resized archive preview because original exceeds Telegram photo limit"
                );
            }
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

#[cfg(feature = "archive")]
fn should_send_original_archive_preview(resize: bool, bytes_len: usize) -> bool {
    !resize && bytes_len <= TELEGRAM_PHOTO_PREVIEW_MAX_BYTES
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
    } else if text.contains(UPDATE_ALREADY_IN_PROGRESS_ERROR) {
        StatusCode::SERVICE_UNAVAILABLE
    } else {
        StatusCode::INTERNAL_SERVER_ERROR
    }
}

async fn handle_webhook_payload(headers: &HeaderMap, body: &[u8]) -> Result<()> {
    let config = Config::from_env();
    verify_telegram_secret(&config, headers)?;
    let update: Update = serde_json::from_slice(body).context("invalid Telegram update JSON")?;

    let bot = Bot::from_config(config);
    let mut in_progress_key = None;
    let mut done_key = None;
    if let Some(update_id) = update.update_id {
        let done = format!("TELEGRAM_UPDATE_DONE#{update_id}");
        match bot.store.has_idempotency(&done).await {
            Ok(true) => {
                tracing::info!(update_id, "skipping already processed Telegram update");
                return Ok(());
            }
            Ok(false) => {}
            Err(error) => {
                tracing::warn!(error = %format!("{error:#}"), "processed-update check failed");
            }
        }

        let busy = format!("TELEGRAM_UPDATE_BUSY#{update_id}");
        match bot
            .store
            .reserve_idempotency(&busy, UPDATE_IN_PROGRESS_SECONDS)
            .await
        {
            Ok(false) => {
                tracing::info!(update_id, "Telegram update is already being processed");
                anyhow::bail!("{UPDATE_ALREADY_IN_PROGRESS_ERROR}: {update_id}");
            }
            Ok(true) => {
                in_progress_key = Some(busy);
                done_key = Some(done);
            }
            Err(error) => {
                tracing::warn!(error = %format!("{error:#}"), "in-progress update reservation failed");
            }
        }
    }

    let result = bot.handle_update(update).await;
    if let Some(key) = in_progress_key.as_deref()
        && let Err(error) = bot.store.forget_idempotency(key).await
    {
        tracing::warn!(error = %format!("{error:#}"), "failed to clear in-progress update marker");
    }
    match result {
        Ok(()) => {
            if let Some(key) = done_key.as_deref()
                && let Err(error) = bot
                    .store
                    .reserve_idempotency(key, UPDATE_IDEMPOTENCY_SECONDS)
                    .await
            {
                tracing::warn!(error = %format!("{error:#}"), "processed-update reservation failed");
            }
        }
        Err(error) => {
            tracing::error!(error = %format!("{error:#}"), "failed to handle Telegram update");
            return Err(error);
        }
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

/// File resolved from an external URL.
struct LinkedFile {
    file: TelegramFile,
    file_name: Option<String>,
    mime: Option<String>,
    source_url: String,
    unique_id: String,
    cleanup_paths: Vec<PathBuf>,
}

/// Disk-backed result of an ffmpeg conversion/remux.
struct ConvertedMediaFile {
    file: TelegramFile,
    file_name: String,
    mime: String,
    unique_id: String,
    cleanup_paths: Vec<PathBuf>,
}

/// Removes temporary files when an upload/download path exits.
#[derive(Default)]
struct TempPathCleanup {
    paths: Vec<PathBuf>,
}

impl TempPathCleanup {
    /// Creates a cleanup guard for paths that should be removed on drop.
    fn new(paths: Vec<PathBuf>) -> Self {
        Self { paths }
    }

    /// Adds a path to remove when the guard is dropped.
    fn push(&mut self, path: PathBuf) {
        self.paths.push(path);
    }

    /// Hands the paths to another owner without removing them.
    fn into_paths(mut self) -> Vec<PathBuf> {
        std::mem::take(&mut self.paths)
    }
}

impl Drop for TempPathCleanup {
    fn drop(&mut self) {
        for path in self.paths.drain(..) {
            if let Err(error) = std::fs::remove_file(&path) {
                tracing::debug!(
                    path = %path.display(),
                    error = %format!("{error:#}"),
                    "failed to remove temporary file"
                );
            }
        }
    }
}

/// First external URL found in a text message.
#[derive(Clone, Debug)]
struct LinkCandidate {
    url: Url,
    token: String,
}

/// Minimal media stream metadata read from ffprobe.
#[derive(Clone, Debug, Eq, PartialEq)]
struct MediaProbe {
    streams: Vec<MediaStreamInfo>,
}

impl MediaProbe {
    fn first_video_codec(&self) -> Option<&str> {
        self.streams
            .iter()
            .find(|stream| stream.kind == "video")
            .and_then(|stream| stream.codec.as_deref())
    }

    fn first_audio_codec(&self) -> Option<&str> {
        self.streams
            .iter()
            .find(|stream| stream.kind == "audio")
            .and_then(|stream| stream.codec.as_deref())
    }

    fn has_video(&self) -> bool {
        self.first_video_codec().is_some()
    }
}

/// One ffprobe stream reduced to the fields needed for Commons format decisions.
#[derive(Clone, Debug, Eq, PartialEq)]
struct MediaStreamInfo {
    kind: String,
    codec: Option<String>,
}

/// What ffmpeg should do to make a media file acceptable to Commons.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct FfmpegPlan {
    kind: FfmpegPlanKind,
    extension: &'static str,
    mime: &'static str,
}

/// Supported ffmpeg operations, from lossless remux/extract through AV1 conversion.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum FfmpegPlanKind {
    RemuxWebm,
    RemuxOgv,
    ExtractOggAudio,
    ExtractMp3,
    ExtractFlac,
    TranscodeAudioOpus,
    CopyVideoTranscodeAudioWebm,
    TranscodeVideoAv1CopyAudio,
    TranscodeVideoAv1Opus,
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

/// Data needed to format a single successful upload reply.
struct UploadSuccessReply<'a> {
    filename: &'a str,
    url: &'a str,
    categories: &'a [String],
    compressed_photo: bool,
    progress: Option<UploadProgress>,
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
        self.remember_group_caption(&message).await;

        if let Some(file) = extract_file(&message) {
            if message.media_group_id.is_some() && defer_uploads_after_ack() {
                send_chat_action_best_effort(&self.telegram, chat_id, "typing").await;
                register_media_group_upload(chat_id, message.media_group_id.as_deref());
                spawn_deferred_upload(self.config.clone(), chat_id, user_id, message);
                return Ok(());
            }
            if defer_uploads_after_ack()
                && should_defer_single_upload_for_text_context(&message, &file)
            {
                let profile = self.store.get_profile(user_id).await;
                if profile.is_ready() && profile.filename_prefix.trim().is_empty() {
                    send_chat_action_best_effort(&self.telegram, chat_id, "typing").await;
                    spawn_deferred_upload(self.config.clone(), chat_id, user_id, message);
                    return Ok(());
                }
            }
            return self.handle_upload(chat_id, user_id, &message).await;
        }

        let text = message_text_for_links(&message).unwrap_or_default();
        let trimmed = text.trim().to_string();
        if trimmed.starts_with('/') {
            return self.handle_command(chat_id, user_id, &trimmed).await;
        }
        if let Some(url) = first_external_url(&trimmed) {
            return self
                .handle_link_upload(chat_id, user_id, &message, url)
                .await;
        }
        let profile = self.store.get_profile(user_id).await;
        if profile.is_ready()
            && crate::commons::parse_settings_command(&trimmed).is_empty()
            && remember_text_context(chat_id, user_id, &trimmed).await
        {
            return Ok(());
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
            OnboardingStep::AwaitingSettingsPrefix => {
                self.telegram
                    .send_message(
                        chat_id,
                        "Send the new <b>filename prefix</b> as a message, or /cancel.",
                        None,
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
            OnboardingStep::AwaitingSettingsPrefix => {
                let prefix = if text.eq_ignore_ascii_case("skip")
                    || text.eq_ignore_ascii_case("clear")
                    || text.eq_ignore_ascii_case("clean")
                {
                    String::new()
                } else {
                    text.to_string()
                };
                profile.filename_prefix = prefix;
                profile.onboarding_step = OnboardingStep::Done;
                touch(&mut profile);
                self.store.put_profile(user_id, &profile).await?;
                self.telegram
                    .send_message(
                        chat_id,
                        &settings_overview(&profile),
                        Some(settings_keyboard(&profile)),
                    )
                    .await
            }
            #[cfg(feature = "archive")]
            OnboardingStep::AwaitingArchivePrefix => {
                let pending = pending_archive_summary_for_user(user_id);
                if text.is_empty() || text.eq_ignore_ascii_case("skip") {
                    return self.prompt_current_archive_prefix(chat_id, user_id).await;
                }

                let Some(pending) = pending else {
                    profile.onboarding_step = OnboardingStep::Done;
                    touch(&mut profile);
                    self.store.put_profile(user_id, &profile).await?;
                    return self
                        .telegram
                        .send_message(
                            chat_id,
                            "I no longer have the pending archive. Please resend the archive.",
                            None,
                        )
                        .await;
                };

                let filename_prefix = text.to_string();
                profile.onboarding_step = OnboardingStep::Done;
                touch(&mut profile);
                self.store.put_profile(user_id, &profile).await?;

                if pending.confirm_before_upload {
                    if let Err(error) =
                        set_pending_archive_filename_prefix(&pending.token, filename_prefix.clone())
                    {
                        tracing::warn!(
                            user_id,
                            chat_id,
                            token = %pending.token,
                            error = %format!("{error:#}"),
                            "failed to store pending archive filename prefix"
                        );
                    }
                    return self
                        .send_archive_confirmation(
                            chat_id,
                            &pending.token,
                            pending.count,
                            Some(&filename_prefix),
                            pending.archive_file_name.as_deref(),
                        )
                        .await;
                }

                let pending_archive = take_pending_archive(&pending.token);
                if let Some(pending_archive) = pending_archive {
                    let text = format!(
                        "Filename prefix set to <code>{}</code>. Uploading archive…",
                        escape_html(&filename_prefix)
                    );
                    self.telegram.send_message(chat_id, &text, None).await.ok();
                    return self
                        .upload_entries(
                            chat_id,
                            user_id,
                            &mut profile,
                            &pending_archive.caption,
                            Some(&filename_prefix),
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
        let callback_message_id = callback
            .message
            .as_ref()
            .and_then(|message| message.message_id);
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
            return self
                .replace_settings_message(
                    chat_id,
                    callback_message_id,
                    &profile,
                    settings_keyboard(&profile),
                )
                .await;
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

        if data == "set:license" {
            return self
                .replace_settings_message(
                    chat_id,
                    callback_message_id,
                    &profile,
                    settings_license_keyboard(&profile),
                )
                .await;
        }

        if data == "set:prefix" {
            return self
                .replace_message_text_or_send(
                    chat_id,
                    callback_message_id,
                    &settings_overview(&profile),
                    Some(settings_prefix_keyboard(&profile)),
                )
                .await;
        }

        if data == "set:prefix:set" {
            profile.onboarding_step = OnboardingStep::AwaitingSettingsPrefix;
            touch(&mut profile);
            self.store.put_profile(user_id, &profile).await?;
            return self
                .replace_message_text_or_send(
                    chat_id,
                    callback_message_id,
                    &settings_prefix_prompt(&profile),
                    Some(settings_prefix_input_keyboard(&profile)),
                )
                .await;
        }

        if data == "set:prefix:clear" {
            profile.filename_prefix.clear();
            if profile.onboarding_step == OnboardingStep::AwaitingSettingsPrefix {
                profile.onboarding_step = OnboardingStep::Done;
            }
            touch(&mut profile);
            self.store.put_profile(user_id, &profile).await?;
            return self
                .replace_settings_message(
                    chat_id,
                    callback_message_id,
                    &profile,
                    settings_keyboard(&profile),
                )
                .await;
        }

        if data == "set:main" {
            if profile.onboarding_step == OnboardingStep::AwaitingSettingsPrefix {
                profile.onboarding_step = OnboardingStep::Done;
                touch(&mut profile);
                self.store.put_profile(user_id, &profile).await?;
            }
            return self
                .replace_settings_message(
                    chat_id,
                    callback_message_id,
                    &profile,
                    settings_keyboard(&profile),
                )
                .await;
        }

        let changed = match data.as_str() {
            "set:links" => {
                profile.return_upload_links = !profile.return_upload_links;
                true
            }
            "set:catlinks" => {
                profile.return_category_links = !profile.return_category_links;
                true
            }
            "set:misscat" => {
                profile.return_missing_category_links = !profile.return_missing_category_links;
                true
            }
            #[cfg(feature = "archive")]
            "set:arclist" => {
                profile.return_archive_file_list = !profile.return_archive_file_list;
                true
            }
            #[cfg(feature = "archive")]
            "set:arcconfirm" => {
                profile.archive_confirm = !profile.archive_confirm;
                true
            }
            "set:dng" => {
                profile.dng_mode = profile.dng_mode.toggled();
                true
            }
            _ => false,
        };
        if !changed {
            return Ok(());
        }
        touch(&mut profile);
        self.store.put_profile(user_id, &profile).await?;
        self.replace_settings_message(
            chat_id,
            callback_message_id,
            &profile,
            settings_keyboard(&profile),
        )
        .await
    }

    /// Replaces a bot message in place; falls back only when Telegram gives no editable id.
    async fn replace_message_text_or_send(
        &self,
        chat_id: i64,
        message_id: Option<i64>,
        text: &str,
        reply_markup: Option<InlineKeyboardMarkup>,
    ) -> Result<()> {
        if let Some(message_id) = message_id {
            match self
                .telegram
                .edit_message_text(chat_id, message_id, text, reply_markup.clone())
                .await
            {
                Ok(()) => return Ok(()),
                Err(error) => {
                    tracing::warn!(
                        chat_id,
                        message_id,
                        error = %format!("{error:#}"),
                        "failed to edit Telegram message; sending a fresh message"
                    );
                }
            }
        }
        self.telegram
            .send_message(chat_id, text, reply_markup)
            .await
    }

    /// Replaces a settings message in place; falls back only when Telegram gives no editable id.
    async fn replace_settings_message(
        &self,
        chat_id: i64,
        message_id: Option<i64>,
        profile: &Profile,
        reply_markup: InlineKeyboardMarkup,
    ) -> Result<()> {
        self.replace_message_text_or_send(
            chat_id,
            message_id,
            &settings_overview(profile),
            Some(reply_markup),
        )
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
                            "Unknown license. Use one of: cc-by-4.0, cc-by-sa-4.0, cc-zero, PD-Russia-expired, PD-Russia, PD-RusEmpire.",
                            None,
                        )
                        .await
                }
            }
            "dng" => {
                if let Some(mode) = DngMode::parse(rest) {
                    profile.dng_mode = mode;
                    touch(&mut profile);
                    self.store.put_profile(user_id, &profile).await?;
                    let text = format!("DNG handling: <b>{}</b>.", escape_html(mode.label()));
                    self.telegram.send_message(chat_id, &text, None).await
                } else {
                    self.telegram
                        .send_message(
                            chat_id,
                            "Unknown DNG mode. Use <code>/settings dng webp</code> or <code>/settings dng extract</code>.",
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
        if profile.onboarding_step == OnboardingStep::AwaitingSettingsPrefix {
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
        let mut text = format!(
            "🖼 <b>Wikimedia Commons uploader</b> ({BOT_USERNAME})\n\nSend me a photo or file and I upload it to <b>Wikimedia Commons</b> under your own account.\n\n📎 <b>Send images as files</b> (attach → File), not as compressed photos, to preserve the original quality.\n\n⚠️ <b>Uploads are public</b> and reusable, even commercially; storage is unlimited, but files you may not share get deleted.\n• ✅ Best: <b>your own</b> photos (nature, animals, food, events) and your own art or scans.\n• ❌ Files from other sites/social media, screenshots, posters, most logos/covers — <b>usually</b> copyrighted (a few exceptions).\n• ✅ Others' work only under a free license: CC BY, CC BY-SA, CC0 or public domain — <b>not</b> NC (Non-Commercial).\n• 📚 Public domain when old: ~<a href=\"https://commons.wikimedia.org/wiki/Commons:Licensing#Ordinary_copyright\">70 years after the author's death</a> (<a href=\"https://commons.wikimedia.org/wiki/Commons:Copyright_rules_by_territory/Belarus\">50 in Belarus</a>), varies by country; photos of buildings/statues also need Freedom of Panorama.\nWhat may be uploaded: https://commons.wikimedia.org/wiki/Commons:Licensing\n\n<b>Set up</b>: run /start, then connect with <b>OAuth</b> (recommended) or a <b>bot password</b> (tick Upload new files + Create, edit, and move pages at https://commons.wikimedia.org/wiki/Special:BotPasswords).\n\n<b>In a caption</b> (per file, whole album too): <code>Categories: A, B</code>, <code>Source: …</code>, <code>Author: …</code>, <code>Date: 2009-12-03</code>, <code>Coord: &lt;map link or lat,lon&gt;</code>.\n\n<b>Links</b>: send or forward an HTTP(S) link to a file/archive, DropMeFiles share page, YouTube/youtu.be, VK video, Rutube, or Apple Podcasts episode. Unsupported audio/video is remuxed when possible or converted to OGG/Opus or WebM AV1/Opus; MP3 and audio OGG stay unchanged, Ogg video is handled as OGV.\n\n<b>Set your defaults</b> any time (for future uploads): <code>category …</code>, <code>author …</code>, <code>prefix …</code>, <code>description …</code>, <code>lang ru</code>, <code>license {{PD-RU-exempt}}</code> — colon optional; short aliases <code>c/a/p/d/l</code>.\n\n<b>Accepted</b>: JPEG, PNG, GIF, SVG, TIFF, WebP, PDF, DjVu, audio (WAV, MP3, OGG, Opus, FLAC), video (WebM, OGV). HEIC and BMP are converted to WebP automatically. DNG defaults to raw development → WebP with embedded JPEG fallback; /settings can force DNG embedded JPEG extraction.\n<b>Max size</b>: {max_upload_size} for accepted files; conversions are limited to {conversion_limit}; archives are limited to {archive_limit}.\n\n<b>Commands</b>: /start, /settings, /forget, /help\n\nMade by {CONTACT} — message me for help or uploading assistance.\n\n<b>Related projects</b>:\n• Browse Commons in Telegram: {RELATED_BROWSE_BOT}\n• gThumb extension: {RELATED_GTHUMB}\n• Browser upload extension: {RELATED_WEB_EXTENSION}\n• CLI upload tool: {RELATED_CLI}\n• Dark Wikipedia theme: {RELATED_DARK_THEME}\n• Wikipedia → man pages: {RELATED_WIKI2MAN}\n\nSource: {}",
            self.config.github_url
        );
        #[cfg(feature = "archive")]
        {
            text.push_str(
                "\n\n📦 <b>Archives</b>: send a <b>.zip</b> (or .rar) and I upload the images inside under one caption/categories. In /settings you can show the archive's file list and require a thumbnail + <b>Confirm</b> step before uploading.",
            );
        }
        text.push_str(&uploads_line);
        self.telegram.send_message(chat_id, &text, None).await
    }

    /// Adds successful uploads to the latest profile without overwriting newer settings.
    async fn record_successful_uploads(&self, user_id: i64, uploaded: u64) -> Result<()> {
        if uploaded == 0 {
            return Ok(());
        }
        let mut profile = self.store.get_profile(user_id).await;
        profile.uploads_count = profile.uploads_count.saturating_add(uploaded);
        touch(&mut profile);
        self.store.put_profile(user_id, &profile).await
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
        filename_prefix: Option<&str>,
        extra_categories: &[String],
        original: TelegramFile,
        file_name: Option<&str>,
        mime: Option<&str>,
        unique_id: &str,
        source_url: Option<&str>,
        auth: &UploadAuth,
        bot_password_session: Option<&CommonsBotPasswordSession>,
        author_username: &str,
    ) -> Result<FileResult> {
        let parsed = parse_caption(caption);
        let source = parsed.source.as_deref().or(source_url);
        let categories = upload_categories(
            &parsed.categories,
            extra_categories,
            &profile.default_categories,
        );
        let metadata = metadata_from_telegram_file(&original);

        // Convert DNG/HEIC/BMP to WebP; remux/transcode unsupported media to Commons formats.
        let sniff = original.read_prefix(FILE_SNIFF_BYTES)?;
        let format = convert::classify(file_name, mime, &sniff);
        let mut provenance = UploadProvenance {
            original_filename: file_name
                .map(str::to_string)
                .unwrap_or_else(|| format!("telegram_{unique_id}")),
            ..UploadProvenance::default()
        };
        let mut upload_cleanup_paths = Vec::new();
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
            match convert::convert(
                &original,
                format,
                self.config.webp_quality,
                profile.dng_mode,
            ) {
                Ok((bytes, ext)) => (UploadData::Bytes(bytes), ext.to_string()),
                Err(error) => {
                    tracing::warn!(
                        file_name = ?file_name,
                        mime = ?mime,
                        unique_id,
                        source_format = ?format,
                        dng_mode = ?profile.dng_mode,
                        error = %format!("{error:#}"),
                        "file conversion failed"
                    );
                    return Ok(FileResult::Rejected {
                        reason: conversion_rejection_reason(
                            format,
                            profile.dng_mode,
                            &original,
                            &error,
                        ),
                    });
                }
            }
        } else {
            let extension = convert::passthrough_extension(file_name, mime);
            if !convert::is_commons_accepted(&extension) {
                if !should_try_ffmpeg_media_conversion(file_name, mime, &extension) {
                    return Ok(FileResult::Rejected {
                        reason: format!("Commons does not accept .{extension} files"),
                    });
                }
                if original.len() > self.config.max_conversion_file_bytes {
                    let limit = format_size_limit(self.config.max_conversion_file_bytes);
                    return Ok(FileResult::Rejected {
                        reason: format!(
                            "This media file needs conversion, and conversion is currently limited to {limit}"
                        ),
                    });
                }
                provenance.original_sha1 = Some(sha1_hex_telegram_file(&original)?);
                provenance.original_md5 = Some(md5_hex_telegram_file(&original)?);
                let converted = match self
                    .convert_telegram_media_with_ffmpeg(&original, file_name, mime, &extension)
                    .await
                {
                    Ok(converted) => converted,
                    Err(error) => {
                        tracing::warn!(
                            file_name = ?file_name,
                            mime = ?mime,
                            unique_id,
                            error = %format!("{error:#}"),
                            "media conversion failed"
                        );
                        return Ok(FileResult::Rejected {
                            reason: format!(
                                "Couldn't convert this media file to a Commons-compatible format: {error}"
                            ),
                        });
                    }
                };
                upload_cleanup_paths = converted.cleanup_paths;
                (
                    upload_data_from_telegram_file(converted.file),
                    file_extension_for_name(&converted.file_name)
                        .unwrap_or_else(|| "webm".to_string()),
                )
            } else {
                (upload_data_from_telegram_file(original), extension)
            }
        };
        let _upload_cleanup = TempPathCleanup::new(upload_cleanup_paths);

        // Duplicate pre-check by content hash.
        let upload_sha1 = sha1_hex_upload_data(&upload_data)?;
        if let Ok(existing) = self.commons.find_by_sha1(&upload_sha1).await
            && !existing.is_empty()
        {
            return Ok(FileResult::Duplicate { titles: existing });
        }

        // Build the filename: caption text as a descriptive prefix and the original stem
        // for per-file uniqueness (emoji dropped, newlines collapsed by build_filename).
        let original_stem = file_stem(file_name);
        let filename_prefix = effective_filename_prefix(
            &profile.filename_prefix,
            filename_prefix,
            &parsed.description,
            original_stem,
        );
        if filename_needs_descriptive_context(filename_prefix, &parsed.description, original_stem) {
            return Ok(FileResult::Rejected {
                reason: format!(
                    "{} is a generic camera filename that needs a caption or filename prefix",
                    original_stem
                ),
            });
        }
        let filename = build_filename(
            filename_prefix,
            &parsed.description,
            original_stem,
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
            source,
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
            } else if profile.onboarding_step == OnboardingStep::AwaitingSettingsPrefix {
                "Send the new filename prefix first 👇"
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

        send_chat_action_best_effort(&self.telegram, chat_id, "typing").await;
        let _upload_chat_action = ChatActionGuard::start(self.telegram.clone(), chat_id, "typing");

        // Resolve the description and categories before any upload work so album items can reuse
        // the captioned item after the deferred media-group delay.
        let mut caption = self.resolve_caption(chat_id, user_id, message).await;
        let progress = media_group_upload_progress(chat_id, message.media_group_id.as_deref());

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
                    .handle_archive(
                        chat_id,
                        user_id,
                        &mut profile,
                        caption.clone(),
                        file.file_name.clone(),
                        original,
                    )
                    .await;
            }
        }

        let mut parsed_caption = parse_caption(&caption);
        let original_stem = file_stem(file.file_name.as_deref());
        let mut filename_prefix = effective_filename_prefix(
            &profile.filename_prefix,
            None,
            &parsed_caption.description,
            original_stem,
        );
        if filename_needs_descriptive_context(
            filename_prefix,
            &parsed_caption.description,
            original_stem,
        ) && let Some(late_caption) = self.wait_for_text_context(chat_id, user_id).await
        {
            caption = late_caption;
            parsed_caption = parse_caption(&caption);
            filename_prefix = effective_filename_prefix(
                &profile.filename_prefix,
                None,
                &parsed_caption.description,
                original_stem,
            );
        }
        if filename_needs_descriptive_context(
            filename_prefix,
            &parsed_caption.description,
            original_stem,
        ) {
            return self
                .reject_generic_img_filename(chat_id, message, original_stem)
                .await;
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

        self.telegram
            .send_chat_action(chat_id, "upload_document")
            .await
            .ok();
        let result = self
            .process_one_file(
                &profile,
                &caption,
                None,
                &[],
                original,
                file.file_name.as_deref(),
                file.mime.as_deref(),
                &file.file_unique_id,
                None,
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
                self.record_successful_uploads(user_id, 1).await.ok();
                self.send_success(
                    chat_id,
                    &profile,
                    UploadSuccessReply {
                        filename: &filename,
                        url: &url,
                        categories: &categories,
                        compressed_photo: file.compressed_photo,
                        progress,
                    },
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
                    "❌ {}.\n\nAccepted: JPEG, PNG, GIF, SVG, TIFF, WebP, PDF, DjVu, audio (WAV/MP3/OGG/Opus/FLAC), video (WebM/OGV). HEIC and BMP are converted automatically. DNG handling is configurable in /settings.",
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

    /// Downloads an external link and uploads the resulting file or extracted archive members.
    async fn handle_link_upload(
        &self,
        chat_id: i64,
        user_id: i64,
        message: &Message,
        link: LinkCandidate,
    ) -> Result<()> {
        let mut profile = self.store.get_profile(user_id).await;
        if !profile.is_ready() {
            let text = if profile.onboarding_step == OnboardingStep::AwaitingSettingsPrefix {
                "Send the new filename prefix first 👇"
            } else {
                "Let's finish connecting your Commons account first 👇"
            };
            self.telegram.send_message(chat_id, text, None).await.ok();
            let step = if profile.onboarding_step == OnboardingStep::Done {
                OnboardingStep::AwaitingUsername
            } else {
                profile.onboarding_step
            };
            return self.prompt_step(chat_id, step).await;
        }

        send_chat_action_best_effort(&self.telegram, chat_id, "typing").await;
        let _typing = ChatActionGuard::start(self.telegram.clone(), chat_id, "typing");
        let caption_source = message_text_for_links(message).unwrap_or_default();
        let caption = caption_without_link(&caption_source, &link);
        let linked = match self.resolve_linked_file(&link.url).await {
            Ok(file) => file,
            Err(error) => {
                tracing::warn!(
                    user_id,
                    chat_id,
                    url = %link.url,
                    error = %format!("{error:#}"),
                    "failed to resolve linked file"
                );
                let text = format!(
                    "❌ Couldn't download that link: {}",
                    escape_html(&format!("{error}"))
                );
                return self.telegram.send_message(chat_id, &text, None).await;
            }
        };
        let _cleanup = TempPathCleanup::new(linked.cleanup_paths.clone());

        #[cfg(feature = "archive")]
        {
            let sniff = linked.file.read_prefix(FILE_SNIFF_BYTES)?;
            if crate::archive::is_archive(linked.file_name.as_deref(), &sniff) {
                if linked.file.len() > self.config.max_archive_file_bytes {
                    return self.reject_archive_too_large(chat_id).await;
                }
                let caption = caption_with_source(&caption, &linked.source_url);
                let original = linked.file.into_bytes()?;
                return self
                    .handle_archive(
                        chat_id,
                        user_id,
                        &mut profile,
                        caption,
                        linked.file_name,
                        original,
                    )
                    .await;
            }
        }

        let parsed_caption = parse_caption(&caption);
        let original_stem = file_stem(linked.file_name.as_deref());
        let filename_prefix = effective_filename_prefix(
            &profile.filename_prefix,
            None,
            &parsed_caption.description,
            original_stem,
        );
        if filename_needs_descriptive_context(
            filename_prefix,
            &parsed_caption.description,
            original_stem,
        ) {
            return self
                .reject_generic_img_filename(chat_id, message, original_stem)
                .await;
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

        self.telegram
            .send_chat_action(chat_id, "upload_document")
            .await
            .ok();
        let result = self
            .process_one_file(
                &profile,
                &caption,
                None,
                &[],
                linked.file,
                linked.file_name.as_deref(),
                linked.mime.as_deref(),
                &linked.unique_id,
                Some(&linked.source_url),
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
                self.record_successful_uploads(user_id, 1).await.ok();
                self.send_success(
                    chat_id,
                    &profile,
                    UploadSuccessReply {
                        filename: &filename,
                        url: &url,
                        categories: &categories,
                        compressed_photo: false,
                        progress: None,
                    },
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
                    "❌ {}.\n\nAccepted: JPEG, PNG, GIF, SVG, TIFF, WebP, PDF, DjVu, audio (WAV/MP3/OGG/Opus/FLAC), video (WebM/OGV). HEIC and BMP are converted automatically. DNG handling is configurable in /settings.",
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

    /// Resolves an HTTP(S) link into a local file ready for the normal upload pipeline.
    async fn resolve_linked_file(&self, url: &Url) -> Result<LinkedFile> {
        if !matches!(url.scheme(), "http" | "https") {
            bail!("only http and https links are supported");
        }
        if is_blocked_url_host(url) {
            bail!("links to local or private hosts are not supported");
        }

        let linked = if needs_ytdlp(url) {
            self.download_with_ytdlp(url).await?
        } else if is_dropmefiles_url(url) {
            let download_url = self.resolve_dropmefiles_download_url(url).await?;
            let mut linked = self.download_direct_url(&download_url).await?;
            linked.source_url = url.as_str().to_string();
            linked
        } else {
            self.download_direct_url(url).await?
        };
        #[cfg(feature = "archive")]
        {
            let sniff = linked.file.read_prefix(FILE_SNIFF_BYTES)?;
            if crate::archive::is_archive(linked.file_name.as_deref(), &sniff) {
                return Ok(linked);
            }
        }
        self.ensure_linked_file_commons_compatible(linked).await
    }

    /// Resolves a DropMeFiles sharing page into the hidden direct download URL.
    async fn resolve_dropmefiles_download_url(&self, url: &Url) -> Result<Url> {
        let client = reqwest::Client::builder()
            .user_agent(&self.config.user_agent)
            .redirect(reqwest::redirect::Policy::limited(10))
            .timeout(std::time::Duration::from_secs(60))
            .build()
            .context("failed to build DropMeFiles HTTP client")?;
        let page = client
            .get(url.clone())
            .send()
            .await
            .context("failed to request DropMeFiles page")?
            .error_for_status()
            .context("DropMeFiles page returned an error status")?
            .text()
            .await
            .context("failed to read DropMeFiles page")?;
        dropmefiles_download_url_from_page(url, &page)
    }

    /// Downloads a direct file URL to the configured temp directory, streaming chunks without
    /// buffering it all in RAM.
    async fn download_direct_url(&self, url: &Url) -> Result<LinkedFile> {
        let client = reqwest::Client::builder()
            .user_agent(&self.config.user_agent)
            .redirect(reqwest::redirect::Policy::limited(10))
            .timeout(std::time::Duration::from_secs(
                DIRECT_URL_DOWNLOAD_TIMEOUT_SECS,
            ))
            .build()
            .context("failed to build linked-file HTTP client")?;
        let mut response = client
            .get(url.clone())
            .send()
            .await
            .context("failed to request linked file")?
            .error_for_status()
            .context("linked file returned an error status")?;
        let headers = response.headers().clone();
        let file_name =
            filename_from_headers_or_url(&headers, url).unwrap_or_else(|| "linked-file".into());
        let mime = content_type(&headers);
        let max_bytes = self.max_external_download_bytes();
        let is_direct_supported_file =
            direct_link_looks_like_commons_file(Some(&file_name), mime.as_deref());
        if let Some(length) = response.content_length() {
            if is_direct_supported_file {
                ensure_commons_file_size_limit(length)?;
            }
            if length > max_bytes {
                bail!(
                    "linked file is {}, larger than the configured {} limit",
                    format_size_limit(length),
                    format_size_limit(max_bytes)
                );
            }
        }

        let path = temp_link_path(&file_name)?;
        let mut file = std::fs::File::create(&path)
            .with_context(|| format!("failed to create temp file {}", path.display()))?;
        let mut written = 0u64;
        while let Some(chunk) = response
            .chunk()
            .await
            .context("failed while reading linked file")?
        {
            written = written.saturating_add(chunk.len() as u64);
            if is_direct_supported_file && written > COMMONS_MAX_FILE_BYTES {
                drop(file);
                std::fs::remove_file(&path).ok();
                bail!("{}", commons_max_file_size_message(Some(written)));
            }
            if written > max_bytes {
                drop(file);
                std::fs::remove_file(&path).ok();
                bail!(
                    "linked file is larger than the configured {} limit",
                    format_size_limit(max_bytes)
                );
            }
            file.write_all(&chunk)
                .with_context(|| format!("failed to write temp file {}", path.display()))?;
        }
        file.flush()
            .with_context(|| format!("failed to flush temp file {}", path.display()))?;
        drop(file);
        let size = std::fs::metadata(&path)
            .with_context(|| format!("failed to stat temp file {}", path.display()))?
            .len();
        if size == 0 {
            std::fs::remove_file(&path).ok();
            bail!("linked file is empty");
        }
        let unique_id = short_id_path(&path)?;
        Ok(LinkedFile {
            file: TelegramFile::LocalPath {
                path: path.clone(),
                size,
            },
            file_name: Some(file_name),
            mime,
            source_url: url.as_str().to_string(),
            unique_id,
            cleanup_paths: vec![path],
        })
    }

    /// Uses yt-dlp for websites where the URL is a page, not the media file itself.
    async fn download_with_ytdlp(&self, url: &Url) -> Result<LinkedFile> {
        let dir = tempfile::tempdir().context("failed to create yt-dlp temp directory")?;
        let output_template = dir.path().join("%(title).180B-%(id)s.%(ext)s");
        let mut command = Command::new(&self.config.ytdlp_path);
        command
            .arg("--no-playlist")
            .arg("--no-progress")
            .arg("--max-filesize")
            .arg(self.max_external_download_bytes().to_string())
            .arg("-o")
            .arg(&output_template);

        if is_apple_podcasts_url(url) {
            command.arg("-f").arg(YTDLP_AUDIO_FORMAT_SELECTOR);
        } else {
            command.arg("-f").arg(YTDLP_VIDEO_FORMAT_SELECTOR);
        }
        if let Some(cookies) = self.prepare_ytdlp_cookies_file()? {
            command.arg("--cookies").arg(cookies);
        }
        command.arg(url.as_str());

        let output = tokio::time::timeout(
            std::time::Duration::from_secs(YTDLP_DOWNLOAD_TIMEOUT_SECS),
            command.output(),
        )
        .await
        .context("yt-dlp timed out")?
        .context("failed to launch yt-dlp")?;
        if !output.status.success() {
            bail!(
                "yt-dlp exited with {}: {}",
                output.status,
                command_stderr(&output.stderr)
            );
        }

        let downloaded = find_downloaded_file(dir.path())
            .with_context(|| format!("yt-dlp did not create a media file for {url}"))?;
        let file_name = downloaded
            .file_name()
            .and_then(|name| name.to_str())
            .map(sanitize_download_filename)
            .unwrap_or_else(|| "linked-media".into());
        let target = temp_link_path(&file_name)?;
        match std::fs::rename(&downloaded, &target) {
            Ok(()) => {}
            Err(rename_error) => {
                std::fs::copy(&downloaded, &target).with_context(|| {
                    format!(
                        "failed to move yt-dlp output {} to {} after rename failed ({rename_error})",
                        downloaded.display(),
                        target.display()
                    )
                })?;
                std::fs::remove_file(&downloaded).ok();
            }
        }
        let size = std::fs::metadata(&target)
            .with_context(|| format!("failed to stat yt-dlp output {}", target.display()))?
            .len();
        if size == 0 {
            std::fs::remove_file(&target).ok();
            bail!("yt-dlp downloaded an empty file");
        }
        let unique_id = short_id_path(&target)?;
        Ok(LinkedFile {
            file: TelegramFile::LocalPath {
                path: target.clone(),
                size,
            },
            mime: file_extension_for_name(&file_name).and_then(mime_for_extension),
            file_name: Some(file_name),
            source_url: url.as_str().to_string(),
            unique_id,
            cleanup_paths: vec![target],
        })
    }

    /// Converts/remuxes linked media only when Commons would reject the downloaded file.
    async fn ensure_linked_file_commons_compatible(
        &self,
        mut linked: LinkedFile,
    ) -> Result<LinkedFile> {
        let name_extension = linked
            .file_name
            .as_deref()
            .and_then(file_extension_for_name);
        let extension = if name_extension
            .as_deref()
            .is_some_and(convert::is_commons_accepted)
        {
            name_extension.unwrap()
        } else if let Some(mime_extension) =
            linked.mime.as_deref().and_then(accepted_extension_for_mime)
        {
            let current_name = linked.file_name.as_deref().unwrap_or("linked-file");
            linked.file_name = Some(filename_with_extension(current_name, &mime_extension));
            mime_extension
        } else {
            name_extension.unwrap_or_default()
        };
        let path = linked
            .file
            .as_path()
            .context("linked downloads must be stored on disk")?
            .to_path_buf();

        if convert::is_commons_accepted(&extension) {
            if matches!(extension.as_str(), "ogg" | "oga") {
                match self.probe_media(&path).await {
                    Ok(probe) if probe.has_video() => {
                        let plan = FfmpegPlan {
                            kind: FfmpegPlanKind::RemuxOgv,
                            extension: "ogv",
                            mime: "video/ogg",
                        };
                        return self.run_ffmpeg_plan(&path, linked, plan).await;
                    }
                    Ok(_) => {}
                    Err(error) => {
                        tracing::warn!(
                            path = %path.display(),
                            error = %format!("{error:#}"),
                            "failed to inspect accepted Ogg file; uploading as-is"
                        );
                    }
                }
            }
            if linked.file.len() > self.config.max_file_bytes {
                bail!(
                    "linked file is {}, larger than the configured {} upload limit",
                    format_size_limit(linked.file.len()),
                    format_size_limit(self.config.max_file_bytes)
                );
            }
            return Ok(linked);
        }

        if linked.file.len() > self.config.max_conversion_file_bytes {
            bail!(
                "linked file needs conversion and is {}, larger than the configured {} conversion limit",
                format_size_limit(linked.file.len()),
                format_size_limit(self.config.max_conversion_file_bytes)
            );
        }
        let probe = self.probe_media(&path).await?;
        let plan = ffmpeg_plan_for_probe(&probe).with_context(|| {
            format!(
                "Commons does not accept .{} files, and ffprobe did not find convertible audio/video streams",
                if extension.is_empty() { "bin" } else { &extension }
            )
        })?;
        let converted = self.run_ffmpeg_plan(&path, linked, plan).await?;
        if converted.file.len() > self.config.max_file_bytes {
            bail!(
                "converted file is {}, larger than the configured {} upload limit",
                format_size_limit(converted.file.len()),
                format_size_limit(self.config.max_file_bytes)
            );
        }
        Ok(converted)
    }

    /// Reads stream codec metadata with ffprobe.
    async fn probe_media(&self, path: &Path) -> Result<MediaProbe> {
        let output = tokio::time::timeout(
            std::time::Duration::from_secs(60),
            Command::new(&self.config.ffprobe_path)
                .arg("-v")
                .arg("error")
                .arg("-show_entries")
                .arg("stream=codec_type,codec_name")
                .arg("-of")
                .arg("json")
                .arg(path)
                .output(),
        )
        .await
        .context("ffprobe timed out")?
        .context("failed to launch ffprobe")?;
        if !output.status.success() {
            bail!(
                "ffprobe exited with {}: {}",
                output.status,
                command_stderr(&output.stderr)
            );
        }

        #[derive(serde::Deserialize)]
        struct FfprobeOutput {
            #[serde(default)]
            streams: Vec<FfprobeStream>,
        }
        #[derive(serde::Deserialize)]
        struct FfprobeStream {
            codec_type: Option<String>,
            codec_name: Option<String>,
        }

        let parsed: FfprobeOutput =
            serde_json::from_slice(&output.stdout).context("ffprobe returned invalid JSON")?;
        Ok(MediaProbe {
            streams: parsed
                .streams
                .into_iter()
                .filter_map(|stream| {
                    Some(MediaStreamInfo {
                        kind: stream.codec_type?,
                        codec: stream.codec_name.map(|codec| codec.to_ascii_lowercase()),
                    })
                })
                .collect(),
        })
    }

    /// Converts/remuxes a direct Telegram audio/video file through ffmpeg.
    async fn convert_telegram_media_with_ffmpeg(
        &self,
        original: &TelegramFile,
        file_name: Option<&str>,
        mime: Option<&str>,
        extension: &str,
    ) -> Result<ConvertedMediaFile> {
        let source_name = ffmpeg_source_filename(file_name, mime);
        let mut cleanup_paths = Vec::new();
        let input_path = match original {
            TelegramFile::LocalPath { path, .. } => path.clone(),
            TelegramFile::Bytes(bytes) => {
                let path = temp_link_path(&source_name)?;
                let mut file = std::fs::File::create(&path)
                    .with_context(|| format!("failed to create temp file {}", path.display()))?;
                file.write_all(bytes)
                    .with_context(|| format!("failed to write temp file {}", path.display()))?;
                file.flush()
                    .with_context(|| format!("failed to flush temp file {}", path.display()))?;
                cleanup_paths.push(path.clone());
                path
            }
        };
        let cleanup = TempPathCleanup::new(cleanup_paths);
        let probe = self.probe_media(&input_path).await.with_context(|| {
            format!(
                "Commons does not accept .{} files, and ffprobe could not inspect it",
                if extension.is_empty() {
                    "bin"
                } else {
                    extension
                }
            )
        })?;
        let plan = ffmpeg_plan_for_probe(&probe).with_context(|| {
            format!(
                "Commons does not accept .{} files, and ffprobe did not find convertible audio/video streams",
                if extension.is_empty() { "bin" } else { extension }
            )
        })?;
        self.run_ffmpeg_plan_to_file(&input_path, &source_name, plan, cleanup.into_paths())
            .await
    }

    /// Runs the selected ffmpeg plan and returns a new disk-backed linked file.
    async fn run_ffmpeg_plan(
        &self,
        input: &Path,
        linked: LinkedFile,
        plan: FfmpegPlan,
    ) -> Result<LinkedFile> {
        let source_name = linked
            .file_name
            .clone()
            .unwrap_or_else(|| "linked-media".to_string());
        let source_url = linked.source_url;
        let converted = self
            .run_ffmpeg_plan_to_file(input, &source_name, plan, linked.cleanup_paths)
            .await?;
        Ok(LinkedFile {
            file: converted.file,
            file_name: Some(converted.file_name),
            mime: Some(converted.mime),
            source_url,
            unique_id: converted.unique_id,
            cleanup_paths: converted.cleanup_paths,
        })
    }

    /// Runs ffmpeg and returns the converted/remuxed file with cleanup paths.
    async fn run_ffmpeg_plan_to_file(
        &self,
        input: &Path,
        source_name: &str,
        plan: FfmpegPlan,
        cleanup_paths: Vec<PathBuf>,
    ) -> Result<ConvertedMediaFile> {
        let mut cleanup = TempPathCleanup::new(cleanup_paths);
        let output_name = filename_with_extension(source_name, plan.extension);
        let output_path = temp_link_path(&output_name)?;
        cleanup.push(output_path.clone());

        match plan.kind {
            FfmpegPlanKind::TranscodeVideoAv1CopyAudio | FfmpegPlanKind::TranscodeVideoAv1Opus => {
                let svt_args = ffmpeg_args_for_plan(input, &output_path, plan, Some("libsvtav1"));
                if let Err(svt_error) = self.run_ffmpeg_args(svt_args).await {
                    std::fs::remove_file(&output_path).ok();
                    tracing::warn!(
                        error = %format!("{svt_error:#}"),
                        "ffmpeg libsvtav1 conversion failed; retrying with libaom-av1"
                    );
                    let aom_args =
                        ffmpeg_args_for_plan(input, &output_path, plan, Some("libaom-av1"));
                    self.run_ffmpeg_args(aom_args).await.with_context(|| {
                        format!(
                            "libsvtav1 failed first ({svt_error:#}); libaom-av1 fallback failed"
                        )
                    })?;
                }
            }
            _ => {
                let args = ffmpeg_args_for_plan(input, &output_path, plan, None);
                self.run_ffmpeg_args(args).await?;
            }
        }

        let size = std::fs::metadata(&output_path)
            .with_context(|| format!("ffmpeg did not write {}", output_path.display()))?
            .len();
        if size == 0 {
            std::fs::remove_file(&output_path).ok();
            bail!("ffmpeg wrote an empty output file");
        }

        let unique_id = short_id_path(&output_path)?;
        Ok(ConvertedMediaFile {
            file: TelegramFile::LocalPath {
                path: output_path,
                size,
            },
            file_name: output_name,
            mime: plan.mime.to_string(),
            unique_id,
            cleanup_paths: cleanup.into_paths(),
        })
    }

    /// Runs ffmpeg with a bounded timeout and returns stderr on failure.
    async fn run_ffmpeg_args(&self, args: Vec<OsString>) -> Result<()> {
        let output = tokio::time::timeout(
            std::time::Duration::from_secs(FFMPEG_TRANSCODE_TIMEOUT_SECS),
            Command::new(&self.config.ffmpeg_path).args(args).output(),
        )
        .await
        .context("ffmpeg timed out")?
        .context("failed to launch ffmpeg")?;
        if !output.status.success() {
            bail!(
                "ffmpeg exited with {}: {}",
                output.status,
                command_stderr(&output.stderr)
            );
        }
        Ok(())
    }

    /// Largest external file this bot is willing to download before deciding what to do.
    fn max_external_download_bytes(&self) -> u64 {
        self.config
            .max_file_bytes
            .max(self.config.max_archive_file_bytes)
            .max(self.config.max_conversion_file_bytes)
    }

    /// Returns the configured yt-dlp cookie file when it exists.
    fn prepare_ytdlp_cookies_file(&self) -> Result<Option<PathBuf>> {
        let Some(path) = &self.config.ytdlp_cookies_path else {
            return Ok(None);
        };
        let path = PathBuf::from(path);
        if path.is_file() {
            return Ok(Some(path));
        }
        tracing::warn!(
            path = %path.display(),
            "configured yt-dlp cookies file does not exist"
        );
        Ok(None)
    }

    /// Resolves the caption for a message, sharing an album's caption across its photos.
    async fn resolve_caption(&self, chat_id: i64, user_id: i64, message: &Message) -> String {
        if let Some(group_id) = &message.media_group_id {
            if let Some(caption) = message
                .caption
                .as_ref()
                .filter(|caption| !caption.trim().is_empty())
            {
                self.store.put_group_caption(group_id, caption).await.ok();
                return caption.clone();
            }
            for _ in 0..MEDIA_GROUP_CAPTION_WAIT_ATTEMPTS {
                if let Some(caption) = self.store.get_group_caption(group_id).await
                    && !caption.trim().is_empty()
                {
                    return caption;
                }
                tokio::time::sleep(std::time::Duration::from_millis(
                    MEDIA_GROUP_CAPTION_WAIT_MS,
                ))
                .await;
            }
            let caption = self
                .store
                .get_group_caption(group_id)
                .await
                .unwrap_or_default();
            if !caption.trim().is_empty() {
                return caption;
            }
            if let Some(caption) = text_context_for_upload(chat_id, user_id, message).await {
                self.store.put_group_caption(group_id, &caption).await.ok();
                return caption;
            }
            return String::new();
        }
        if let Some(caption) = message
            .caption
            .clone()
            .filter(|caption| !caption.trim().is_empty())
        {
            return caption;
        }
        text_context_for_upload(chat_id, user_id, message)
            .await
            .unwrap_or_default()
    }

    /// Waits briefly for an adjacent text-only message to become upload caption context.
    async fn wait_for_text_context(&self, chat_id: i64, user_id: i64) -> Option<String> {
        for attempt in 0..TEXT_CONTEXT_WAIT_ATTEMPTS {
            if let Some(text) = peek_text_context(chat_id, user_id).await
                && !text.trim().is_empty()
            {
                return Some(text);
            }
            if attempt + 1 < TEXT_CONTEXT_WAIT_ATTEMPTS {
                tokio::time::sleep(std::time::Duration::from_millis(TEXT_CONTEXT_WAIT_MS)).await;
            }
        }
        None
    }

    /// Stores an album caption before any file download/conversion work starts.
    async fn remember_group_caption(&self, message: &Message) {
        let (Some(group_id), Some(caption)) = (&message.media_group_id, &message.caption) else {
            return;
        };
        if caption.trim().is_empty() {
            return;
        }
        self.store.put_group_caption(group_id, caption).await.ok();
    }

    /// Tells the user that a generic camera filename needs a caption or configured prefix.
    async fn reject_generic_img_filename(
        &self,
        chat_id: i64,
        message: &Message,
        original_stem: &str,
    ) -> Result<()> {
        if let Some(group_id) = &message.media_group_id
            && !self
                .store
                .reserve_idempotency(&format!("GENERIC_IMG_NUDGE#{chat_id}#{group_id}"), 300)
                .await
                .unwrap_or(true)
        {
            return Ok(());
        }
        let sample = if original_stem.trim().is_empty() {
            "IMG_..."
        } else {
            original_stem
        };
        let text = format!(
            "❌ <code>{}</code> is a generic camera filename that Commons rejects. Send the album again with a caption, or set a filename prefix in /settings.",
            escape_html(sample)
        );
        self.telegram.send_message(chat_id, &text, None).await
    }

    /// Sends the post-upload confirmation, honoring the user's link settings.
    async fn send_success(
        &self,
        chat_id: i64,
        profile: &Profile,
        reply: UploadSuccessReply<'_>,
    ) -> Result<()> {
        let label = match reply.progress {
            Some(progress) => format!("Uploaded {}/{}", progress.current, progress.total),
            None => "Uploaded".to_string(),
        };
        let mut text = if profile.return_upload_links {
            format!(
                "✅ {label}: <a href=\"{}\">{}</a>",
                reply.url,
                escape_html(reply.filename)
            )
        } else {
            format!("✅ {label} <code>{}</code>", escape_html(reply.filename))
        };
        if reply.compressed_photo {
            text.push_str("\nℹ️ This was a compressed photo; send it as a file for full quality.");
        }
        if profile.return_category_links && !reply.categories.is_empty() {
            text.push_str("\n\n<b>Categories</b>:");
            for category in reply.categories {
                text.push_str(&format!(
                    "\n• <a href=\"{}\">{}</a>",
                    category_url(category),
                    escape_html(category)
                ));
            }
        }
        if profile.return_missing_category_links
            && !reply.categories.is_empty()
            && let Ok(missing) = self.commons.missing_categories(reply.categories).await
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
        caption: String,
        file_name: Option<String>,
        original: Vec<u8>,
    ) -> Result<()> {
        // Evict expired or memory-pressuring staged archives before unpacking another.
        prune_pending(original.len());
        let archive_limit = self.config.max_archive_file_bytes;
        let extraction_file_name = file_name.clone();
        let entries = match tokio::task::spawn_blocking(move || {
            crate::archive::extract_images_with_limit(
                &original,
                extraction_file_name.as_deref(),
                archive_limit,
            )
        })
        .await
        {
            Ok(Ok(entries)) => entries,
            Ok(Err(error)) => {
                let text = format!(
                    "❌ Couldn't read the archive: {}",
                    escape_html(&format!("{error}"))
                );
                return self.telegram.send_message(chat_id, &text, None).await;
            }
            Err(error) => {
                let text = format!(
                    "❌ Couldn't read the archive: {}",
                    escape_html(&format!("archive extraction task failed: {error}"))
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
                caption: caption.clone(),
                filename_prefix: None,
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

        self.upload_entries(chat_id, user_id, profile, &caption, None, &[], entries)
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
        let mut filename_prefix = pending.filename_prefix;
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
                self.clear_archive_prefix_step(user_id).await?;

                filename_prefix = Some(prefix.clone());

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
                if let Some(prefix) = &filename_prefix {
                    prefix_message = format!(
                        "Using filename prefix <code>{}</code>.\n",
                        escape_html(prefix)
                    );
                }
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
            filename_prefix,
            extra_categories,
            pending.entries,
        );
        Ok(())
    }

    /// Uploads every extracted image, replying with an aggregate summary.
    #[allow(clippy::too_many_arguments)]
    async fn upload_entries(
        &self,
        chat_id: i64,
        user_id: i64,
        profile: &mut Profile,
        caption: &str,
        filename_prefix: Option<&str>,
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
                    filename_prefix,
                    extra_categories,
                    TelegramFile::Bytes(entry.bytes),
                    Some(&entry.name),
                    None,
                    &unique,
                    None,
                    &auth,
                    bot_password_session.as_ref(),
                    &author_username,
                )
                .await
            {
                Ok(FileResult::Uploaded { filename, url, .. }) => {
                    uploaded += 1;
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
        self.record_successful_uploads(user_id, uploaded.into())
            .await
            .ok();

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
    filename_prefix: Option<String>,
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
    filename_prefix: Option<String>,
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
fn set_pending_archive_filename_prefix(token: &str, filename_prefix: String) -> Result<()> {
    let mut map = archive_pending().lock().unwrap();
    if let Some(pending) = map.get_mut(token) {
        pending.filename_prefix = Some(filename_prefix.clone());
    }
    drop(map);

    let manifest_path = pending_archive_manifest_path(token);
    if !manifest_path.exists() {
        return Ok(());
    }
    let mut manifest = read_pending_archive_manifest(token)?;
    manifest.filename_prefix = Some(filename_prefix);
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
        filename_prefix: pending.filename_prefix.clone(),
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
        filename_prefix: manifest.filename_prefix,
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

/// Converts a low-level conversion failure into a user-facing rejection reason.
fn conversion_rejection_reason(
    format: convert::SourceFormat,
    dng_mode: DngMode,
    original: &[u8],
    error: &anyhow::Error,
) -> String {
    match (format, dng_mode) {
        (convert::SourceFormat::Dng, DngMode::ConvertToWebp) => {
            debug_assert!(!convert::dng_has_embedded_jpeg(original));
            "This DNG cannot be developed into WebP on this server, and I could not find a usable embedded JPEG preview. Export it to JPEG, TIFF, or WebP first.".to_string()
        }
        (convert::SourceFormat::Dng, DngMode::ExtractEmbeddedJpeg) => {
            "This DNG does not contain a usable embedded JPEG preview. Run /settings dng webp and resend it, or export it to JPEG, TIFF, or WebP first.".to_string()
        }
        _ => format!("Couldn't convert this file: {error}"),
    }
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
        "⚙️ <b>Settings</b>\nCommons account: <code>{}</code>\nLicense: <b>{}</b>\nFilename prefix: <code>{}</code>\nDefault categories: {}\nDNG handling: <b>{}</b>\nReturn upload links: <b>{}</b>\nReturn category links: <b>{}</b>\nReturn non-existing category links: <b>{}</b>",
        escape_html(&account),
        escape_html(profile.license.label()),
        escape_html(&prefix),
        escape_html(&categories),
        escape_html(profile.dng_mode.label()),
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
        "\n\nButtons below toggle options; Filename prefix and License open submenus.\nText commands:\n<code>/settings prefix Your Prefix</code>\n<code>/settings categories Cat A, Cat B</code>\n<code>/settings license cc-by-4.0</code>\n<code>/settings dng webp</code> or <code>/settings dng extract</code>",
    );
    text
}

/// Builds the main settings inline keyboard.
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
        vec![settings_prefix_button(profile)],
        vec![dng_mode_button(profile.dng_mode)],
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
    rows.push(vec![settings_license_button(profile.license)]);
    InlineKeyboardMarkup {
        inline_keyboard: rows,
    }
}

/// Builds the settings entry point for filename-prefix actions.
fn settings_prefix_button(profile: &Profile) -> InlineKeyboardButton {
    let value = if profile.filename_prefix.is_empty() {
        "(none)".to_string()
    } else {
        compact_button_value(&profile.filename_prefix)
    };
    InlineKeyboardButton {
        text: format!("Filename prefix: {value}"),
        callback_data: Some("set:prefix".to_string()),
        url: None,
    }
}

/// Builds the filename-prefix submenu.
fn settings_prefix_keyboard(profile: &Profile) -> InlineKeyboardMarkup {
    let mut rows = vec![vec![InlineKeyboardButton {
        text: "Set filename prefix".to_string(),
        callback_data: Some("set:prefix:set".to_string()),
        url: None,
    }]];
    let clear_label = if profile.filename_prefix.is_empty() {
        "Clear filename prefix (already none)"
    } else {
        "Clear filename prefix"
    };
    rows.push(vec![InlineKeyboardButton {
        text: clear_label.to_string(),
        callback_data: Some("set:prefix:clear".to_string()),
        url: None,
    }]);
    rows.push(vec![InlineKeyboardButton {
        text: "← Back to settings".to_string(),
        callback_data: Some("set:main".to_string()),
        url: None,
    }]);
    InlineKeyboardMarkup {
        inline_keyboard: rows,
    }
}

/// Text shown while waiting for a `/settings` filename-prefix value.
fn settings_prefix_prompt(profile: &Profile) -> String {
    let current = if profile.filename_prefix.is_empty() {
        "(none)".to_string()
    } else {
        profile.filename_prefix.clone()
    };
    format!(
        "Send the new <b>filename prefix</b> as a message.\n\nCurrent prefix: <code>{}</code>\n\nSend <code>clear</code> to remove it.",
        escape_html(&current)
    )
}

/// Keyboard shown while the next text message will become the filename prefix.
fn settings_prefix_input_keyboard(profile: &Profile) -> InlineKeyboardMarkup {
    InlineKeyboardMarkup {
        inline_keyboard: vec![
            vec![InlineKeyboardButton {
                text: if profile.filename_prefix.is_empty() {
                    "Clear filename prefix (already none)".to_string()
                } else {
                    "Clear filename prefix".to_string()
                },
                callback_data: Some("set:prefix:clear".to_string()),
                url: None,
            }],
            vec![InlineKeyboardButton {
                text: "← Back to settings".to_string(),
                callback_data: Some("set:main".to_string()),
                url: None,
            }],
        ],
    }
}

/// Keeps long setting values readable inside Telegram inline buttons.
fn compact_button_value(value: &str) -> String {
    const MAX_CHARS: usize = 28;
    let mut chars = value.chars();
    let compact: String = chars.by_ref().take(MAX_CHARS).collect();
    if chars.next().is_some() {
        format!("{compact}…")
    } else {
        compact
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

/// Builds the DNG mode toggle button.
fn dng_mode_button(mode: DngMode) -> InlineKeyboardButton {
    InlineKeyboardButton {
        text: format!("DNG: {}", mode.label()),
        callback_data: Some("set:dng".to_string()),
        url: None,
    }
}

/// Builds the compact settings entry point for the license submenu.
fn settings_license_button(license: License) -> InlineKeyboardButton {
    InlineKeyboardButton {
        text: format!("License: {}", license.label()),
        callback_data: Some("set:license".to_string()),
        url: None,
    }
}

/// Builds the settings license submenu.
fn settings_license_keyboard(profile: &Profile) -> InlineKeyboardMarkup {
    let mut rows: Vec<Vec<InlineKeyboardButton>> = License::all()
        .iter()
        .map(|license| {
            let selected = if *license == profile.license {
                "✓ "
            } else {
                ""
            };
            vec![InlineKeyboardButton {
                text: format!("{selected}{}", license.label()),
                callback_data: Some(format!(
                    "{}{}",
                    crate::telegram::LICENSE_CALLBACK_PREFIX,
                    license.as_key()
                )),
                url: None,
            }]
        })
        .collect();
    rows.push(vec![InlineKeyboardButton {
        text: "← Back to settings".to_string(),
        callback_data: Some("set:main".to_string()),
        url: None,
    }]);
    InlineKeyboardMarkup {
        inline_keyboard: rows,
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

/// Remembers a text-only message as context for nearby uncaptained uploads.
async fn remember_text_context(chat_id: i64, user_id: i64, text: &str) -> bool {
    let text = text.trim();
    if text.is_empty() {
        return false;
    }
    let now = now_ts();
    let mut contexts = TEXT_CONTEXTS.write().await;
    contexts.retain(|_, context| context.expires_at >= now);
    contexts.insert(
        (chat_id, user_id),
        TextContext {
            text: text.to_string(),
            expires_at: now + TEXT_CONTEXT_TTL_SECONDS,
        },
    );
    tracing::info!(user_id, chat_id, "remembered text for nearby upload");
    true
}

/// Returns recent text for an uncaptained upload.
///
/// Telegram can forward several adjacent message bubbles as one user action. For forwarded media,
/// the text bubble is treated as shared context and is not consumed, so both the preceding and
/// following media batches can use it. Normal uploads keep the older one-shot behavior.
async fn text_context_for_upload(chat_id: i64, user_id: i64, message: &Message) -> Option<String> {
    if message.is_forwarded() {
        peek_text_context(chat_id, user_id).await
    } else {
        take_text_context(chat_id, user_id).await
    }
}

/// Returns recent text context without consuming it.
async fn peek_text_context(chat_id: i64, user_id: i64) -> Option<String> {
    let now = now_ts();
    let mut contexts = TEXT_CONTEXTS.write().await;
    contexts.retain(|_, context| context.expires_at >= now);
    contexts
        .get(&(chat_id, user_id))
        .filter(|context| context.expires_at >= now)
        .map(|context| context.text.clone())
}

/// Consumes recent text for the next normal uncaptained upload in the same chat.
async fn take_text_context(chat_id: i64, user_id: i64) -> Option<String> {
    let now = now_ts();
    let mut contexts = TEXT_CONTEXTS.write().await;
    contexts.retain(|_, context| context.expires_at >= now);
    contexts
        .remove(&(chat_id, user_id))
        .filter(|context| context.expires_at >= now)
        .map(|context| context.text)
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

/// Returns lower-case SHA-1 hex of a Telegram file without loading disk-backed files.
fn sha1_hex_telegram_file(file: &TelegramFile) -> Result<String> {
    match file {
        TelegramFile::Bytes(bytes) => Ok(sha1_hex(bytes)),
        TelegramFile::LocalPath { path, .. } => sha1_hex_path(path),
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

/// Returns lower-case MD5 hex of a Telegram file without loading disk-backed files.
fn md5_hex_telegram_file(file: &TelegramFile) -> Result<String> {
    match file {
        TelegramFile::Bytes(bytes) => Ok(md5_hex(bytes)),
        TelegramFile::LocalPath { path, .. } => md5_hex_path(path),
    }
}

/// Returns lower-case MD5 hex of a file, streaming it in bounded chunks.
fn md5_hex_path(path: &Path) -> Result<String> {
    use md5::{Digest, Md5};
    use std::io::Read;

    let mut file = std::fs::File::open(path)
        .with_context(|| format!("failed to open upload file {}", path.display()))?;
    let mut hasher = Md5::new();
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

fn commons_max_file_size_message(size: Option<u64>) -> String {
    match size {
        Some(size) => format!(
            "linked file is {}, but Wikimedia Commons supports files only up to {}. See {}",
            format_size_limit(size),
            format_size_limit(COMMONS_MAX_FILE_BYTES),
            COMMONS_MAX_FILE_SIZE_DOC
        ),
        None => format!(
            "Wikimedia Commons supports files only up to {}. See {}",
            format_size_limit(COMMONS_MAX_FILE_BYTES),
            COMMONS_MAX_FILE_SIZE_DOC
        ),
    }
}

fn ensure_commons_file_size_limit(size: u64) -> Result<()> {
    if size > COMMONS_MAX_FILE_BYTES {
        bail!("{}", commons_max_file_size_message(Some(size)));
    }
    Ok(())
}

fn direct_link_looks_like_commons_file(file_name: Option<&str>, mime: Option<&str>) -> bool {
    file_name
        .and_then(file_extension_for_name)
        .as_deref()
        .is_some_and(convert::is_commons_accepted)
        || mime
            .and_then(accepted_extension_for_mime)
            .is_some_and(|extension| convert::is_commons_accepted(&extension))
}

/// Returns the filename stem (without extension), if a filename is present.
fn file_stem(file_name: Option<&str>) -> &str {
    file_name
        .map(|name| name.rsplit_once('.').map(|(stem, _)| stem).unwrap_or(name))
        .unwrap_or("")
}

fn effective_filename_prefix<'a>(
    profile_prefix: &'a str,
    upload_prefix: Option<&'a str>,
    caption_description: &str,
    original_stem: &str,
) -> &'a str {
    if let Some(upload_prefix) = upload_prefix {
        return upload_prefix;
    }
    if original_stem.starts_with("IMG_") && !caption_description.trim().is_empty() {
        return "";
    }
    profile_prefix
}

/// Returns true when Commons would reject an `IMG_...` filename for lacking descriptive context.
fn filename_needs_descriptive_context(
    filename_prefix: &str,
    caption_description: &str,
    original_stem: &str,
) -> bool {
    original_stem.starts_with("IMG_")
        && filename_prefix.trim().is_empty()
        && caption_description.trim().is_empty()
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

/// Returns message text that may contain a link, including forwarded caption-style text.
fn message_text_for_links(message: &Message) -> Option<String> {
    message
        .text
        .clone()
        .or_else(|| message.caption.clone())
        .filter(|text| !text.trim().is_empty())
}

/// Finds the first external URL in a Telegram text/caption.
fn first_external_url(text: &str) -> Option<LinkCandidate> {
    text.split_whitespace().find_map(|raw| {
        let token = trim_url_token(raw);
        if token.is_empty() {
            return None;
        }
        let parsed = parse_url_token(token)?;
        if matches!(parsed.scheme(), "http" | "https") {
            Some(LinkCandidate {
                url: parsed,
                token: raw.to_string(),
            })
        } else {
            None
        }
    })
}

/// Parses a token as an URL, accepting common link forms without an explicit scheme.
fn parse_url_token(token: &str) -> Option<Url> {
    Url::parse(token).ok().or_else(|| {
        let lower = token.to_ascii_lowercase();
        if known_link_host_token(&lower) || looks_like_bare_url(&lower) {
            Url::parse(&format!("https://{token}")).ok()
        } else {
            None
        }
    })
}

/// Removes punctuation Telegram users commonly place around pasted URLs.
fn trim_url_token(token: &str) -> &str {
    token
        .trim_matches(|ch: char| {
            matches!(
                ch,
                '<' | '>' | '"' | '\'' | '`' | '(' | ')' | '[' | ']' | '{' | '}' | ',' | ';'
            )
        })
        .trim_end_matches(['.', '!', '?'])
}

/// True when a token starts with a media site host the bot explicitly supports.
fn known_link_host_token(lower: &str) -> bool {
    const HOSTS: &[&str] = &[
        "youtube.com/",
        "www.youtube.com/",
        "m.youtube.com/",
        "youtu.be/",
        "vk.com/",
        "m.vk.com/",
        "vkvideo.ru/",
        "www.vkvideo.ru/",
        "rutube.ru/",
        "www.rutube.ru/",
        "podcasts.apple.com/",
    ];
    HOSTS.iter().any(|host| lower.starts_with(host))
}

/// Conservative fallback for bare direct links such as `example.org/file.zip`.
fn looks_like_bare_url(lower: &str) -> bool {
    lower.contains('/')
        && lower
            .split('/')
            .next()
            .is_some_and(|host| host.contains('.') && !host.contains('@'))
}

/// Removes the detected URL from the message text so the remaining text becomes the caption.
fn caption_without_link(text: &str, link: &LinkCandidate) -> String {
    let caption = text.replacen(&link.token, "", 1);
    caption
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>()
        .join("\n")
}

/// Adds a source directive for linked archives unless the user already supplied one.
#[cfg(feature = "archive")]
fn caption_with_source(caption: &str, source_url: &str) -> String {
    if parse_caption(caption).source.is_some() {
        return caption.to_string();
    }
    let trimmed = caption.trim();
    if trimmed.is_empty() {
        format!("Source: {source_url}")
    } else {
        format!("{trimmed}\nSource: {source_url}")
    }
}

/// Blocks obvious SSRF targets for direct link downloads.
fn is_blocked_url_host(url: &Url) -> bool {
    let Some(host) = url.host_str() else {
        return true;
    };
    let host = host.trim_matches(&['[', ']'][..]).to_ascii_lowercase();
    if matches!(host.as_str(), "localhost" | "localhost.localdomain") {
        return true;
    }
    if let Ok(ip) = host.parse::<std::net::IpAddr>() {
        return match ip {
            std::net::IpAddr::V4(ip) => {
                ip.is_private()
                    || ip.is_loopback()
                    || ip.is_link_local()
                    || ip.is_broadcast()
                    || ip.is_documentation()
                    || ip.octets()[0] == 0
            }
            std::net::IpAddr::V6(ip) => {
                ip.is_loopback() || ip.is_unspecified() || ip.is_unique_local()
            }
        };
    }
    false
}

/// Returns true when yt-dlp should resolve a page URL to media files.
fn needs_ytdlp(url: &Url) -> bool {
    is_youtube_url(url) || is_vk_url(url) || is_rutube_url(url) || is_apple_podcasts_url(url)
}

/// Returns true for YouTube URLs handled through yt-dlp.
fn is_youtube_url(url: &Url) -> bool {
    host_matches(url, &["youtube.com", "youtu.be"])
}

/// Returns true for VK and VK Video URLs handled through yt-dlp.
fn is_vk_url(url: &Url) -> bool {
    host_matches(url, &["vk.com", "vkvideo.ru"])
}

/// Returns true for Rutube URLs handled through yt-dlp.
fn is_rutube_url(url: &Url) -> bool {
    host_matches(url, &["rutube.ru"])
}

/// Returns true for Apple Podcasts episode URLs handled through yt-dlp.
fn is_apple_podcasts_url(url: &Url) -> bool {
    host_matches(url, &["podcasts.apple.com"])
}

/// Returns true for a DropMeFiles short sharing page that needs site-specific resolving.
fn is_dropmefiles_url(url: &Url) -> bool {
    host_matches(url, &["dropmefiles.com"]) && dropmefiles_upload_id(url).is_some()
}

/// Extracts the alphanumeric DropMeFiles upload id from `https://dropmefiles.com/<id>`.
fn dropmefiles_upload_id(url: &Url) -> Option<String> {
    let mut segments = url.path_segments()?;
    let id = segments.next()?.trim();
    if id.is_empty()
        || segments.next().is_some()
        || !id.chars().all(|ch| ch.is_ascii_alphanumeric())
    {
        return None;
    }
    Some(id.to_string())
}

/// Returns true when the URL host is exactly one of `domains` or one of their subdomains.
fn host_matches(url: &Url, domains: &[&str]) -> bool {
    let Some(host) = url.host_str().map(str::to_ascii_lowercase) else {
        return false;
    };
    domains
        .iter()
        .any(|domain| host == *domain || host.ends_with(&format!(".{domain}")))
}

/// Extracts the direct download URL from a DropMeFiles sharing page.
fn dropmefiles_download_url_from_page(page_url: &Url, html: &str) -> Result<Url> {
    if html.contains("id=\"passwordForm\"") || html.contains("id='passwordForm'") {
        bail!("password-protected DropMeFiles links are not supported");
    }

    let upload_id = js_string_var(html, "UPLOADID")
        .or_else(|| dropmefiles_upload_id(page_url))
        .context("DropMeFiles page did not expose an upload id")?;
    let status = js_i64_var(html, "USTATUS");
    let files = js_i64_var(html, "UFILES");
    if status.is_some_and(|status| status > 0) || files == Some(0) {
        bail!("DropMeFiles upload is still in progress. Please retry after it finishes uploading");
    }

    if let Some(href) = html_attr_value(html, "data-href") {
        return resolve_dropmefiles_download_url(page_url, &href);
    }

    let dserver = js_string_var(html, "DSERVERURL")
        .or_else(|| js_string_var(html, "SERVERURL"))
        .context("DropMeFiles page did not expose a download server")?;
    if files == Some(1)
        && let Some(file_id) = dropmefiles_file_ids(html).into_iter().next()
    {
        let base = Url::parse(&dserver).context("DropMeFiles download server URL is invalid")?;
        return base
            .join(&format!("/dl/{upload_id}/{file_id}"))
            .context("DropMeFiles direct download URL is invalid");
    }

    bail!("DropMeFiles page did not expose a download URL");
}

/// Converts a raw DropMeFiles download attribute into an absolute URL.
fn resolve_dropmefiles_download_url(page_url: &Url, raw: &str) -> Result<Url> {
    let value = html_unescape_attr(raw).trim().to_string();
    if value.is_empty() || value.starts_with("javascript:") {
        bail!("DropMeFiles page did not expose a download URL");
    }
    if value.starts_with("//") {
        return Url::parse(&format!("{}:{value}", page_url.scheme()))
            .context("DropMeFiles protocol-relative download URL is invalid");
    }
    Url::parse(&value)
        .or_else(|_| page_url.join(&value))
        .context("DropMeFiles download URL is invalid")
}

/// Reads a single-quoted or double-quoted JavaScript variable from legacy inline page scripts.
fn js_string_var(html: &str, name: &str) -> Option<String> {
    let var = js_var_value(html, name)?;
    let mut chars = var.chars();
    let quote = chars.next()?;
    if quote != '\'' && quote != '"' {
        return None;
    }
    let mut value = String::new();
    let mut escaped = false;
    for ch in chars {
        if escaped {
            value.push(ch);
            escaped = false;
        } else if ch == '\\' {
            escaped = true;
        } else if ch == quote {
            return Some(value);
        } else {
            value.push(ch);
        }
    }
    None
}

/// Reads an integer JavaScript variable from legacy inline page scripts.
fn js_i64_var(html: &str, name: &str) -> Option<i64> {
    let var = js_var_value(html, name)?.trim_start();
    let value = var
        .chars()
        .take_while(|ch| ch.is_ascii_digit() || *ch == '-')
        .collect::<String>();
    if value.is_empty() {
        None
    } else {
        value.parse().ok()
    }
}

/// Returns the text immediately after `var <name> =` in a page script.
fn js_var_value<'a>(html: &'a str, name: &str) -> Option<&'a str> {
    let marker = format!("var {name}");
    let after_marker = html
        .find(&marker)
        .map(|index| &html[index + marker.len()..])?;
    let after_equals = after_marker.split_once('=')?.1;
    Some(after_equals.trim_start())
}

/// Extracts a quoted HTML attribute value from a tag or page fragment.
fn html_attr_value(html: &str, attr: &str) -> Option<String> {
    let pattern = format!("{attr}=");
    let mut rest = html;
    while let Some(index) = rest.find(&pattern) {
        let after = &rest[index + pattern.len()..];
        let mut chars = after.chars();
        let quote = chars.next()?;
        if quote == '\'' || quote == '"' {
            let value = chars.take_while(|ch| *ch != quote).collect::<String>();
            if !value.trim().is_empty() {
                return Some(value);
            }
        }
        rest = &after[quote.len_utf8()..];
    }
    None
}

/// Finds DropMeFiles per-file ids from completed single-file download pages.
fn dropmefiles_file_ids(html: &str) -> Vec<String> {
    let mut ids = Vec::new();
    let mut rest = html;
    while let Some(class_index) = rest.find("fileDownload") {
        let before = &rest[..class_index];
        let tag_start = before.rfind('<').unwrap_or(0);
        let after = &rest[class_index..];
        let tag_end = after
            .find('>')
            .map(|index| class_index + index)
            .unwrap_or(rest.len());
        let tag = &rest[tag_start..tag_end];
        if let Some(id) = html_attr_value(tag, "id")
            && !id.is_empty()
            && id
                .chars()
                .all(|ch| ch.is_ascii_alphanumeric() || ch == '_' || ch == '-')
        {
            ids.push(id);
        }
        rest = &rest[class_index + "fileDownload".len()..];
    }
    ids
}

/// Decodes the small subset of HTML entities used inside DropMeFiles attributes.
fn html_unescape_attr(value: &str) -> String {
    value
        .replace("&amp;", "&")
        .replace("&quot;", "\"")
        .replace("&#039;", "'")
        .replace("&#39;", "'")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
}

/// Extracts a filename from HTTP headers or the URL path.
fn filename_from_headers_or_url(headers: &HeaderMap, url: &Url) -> Option<String> {
    headers
        .get(http::header::CONTENT_DISPOSITION)
        .and_then(|value| value.to_str().ok())
        .and_then(content_disposition_filename)
        .or_else(|| filename_from_url(url))
        .map(|name| sanitize_download_filename(&name))
}

/// Parses the simple `filename=` forms commonly returned in Content-Disposition.
fn content_disposition_filename(value: &str) -> Option<String> {
    for part in value.split(';').map(str::trim) {
        if let Some(rest) = part.strip_prefix("filename*=") {
            let rest = rest.trim_matches('"');
            let encoded = rest
                .split_once("''")
                .map(|(_, encoded)| encoded)
                .unwrap_or(rest);
            if let Ok(decoded) = urlencoding::decode(encoded) {
                let decoded = decoded.trim();
                if !decoded.is_empty() {
                    return Some(decoded.to_string());
                }
            }
        }
        if let Some(rest) = part.strip_prefix("filename=") {
            let name = rest.trim_matches('"').trim();
            if !name.is_empty() {
                return Some(name.to_string());
            }
        }
    }
    None
}

/// Uses the URL path's final segment as a fallback filename.
fn filename_from_url(url: &Url) -> Option<String> {
    let segment = url
        .path_segments()
        .and_then(|mut segments| segments.next_back())
        .unwrap_or_default();
    if segment.is_empty() {
        return None;
    }
    urlencoding::decode(segment)
        .ok()
        .map(|name| name.to_string())
        .filter(|name| !name.trim().is_empty())
}

/// Returns the HTTP content type without parameters.
fn content_type(headers: &HeaderMap) -> Option<String> {
    headers
        .get(http::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.split(';').next())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}

/// Sanitizes a remote filename for local temp storage while preserving Unicode.
fn sanitize_download_filename(name: &str) -> String {
    let basename = name
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or(name)
        .trim()
        .trim_matches('.');
    let mut safe = basename
        .chars()
        .map(|ch| {
            if ch.is_control()
                || matches!(
                    ch,
                    '\0' | '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|'
                )
            {
                '_'
            } else {
                ch
            }
        })
        .collect::<String>();
    while safe.contains("__") {
        safe = safe.replace("__", "_");
    }
    let safe = safe.trim_matches('_').trim();
    let safe = if safe.is_empty() {
        "linked-media"
    } else {
        safe
    };
    let mut chars = safe.chars();
    let shortened = chars.by_ref().take(180).collect::<String>();
    if chars.next().is_some() {
        shortened
    } else {
        safe.to_string()
    }
}

/// Returns a lower-case filename extension, without a leading dot.
fn file_extension_for_name(name: &str) -> Option<String> {
    name.rsplit_once('.')
        .map(|(_, ext)| ext.to_ascii_lowercase())
        .filter(|ext| {
            !ext.is_empty() && ext.len() <= 8 && ext.chars().all(|ch| ch.is_ascii_alphanumeric())
        })
}

/// Returns true when an unsupported file is likely audio/video that ffmpeg can convert.
fn should_try_ffmpeg_media_conversion(
    file_name: Option<&str>,
    mime: Option<&str>,
    extension: &str,
) -> bool {
    let extension = file_name
        .and_then(file_extension_for_name)
        .unwrap_or_else(|| extension.trim_start_matches('.').to_ascii_lowercase());
    if matches!(
        extension.as_str(),
        "mov"
            | "qt"
            | "mp4"
            | "m4v"
            | "m4a"
            | "mkv"
            | "avi"
            | "wmv"
            | "flv"
            | "3gp"
            | "3g2"
            | "mts"
            | "m2ts"
            | "ts"
            | "aac"
            | "wma"
    ) {
        return true;
    }
    mime.map(str::to_ascii_lowercase).is_some_and(|mime| {
        mime.starts_with("video/")
            || mime.starts_with("audio/")
            || matches!(mime.as_str(), "application/ogg" | "application/x-matroska")
    })
}

/// Builds a stable local source filename for ffmpeg when Telegram did not provide one.
fn ffmpeg_source_filename(file_name: Option<&str>, mime: Option<&str>) -> String {
    if let Some(file_name) = file_name.filter(|name| !name.trim().is_empty()) {
        return sanitize_download_filename(file_name);
    }
    let extension = mime
        .and_then(ffmpeg_input_extension_for_mime)
        .unwrap_or("media");
    format!("telegram-media.{extension}")
}

/// Maps common non-Commons media MIME types to an ffmpeg-friendly input extension.
fn ffmpeg_input_extension_for_mime(mime: &str) -> Option<&'static str> {
    match mime.to_ascii_lowercase().as_str() {
        "video/quicktime" => Some("mov"),
        "video/mp4" => Some("mp4"),
        "video/x-matroska" | "application/x-matroska" => Some("mkv"),
        "video/x-msvideo" => Some("avi"),
        "audio/aac" => Some("aac"),
        "audio/mp4" | "audio/x-m4a" => Some("m4a"),
        _ => None,
    }
}

/// Maps extensions to MIME types used when re-entering the upload pipeline.
fn mime_for_extension(extension: String) -> Option<String> {
    let mime = match extension.as_str() {
        "jpg" | "jpeg" => "image/jpeg",
        "png" => "image/png",
        "gif" => "image/gif",
        "svg" => "image/svg+xml",
        "tif" | "tiff" => "image/tiff",
        "webp" => "image/webp",
        "pdf" => "application/pdf",
        "djvu" => "image/vnd.djvu",
        "ogg" | "oga" => "audio/ogg",
        "opus" => "audio/opus",
        "mp3" => "audio/mpeg",
        "wav" => "audio/wav",
        "flac" => "audio/flac",
        "webm" => "video/webm",
        "ogv" => "video/ogg",
        "mpg" | "mpeg" => "video/mpeg",
        _ => return None,
    };
    Some(mime.to_string())
}

/// Maps a trusted HTTP content type to an accepted Commons extension.
fn accepted_extension_for_mime(mime: &str) -> Option<String> {
    let extension = convert::passthrough_extension(None, Some(mime));
    if extension != "bin" && convert::is_commons_accepted(&extension) {
        Some(extension)
    } else {
        None
    }
}

/// Replaces a filename extension while preserving the stem.
fn filename_with_extension(file_name: &str, extension: &str) -> String {
    let stem = file_stem(Some(file_name)).trim();
    let stem = if stem.is_empty() {
        "linked-media"
    } else {
        stem
    };
    sanitize_download_filename(&format!("{stem}.{extension}"))
}

/// Creates a unique temp path for a linked download or conversion output.
fn temp_link_path(file_name: &str) -> Result<PathBuf> {
    let safe_name = sanitize_download_filename(file_name);
    let random: u64 = rand::random();
    let path = std::env::temp_dir().join(format!(
        "commons-link-{}-{random:016x}-{safe_name}",
        std::process::id()
    ));
    Ok(path)
}

/// Finds the main yt-dlp output file in a temp directory.
fn find_downloaded_file(dir: &Path) -> Result<PathBuf> {
    let mut files = Vec::new();
    for entry in
        std::fs::read_dir(dir).with_context(|| format!("failed to read {}", dir.display()))?
    {
        let entry = entry?;
        let path = entry.path();
        if !entry.file_type()?.is_file() {
            continue;
        }
        let name = path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or_default();
        if name.ends_with(".part")
            || name.ends_with(".ytdl")
            || name.ends_with(".temp")
            || name.ends_with(".tmp")
        {
            continue;
        }
        let len = entry.metadata()?.len();
        files.push((len, path));
    }
    files
        .into_iter()
        .max_by_key(|(len, _)| *len)
        .map(|(_, path)| path)
        .context("no downloaded media file found")
}

/// Returns a short content id for a local file.
fn short_id_path(path: &Path) -> Result<String> {
    Ok(sha1_hex_path(path)?[..10].to_string())
}

/// Compact command stderr for user-facing download/conversion errors.
fn command_stderr(stderr: &[u8]) -> String {
    let text = String::from_utf8_lossy(stderr);
    let text = text.trim();
    if text.is_empty() {
        "(no stderr)".to_string()
    } else {
        const MAX_CHARS: usize = 700;
        let mut chars = text.chars();
        let truncated = chars.by_ref().take(MAX_CHARS).collect::<String>();
        if chars.next().is_some() {
            format!("{truncated}…")
        } else {
            truncated
        }
    }
}

/// Chooses the cheapest ffmpeg action that yields a Commons-accepted file.
fn ffmpeg_plan_for_probe(probe: &MediaProbe) -> Option<FfmpegPlan> {
    let video = probe.first_video_codec();
    let audio = probe.first_audio_codec();
    match video {
        None => match audio {
            Some("mp3") => Some(FfmpegPlan {
                kind: FfmpegPlanKind::ExtractMp3,
                extension: "mp3",
                mime: "audio/mpeg",
            }),
            Some("flac") => Some(FfmpegPlan {
                kind: FfmpegPlanKind::ExtractFlac,
                extension: "flac",
                mime: "audio/flac",
            }),
            Some(codec) if is_ogg_audio_codec(codec) => Some(FfmpegPlan {
                kind: FfmpegPlanKind::ExtractOggAudio,
                extension: "ogg",
                mime: "audio/ogg",
            }),
            Some(_) => Some(FfmpegPlan {
                kind: FfmpegPlanKind::TranscodeAudioOpus,
                extension: "ogg",
                mime: "audio/ogg",
            }),
            None => None,
        },
        Some(codec) if codec == "theora" && audio.is_none_or(is_ogg_audio_codec) => {
            Some(FfmpegPlan {
                kind: FfmpegPlanKind::RemuxOgv,
                extension: "ogv",
                mime: "video/ogg",
            })
        }
        Some(codec) if is_webm_video_codec(codec) && audio.is_none_or(is_webm_audio_codec) => {
            Some(FfmpegPlan {
                kind: FfmpegPlanKind::RemuxWebm,
                extension: "webm",
                mime: "video/webm",
            })
        }
        Some(codec) if is_webm_video_codec(codec) => Some(FfmpegPlan {
            kind: FfmpegPlanKind::CopyVideoTranscodeAudioWebm,
            extension: "webm",
            mime: "video/webm",
        }),
        Some(_) if audio.is_none_or(is_webm_audio_codec) => Some(FfmpegPlan {
            kind: FfmpegPlanKind::TranscodeVideoAv1CopyAudio,
            extension: "webm",
            mime: "video/webm",
        }),
        Some(_) => Some(FfmpegPlan {
            kind: FfmpegPlanKind::TranscodeVideoAv1Opus,
            extension: "webm",
            mime: "video/webm",
        }),
    }
}

fn is_webm_video_codec(codec: &str) -> bool {
    matches!(codec, "av1" | "vp8" | "vp9")
}

fn is_webm_audio_codec(codec: &str) -> bool {
    matches!(codec, "opus" | "vorbis")
}

fn is_ogg_audio_codec(codec: &str) -> bool {
    matches!(codec, "opus" | "vorbis")
}

/// Builds ffmpeg arguments for a media conversion plan.
fn ffmpeg_args_for_plan(
    input: &Path,
    output: &Path,
    plan: FfmpegPlan,
    av1_encoder: Option<&str>,
) -> Vec<OsString> {
    let mut args = vec![
        "-y".into(),
        "-hide_banner".into(),
        "-nostdin".into(),
        "-i".into(),
        input.as_os_str().to_os_string(),
    ];
    match plan.kind {
        FfmpegPlanKind::RemuxWebm | FfmpegPlanKind::RemuxOgv => {
            args.extend(os_args(["-map", "0", "-c", "copy"]));
        }
        FfmpegPlanKind::ExtractOggAudio
        | FfmpegPlanKind::ExtractMp3
        | FfmpegPlanKind::ExtractFlac => {
            args.extend(os_args(["-vn", "-map", "0:a:0", "-c:a", "copy"]));
        }
        FfmpegPlanKind::TranscodeAudioOpus => {
            args.extend(os_args([
                "-vn", "-map", "0:a:0", "-c:a", "libopus", "-b:a", "128k", "-f", "ogg",
            ]));
        }
        FfmpegPlanKind::CopyVideoTranscodeAudioWebm => {
            args.extend(os_args([
                "-map", "0:v:0", "-map", "0:a:0?", "-c:v", "copy", "-c:a", "libopus", "-b:a",
                "128k",
            ]));
        }
        FfmpegPlanKind::TranscodeVideoAv1CopyAudio => {
            args.extend(os_args(["-map", "0:v:0", "-map", "0:a:0?"]));
            append_av1_encoder_args(&mut args, av1_encoder.unwrap_or("libsvtav1"));
            args.extend(os_args(["-c:a", "copy"]));
        }
        FfmpegPlanKind::TranscodeVideoAv1Opus => {
            args.extend(os_args(["-map", "0:v:0", "-map", "0:a:0?"]));
            append_av1_encoder_args(&mut args, av1_encoder.unwrap_or("libsvtav1"));
            args.extend(os_args(["-c:a", "libopus", "-b:a", "128k"]));
        }
    }
    args.push(output.as_os_str().to_os_string());
    args
}

fn append_av1_encoder_args(args: &mut Vec<OsString>, encoder: &str) {
    match encoder {
        "libaom-av1" => args.extend(os_args([
            "-c:v",
            "libaom-av1",
            "-crf",
            "35",
            "-b:v",
            "0",
            "-cpu-used",
            "6",
            "-row-mt",
            "1",
            "-pix_fmt",
            "yuv420p",
        ])),
        _ => args.extend(os_args([
            "-c:v",
            "libsvtav1",
            "-crf",
            "35",
            "-preset",
            "8",
            "-pix_fmt",
            "yuv420p",
        ])),
    }
}

fn os_args<const N: usize>(args: [&str; N]) -> Vec<OsString> {
    args.into_iter().map(OsString::from).collect()
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
    use super::{
        COMMONS_MAX_FILE_BYTES, COMMONS_MAX_FILE_SIZE_DOC, FfmpegPlan, FfmpegPlanKind, MediaProbe,
        MediaStreamInfo, TEXT_CONTEXTS, TextContext, UPDATE_ALREADY_IN_PROGRESS_ERROR,
        caption_without_link, commons_max_file_size_message, conversion_rejection_reason,
        direct_link_looks_like_commons_file, dropmefiles_download_url_from_page,
        dropmefiles_file_ids, dropmefiles_upload_id, effective_filename_prefix,
        ensure_commons_file_size_limit, ffmpeg_plan_for_probe, filename_needs_descriptive_context,
        first_external_url, is_dropmefiles_url, media_group_upload_progress, merge_categories,
        now_ts, parse_category_list, register_media_group_upload, remember_text_context,
        settings_keyboard, settings_license_keyboard, settings_prefix_keyboard,
        should_try_ffmpeg_media_conversion, status_for_webhook_error, take_text_context,
        text_context_for_upload,
    };
    use crate::commons::{build_filename, parse_caption};
    use crate::convert::SourceFormat;
    use crate::models::{Chat, DngMode, License, Message, Profile, User};
    use http::StatusCode;

    fn test_message(chat_id: i64, user_id: i64, forwarded: bool) -> Message {
        Message {
            message_id: Some(1),
            chat: Chat { id: chat_id },
            from: Some(User { id: user_id }),
            forward_origin: forwarded.then(|| serde_json::json!({"type": "user"})),
            forward_from: None,
            forward_sender_name: None,
            forward_from_chat: None,
            forward_date: None,
            text: None,
            caption: None,
            media_group_id: None,
            document: None,
            photo: None,
            audio: None,
            voice: None,
            video: None,
        }
    }

    #[test]
    fn in_progress_webhook_errors_are_retryable() {
        let error = anyhow::anyhow!("{UPDATE_ALREADY_IN_PROGRESS_ERROR}: 123");
        assert_eq!(
            status_for_webhook_error(&error),
            StatusCode::SERVICE_UNAVAILABLE
        );
    }

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

    #[test]
    fn finds_external_urls_in_forwarded_text_forms() {
        let link = first_external_url("Forwarded: see youtu.be/abc123?si=x").unwrap();
        assert_eq!(link.url.as_str(), "https://youtu.be/abc123?si=x");
        assert_eq!(
            caption_without_link("Forwarded: see youtu.be/abc123?si=x", &link),
            "Forwarded: see"
        );

        let direct = first_external_url("Archive: https://example.org/a.rar").unwrap();
        assert_eq!(direct.url.as_str(), "https://example.org/a.rar");
        assert_eq!(
            caption_without_link("Archive: https://example.org/a.rar", &direct),
            "Archive:"
        );
    }

    #[test]
    fn media_group_progress_counts_registered_album_items() {
        let chat_id = -9_001_003;
        let group_id = "album-progress-test";
        register_media_group_upload(chat_id, Some(group_id));
        register_media_group_upload(chat_id, Some(group_id));

        assert_eq!(
            media_group_upload_progress(chat_id, Some(group_id)),
            Some(super::UploadProgress {
                current: 1,
                total: 2,
            })
        );
        assert_eq!(
            media_group_upload_progress(chat_id, Some(group_id)),
            Some(super::UploadProgress {
                current: 2,
                total: 2,
            })
        );
        assert_eq!(media_group_upload_progress(chat_id, Some(group_id)), None);
    }

    #[test]
    fn detects_dropmefiles_share_links() {
        let url = url::Url::parse("https://dropmefiles.com/iJAKb").unwrap();
        assert!(is_dropmefiles_url(&url));
        assert_eq!(dropmefiles_upload_id(&url).as_deref(), Some("iJAKb"));

        let root = url::Url::parse("https://dropmefiles.com/").unwrap();
        assert!(!is_dropmefiles_url(&root));
    }

    #[test]
    fn resolves_dropmefiles_ready_download_href() {
        let page_url = url::Url::parse("https://dropmefiles.com/abc12").unwrap();
        let html = r#"
            <script>
            var DSERVERURL = 'https://drop5.dropmefile.com';
            var UPLOADID = 'abc12';
            var USTATUS = 0;
            var UFILES = 2;
            </script>
            <a class="download_btn start_dl_btn" data-href="https://drop5.dropmefile.com/dl/abc12?x=1&amp;y=2">download</a>
        "#;

        let resolved = dropmefiles_download_url_from_page(&page_url, html).unwrap();
        assert_eq!(
            resolved.as_str(),
            "https://drop5.dropmefile.com/dl/abc12?x=1&y=2"
        );
    }

    #[test]
    fn resolves_dropmefiles_single_file_fallback() {
        let page_url = url::Url::parse("https://dropmefiles.com/abc12").unwrap();
        let html = r#"
            <script>
            var DSERVERURL = 'https://drop5.dropmefile.com';
            var UPLOADID = 'abc12';
            var USTATUS = 0;
            var UFILES = 1;
            </script>
            <li class="fileDownload" id="file_7" data-fsize="100"></li>
        "#;

        assert_eq!(dropmefiles_file_ids(html), vec!["file_7"]);
        let resolved = dropmefiles_download_url_from_page(&page_url, html).unwrap();
        assert_eq!(
            resolved.as_str(),
            "https://drop5.dropmefile.com/dl/abc12/file_7"
        );
    }

    #[test]
    fn rejects_dropmefiles_upload_still_in_progress() {
        let page_url = url::Url::parse("https://dropmefiles.com/iJAKb").unwrap();
        let html = r#"
            <script>
            var UPLOADID = 'iJAKb';
            var USTATUS = 2;
            var UFILES = 0;
            </script>
        "#;

        let error = dropmefiles_download_url_from_page(&page_url, html).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("DropMeFiles upload is still in progress")
        );
    }

    #[tokio::test]
    async fn text_context_is_reusable_for_forwarded_uploads() {
        let chat_id = -9_001_001;
        let user_id = 9_001_001;
        let message = test_message(chat_id, user_id, true);
        TEXT_CONTEXTS.write().await.insert(
            (chat_id, user_id),
            TextContext {
                text: "Храм Вознесения Господня\nCategories: Churches".into(),
                expires_at: now_ts() + 60,
            },
        );

        assert_eq!(
            text_context_for_upload(chat_id, user_id, &message)
                .await
                .as_deref(),
            Some("Храм Вознесения Господня\nCategories: Churches")
        );
        assert_eq!(
            text_context_for_upload(chat_id, user_id, &message)
                .await
                .as_deref(),
            Some("Храм Вознесения Господня\nCategories: Churches")
        );
    }

    #[tokio::test]
    async fn text_context_is_consumed_for_normal_uploads() {
        let chat_id = -9_001_004;
        let user_id = 9_001_004;
        let message = test_message(chat_id, user_id, false);
        TEXT_CONTEXTS.write().await.insert(
            (chat_id, user_id),
            TextContext {
                text: "Фонтан усадьбы".into(),
                expires_at: now_ts() + 60,
            },
        );

        assert_eq!(
            text_context_for_upload(chat_id, user_id, &message)
                .await
                .as_deref(),
            Some("Фонтан усадьбы")
        );
        assert_eq!(
            text_context_for_upload(chat_id, user_id, &message).await,
            None
        );
    }

    #[tokio::test]
    async fn plain_text_can_be_remembered_as_upload_context() {
        let chat_id = -9_001_005;
        let user_id = 9_001_005;
        let message = test_message(chat_id, user_id, false);

        assert!(remember_text_context(chat_id, user_id, "Фонтан усадьбы").await);
        assert_eq!(
            text_context_for_upload(chat_id, user_id, &message)
                .await
                .as_deref(),
            Some("Фонтан усадьбы")
        );
    }

    #[tokio::test]
    async fn expired_text_context_is_ignored() {
        let chat_id = -9_001_002;
        let user_id = 9_001_002;
        TEXT_CONTEXTS.write().await.insert(
            (chat_id, user_id),
            TextContext {
                text: "Too old".into(),
                expires_at: now_ts() - 1,
            },
        );

        assert_eq!(take_text_context(chat_id, user_id).await, None);
    }

    #[test]
    fn commons_size_limit_message_links_to_docs() {
        let error = ensure_commons_file_size_limit(COMMONS_MAX_FILE_BYTES + 1)
            .expect_err("file above Commons max must be rejected");
        let message = error.to_string();
        assert!(message.contains("Wikimedia Commons supports files only up to"));
        assert!(message.contains("5.0 GB (5120 MB)"));
        assert!(message.contains(COMMONS_MAX_FILE_SIZE_DOC));
        assert!(ensure_commons_file_size_limit(COMMONS_MAX_FILE_BYTES).is_ok());
        assert_eq!(
            commons_max_file_size_message(None),
            format!(
                "Wikimedia Commons supports files only up to 5.0 GB (5120 MB). See {COMMONS_MAX_FILE_SIZE_DOC}"
            )
        );
    }

    #[test]
    fn commons_size_limit_applies_only_to_supported_direct_files() {
        assert!(direct_link_looks_like_commons_file(
            Some("video.webm"),
            None
        ));
        assert!(direct_link_looks_like_commons_file(
            Some("download"),
            Some("image/jpeg")
        ));
        assert!(!direct_link_looks_like_commons_file(
            Some("archive.zip"),
            None
        ));
        assert!(!direct_link_looks_like_commons_file(
            Some("archive.rar"),
            None
        ));
        assert!(!direct_link_looks_like_commons_file(
            Some("video.mp4"),
            Some("video/mp4")
        ));
        assert!(!direct_link_looks_like_commons_file(None, None));
    }

    #[test]
    fn mov_files_are_ffmpeg_conversion_candidates() {
        assert!(should_try_ffmpeg_media_conversion(
            Some("clip.mov"),
            None,
            "mov"
        ));
        assert!(should_try_ffmpeg_media_conversion(
            None,
            Some("video/quicktime"),
            "bin"
        ));
        assert!(should_try_ffmpeg_media_conversion(
            Some("clip.MOV"),
            Some("application/octet-stream"),
            "mov"
        ));
        assert!(!should_try_ffmpeg_media_conversion(
            Some("archive.zip"),
            None,
            "zip"
        ));
    }

    #[test]
    fn media_probe_plans_remux_extract_or_convert() {
        assert_eq!(plan(None, Some("mp3")).kind, FfmpegPlanKind::ExtractMp3);
        assert_eq!(
            plan(None, Some("opus")).kind,
            FfmpegPlanKind::ExtractOggAudio
        );
        assert_eq!(
            plan(Some("av1"), Some("opus")),
            FfmpegPlan {
                kind: FfmpegPlanKind::RemuxWebm,
                extension: "webm",
                mime: "video/webm",
            }
        );
        assert_eq!(
            plan(Some("theora"), Some("vorbis")),
            FfmpegPlan {
                kind: FfmpegPlanKind::RemuxOgv,
                extension: "ogv",
                mime: "video/ogg",
            }
        );
        assert_eq!(
            plan(Some("av1"), Some("aac")).kind,
            FfmpegPlanKind::CopyVideoTranscodeAudioWebm
        );
        assert_eq!(
            plan(Some("h264"), Some("aac")).kind,
            FfmpegPlanKind::TranscodeVideoAv1Opus
        );
    }

    fn plan(video: Option<&str>, audio: Option<&str>) -> FfmpegPlan {
        ffmpeg_plan_for_probe(&probe(video, audio)).unwrap()
    }

    fn probe(video: Option<&str>, audio: Option<&str>) -> MediaProbe {
        let mut streams = Vec::new();
        if let Some(codec) = video {
            streams.push(MediaStreamInfo {
                kind: "video".into(),
                codec: Some(codec.into()),
            });
        }
        if let Some(codec) = audio {
            streams.push(MediaStreamInfo {
                kind: "audio".into(),
                codec: Some(codec.into()),
            });
        }
        MediaProbe { streams }
    }

    #[test]
    fn img_filename_uses_caption_instead_of_stored_prefix() {
        let parsed = parse_caption("Храм Вознесения Господня\n📍Ждановичи");
        let prefix = effective_filename_prefix(
            "2014,_Минск,Боруны,_Гольшаны,_и_еще_что_то_",
            None,
            &parsed.description,
            "IMG_1910",
        );
        let filename = build_filename(prefix, &parsed.description, "IMG_1910", "webp", "x");

        assert_eq!(prefix, "");
        assert_eq!(filename, "Храм Вознесения Господня Ждановичи IMG_1910.webp");
    }

    #[test]
    fn explicit_upload_prefix_wins_for_img_filename() {
        let prefix = effective_filename_prefix(
            "Stored default",
            Some("Archive_"),
            "Храм Вознесения Господня",
            "IMG_1910",
        );

        assert_eq!(prefix, "Archive_");
    }

    #[test]
    fn bare_img_filename_needs_description_or_prefix() {
        assert!(filename_needs_descriptive_context("", "", "IMG_1910"));
        assert!(!filename_needs_descriptive_context(
            "",
            "Храм Вознесения Господня",
            "IMG_1910"
        ));
        assert!(!filename_needs_descriptive_context(
            "Belarus_2014_",
            "",
            "IMG_1910"
        ));
        assert!(!filename_needs_descriptive_context("", "", "DSC_1910"));
    }

    #[test]
    fn settings_keyboard_uses_single_license_submenu_button() {
        let profile = Profile {
            license: License::CcBySa40,
            ..Profile::default()
        };
        let keyboard = settings_keyboard(&profile);

        let license_buttons: Vec<_> = keyboard
            .inline_keyboard
            .iter()
            .flat_map(|row| row.iter())
            .filter(|button| button.callback_data.as_deref() == Some("set:license"))
            .collect();
        assert_eq!(license_buttons.len(), 1);
        assert_eq!(license_buttons[0].text, "License: CC BY-SA 4.0");
        assert!(
            keyboard
                .inline_keyboard
                .iter()
                .flat_map(|row| row.iter())
                .all(|button| !button
                    .callback_data
                    .as_deref()
                    .unwrap_or_default()
                    .starts_with(crate::telegram::LICENSE_CALLBACK_PREFIX))
        );
    }

    #[test]
    fn settings_keyboard_has_prefix_submenu_button() {
        let profile = Profile {
            filename_prefix: "A long filename prefix for a trip".into(),
            ..Profile::default()
        };
        let keyboard = settings_keyboard(&profile);
        let prefix_button = keyboard
            .inline_keyboard
            .iter()
            .flat_map(|row| row.iter())
            .find(|button| button.callback_data.as_deref() == Some("set:prefix"))
            .expect("settings should include a filename-prefix submenu");

        assert!(prefix_button.text.starts_with("Filename prefix: "));
    }

    #[test]
    fn settings_prefix_keyboard_can_set_clear_and_return() {
        let profile = Profile {
            filename_prefix: "Trip".into(),
            ..Profile::default()
        };
        let keyboard = settings_prefix_keyboard(&profile);
        let callbacks: Vec<_> = keyboard
            .inline_keyboard
            .iter()
            .flat_map(|row| row.iter())
            .map(|button| button.callback_data.as_deref().unwrap_or_default())
            .collect();

        assert_eq!(
            callbacks,
            vec!["set:prefix:set", "set:prefix:clear", "set:main"]
        );
    }

    #[test]
    fn settings_license_keyboard_lists_licenses_and_back() {
        let profile = Profile {
            license: License::Cc0,
            ..Profile::default()
        };
        let keyboard = settings_license_keyboard(&profile);
        let callbacks: Vec<_> = keyboard
            .inline_keyboard
            .iter()
            .flat_map(|row| row.iter())
            .map(|button| button.callback_data.as_deref().unwrap_or_default())
            .collect();

        assert!(callbacks.contains(&"license:cc-by-4.0"));
        assert!(callbacks.contains(&"license:cc-by-sa-4.0"));
        assert!(callbacks.contains(&"license:cc-zero"));
        assert!(callbacks.contains(&"license:PD-Russia-expired"));
        assert!(callbacks.contains(&"license:PD-Russia"));
        assert!(callbacks.contains(&"license:PD-RusEmpire"));
        assert_eq!(callbacks.last(), Some(&"set:main"));
        assert!(
            keyboard
                .inline_keyboard
                .iter()
                .flatten()
                .any(|button| button.text == "✓ CC0 (public domain)")
        );
    }

    #[test]
    fn dng_conversion_error_suggests_export_without_preview() {
        let reason = conversion_rejection_reason(
            SourceFormat::Dng,
            DngMode::ConvertToWebp,
            b"II*\0not-a-jpeg",
            &anyhow::anyhow!("decoder failed"),
        );

        assert!(reason.contains("Export it to JPEG, TIFF, or WebP first"));
        assert!(!reason.contains("decoder failed"));
    }
}

#[cfg(all(test, feature = "archive"))]
mod archive_tests {
    use super::{
        PENDING_TTL_SECS, PendingArchiveManifest, TELEGRAM_PHOTO_PREVIEW_MAX_BYTES,
        archive_confirmation_buttons, archive_entry_needs_filename_prefix, archive_name_category,
        archive_name_prefix, archive_needs_filename_prefix, archive_prefix_keyboard, is_low_memory,
        parse_mem_available_kb, pending_is_expired, short_id, should_send_original_archive_preview,
        upload_categories,
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
    fn original_archive_preview_is_skipped_when_telegram_would_reject_it() {
        assert!(should_send_original_archive_preview(
            false,
            TELEGRAM_PHOTO_PREVIEW_MAX_BYTES
        ));
        assert!(!should_send_original_archive_preview(
            false,
            TELEGRAM_PHOTO_PREVIEW_MAX_BYTES + 1
        ));
        assert!(!should_send_original_archive_preview(false, usize::MAX));
        assert!(!should_send_original_archive_preview(true, 1));
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
        assert_eq!(manifest.filename_prefix, None);
        assert_eq!(manifest.archive_file_name, None);
    }
}
