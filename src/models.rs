use serde::Deserialize;

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
}

impl License {
    /// Lists every license in the order shown on the picker keyboard.
    pub fn all() -> [License; 3] {
        [License::CcBy40, License::CcBySa40, License::Cc0]
    }

    /// Parses a stored value or callback key, accepting a few aliases.
    pub fn parse(value: &str) -> Option<License> {
        match value.trim().to_ascii_lowercase().as_str() {
            "cc-by-4.0" | "cc_by_4.0" | "ccby40" | "cc-by" => Some(License::CcBy40),
            "cc-by-sa-4.0" | "cc_by_sa_4.0" | "ccbysa40" | "cc-by-sa" => Some(License::CcBySa40),
            "cc-zero" | "cc0" | "cc0-1.0" | "cc-0" => Some(License::Cc0),
            _ => None,
        }
    }

    /// Returns the stable storage/callback key, identical to the Commons
    /// `{{self|...}}` template name.
    pub fn as_key(self) -> &'static str {
        match self {
            License::CcBy40 => "cc-by-4.0",
            License::CcBySa40 => "cc-by-sa-4.0",
            License::Cc0 => "cc-zero",
        }
    }

    /// Returns the human-readable label used in messages and buttons.
    pub fn label(self) -> &'static str {
        match self {
            License::CcBy40 => "CC BY 4.0",
            License::CcBySa40 => "CC BY-SA 4.0",
            License::Cc0 => "CC0 (public domain)",
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
    /// Waiting for the license selection.
    AwaitingLicense,
    /// Waiting for the filename prefix.
    AwaitingPrefix,
    /// Onboarding complete; ready to upload.
    Done,
}

impl OnboardingStep {
    /// Parses a stored step value.
    pub fn parse(value: &str) -> Option<OnboardingStep> {
        match value {
            "awaiting_username" => Some(OnboardingStep::AwaitingUsername),
            "awaiting_password" => Some(OnboardingStep::AwaitingPassword),
            "awaiting_license" => Some(OnboardingStep::AwaitingLicense),
            "awaiting_prefix" => Some(OnboardingStep::AwaitingPrefix),
            "done" => Some(OnboardingStep::Done),
            _ => None,
        }
    }

    /// Returns the stable storage string for the step.
    pub fn as_str(self) -> &'static str {
        match self {
            OnboardingStep::AwaitingUsername => "awaiting_username",
            OnboardingStep::AwaitingPassword => "awaiting_password",
            OnboardingStep::AwaitingLicense => "awaiting_license",
            OnboardingStep::AwaitingPrefix => "awaiting_prefix",
            OnboardingStep::Done => "done",
        }
    }
}

/// One user's stored profile (one DynamoDB item per Telegram user).
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Profile {
    /// Commons bot-password username, e.g. `Example@uploader`.
    pub commons_username: Option<String>,
    /// AES-GCM ciphertext (base64) of the bot-password token.
    pub credential_ciphertext: Option<String>,
    /// License applied to uploads.
    pub license: License,
    /// Prefix prepended to generated Commons filenames.
    pub filename_prefix: String,
    /// Current onboarding step.
    pub onboarding_step: OnboardingStep,
    /// Categories added to every upload (user-configured default).
    pub default_categories: Vec<String>,
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
    /// Number of successful uploads (for admin stats).
    pub uploads_count: u64,
    /// Unix timestamp of profile creation.
    pub created_at: i64,
    /// Unix timestamp of the last update.
    pub updated_at: i64,
}

impl Profile {
    /// Returns true when onboarding is complete and credentials are stored.
    pub fn is_ready(&self) -> bool {
        self.onboarding_step == OnboardingStep::Done
            && self.commons_username.is_some()
            && self.credential_ciphertext.is_some()
    }
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
    /// Chat the message belongs to.
    pub chat: Chat,
    /// Sender.
    pub from: Option<User>,
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
    use super::{License, OnboardingStep, Profile};

    #[test]
    fn license_parse_round_trips_keys() {
        for license in License::all() {
            assert_eq!(License::parse(license.as_key()), Some(license));
        }
        assert_eq!(License::parse("CC0"), Some(License::Cc0));
        assert_eq!(License::parse("nonsense"), None);
    }

    #[test]
    fn onboarding_step_round_trips() {
        for step in [
            OnboardingStep::AwaitingUsername,
            OnboardingStep::AwaitingPassword,
            OnboardingStep::AwaitingLicense,
            OnboardingStep::AwaitingPrefix,
            OnboardingStep::Done,
        ] {
            assert_eq!(OnboardingStep::parse(step.as_str()), Some(step));
        }
        assert_eq!(OnboardingStep::parse("bogus"), None);
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
}
