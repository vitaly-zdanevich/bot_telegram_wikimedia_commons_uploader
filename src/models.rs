use serde::{Deserialize, Serialize};
use sha1::{Digest, Sha1};

/// Creative Commons license a user can pick for their uploads.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum License {
    /// Creative Commons Attribution 4.0 International (the default).
    #[default]
    CcBy40,
    /// Creative Commons Attribution-ShareAlike 4.0 International.
    CcBySa40,
    /// Creative Commons Zero 1.0 Universal (public domain dedication).
    Cc0,
    /// Public domain in Russia because copyright expired.
    PdRussiaExpired,
    /// Public domain in Russia.
    PdRussia,
    /// Public domain in the Russian Empire.
    PdRusEmpire,
}

impl License {
    /// Lists every license in the order shown on the picker keyboard.
    pub fn all() -> [License; 6] {
        [
            License::CcBy40,
            License::CcBySa40,
            License::Cc0,
            License::PdRussiaExpired,
            License::PdRussia,
            License::PdRusEmpire,
        ]
    }

    /// Parses a stored value or callback key, accepting a few aliases.
    pub fn parse(value: &str) -> Option<License> {
        match value.trim().to_ascii_lowercase().as_str() {
            "cc-by-4.0" | "cc_by_4.0" | "ccby40" | "cc-by" => Some(License::CcBy40),
            "cc-by-sa-4.0" | "cc_by_sa_4.0" | "ccbysa40" | "cc-by-sa" => Some(License::CcBySa40),
            "cc-zero" | "cc0" | "cc0-1.0" | "cc-0" => Some(License::Cc0),
            "pd-russia-expired" | "pd_russia_expired" => Some(License::PdRussiaExpired),
            "pd-russia" | "pd_russia" => Some(License::PdRussia),
            "pd-rusempire" | "pd-rus-empire" | "pd_rusempire" | "pd_rus_empire" => {
                Some(License::PdRusEmpire)
            }
            _ => None,
        }
    }

    /// Returns the stable storage/callback key.
    pub fn as_key(self) -> &'static str {
        match self {
            License::CcBy40 => "cc-by-4.0",
            License::CcBySa40 => "cc-by-sa-4.0",
            License::Cc0 => "cc-zero",
            License::PdRussiaExpired => "PD-Russia-expired",
            License::PdRussia => "PD-Russia",
            License::PdRusEmpire => "PD-RusEmpire",
        }
    }

    /// Returns the human-readable label used in messages and buttons.
    pub fn label(self) -> &'static str {
        match self {
            License::CcBy40 => "CC BY 4.0",
            License::CcBySa40 => "CC BY-SA 4.0",
            License::Cc0 => "CC0 (public domain)",
            License::PdRussiaExpired => "PD-Russia-expired",
            License::PdRussia => "PD-Russia",
            License::PdRusEmpire => "PD-RusEmpire",
        }
    }

    /// Renders the Commons license template for this configured license.
    pub fn wikitext(self) -> String {
        match self {
            License::CcBy40 | License::CcBySa40 | License::Cc0 => {
                format!("{{{{self|{}}}}}", self.as_key())
            }
            License::PdRussiaExpired | License::PdRussia | License::PdRusEmpire => {
                format!("{{{{{}}}}}", self.as_key())
            }
        }
    }
}

/// How incoming DNG raw files are turned into a Commons-accepted upload.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum DngMode {
    /// Develop the raw image and upload a WebP.
    #[default]
    ConvertToWebp,
    /// Extract and upload the DNG's embedded JPEG preview.
    ExtractEmbeddedJpeg,
}

impl DngMode {
    /// Parses a stored value or settings command alias.
    pub fn parse(value: &str) -> Option<DngMode> {
        match value.trim().to_ascii_lowercase().as_str() {
            "convert" | "webp" | "convert-to-webp" | "convert_to_webp" | "develop" | "raw" => {
                Some(DngMode::ConvertToWebp)
            }
            "extract"
            | "jpeg"
            | "jpg"
            | "embedded"
            | "embedded-jpeg"
            | "embedded_jpeg"
            | "extract-embedded-jpeg"
            | "extract_embedded_jpeg" => Some(DngMode::ExtractEmbeddedJpeg),
            _ => None,
        }
    }

    /// Returns the stable storage key.
    pub fn as_key(self) -> &'static str {
        match self {
            DngMode::ConvertToWebp => "convert-to-webp",
            DngMode::ExtractEmbeddedJpeg => "extract-embedded-jpeg",
        }
    }

    /// Returns the label shown in settings.
    pub fn label(self) -> &'static str {
        match self {
            DngMode::ConvertToWebp => "convert to WebP, fallback JPEG",
            DngMode::ExtractEmbeddedJpeg => "extract embedded JPEG",
        }
    }

    /// Returns the other mode for the settings toggle button.
    pub fn toggled(self) -> DngMode {
        match self {
            DngMode::ConvertToWebp => DngMode::ExtractEmbeddedJpeg,
            DngMode::ExtractEmbeddedJpeg => DngMode::ConvertToWebp,
        }
    }
}

/// Step of the per-user onboarding conversation.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum OnboardingStep {
    /// Waiting for the Commons bot-password username (`User@label`).
    #[default]
    AwaitingUsername,
    /// Waiting for the bot-password token.
    AwaitingPassword,
    /// Waiting for the pasted OAuth verifier code (out-of-band flow).
    AwaitingOAuthVerifier,
    /// Waiting for the OAuth2 browser callback to complete.
    AwaitingOAuth2Callback,
    /// Waiting for the license selection.
    AwaitingLicense,
    /// Waiting for the filename prefix.
    AwaitingPrefix,
    /// Waiting for a filename prefix requested from `/settings`.
    AwaitingSettingsPrefix,
    /// Waiting for a filename prefix required by a staged archive with generic names.
    AwaitingArchivePrefix,
    /// Onboarding complete; ready to upload.
    Done,
}

impl OnboardingStep {
    /// Parses a stored step value.
    pub fn parse(value: &str) -> Option<OnboardingStep> {
        match value {
            "awaiting_username" => Some(OnboardingStep::AwaitingUsername),
            "awaiting_password" => Some(OnboardingStep::AwaitingPassword),
            "awaiting_oauth_verifier" => Some(OnboardingStep::AwaitingOAuthVerifier),
            "awaiting_oauth2_callback" => Some(OnboardingStep::AwaitingOAuth2Callback),
            "awaiting_license" => Some(OnboardingStep::AwaitingLicense),
            "awaiting_prefix" => Some(OnboardingStep::AwaitingPrefix),
            "awaiting_settings_prefix" => Some(OnboardingStep::AwaitingSettingsPrefix),
            "awaiting_archive_prefix" => Some(OnboardingStep::AwaitingArchivePrefix),
            "done" => Some(OnboardingStep::Done),
            _ => None,
        }
    }

    /// Returns the stable storage string for the step.
    pub fn as_str(self) -> &'static str {
        match self {
            OnboardingStep::AwaitingUsername => "awaiting_username",
            OnboardingStep::AwaitingPassword => "awaiting_password",
            OnboardingStep::AwaitingOAuthVerifier => "awaiting_oauth_verifier",
            OnboardingStep::AwaitingOAuth2Callback => "awaiting_oauth2_callback",
            OnboardingStep::AwaitingLicense => "awaiting_license",
            OnboardingStep::AwaitingPrefix => "awaiting_prefix",
            OnboardingStep::AwaitingSettingsPrefix => "awaiting_settings_prefix",
            OnboardingStep::AwaitingArchivePrefix => "awaiting_archive_prefix",
            OnboardingStep::Done => "done",
        }
    }
}

/// One connected Wikimedia Commons account that can be selected for uploads.
#[derive(Clone, Debug, Eq, PartialEq, Deserialize, Serialize)]
pub struct CommonsAccount {
    /// Stable short id used in Telegram callback data.
    pub id: String,
    /// Commons username, or bot-password username (`User@label`) for bot-password auth.
    pub commons_username: Option<String>,
    /// AES-GCM ciphertext (base64) of the bot-password token.
    pub credential_ciphertext: Option<String>,
    /// AES-GCM ciphertext of the OAuth 1.0a access token+secret (`token\nsecret`).
    pub oauth_ciphertext: Option<String>,
    /// AES-GCM ciphertext of OAuth2 access/refresh tokens as JSON.
    pub oauth2_ciphertext: Option<String>,
    /// Unix timestamp of account creation in this bot.
    pub created_at: i64,
    /// Unix timestamp of the last credential update.
    pub updated_at: i64,
}

impl CommonsAccount {
    /// Returns true when this account has enough encrypted credential material to authenticate.
    pub fn has_credentials(&self) -> bool {
        self.oauth2_ciphertext.is_some()
            || self.oauth_ciphertext.is_some()
            || (self.commons_username.is_some() && self.credential_ciphertext.is_some())
    }

    /// Labels the stored authentication method for account switcher buttons.
    pub fn auth_method_label(&self) -> &'static str {
        if self.oauth2_ciphertext.is_some() {
            "OAuth2"
        } else if self.oauth_ciphertext.is_some() {
            "OAuth1"
        } else if self.credential_ciphertext.is_some() {
            "Bot password"
        } else {
            "none"
        }
    }
}

/// One user's stored profile (one DynamoDB item per Telegram user).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Profile {
    /// Commons bot-password username, e.g. `Example@uploader`.
    pub commons_username: Option<String>,
    /// AES-GCM ciphertext (base64) of the bot-password token.
    pub credential_ciphertext: Option<String>,
    /// AES-GCM ciphertext of the OAuth 1.0a access token+secret (`token\nsecret`).
    pub oauth_ciphertext: Option<String>,
    /// AES-GCM ciphertext of the transient OAuth request token+secret during onboarding.
    pub oauth_pending_ciphertext: Option<String>,
    /// AES-GCM ciphertext of OAuth2 access/refresh tokens as JSON.
    pub oauth2_ciphertext: Option<String>,
    /// Stored Commons accounts available for quick switching.
    pub accounts: Vec<CommonsAccount>,
    /// Id of the account copied into the active credential fields.
    pub active_account_id: Option<String>,
    /// License applied to uploads.
    pub license: License,
    /// Prefix prepended to generated Commons filenames.
    pub filename_prefix: String,
    /// Current onboarding step.
    pub onboarding_step: OnboardingStep,
    /// Categories added to every upload (user-configured default).
    pub default_categories: Vec<String>,
    /// Categories previously used in successful uploads, shown as quick picks in settings.
    pub used_categories: Vec<String>,
    /// Default author override applied when an upload's caption sets none.
    pub default_author: Option<String>,
    /// Default description used when an upload has no caption text.
    pub default_description: Option<String>,
    /// Default description language code that wraps the description (e.g. `ru`).
    pub default_lang: Option<String>,
    /// Custom license wikitext/template overriding the picked license.
    pub license_override: Option<String>,
    /// Whether to reply with the Commons file link after each successful upload.
    pub return_upload_links: bool,
    /// Whether to reply with links to the categories used in each upload.
    pub return_category_links: bool,
    /// Whether to reply with links to categories that do not yet exist on Commons.
    pub return_missing_category_links: bool,
    /// Whether upload success replies include resolution, EXIF camera model, and EXIF date.
    pub return_upload_metadata: bool,
    /// Whether to reply with the file list found inside an archive (off by default).
    pub return_archive_file_list: bool,
    /// Whether to show archive thumbnails and require a Confirm tap (on by default).
    pub archive_confirm: bool,
    /// How DNG raw files are uploaded.
    pub dng_mode: DngMode,
    /// Number of successful uploads (for admin stats).
    pub uploads_count: u64,
    /// Unix timestamp of profile creation.
    pub created_at: i64,
    /// Unix timestamp of the last update.
    pub updated_at: i64,
}

impl Default for Profile {
    fn default() -> Self {
        Self {
            commons_username: None,
            credential_ciphertext: None,
            oauth_ciphertext: None,
            oauth_pending_ciphertext: None,
            oauth2_ciphertext: None,
            accounts: Vec::new(),
            active_account_id: None,
            license: License::default(),
            filename_prefix: String::new(),
            onboarding_step: OnboardingStep::default(),
            default_categories: Vec::new(),
            used_categories: Vec::new(),
            default_author: None,
            default_description: None,
            default_lang: None,
            license_override: None,
            return_upload_links: true,
            return_category_links: false,
            return_missing_category_links: false,
            return_upload_metadata: true,
            return_archive_file_list: false,
            archive_confirm: true,
            dng_mode: DngMode::default(),
            uploads_count: 0,
            created_at: 0,
            updated_at: 0,
        }
    }
}

impl Profile {
    /// Returns true when onboarding is complete and some credential is stored
    /// (an OAuth token, or a bot-password token with its username).
    pub fn is_ready(&self) -> bool {
        self.onboarding_step == OnboardingStep::Done
            && (self.oauth2_ciphertext.is_some()
                || self.oauth_ciphertext.is_some()
                || (self.commons_username.is_some() && self.credential_ciphertext.is_some()))
    }

    /// Returns true when the active credential fields contain usable auth material.
    pub fn has_active_credentials(&self) -> bool {
        self.oauth2_ciphertext.is_some()
            || self.oauth_ciphertext.is_some()
            || (self.commons_username.is_some() && self.credential_ciphertext.is_some())
    }

    /// Removes active credential material while leaving stored accounts and selection untouched.
    ///
    /// Keeping the selected account id lets callers restore the previous account if a new
    /// account connection is cancelled before credentials are completed.
    pub fn clear_active_credentials(&mut self) {
        self.commons_username = None;
        self.credential_ciphertext = None;
        self.oauth_ciphertext = None;
        self.oauth_pending_ciphertext = None;
        self.oauth2_ciphertext = None;
    }

    /// Adds or updates the active credential set in the stored account list.
    pub fn upsert_active_account(&mut self, now: i64) -> bool {
        if !self.has_active_credentials() {
            return false;
        }
        let Some(id) = self.active_account_id.clone().or_else(|| {
            active_account_id(
                self.commons_username.as_deref(),
                self.credential_ciphertext.as_deref(),
                self.oauth_ciphertext.as_deref(),
                self.oauth2_ciphertext.as_deref(),
            )
        }) else {
            return false;
        };
        let account = CommonsAccount {
            id: id.clone(),
            commons_username: self.commons_username.clone(),
            credential_ciphertext: self.credential_ciphertext.clone(),
            oauth_ciphertext: self.oauth_ciphertext.clone(),
            oauth2_ciphertext: self.oauth2_ciphertext.clone(),
            created_at: now,
            updated_at: now,
        };
        self.active_account_id = Some(id.clone());
        if let Some(existing) = self.accounts.iter_mut().find(|account| account.id == id) {
            let created_at = existing.created_at;
            *existing = CommonsAccount {
                created_at,
                ..account
            };
            true
        } else {
            self.accounts.push(account);
            true
        }
    }

    /// Copies one stored account into the active credential fields.
    pub fn switch_account(&mut self, account_id: &str) -> bool {
        let Some(account) = self
            .accounts
            .iter()
            .find(|account| account.id == account_id && account.has_credentials())
            .cloned()
        else {
            return false;
        };
        self.commons_username = account.commons_username;
        self.credential_ciphertext = account.credential_ciphertext;
        self.oauth_ciphertext = account.oauth_ciphertext;
        self.oauth2_ciphertext = account.oauth2_ciphertext;
        self.oauth_pending_ciphertext = None;
        self.active_account_id = Some(account.id);
        true
    }

    /// Restores the selected stored account into the active credential fields.
    pub fn restore_active_account(&mut self) -> bool {
        let Some(id) = self.active_account_id.clone() else {
            return false;
        };
        self.switch_account(&id)
    }

    /// Returns the stored account currently selected for uploads, if present.
    pub fn active_account(&self) -> Option<&CommonsAccount> {
        let id = self.active_account_id.as_deref()?;
        self.accounts.iter().find(|account| account.id == id)
    }
}

/// Builds a stable account id from a username when possible, otherwise from credentials.
fn active_account_id(
    username: Option<&str>,
    bot_password_ciphertext: Option<&str>,
    oauth_ciphertext: Option<&str>,
    oauth2_ciphertext: Option<&str>,
) -> Option<String> {
    let source = username
        .and_then(|value| {
            let account = value.split('@').next().unwrap_or(value).trim();
            (!account.is_empty()).then(|| format!("user:{}", account.to_ascii_lowercase()))
        })
        .or_else(|| oauth2_ciphertext.map(|value| format!("oauth2:{value}")))
        .or_else(|| oauth_ciphertext.map(|value| format!("oauth1:{value}")))
        .or_else(|| bot_password_ciphertext.map(|value| format!("botpass:{value}")))?;
    let digest = Sha1::digest(source.as_bytes());
    Some(format!(
        "acct{:016x}",
        u64::from_be_bytes(digest[..8].try_into().ok()?)
    ))
}

/// Provenance of an upload, recorded on the Commons file page.
///
/// For DNG → WebP conversions the original DNG cannot be hosted on Commons, so its
/// hashes and name are stored as metadata to allow matching the source by hash.
#[derive(Clone, Debug, Default)]
pub struct UploadProvenance {
    /// Original file name as received from Telegram.
    pub original_filename: String,
    /// Lower-case SHA-1 hex of the original bytes (set when converted).
    pub original_sha1: Option<String>,
    /// Lower-case MD5 hex of the original bytes (set when converted).
    pub original_md5: Option<String>,
}

/// Telegram update subset handled by this bot.
#[derive(Clone, Debug, Deserialize)]
pub struct Update {
    /// Monotonic Telegram update id used to suppress webhook retries.
    pub update_id: Option<i64>,
    /// Incoming message.
    pub message: Option<Message>,
    /// Callback query from an inline keyboard (license picker).
    pub callback_query: Option<CallbackQuery>,
}

/// Telegram message subset used by the app.
#[derive(Clone, Debug, Deserialize)]
pub struct Message {
    /// Telegram message id (used for `deleteMessage`).
    pub message_id: Option<i64>,
    /// Telegram message send timestamp, in Unix seconds.
    pub date: Option<i64>,
    /// Chat the message belongs to.
    pub chat: Chat,
    /// Sender.
    pub from: Option<User>,
    /// Forward attribution from current Telegram Bot API payloads.
    pub forward_origin: Option<serde_json::Value>,
    /// Forwarded-from user when Telegram exposes the original sender.
    pub forward_from: Option<User>,
    /// Forwarded-from display name when the original sender hides their account.
    pub forward_sender_name: Option<String>,
    /// Forwarded-from chat/channel for channel or group-origin messages.
    pub forward_from_chat: Option<Chat>,
    /// Forward timestamp from older Telegram Bot API payloads.
    pub forward_date: Option<i64>,
    /// Plain text body (commands, onboarding answers).
    pub text: Option<String>,
    /// Caption attached to a photo or document.
    pub caption: Option<String>,
    /// Album id shared by all photos sent together.
    pub media_group_id: Option<String>,
    /// Document attachment (original-quality file: image, DNG, HEIC, audio, or video).
    pub document: Option<Document>,
    /// Photo sizes (compressed image); largest is last.
    pub photo: Option<Vec<PhotoSize>>,
    /// Audio attachment (e.g. MP3).
    pub audio: Option<Audio>,
    /// Voice message (OGG/Opus).
    pub voice: Option<Voice>,
    /// Video attachment (e.g. WebM).
    pub video: Option<Video>,
}

impl Message {
    /// Returns true when Telegram marked this message as forwarded.
    pub fn is_forwarded(&self) -> bool {
        self.forward_origin.is_some()
            || self.forward_from.is_some()
            || self.forward_sender_name.is_some()
            || self.forward_from_chat.is_some()
            || self.forward_date.is_some()
    }
}

/// Telegram chat subset.
#[derive(Clone, Debug, Deserialize)]
pub struct Chat {
    /// Chat id.
    pub id: i64,
}

/// Telegram user subset.
#[derive(Clone, Debug, Deserialize)]
pub struct User {
    /// User id.
    pub id: i64,
}

/// Telegram callback query subset (inline keyboard presses).
#[derive(Clone, Debug, Deserialize)]
pub struct CallbackQuery {
    /// Callback query id (answered to clear the client spinner).
    pub id: String,
    /// Sender.
    pub from: User,
    /// Message that owns the button.
    pub message: Option<Message>,
    /// Callback data payload.
    pub data: Option<String>,
}

/// Telegram document attachment subset.
#[derive(Clone, Debug, Deserialize)]
pub struct Document {
    /// File id used with `getFile`.
    pub file_id: String,
    /// Stable id unique per file (used to disambiguate album filenames).
    pub file_unique_id: String,
    /// Original file name, if provided by the client.
    pub file_name: Option<String>,
    /// MIME type, if provided by the client.
    pub mime_type: Option<String>,
    /// File size in bytes, if known.
    pub file_size: Option<u64>,
    /// Telegram-generated preview image, if Telegram provided one.
    #[serde(default)]
    pub thumbnail: Option<PhotoSize>,
    /// Legacy Telegram-generated preview image field.
    #[serde(default)]
    pub thumb: Option<PhotoSize>,
}

/// Telegram photo size subset (one entry per compressed resolution).
#[derive(Clone, Debug, Deserialize)]
pub struct PhotoSize {
    /// File id used with `getFile`.
    pub file_id: String,
    /// Stable id unique per file.
    pub file_unique_id: String,
    /// Width in pixels.
    pub width: u64,
    /// Height in pixels.
    pub height: u64,
    /// File size in bytes, if known.
    pub file_size: Option<u64>,
}

/// Telegram audio attachment subset.
#[derive(Clone, Debug, Deserialize)]
pub struct Audio {
    /// File id used with `getFile`.
    pub file_id: String,
    /// Stable id unique per file.
    pub file_unique_id: String,
    /// Original file name, if provided.
    pub file_name: Option<String>,
    /// MIME type, if provided.
    pub mime_type: Option<String>,
    /// File size in bytes, if known.
    pub file_size: Option<u64>,
}

/// Telegram voice-message subset.
#[derive(Clone, Debug, Deserialize)]
pub struct Voice {
    /// File id used with `getFile`.
    pub file_id: String,
    /// Stable id unique per file.
    pub file_unique_id: String,
    /// MIME type, if provided (usually audio/ogg).
    pub mime_type: Option<String>,
    /// File size in bytes, if known.
    pub file_size: Option<u64>,
}

/// Telegram video attachment subset.
#[derive(Clone, Debug, Deserialize)]
pub struct Video {
    /// File id used with `getFile`.
    pub file_id: String,
    /// Stable id unique per file.
    pub file_unique_id: String,
    /// Original file name, if provided.
    pub file_name: Option<String>,
    /// MIME type, if provided.
    pub mime_type: Option<String>,
    /// File size in bytes, if known.
    pub file_size: Option<u64>,
}

#[cfg(test)]
mod tests {
    use super::{Chat, DngMode, Document, License, Message, OnboardingStep, Profile};

    #[test]
    fn license_parse_round_trips_keys() {
        for license in License::all() {
            assert_eq!(License::parse(license.as_key()), Some(license));
        }
        assert_eq!(License::parse("CC0"), Some(License::Cc0));
        assert_eq!(
            License::parse("PD-Russia-expired"),
            Some(License::PdRussiaExpired)
        );
        assert_eq!(License::parse("pd-rus-empire"), Some(License::PdRusEmpire));
        assert_eq!(License::parse("nonsense"), None);
        assert_eq!(License::CcBy40.wikitext(), "{{self|cc-by-4.0}}");
        assert_eq!(License::PdRussiaExpired.wikitext(), "{{PD-Russia-expired}}");
    }

    #[test]
    fn onboarding_step_round_trips() {
        for step in [
            OnboardingStep::AwaitingUsername,
            OnboardingStep::AwaitingPassword,
            OnboardingStep::AwaitingOAuthVerifier,
            OnboardingStep::AwaitingOAuth2Callback,
            OnboardingStep::AwaitingLicense,
            OnboardingStep::AwaitingPrefix,
            OnboardingStep::AwaitingSettingsPrefix,
            OnboardingStep::AwaitingArchivePrefix,
            OnboardingStep::Done,
        ] {
            assert_eq!(OnboardingStep::parse(step.as_str()), Some(step));
        }
        assert_eq!(OnboardingStep::parse("bogus"), None);
    }

    #[test]
    fn dng_mode_parses_labels_and_toggles() {
        assert_eq!(
            DngMode::parse("convert-to-webp"),
            Some(DngMode::ConvertToWebp)
        );
        assert_eq!(
            DngMode::parse("extract"),
            Some(DngMode::ExtractEmbeddedJpeg)
        );
        assert_eq!(DngMode::parse("bogus"), None);
        assert_eq!(DngMode::ConvertToWebp.as_key(), "convert-to-webp");
        assert_eq!(
            DngMode::ConvertToWebp.label(),
            "convert to WebP, fallback JPEG"
        );
        assert_eq!(
            DngMode::ExtractEmbeddedJpeg.label(),
            "extract embedded JPEG"
        );
        assert_eq!(
            DngMode::ConvertToWebp.toggled(),
            DngMode::ExtractEmbeddedJpeg
        );
        assert_eq!(
            DngMode::ExtractEmbeddedJpeg.toggled(),
            DngMode::ConvertToWebp
        );
    }

    #[test]
    fn profile_is_ready_requires_credentials_and_done() {
        let mut profile = Profile {
            onboarding_step: OnboardingStep::Done,
            ..Profile::default()
        };
        assert!(!profile.is_ready());
        profile.commons_username = Some("Example@uploader".into());
        profile.credential_ciphertext = Some("ciphertext".into());
        assert!(profile.is_ready());
    }

    #[test]
    fn profile_stores_and_switches_multiple_accounts() {
        let mut profile = Profile {
            commons_username: Some("Example@bot".into()),
            credential_ciphertext: Some("bot-secret".into()),
            onboarding_step: OnboardingStep::Done,
            ..Profile::default()
        };

        assert!(profile.upsert_active_account(10));
        let first_id = profile.active_account_id.clone().unwrap();
        assert_eq!(profile.accounts.len(), 1);
        assert_eq!(
            profile.active_account().unwrap().auth_method_label(),
            "Bot password"
        );

        profile.active_account_id = None;
        profile.commons_username = Some("Second".into());
        profile.credential_ciphertext = None;
        profile.oauth2_ciphertext = Some("oauth2-secret".into());
        assert!(profile.upsert_active_account(20));
        let second_id = profile.active_account_id.clone().unwrap();
        assert_ne!(first_id, second_id);
        assert_eq!(profile.accounts.len(), 2);

        assert!(profile.switch_account(&first_id));
        assert_eq!(profile.commons_username.as_deref(), Some("Example@bot"));
        assert_eq!(profile.credential_ciphertext.as_deref(), Some("bot-secret"));
        assert_eq!(profile.oauth2_ciphertext, None);
    }

    #[test]
    fn detects_forwarded_messages() {
        let normal = Message {
            message_id: Some(1),
            date: Some(1_000),
            chat: Chat { id: 1 },
            from: None,
            forward_origin: None,
            forward_from: None,
            forward_sender_name: None,
            forward_from_chat: None,
            forward_date: None,
            text: Some("hello".into()),
            caption: None,
            media_group_id: None,
            document: None,
            photo: None,
            audio: None,
            voice: None,
            video: None,
        };
        assert!(!normal.is_forwarded());

        let forwarded = Message {
            forward_sender_name: Some("Hidden sender".into()),
            ..normal
        };
        assert!(forwarded.is_forwarded());
    }

    #[test]
    fn document_deserializes_thumbnail_and_legacy_thumb() {
        let document: Document = serde_json::from_value(serde_json::json!({
            "file_id": "file",
            "file_unique_id": "unique",
            "thumbnail": {
                "file_id": "thumbnail-file",
                "file_unique_id": "thumbnail-unique",
                "width": 320,
                "height": 240,
                "file_size": 12345
            },
            "thumb": {
                "file_id": "thumb-file",
                "file_unique_id": "thumb-unique",
                "width": 160,
                "height": 120,
                "file_size": 6789
            }
        }))
        .unwrap();

        let thumbnail = document.thumbnail.unwrap();
        assert_eq!(thumbnail.file_id, "thumbnail-file");
        assert_eq!(thumbnail.file_unique_id, "thumbnail-unique");
        assert_eq!(thumbnail.file_size, Some(12345));

        let thumb = document.thumb.unwrap();
        assert_eq!(thumb.file_id, "thumb-file");
        assert_eq!(thumb.file_unique_id, "thumb-unique");
        assert_eq!(thumb.file_size, Some(6789));
    }
}
