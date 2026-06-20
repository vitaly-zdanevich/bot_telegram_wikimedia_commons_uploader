use crate::commons::{
    CommonsClient, DescriptionParams, UploadOutcome, UploadRequest, build_filename, build_wikitext,
    category_url, parse_caption,
};
use crate::config::Config;
use crate::convert;
use crate::crypto::Cipher;
use crate::metadata;
use crate::models::{
    CallbackQuery, License, Message, OnboardingStep, Profile, Update, UploadProvenance,
};
use crate::store::Store;
use crate::telegram::{
    InlineKeyboardButton, InlineKeyboardMarkup, TelegramClient, escape_html, license_keyboard,
};
use anyhow::{Context, Result};
use lambda_http::{Body, Request, Response};

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

/// Handles one AWS Lambda HTTP request from the Telegram webhook.
pub async fn handle_lambda_request(request: Request) -> Result<Response<Body>> {
    let config = Config::from_env();
    verify_telegram_secret(&config, &request)?;
    let update: Update =
        serde_json::from_slice(request.body().as_ref()).context("invalid Telegram update JSON")?;

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
                return ok_response();
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
    ok_response()
}

/// Returns the standard Telegram webhook success response.
fn ok_response() -> Result<Response<Body>> {
    Ok(Response::builder()
        .status(200)
        .body(Body::Text("ok".into()))?)
}

/// Verifies the Telegram webhook secret header when configured.
fn verify_telegram_secret(config: &Config, request: &Request) -> Result<()> {
    let Some(expected) = &config.telegram_webhook_secret else {
        return Ok(());
    };
    let actual = request
        .headers()
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

impl Bot {
    /// Builds a bot from runtime configuration.
    fn from_config(config: Config) -> Self {
        let telegram = TelegramClient::new(config.telegram_bot_token.clone().unwrap_or_default());
        let store = Store::new(&config);
        let commons = CommonsClient::new(config.commons_api_url.clone(), config.user_agent.clone());
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
        self.prompt_step(chat_id, profile.onboarding_step).await
    }

    /// Sends the prompt for the current onboarding step.
    async fn prompt_step(&self, chat_id: i64, step: OnboardingStep) -> Result<()> {
        match step {
            OnboardingStep::AwaitingUsername => {
                let text = "👋 I upload your photos and files to <b>Wikimedia Commons</b> under <b>your</b> account.\n\nFirst create a <b>Bot Password</b> so you never share your real password:\n1. Open https://commons.wikimedia.org/wiki/Special:BotPasswords\n2. Use a label like <code>telegram</code> and enable only the <b>Upload</b> grants.\n3. You'll get a username like <code>YourName@telegram</code> and a token.\n\nNow send me your bot-password <b>username</b> (e.g. <code>YourName@telegram</code>).";
                self.telegram.send_message(chat_id, text, None).await
            }
            OnboardingStep::AwaitingPassword => {
                self.telegram
                    .send_message(
                        chat_id,
                        "Now send the <b>bot password token</b>. I delete your message immediately and store the token encrypted.",
                        None,
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
                        "Send a <b>filename prefix</b> for your uploads, or send <code>skip</code> for none.",
                        None,
                    )
                    .await
            }
            OnboardingStep::Done => {
                self.telegram
                    .send_message(chat_id, "✅ All set! Send me a photo or file.", None)
                    .await
            }
        }
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
                            "❌ Login failed: {}\n\nCheck the username and token, then send the token again.",
                            escape_html(&format!("{error}"))
                        );
                        self.telegram.send_message(chat_id, &text, None).await
                    }
                }
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
                    .send_message(
                        chat_id,
                        "✅ All set! Send me a photo or file and I'll upload it to Wikimedia Commons. Tip: a caption becomes the description, and a line like <code>Categories: Minsk, Belarus</code> sets categories.",
                        None,
                    )
                    .await
            }
            OnboardingStep::Done => {
                self.telegram
                    .send_message(
                        chat_id,
                        "Send me a photo or file to upload, or /help for options.",
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
            return Ok(());
        };
        let mut profile = self.store.get_profile(user_id).await;

        if let Some(key) = data.strip_prefix("lic:") {
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
        let text = format!(
            "🖼 <b>Wikimedia Commons uploader</b> ({BOT_USERNAME})\n\nSend me a photo or file and I upload it to <b>Wikimedia Commons</b> under your own account.\n\n<b>Set up</b>: create a scoped bot password (Upload grant only) at https://commons.wikimedia.org/wiki/Special:BotPasswords then run /start.\n\n<b>Captions</b>: the caption becomes the description. Extra lines: <code>Categories: A, B</code>, <code>Source: https://…</code>, <code>Author: Name</code> (also apply to a whole album).\n\n<b>Accepted</b>: JPEG, PNG, GIF, SVG, TIFF, WebP, PDF, DjVu, audio (WAV, MP3, OGG, Opus, FLAC), video (WebM, OGV). DNG, HEIC and BMP are converted to WebP automatically.\n<b>Max size</b>: 20 MB (Telegram bot download limit).\n\n<b>Commands</b>: /start, /settings, /forget, /help\n\nMade by {CONTACT} — message me for help or uploading assistance.\n\n<b>Related projects</b>:\n• Browse Commons in Telegram: {RELATED_BROWSE_BOT}\n• gThumb extension: {RELATED_GTHUMB}\n• Browser upload extension: {RELATED_WEB_EXTENSION}\n• CLI upload tool: {RELATED_CLI}\n• Dark Wikipedia theme: {RELATED_DARK_THEME}\n• Wikipedia → man pages: {RELATED_WIKI2MAN}\n\nSource: {}{uploads_line}",
            self.config.github_url
        );
        self.telegram.send_message(chat_id, &text, None).await
    }

    /// Downloads, converts if needed, and uploads one file to Commons.
    async fn handle_upload(&self, chat_id: i64, user_id: i64, message: &Message) -> Result<()> {
        let mut profile = self.store.get_profile(user_id).await;
        if !profile.is_ready() {
            return self
                .telegram
                .send_message(
                    chat_id,
                    "Please run /start first to connect your Commons account.",
                    None,
                )
                .await;
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

        self.telegram
            .send_chat_action(chat_id, "upload_document")
            .await
            .ok();
        let original = match self
            .telegram
            .download_by_file_id(&file.file_id, self.config.max_file_bytes)
            .await
        {
            Ok(bytes) => bytes,
            Err(error) => {
                tracing::warn!(error = %format!("{error:#}"), "download failed");
                return self.reject_too_large(chat_id).await;
            }
        };

        // Resolve the description and categories, sharing an album's caption across photos.
        let caption = self.resolve_caption(message).await;
        let parsed = parse_caption(&caption);
        let categories = merge_categories(&parsed.categories, &profile.default_categories);
        let metadata = metadata::extract(&original);

        // Convert DNG/HEIC/BMP to WebP; pass everything else through if Commons accepts it.
        let format = convert::classify(file.file_name.as_deref(), file.mime.as_deref(), &original);
        let mut provenance = UploadProvenance {
            original_filename: file
                .file_name
                .clone()
                .unwrap_or_else(|| format!("telegram_{}", file.file_unique_id)),
            ..UploadProvenance::default()
        };
        let (upload_bytes, extension) = if format.needs_conversion() {
            provenance.original_sha1 = Some(sha1_hex(&original));
            provenance.original_md5 = Some(md5_hex(&original));
            let webp = convert::to_webp(&original, format, self.config.webp_quality)?;
            (webp, "webp".to_string())
        } else {
            let extension =
                convert::passthrough_extension(file.file_name.as_deref(), file.mime.as_deref());
            if !convert::is_commons_accepted(&extension) {
                let text = format!(
                    "❌ Commons does not accept <code>.{}</code> files. Accepted: JPEG, PNG, GIF, SVG, TIFF, WebP, PDF, DjVu, audio (WAV/MP3/OGG/Opus/FLAC), video (WebM/OGV). DNG, HEIC and BMP are converted automatically.",
                    escape_html(&extension)
                );
                return self.telegram.send_message(chat_id, &text, None).await;
            }
            (original, extension)
        };

        // Duplicate pre-check by content hash.
        let upload_sha1 = sha1_hex(&upload_bytes);
        if let Ok(existing) = self.commons.find_by_sha1(&upload_sha1).await
            && !existing.is_empty()
        {
            let links = existing
                .iter()
                .map(|title| format!("• {}", commons_title_url(title)))
                .collect::<Vec<_>>()
                .join("\n");
            let text =
                format!("⚠️ This exact file already exists on Commons:\n{links}\n\nSkipped.");
            return self.telegram.send_message(chat_id, &text, None).await;
        }

        // Build the filename and wikitext.
        let base = first_non_empty(&[
            first_line(&parsed.description),
            file_stem(file.file_name.as_deref()),
        ])
        .unwrap_or("image")
        .to_string();
        let unique_suffix = message
            .media_group_id
            .as_ref()
            .map(|_| file.file_unique_id.as_str());
        let filename = build_filename(
            &profile.filename_prefix,
            &base,
            &extension,
            &filename_timestamp(),
            unique_suffix,
        );
        let (latitude, longitude) = match metadata.coordinates() {
            Some((latitude, longitude)) => (Some(latitude), Some(longitude)),
            None => (None, None),
        };
        let date = parsed
            .date
            .clone()
            .or_else(|| metadata.date.clone())
            .unwrap_or_else(today_iso);
        let username = profile.commons_username.clone().unwrap_or_default();
        let wikitext = build_wikitext(&DescriptionParams {
            description: &parsed.description,
            author_username: &username,
            author_override: parsed.author.as_deref(),
            source: parsed.source.as_deref(),
            license: profile.license,
            categories: &categories,
            date: &date,
            latitude,
            longitude,
            provenance: &provenance,
        });

        let Some(cipher) = &self.cipher else {
            return self
                .telegram
                .send_message(
                    chat_id,
                    "⚠️ The bot is missing its encryption key; cannot read your credentials.",
                    None,
                )
                .await;
        };
        let password = cipher
            .decrypt(profile.credential_ciphertext.as_deref().unwrap_or_default())
            .context("failed to decrypt stored credentials")?;

        let outcome = self
            .commons
            .upload(&UploadRequest {
                username,
                password,
                filename: filename.clone(),
                bytes: upload_bytes,
                wikitext,
                comment: format!("Uploaded via Telegram bot {BOT_USERNAME}"),
            })
            .await?;

        match outcome {
            UploadOutcome::Success { url, .. } => {
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
            UploadOutcome::Failed { message } => {
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

    /// Tells the user a file is over the 20 MB Telegram download limit.
    async fn reject_too_large(&self, chat_id: i64) -> Result<()> {
        let limit_mb = self.config.max_file_bytes / (1024 * 1024);
        let text = format!(
            "❌ That file is too large. Telegram only lets bots download files up to {limit_mb} MB. Please send a smaller export."
        );
        self.telegram.send_message(chat_id, &text, None).await
    }
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
    format!(
        "⚙️ <b>Settings</b>\nCommons account: <code>{}</code>\nLicense: <b>{}</b>\nFilename prefix: <code>{}</code>\nDefault categories: {}\nReturn upload links: <b>{}</b>\nReturn category links: <b>{}</b>\nReturn non-existing category links: <b>{}</b>\n\nButtons below toggle options and the license.\nText commands:\n<code>/settings prefix Your Prefix</code>\n<code>/settings categories Cat A, Cat B</code>\n<code>/settings license cc-by-4.0</code>",
        escape_html(&account),
        escape_html(profile.license.label()),
        escape_html(&prefix),
        escape_html(&categories),
        on_off(profile.return_upload_links),
        on_off(profile.return_category_links),
        on_off(profile.return_missing_category_links),
    )
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

/// Renders a boolean as a human on/off label.
fn on_off(value: bool) -> &'static str {
    if value { "on" } else { "off" }
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

/// Returns a filename-safe timestamp `YYYY-MM-DD HH-MM-SS` (UTC).
fn filename_timestamp() -> String {
    time::OffsetDateTime::now_utc()
        .format(&time::macros::format_description!(
            "[year]-[month]-[day] [hour]-[minute]-[second]"
        ))
        .unwrap_or_default()
}

/// Returns lower-case SHA-1 hex of the bytes.
fn sha1_hex(bytes: &[u8]) -> String {
    use sha1::{Digest, Sha1};
    hex::encode(Sha1::digest(bytes))
}

/// Returns lower-case MD5 hex of the bytes.
fn md5_hex(bytes: &[u8]) -> String {
    use md5::{Digest, Md5};
    hex::encode(Md5::digest(bytes))
}

/// Returns the first non-empty trimmed line of a string.
fn first_line(text: &str) -> &str {
    text.lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or("")
}

/// Returns the filename stem (without extension), if a filename is present.
fn file_stem(file_name: Option<&str>) -> &str {
    file_name
        .map(|name| name.rsplit_once('.').map(|(stem, _)| stem).unwrap_or(name))
        .unwrap_or("")
}

/// Returns the first non-empty candidate string.
fn first_non_empty<'a>(candidates: &[&'a str]) -> Option<&'a str> {
    candidates
        .iter()
        .copied()
        .find(|value| !value.trim().is_empty())
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
    use super::{first_line, first_non_empty, merge_categories, parse_category_list};

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
    fn first_line_skips_blank_lines() {
        assert_eq!(first_line("\n  \nHello\nworld"), "Hello");
        assert_eq!(first_line("   "), "");
    }

    #[test]
    fn first_non_empty_picks_first() {
        assert_eq!(first_non_empty(&["", "  ", "x", "y"]), Some("x"));
        assert_eq!(first_non_empty(&["", "  "]), None);
    }
}
