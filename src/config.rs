use crate::models::License;
use std::env;

/// Default maximum downloadable file size — the Telegram cloud Bot API `getFile` cap.
const DEFAULT_MAX_FILE_MB: u64 = 20;
/// Default cap for image formats that need full decode/re-encode in memory.
const DEFAULT_MAX_CONVERSION_FILE_MB: u64 = 100;
/// Default cap for archive input and extracted uploadable members.
const DEFAULT_MAX_ARCHIVE_FILE_MB: u64 = 100;
/// Default lossy WebP quality used when converting DNG files.
const DEFAULT_WEBP_QUALITY: f32 = 90.0;
/// Default Commons Action API endpoint.
const DEFAULT_COMMONS_API_URL: &str = "https://commons.wikimedia.org/w/api.php";
/// Default project repository URL shown in `/help`.
const DEFAULT_GITHUB_URL: &str =
    "https://github.com/vitaly-zdanevich/bot_telegram_wikimedia_commons_uploader";
/// Default resource name prefix.
const DEFAULT_PROJECT_NAME: &str = "telegram-wikimedia-commons-uploader-bot";
/// Default Telegram Bot API base URL (cloud API; 20 MB download cap).
const DEFAULT_TELEGRAM_API_BASE: &str = "https://api.telegram.org";
/// Whether archive previews are resized to small JPEG thumbnails before sending.
const DEFAULT_ARCHIVE_THUMBNAIL_RESIZE: bool = false;

/// Runtime configuration loaded from environment variables.
#[derive(Clone, Debug)]
pub struct Config {
    /// Telegram bot token from BotFather.
    pub telegram_bot_token: Option<String>,
    /// Secret expected in the `X-Telegram-Bot-Api-Secret-Token` header.
    pub telegram_webhook_secret: Option<String>,
    /// Telegram Bot API base URL — the cloud API, or a self-hosted server for up to 2 GB.
    pub telegram_api_base: String,
    /// Telegram user ids allowed to use admin commands.
    pub admin_user_ids: Vec<i64>,
    /// Project repository URL shown in `/help`.
    pub github_url: String,
    /// AWS region for DynamoDB.
    pub aws_region: String,
    /// DynamoDB table name for profiles and idempotency.
    pub dynamodb_table: Option<String>,
    /// SQLite database path (server mode); takes precedence over DynamoDB when set.
    pub sqlite_path: Option<String>,
    /// Base64 32-byte master key for AES-GCM credential encryption.
    pub credential_enc_key: Option<String>,
    /// Default license offered during onboarding.
    pub default_license: License,
    /// Lossy WebP quality (1-100) used for DNG conversion.
    pub webp_quality: f32,
    /// Maximum file size the bot will download from Telegram.
    pub max_file_bytes: u64,
    /// Maximum file size for formats that need conversion in memory.
    pub max_conversion_file_bytes: u64,
    /// Maximum archive input size and extracted uploadable member total.
    pub max_archive_file_bytes: u64,
    /// Whether archive preview photos are decoded/resized before sending to Telegram.
    pub archive_thumbnail_resize: bool,
    /// Commons Action API endpoint.
    pub commons_api_url: String,
    /// User-Agent sent to Commons, per MediaWiki API etiquette.
    pub user_agent: String,
    /// Optional HTTP(S) proxy URL for Commons traffic (to upload from a non-blocked IP).
    pub commons_proxy: Option<String>,
    /// OAuth 1.0a consumer key (from Special:OAuthConsumerRegistration); enables OAuth login.
    pub oauth_consumer_key: Option<String>,
    /// OAuth 1.0a consumer secret.
    pub oauth_consumer_secret: Option<String>,
}

impl Config {
    /// Loads configuration from process environment variables.
    pub fn from_env() -> Self {
        Self::from_env_lookup(|key| env::var(key).ok())
    }

    /// Builds configuration from a supplied environment lookup function.
    fn from_env_lookup(mut lookup: impl FnMut(&str) -> Option<String>) -> Self {
        let project_name = lookup("PROJECT_NAME").unwrap_or_else(|| DEFAULT_PROJECT_NAME.into());
        let aws_region = lookup("AWS_REGION")
            .or_else(|| lookup("AWS_DEFAULT_REGION"))
            .unwrap_or_else(|| "us-east-1".into());
        let max_file_bytes = lookup("MAX_FILE_MB")
            .and_then(|value| value.parse::<u64>().ok())
            .unwrap_or(DEFAULT_MAX_FILE_MB)
            * 1024
            * 1024;
        let max_conversion_file_bytes = lookup("MAX_CONVERSION_FILE_MB")
            .and_then(|value| value.parse::<u64>().ok())
            .unwrap_or(DEFAULT_MAX_CONVERSION_FILE_MB)
            * 1024
            * 1024;
        let max_archive_file_bytes = lookup("MAX_ARCHIVE_FILE_MB")
            .or_else(|| lookup("MAX_IN_MEMORY_FILE_MB"))
            .and_then(|value| value.parse::<u64>().ok())
            .unwrap_or(DEFAULT_MAX_ARCHIVE_FILE_MB)
            * 1024
            * 1024;
        let webp_quality = lookup("WEBP_QUALITY")
            .and_then(|value| value.parse::<f32>().ok())
            .map(|quality| quality.clamp(1.0, 100.0))
            .unwrap_or(DEFAULT_WEBP_QUALITY);
        let default_license = lookup("DEFAULT_LICENSE")
            .and_then(|value| License::parse(&value))
            .unwrap_or_default();
        let archive_thumbnail_resize = lookup("ARCHIVE_THUMBNAIL_RESIZE")
            .and_then(|value| parse_bool(&value))
            .unwrap_or(DEFAULT_ARCHIVE_THUMBNAIL_RESIZE);
        let github_url = lookup("GITHUB_URL").unwrap_or_else(|| DEFAULT_GITHUB_URL.into());
        let user_agent = lookup("COMMONS_USER_AGENT").unwrap_or_else(|| {
            format!(
                "{project_name}/{} ({github_url})",
                env!("CARGO_PKG_VERSION")
            )
        });

        Self {
            telegram_bot_token: lookup("TELEGRAM_BOT_TOKEN").filter(|value| !value.is_empty()),
            telegram_webhook_secret: lookup("TELEGRAM_WEBHOOK_SECRET")
                .filter(|value| !value.is_empty()),
            telegram_api_base: lookup("TELEGRAM_API_BASE")
                .filter(|value| !value.trim().is_empty())
                .unwrap_or_else(|| DEFAULT_TELEGRAM_API_BASE.into()),
            admin_user_ids: parse_admin_ids(&lookup("ADMIN_TELEGRAM_USER_IDS").unwrap_or_default()),
            github_url,
            aws_region,
            dynamodb_table: lookup("DYNAMODB_TABLE").filter(|value| !value.is_empty()),
            sqlite_path: lookup("SQLITE_PATH").filter(|value| !value.trim().is_empty()),
            credential_enc_key: lookup("CREDENTIAL_ENC_KEY")
                .filter(|value| !value.trim().is_empty()),
            default_license,
            webp_quality,
            max_file_bytes,
            max_conversion_file_bytes,
            max_archive_file_bytes,
            archive_thumbnail_resize,
            commons_api_url: lookup("COMMONS_API_URL")
                .filter(|value| !value.is_empty())
                .unwrap_or_else(|| DEFAULT_COMMONS_API_URL.into()),
            user_agent,
            commons_proxy: lookup("COMMONS_PROXY").filter(|value| !value.trim().is_empty()),
            oauth_consumer_key: lookup("OAUTH_CONSUMER_KEY")
                .filter(|value| !value.trim().is_empty()),
            oauth_consumer_secret: lookup("OAUTH_CONSUMER_SECRET")
                .filter(|value| !value.trim().is_empty()),
        }
    }

    /// Returns true when a Telegram user id belongs to an administrator.
    pub fn is_admin(&self, user_id: i64) -> bool {
        self.admin_user_ids.contains(&user_id)
    }
}

fn parse_bool(value: &str) -> Option<bool> {
    match value.trim().to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "y" | "on" => Some(true),
        "0" | "false" | "no" | "n" | "off" => Some(false),
        _ => None,
    }
}

/// Parses comma-separated Telegram numeric user ids.
fn parse_admin_ids(value: &str) -> Vec<i64> {
    value
        .split(',')
        .filter_map(|part| part.trim().parse::<i64>().ok())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::{
        Config, DEFAULT_ARCHIVE_THUMBNAIL_RESIZE, DEFAULT_MAX_ARCHIVE_FILE_MB,
        DEFAULT_MAX_CONVERSION_FILE_MB, DEFAULT_WEBP_QUALITY, parse_admin_ids,
    };
    use crate::models::License;
    use std::collections::HashMap;

    fn config_from_pairs(pairs: &[(&str, &str)]) -> Config {
        let values = pairs
            .iter()
            .map(|(key, value)| ((*key).to_string(), (*value).to_string()))
            .collect::<HashMap<_, _>>();
        Config::from_env_lookup(|key| values.get(key).cloned())
    }

    #[test]
    fn parses_admin_ids() {
        assert_eq!(parse_admin_ids("1, 2, bad,3"), vec![1, 2, 3]);
    }

    #[test]
    fn loads_defaults_when_environment_is_absent() {
        let config = config_from_pairs(&[]);

        assert_eq!(config.telegram_bot_token, None);
        assert_eq!(config.aws_region, "us-east-1");
        assert_eq!(config.dynamodb_table, None);
        assert_eq!(config.credential_enc_key, None);
        assert_eq!(config.default_license, License::CcBy40);
        assert_eq!(config.webp_quality, DEFAULT_WEBP_QUALITY);
        assert_eq!(config.max_file_bytes, 20 * 1024 * 1024);
        assert_eq!(
            config.max_conversion_file_bytes,
            DEFAULT_MAX_CONVERSION_FILE_MB * 1024 * 1024
        );
        assert_eq!(
            config.max_archive_file_bytes,
            DEFAULT_MAX_ARCHIVE_FILE_MB * 1024 * 1024
        );
        assert_eq!(
            config.archive_thumbnail_resize,
            DEFAULT_ARCHIVE_THUMBNAIL_RESIZE
        );
        assert_eq!(
            config.commons_api_url,
            "https://commons.wikimedia.org/w/api.php"
        );
    }

    #[test]
    fn loads_and_clamps_explicit_values() {
        let config = config_from_pairs(&[
            ("TELEGRAM_BOT_TOKEN", "token"),
            ("TELEGRAM_WEBHOOK_SECRET", "secret"),
            ("ADMIN_TELEGRAM_USER_IDS", "42,bad,7"),
            ("AWS_REGION", "eu-central-1"),
            ("DYNAMODB_TABLE", "profiles"),
            ("CREDENTIAL_ENC_KEY", "a2V5"),
            ("DEFAULT_LICENSE", "cc-zero"),
            ("WEBP_QUALITY", "250"),
            ("MAX_FILE_MB", "10"),
            ("MAX_CONVERSION_FILE_MB", "128"),
            ("MAX_ARCHIVE_FILE_MB", "2048"),
            ("ARCHIVE_THUMBNAIL_RESIZE", "false"),
        ]);

        assert_eq!(config.telegram_bot_token.as_deref(), Some("token"));
        assert_eq!(config.telegram_webhook_secret.as_deref(), Some("secret"));
        assert_eq!(config.admin_user_ids, vec![42, 7]);
        assert_eq!(config.aws_region, "eu-central-1");
        assert_eq!(config.dynamodb_table.as_deref(), Some("profiles"));
        assert_eq!(config.credential_enc_key.as_deref(), Some("a2V5"));
        assert_eq!(config.default_license, License::Cc0);
        assert_eq!(config.webp_quality, 100.0);
        assert_eq!(config.max_file_bytes, 10 * 1024 * 1024);
        assert_eq!(config.max_conversion_file_bytes, 128 * 1024 * 1024);
        assert_eq!(config.max_archive_file_bytes, 2048 * 1024 * 1024);
        assert!(!config.archive_thumbnail_resize);
        assert!(config.is_admin(42));
        assert!(!config.is_admin(1));
    }

    #[test]
    fn max_in_memory_file_mb_remains_archive_fallback() {
        let config = config_from_pairs(&[("MAX_IN_MEMORY_FILE_MB", "512")]);

        assert_eq!(config.max_conversion_file_bytes, 100 * 1024 * 1024);
        assert_eq!(config.max_archive_file_bytes, 512 * 1024 * 1024);
    }
}
