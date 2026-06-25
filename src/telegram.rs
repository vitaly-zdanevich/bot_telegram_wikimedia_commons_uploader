use crate::models::License;
use anyhow::{Context, Result, bail};
use reqwest::Client;
use serde::Serialize;
use serde_json::{Value, json};
use std::path::Path;

/// Telegram Bot API client.
#[derive(Clone)]
pub struct TelegramClient {
    client: Client,
    token: String,
    base_url: String,
}

impl TelegramClient {
    /// Creates a Telegram API client for a base URL (cloud API or self-hosted server).
    pub fn new(token: impl Into<String>, base_url: impl Into<String>) -> Self {
        Self {
            client: Client::new(),
            token: token.into(),
            base_url: base_url.into().trim_end_matches('/').to_string(),
        }
    }

    /// Sends an HTML-formatted text message, splitting anything over Telegram's
    /// per-message limit into several messages (the keyboard rides the last one).
    pub async fn send_message(
        &self,
        chat_id: i64,
        text: &str,
        reply_markup: Option<InlineKeyboardMarkup>,
    ) -> Result<()> {
        let chunks = split_for_telegram(text, TELEGRAM_MESSAGE_LIMIT);
        let last = chunks.len().saturating_sub(1);
        for (index, chunk) in chunks.iter().enumerate() {
            let mut payload = json!({
                "chat_id": chat_id,
                "text": chunk,
                "parse_mode": "HTML",
                "disable_web_page_preview": true,
            });
            if index == last
                && let Some(markup) = &reply_markup
            {
                payload["reply_markup"] = serde_json::to_value(markup)?;
            }
            self.post_json("sendMessage", &payload).await?;
        }
        Ok(())
    }

    /// Deletes a message; used to scrub the user's bot-password message from the chat.
    pub async fn delete_message(&self, chat_id: i64, message_id: i64) -> Result<()> {
        self.post_json(
            "deleteMessage",
            &json!({"chat_id": chat_id, "message_id": message_id}),
        )
        .await?;
        Ok(())
    }

    /// Acknowledges a callback query so the client stops showing a spinner.
    pub async fn answer_callback_query(
        &self,
        callback_query_id: &str,
        text: Option<&str>,
    ) -> Result<()> {
        let mut payload = json!({"callback_query_id": callback_query_id});
        if let Some(text) = text {
            payload["text"] = json!(text);
        }
        self.post_json("answerCallbackQuery", &payload).await?;
        Ok(())
    }

    /// Sends a chat action such as `upload_photo` while the user waits.
    pub async fn send_chat_action(&self, chat_id: i64, action: &str) -> Result<()> {
        self.post_json(
            "sendChatAction",
            &json!({"chat_id": chat_id, "action": action}),
        )
        .await?;
        Ok(())
    }

    /// Resolves a Telegram `file_id` to its temporary download path via `getFile`.
    pub async fn get_file_path(&self, file_id: &str) -> Result<String> {
        let value = self
            .post_json("getFile", &json!({"file_id": file_id}))
            .await?;
        value
            .get("result")
            .and_then(|result| result.get("file_path"))
            .and_then(Value::as_str)
            .map(str::to_string)
            .context("Telegram getFile response is missing file_path")
    }

    /// Downloads a file by path, rejecting anything larger than `max_bytes`.
    ///
    /// The Telegram cloud Bot API only serves files up to 20 MB, so larger uploads
    /// cannot be retrieved and are reported to the user instead.
    pub async fn download_file(&self, file_path: &str, max_bytes: u64) -> Result<Vec<u8>> {
        if Path::new(file_path).is_absolute() {
            let metadata = std::fs::metadata(file_path)
                .with_context(|| format!("failed to stat Telegram file {file_path}"))?;
            if metadata.len() > max_bytes {
                bail!(
                    "file is {} bytes, larger than the {max_bytes} byte limit",
                    metadata.len()
                );
            }
            return std::fs::read(file_path)
                .with_context(|| format!("failed to read Telegram file {file_path}"));
        }

        let url = format!("{}/file/bot{}/{file_path}", self.base_url, self.token);
        let response = self.client.get(url).send().await?.error_for_status()?;
        if let Some(length) = response.content_length()
            && length > max_bytes
        {
            bail!("file is {length} bytes, larger than the {max_bytes} byte limit");
        }
        let bytes = response.bytes().await?;
        if bytes.len() as u64 > max_bytes {
            bail!(
                "file is {} bytes, larger than the {max_bytes} byte limit",
                bytes.len()
            );
        }
        Ok(bytes.to_vec())
    }

    /// Downloads a file by `file_id` (resolves the path, then downloads it).
    pub async fn download_by_file_id(&self, file_id: &str, max_bytes: u64) -> Result<Vec<u8>> {
        let path = self.get_file_path(file_id).await?;
        self.download_file(&path, max_bytes).await
    }

    /// Sends a photo (used for archive thumbnails). Built only with the `archive` feature.
    #[cfg(feature = "archive")]
    pub async fn send_photo(
        &self,
        chat_id: i64,
        photo: Vec<u8>,
        file_name: &str,
        caption: Option<&str>,
        reply_markup: Option<InlineKeyboardMarkup>,
    ) -> Result<()> {
        let mut form = reqwest::multipart::Form::new()
            .text("chat_id", chat_id.to_string())
            .part(
                "photo",
                reqwest::multipart::Part::bytes(photo).file_name(file_name.to_string()),
            );
        if let Some(caption) = caption {
            form = form.text("caption", caption.to_string());
        }
        if let Some(markup) = reply_markup {
            form = form.text("reply_markup", serde_json::to_string(&markup)?);
        }
        let response = self
            .client
            .post(self.method_url("sendPhoto"))
            .multipart(form)
            .send()
            .await?;
        let status = response.status();
        if !status.is_success() {
            let body = response.text().await?;
            bail!("Telegram sendPhoto failed with HTTP {status}: {body}");
        }
        Ok(())
    }

    /// Long-polls for updates (server mode), returning the parsed updates.
    pub async fn get_updates(
        &self,
        offset: i64,
        timeout_secs: u64,
    ) -> Result<Vec<crate::models::Update>> {
        let payload = json!({
            "offset": offset,
            "timeout": timeout_secs,
            "allowed_updates": ["message", "callback_query"],
        });
        let value = self.post_json("getUpdates", &payload).await?;
        let result = value.get("result").cloned().unwrap_or(Value::Null);
        Ok(serde_json::from_value(result).unwrap_or_default())
    }

    /// Sends a JSON request to a Telegram Bot API method.
    async fn post_json(&self, method: &str, payload: &Value) -> Result<Value> {
        let response = self
            .client
            .post(self.method_url(method))
            .json(payload)
            .send()
            .await?;
        let status = response.status();
        let body = response.text().await?;
        if !status.is_success() {
            bail!("Telegram method {method} failed with HTTP {status}: {body}");
        }
        Ok(serde_json::from_str(&body)?)
    }

    /// Builds the Telegram method URL against the configured base.
    fn method_url(&self, method: &str) -> String {
        format!("{}/bot{}/{method}", self.base_url, self.token)
    }
}

/// Telegram inline keyboard markup.
#[derive(Clone, Debug, Serialize)]
pub struct InlineKeyboardMarkup {
    /// Button rows.
    pub inline_keyboard: Vec<Vec<InlineKeyboardButton>>,
}

/// Telegram inline keyboard button.
#[derive(Clone, Debug, Serialize)]
pub struct InlineKeyboardButton {
    /// Button label.
    pub text: String,
    /// Callback data sent back when pressed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub callback_data: Option<String>,
    /// External URL opened when pressed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
}

/// Callback-data prefix for license-selection buttons (shared with the handler).
pub const LICENSE_CALLBACK_PREFIX: &str = "license:";

/// Builds the license-picker keyboard, one license per row.
pub fn license_keyboard() -> InlineKeyboardMarkup {
    InlineKeyboardMarkup {
        inline_keyboard: License::all()
            .iter()
            .map(|license| {
                vec![InlineKeyboardButton {
                    text: license.label().to_string(),
                    callback_data: Some(format!("{LICENSE_CALLBACK_PREFIX}{}", license.as_key())),
                    url: None,
                }]
            })
            .collect(),
    }
}

/// Telegram's maximum message length, measured in UTF-16 code units.
const TELEGRAM_MESSAGE_LIMIT: usize = 4096;

/// Returns the UTF-16 length Telegram uses to enforce message limits.
fn utf16_len(text: &str) -> usize {
    text.chars().map(char::len_utf16).sum()
}

/// Splits `text` into chunks within `limit` UTF-16 units, preferring line boundaries.
///
/// Each line keeps its own balanced HTML (our tags never span newlines), so every chunk
/// is independently valid. A single line longer than `limit` is hard-split by characters.
fn split_for_telegram(text: &str, limit: usize) -> Vec<String> {
    if utf16_len(text) <= limit {
        return vec![text.to_string()];
    }
    let mut chunks = Vec::new();
    let mut current = String::new();
    for line in text.split_inclusive('\n') {
        if utf16_len(line) > limit {
            if !current.is_empty() {
                chunks.push(std::mem::take(&mut current));
            }
            for ch in line.chars() {
                if utf16_len(&current) + ch.len_utf16() > limit {
                    chunks.push(std::mem::take(&mut current));
                }
                current.push(ch);
            }
        } else {
            if utf16_len(&current) + utf16_len(line) > limit {
                chunks.push(std::mem::take(&mut current));
            }
            current.push_str(line);
        }
    }
    if !current.is_empty() {
        chunks.push(current);
    }
    chunks
}

/// Escapes the three characters that are special in Telegram HTML message text.
pub fn escape_html(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

#[cfg(test)]
mod tests {
    use super::{TelegramClient, escape_html, license_keyboard, split_for_telegram, utf16_len};
    use crate::models::License;

    #[tokio::test]
    async fn download_file_reads_absolute_local_path() {
        let path = std::env::temp_dir().join(format!(
            "telegram-local-file-{}-{}.bin",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::write(&path, b"hello").unwrap();

        let client = TelegramClient::new("token", "http://example.test");
        let bytes = client
            .download_file(path.to_str().unwrap(), 10)
            .await
            .unwrap();

        std::fs::remove_file(path).unwrap();
        assert_eq!(bytes, b"hello");
    }

    #[test]
    fn license_keyboard_has_one_button_per_license() {
        let keyboard = license_keyboard();
        assert_eq!(keyboard.inline_keyboard.len(), License::all().len());
        assert_eq!(
            keyboard.inline_keyboard[0][0].callback_data.as_deref(),
            Some("license:cc-by-4.0")
        );
    }

    #[test]
    fn escapes_html_special_characters() {
        assert_eq!(escape_html("a<b>&c"), "a&lt;b&gt;&amp;c");
    }

    #[test]
    fn short_message_is_a_single_chunk() {
        assert_eq!(
            split_for_telegram("hello\nworld", 4096),
            vec!["hello\nworld".to_string()]
        );
    }

    #[test]
    fn long_message_splits_on_lines_and_rejoins() {
        let text = vec!["x".repeat(100); 60].join("\n");
        let chunks = split_for_telegram(&text, 1000);
        assert!(chunks.len() > 1);
        assert!(chunks.iter().all(|chunk| utf16_len(chunk) <= 1000));
        assert_eq!(chunks.concat(), text);
    }

    #[test]
    fn overlong_single_line_is_hard_split() {
        let text = "y".repeat(2500);
        let chunks = split_for_telegram(&text, 1000);
        assert!(chunks.len() >= 3);
        assert!(chunks.iter().all(|chunk| utf16_len(chunk) <= 1000));
        assert_eq!(chunks.concat(), text);
    }

    #[test]
    fn license_button_callback_data_parses_back() {
        use super::LICENSE_CALLBACK_PREFIX;
        for row in license_keyboard().inline_keyboard {
            let data = row[0].callback_data.as_deref().unwrap();
            let key = data
                .strip_prefix(LICENSE_CALLBACK_PREFIX)
                .expect("callback data must use the shared license prefix");
            assert!(License::parse(key).is_some());
        }
    }
}
