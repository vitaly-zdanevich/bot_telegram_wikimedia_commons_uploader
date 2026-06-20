use crate::models::License;
use std::env;

/// Default maximum downloadable file size — the Telegram cloud Bot API `getFile` cap.
const DEFAULT_MAX_FILE_MB: u64 = 20;
/// Default lossy WebP quality used when converting DNG files.
const DEFAULT_WEBP_QUALITY: f32 = 90.0;
/// Default Commons Action API endpoint.
const DEFAULT_COMMONS_API_URL: &str = "https://commons.wikimedia.org/w/api.php";
/// Default project repository URL shown in `/help`.
const DEFAULT_GITHUB_URL: &str =
    "https://github.com/vitaly-zdanevich/bot_telegram_wikimedia_commons_uploader";
/// Default resource name prefix.
const DEFAULT_PROJECT_NAME: &str = "telegram-wikimedia-commons-uploader-bot";

/// Runtime configuration loaded from environment variables.
#[derive(Clone, Debug)]
pub struct Config {
    /// Telegram bot token from BotFather.
    pub telegram_bot_token: Option<String>,
    /// Secret expected in the `X-Telegram-Bot-Api-Secret-Token` header.
    pub telegram_webhook_secret: Option<String>,
    /// Telegram user ids allowed to use admin commands.
    pub admin_user_ids: Vec<i64>,
    /// Project repository URL shown in `/help`.
    pub github_url: String,
    /// AWS region for DynamoDB.
    pub aws_region: String,
    /// DynamoDB table name for profiles and idempotency.
    pub dynamodb_table: Option<String>,
    /// Base64 32-byte master key for AES-GCM credential encryption.
    pub credential_enc_key: Option<String>,
    /// Default license offered during onboarding.
    pub default_license: License,
    /// Lossy WebP quality (1-100) used for DNG conversion.
    pub webp_quality: f32,
    /// Maximum file size the bot will download from Telegram.
    pub max_file_bytes: u64,
    /// Commons Action API endpoint.
    pub commons_api_url: String,
    /// User-Agent sent to Commons, per MediaWiki API etiquette.
    pub user_agent: String,
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
        let webp_quality = lookup("WEBP_QUALITY")
            .and_then(|value| value.parse::<f32>().ok())
            .map(|quality| quality.clamp(1.0, 100.0))
            .unwrap_or(DEFAULT_WEBP_QUALITY);
        let default_license = lookup("DEFAULT_LICENSE")
            .and_then(|value| License::parse(&value))
            .unwrap_or_default();
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
            admin_user_ids: parse_admin_ids(&lookup("ADMIN_TELEGRAM_USER_IDS").unwrap_or_default()),
            github_url,
            aws_region,
            dynamodb_table: lookup("DYNAMODB_TABLE").filter(|value| !value.is_empty()),
            credential_enc_key: lookup("CREDENTIAL_ENC_KEY")
                .filter(|value| !value.trim().is_empty()),
            default_license,
            webp_quality,
            max_file_bytes,
            commons_api_url: lookup("COMMONS_API_URL")
                .filter(|value| !value.is_empty())
                .unwrap_or_else(|| DEFAULT_COMMONS_API_URL.into()),
            user_agent,
        }
    }

    /// Returns true when a Telegram user id belongs to an administrator.
    pub fn is_admin(&self, user_id: i64) -> bool {
        self.admin_user_ids.contains(&user_id)
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
    use super::{Config, DEFAULT_WEBP_QUALITY, parse_admin_ids};
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
        assert!(config.is_admin(42));
        assert!(!config.is_admin(1));
    }
}
