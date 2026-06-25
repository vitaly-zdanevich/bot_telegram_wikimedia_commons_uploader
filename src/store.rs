use crate::aws::AwsJsonClient;
use crate::config::Config;
use crate::models::{License, OnboardingStep, Profile};
use anyhow::Result;
use once_cell::sync::Lazy;
use serde_json::{Value, json};
use std::collections::HashMap;
use time::OffsetDateTime;
use tokio::sync::RwLock;

/// Per-user profiles cached in the warm process to avoid repeat backend reads.
static PROFILE_CACHE: Lazy<RwLock<HashMap<i64, Profile>>> =
    Lazy::new(|| RwLock::new(HashMap::new()));
/// Media-group captions cached by `media_group_id`, with a unix expiry timestamp.
static GROUP_CAPTION_CACHE: Lazy<RwLock<HashMap<String, (String, i64)>>> =
    Lazy::new(|| RwLock::new(HashMap::new()));
/// Idempotency reservations cached in RAM when there is no durable backend.
static IDEMPOTENCY_RAM: Lazy<RwLock<HashMap<String, i64>>> =
    Lazy::new(|| RwLock::new(HashMap::new()));

/// How long an album's caption is remembered for its later photos.
const GROUP_CAPTION_TTL_SECONDS: i64 = 10 * 60;

/// Aggregate upload statistics for the admin `/stat` command.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct UploadStats {
    /// Number of onboarded user profiles.
    pub users: u64,
    /// Sum of successful uploads across all users.
    pub uploads: u64,
}

/// Durable storage backend, chosen at startup from configuration.
enum Backend {
    /// No durable storage; RAM caches only (local runs / missing config).
    Memory,
    /// AWS DynamoDB (Lambda deployment).
    Dynamo { table: String, aws: AwsJsonClient },
    /// SQLite file (long-living server deployment).
    #[cfg(feature = "sqlite")]
    Sqlite(std::sync::Mutex<rusqlite::Connection>),
}

/// Storage for user profiles, album captions, and webhook idempotency.
///
/// Reads consult a process-wide RAM cache first. The durable backend is SQLite when
/// `SQLITE_PATH` is set (server mode), otherwise DynamoDB, otherwise in-memory only.
pub struct Store {
    backend: Backend,
}

impl Store {
    /// Creates a store from runtime configuration.
    pub fn new(config: &Config) -> Self {
        #[cfg(feature = "sqlite")]
        if let Some(path) = config.sqlite_path.as_deref() {
            match open_sqlite(path) {
                Ok(connection) => {
                    return Self {
                        backend: Backend::Sqlite(std::sync::Mutex::new(connection)),
                    };
                }
                Err(error) => {
                    tracing::error!(error = %format!("{error:#}"), "failed to open SQLite; using in-memory store");
                }
            }
        }

        let aws = AwsJsonClient::new(config.aws_region.clone());
        match config.dynamodb_table.clone() {
            Some(table) if aws.has_credentials() => Self {
                backend: Backend::Dynamo { table, aws },
            },
            _ => Self {
                backend: Backend::Memory,
            },
        }
    }

    /// Loads a user profile, using the RAM cache before the backend.
    pub async fn get_profile(&self, user_id: i64) -> Profile {
        if let Some(cached) = PROFILE_CACHE.read().await.get(&user_id).cloned() {
            return cached;
        }
        let loaded = self.load_profile(user_id).await.unwrap_or_default();
        PROFILE_CACHE.write().await.insert(user_id, loaded.clone());
        loaded
    }

    /// Loads one profile directly from the backend.
    async fn load_profile(&self, user_id: i64) -> Result<Profile> {
        match &self.backend {
            Backend::Memory => Ok(Profile::default()),
            Backend::Dynamo { table, aws } => {
                let response = aws
                    .post_json(
                        "dynamodb",
                        "DynamoDB_20120810.GetItem",
                        json!({
                            "TableName": table,
                            "Key": {"pk": {"S": format!("USER#{user_id}")}, "sk": {"S": "PROFILE"}},
                        }),
                    )
                    .await?;
                Ok(response
                    .get("Item")
                    .map(item_to_profile)
                    .unwrap_or_default())
            }
            #[cfg(feature = "sqlite")]
            Backend::Sqlite(connection) => sqlite_load_profile(connection, user_id),
        }
    }

    /// Saves a user profile and refreshes the RAM cache.
    pub async fn put_profile(&self, user_id: i64, profile: &Profile) -> Result<()> {
        PROFILE_CACHE.write().await.insert(user_id, profile.clone());
        match &self.backend {
            Backend::Memory => Ok(()),
            Backend::Dynamo { table, aws } => {
                aws.post_json(
                    "dynamodb",
                    "DynamoDB_20120810.PutItem",
                    json!({"TableName": table, "Item": profile_to_item(user_id, profile)}),
                )
                .await?;
                Ok(())
            }
            #[cfg(feature = "sqlite")]
            Backend::Sqlite(connection) => sqlite_put_profile(connection, user_id, profile),
        }
    }

    /// Deletes a user profile and its cached copy (used by `/forget`).
    pub async fn delete_profile(&self, user_id: i64) -> Result<()> {
        PROFILE_CACHE.write().await.remove(&user_id);
        match &self.backend {
            Backend::Memory => Ok(()),
            Backend::Dynamo { table, aws } => {
                aws.post_json(
                    "dynamodb",
                    "DynamoDB_20120810.DeleteItem",
                    json!({
                        "TableName": table,
                        "Key": {"pk": {"S": format!("USER#{user_id}")}, "sk": {"S": "PROFILE"}},
                    }),
                )
                .await?;
                Ok(())
            }
            #[cfg(feature = "sqlite")]
            Backend::Sqlite(connection) => {
                connection.lock().expect("sqlite mutex poisoned").execute(
                    "DELETE FROM profiles WHERE user_id = ?1",
                    rusqlite::params![user_id],
                )?;
                Ok(())
            }
        }
    }

    /// Remembers an album's caption so the album's later photos can reuse it.
    pub async fn put_group_caption(&self, group_id: &str, caption: &str) -> Result<()> {
        let expires_at = now() + GROUP_CAPTION_TTL_SECONDS;
        GROUP_CAPTION_CACHE
            .write()
            .await
            .insert(group_id.to_string(), (caption.to_string(), expires_at));
        match &self.backend {
            Backend::Memory => Ok(()),
            Backend::Dynamo { table, aws } => {
                aws.post_json(
                    "dynamodb",
                    "DynamoDB_20120810.PutItem",
                    json!({
                        "TableName": table,
                        "Item": {
                            "pk": {"S": format!("MEDIAGROUP#{group_id}")},
                            "sk": {"S": "CAPTION"},
                            "caption": {"S": caption},
                            "expires_at": {"N": expires_at.to_string()},
                        },
                    }),
                )
                .await?;
                Ok(())
            }
            #[cfg(feature = "sqlite")]
            Backend::Sqlite(connection) => {
                connection.lock().expect("sqlite mutex poisoned").execute(
                    "INSERT OR REPLACE INTO group_captions(group_id, caption, expires_at) VALUES (?1, ?2, ?3)",
                    rusqlite::params![group_id, caption, expires_at],
                )?;
                Ok(())
            }
        }
    }

    /// Returns an album's caption from the RAM cache, then the backend.
    pub async fn get_group_caption(&self, group_id: &str) -> Option<String> {
        let now = now();
        {
            let mut cache = GROUP_CAPTION_CACHE.write().await;
            cache.retain(|_, (_, expiry)| *expiry >= now);
            if let Some((caption, _)) = cache.get(group_id) {
                return Some(caption.clone());
            }
        }
        let caption = match &self.backend {
            Backend::Memory => None,
            Backend::Dynamo { table, aws } => aws
                .post_json(
                    "dynamodb",
                    "DynamoDB_20120810.GetItem",
                    json!({
                        "TableName": table,
                        "Key": {"pk": {"S": format!("MEDIAGROUP#{group_id}")}, "sk": {"S": "CAPTION"}},
                    }),
                )
                .await
                .ok()
                .and_then(|response| {
                    let item = response.get("Item")?;
                    if attr_i64(item, "expires_at").is_some_and(|expiry| expiry < now) {
                        return None;
                    }
                    attr_string(item, "caption")
                }),
            #[cfg(feature = "sqlite")]
            Backend::Sqlite(connection) => sqlite_get_group_caption(connection, group_id, now).ok().flatten(),
        };
        if let Some(caption) = &caption {
            GROUP_CAPTION_CACHE.write().await.insert(
                group_id.to_string(),
                (caption.clone(), now + GROUP_CAPTION_TTL_SECONDS),
            );
        }
        caption
    }

    /// Reserves an idempotency key, returning false when it was already seen.
    pub async fn reserve_idempotency(&self, key: &str, retention_seconds: i64) -> Result<bool> {
        let now = now();
        let expires_at = now.saturating_add(retention_seconds.max(1));
        match &self.backend {
            Backend::Memory => reserve_in_ram(key, now, expires_at).await,
            Backend::Dynamo { table, aws } => {
                let result = aws
                    .post_json(
                        "dynamodb",
                        "DynamoDB_20120810.PutItem",
                        json!({
                            "TableName": table,
                            "Item": {
                                "pk": {"S": key},
                                "sk": {"S": "IDEMPOTENCY"},
                                "expires_at": {"N": expires_at.to_string()},
                            },
                            "ConditionExpression": "attribute_not_exists(pk) OR expires_at < :now",
                            "ExpressionAttributeValues": {":now": {"N": now.to_string()}},
                        }),
                    )
                    .await;
                match result {
                    Ok(_) => Ok(true),
                    Err(error) if is_conditional_check_failed(&error) => Ok(false),
                    Err(error) => Err(error),
                }
            }
            #[cfg(feature = "sqlite")]
            Backend::Sqlite(connection) => sqlite_reserve(connection, key, now, expires_at),
        }
    }

    /// Totals onboarded profiles and uploads (admin `/stat`).
    pub async fn aggregate_stats(&self) -> Result<UploadStats> {
        match &self.backend {
            Backend::Memory => Ok(UploadStats::default()),
            Backend::Dynamo { table, aws } => {
                let mut stats = UploadStats::default();
                let mut start_key: Option<Value> = None;
                loop {
                    let mut request = json!({
                        "TableName": table,
                        "FilterExpression": "sk = :profile",
                        "ExpressionAttributeValues": {":profile": {"S": "PROFILE"}},
                        "ProjectionExpression": "uploads_count",
                    });
                    if let Some(key) = &start_key {
                        request["ExclusiveStartKey"] = key.clone();
                    }
                    let response = aws
                        .post_json("dynamodb", "DynamoDB_20120810.Scan", request)
                        .await?;
                    if let Some(items) = response.get("Items").and_then(Value::as_array) {
                        for item in items {
                            stats.users += 1;
                            stats.uploads +=
                                attr_i64(item, "uploads_count").unwrap_or(0).max(0) as u64;
                        }
                    }
                    match response.get("LastEvaluatedKey") {
                        Some(key) if !key.is_null() => start_key = Some(key.clone()),
                        _ => break,
                    }
                }
                Ok(stats)
            }
            #[cfg(feature = "sqlite")]
            Backend::Sqlite(connection) => sqlite_aggregate_stats(connection),
        }
    }
}

/// Returns the current unix timestamp in seconds.
fn now() -> i64 {
    OffsetDateTime::now_utc().unix_timestamp()
}

/// Reserves a key in the warm-process RAM cache.
async fn reserve_in_ram(key: &str, now: i64, expires_at: i64) -> Result<bool> {
    let mut seen = IDEMPOTENCY_RAM.write().await;
    seen.retain(|_, expiry| *expiry >= now);
    if seen.get(key).is_some_and(|expiry| *expiry >= now) {
        return Ok(false);
    }
    seen.insert(key.to_string(), expires_at);
    Ok(true)
}

/// Returns true for DynamoDB conditional-write failures.
fn is_conditional_check_failed(error: &anyhow::Error) -> bool {
    format!("{error:#}").contains("ConditionalCheckFailedException")
}

/// Converts a profile into a DynamoDB item.
fn profile_to_item(user_id: i64, profile: &Profile) -> Value {
    let mut item = json!({
        "pk": {"S": format!("USER#{user_id}")},
        "sk": {"S": "PROFILE"},
        "license": {"S": profile.license.as_key()},
        "filename_prefix": {"S": profile.filename_prefix},
        "onboarding_step": {"S": profile.onboarding_step.as_str()},
        "default_categories": {"L": profile.default_categories.iter().map(|category| json!({"S": category})).collect::<Vec<_>>()},
        "return_upload_links": {"BOOL": profile.return_upload_links},
        "return_category_links": {"BOOL": profile.return_category_links},
        "return_missing_category_links": {"BOOL": profile.return_missing_category_links},
        "return_archive_file_list": {"BOOL": profile.return_archive_file_list},
        "archive_confirm": {"BOOL": profile.archive_confirm},
        "uploads_count": {"N": profile.uploads_count.to_string()},
        "created_at": {"N": profile.created_at.to_string()},
        "updated_at": {"N": profile.updated_at.to_string()},
    });
    if let Some(username) = &profile.commons_username {
        item["commons_username"] = json!({"S": username});
    }
    if let Some(ciphertext) = &profile.credential_ciphertext {
        item["credential_ciphertext"] = json!({"S": ciphertext});
    }
    if let Some(ciphertext) = &profile.oauth_ciphertext {
        item["oauth_ciphertext"] = json!({"S": ciphertext});
    }
    if let Some(ciphertext) = &profile.oauth_pending_ciphertext {
        item["oauth_pending_ciphertext"] = json!({"S": ciphertext});
    }
    if let Some(author) = &profile.default_author {
        item["default_author"] = json!({"S": author});
    }
    if let Some(description) = &profile.default_description {
        item["default_description"] = json!({"S": description});
    }
    if let Some(lang) = &profile.default_lang {
        item["default_lang"] = json!({"S": lang});
    }
    if let Some(license) = &profile.license_override {
        item["license_override"] = json!({"S": license});
    }
    item
}

/// Converts a DynamoDB item into a profile.
fn item_to_profile(item: &Value) -> Profile {
    Profile {
        commons_username: attr_string(item, "commons_username"),
        credential_ciphertext: attr_string(item, "credential_ciphertext"),
        oauth_ciphertext: attr_string(item, "oauth_ciphertext"),
        oauth_pending_ciphertext: attr_string(item, "oauth_pending_ciphertext"),
        license: attr_string(item, "license")
            .and_then(|value| License::parse(&value))
            .unwrap_or_default(),
        filename_prefix: attr_string(item, "filename_prefix").unwrap_or_default(),
        onboarding_step: attr_string(item, "onboarding_step")
            .and_then(|value| OnboardingStep::parse(&value))
            .unwrap_or_default(),
        default_categories: attr_string_list(item, "default_categories"),
        default_author: attr_string(item, "default_author"),
        default_description: attr_string(item, "default_description"),
        default_lang: attr_string(item, "default_lang"),
        license_override: attr_string(item, "license_override"),
        return_upload_links: attr_bool(item, "return_upload_links")
            .unwrap_or_else(|| Profile::default().return_upload_links),
        return_category_links: attr_bool(item, "return_category_links").unwrap_or(false),
        return_missing_category_links: attr_bool(item, "return_missing_category_links")
            .unwrap_or(false),
        return_archive_file_list: attr_bool(item, "return_archive_file_list").unwrap_or(false),
        archive_confirm: attr_bool(item, "archive_confirm").unwrap_or(true),
        uploads_count: attr_i64(item, "uploads_count").unwrap_or(0).max(0) as u64,
        created_at: attr_i64(item, "created_at").unwrap_or(0),
        updated_at: attr_i64(item, "updated_at").unwrap_or(0),
    }
}

/// Reads a DynamoDB string attribute.
fn attr_string(item: &Value, key: &str) -> Option<String> {
    item.get(key)?.get("S")?.as_str().map(str::to_string)
}

/// Reads a DynamoDB number attribute as `i64`.
fn attr_i64(item: &Value, key: &str) -> Option<i64> {
    item.get(key)?.get("N")?.as_str()?.parse().ok()
}

/// Reads a DynamoDB boolean attribute.
fn attr_bool(item: &Value, key: &str) -> Option<bool> {
    item.get(key)?.get("BOOL")?.as_bool()
}

/// Reads a DynamoDB string-list attribute.
fn attr_string_list(item: &Value, key: &str) -> Vec<String> {
    item.get(key)
        .and_then(|value| value.get("L"))
        .and_then(Value::as_array)
        .map(|entries| {
            entries
                .iter()
                .filter_map(|entry| entry.get("S").and_then(Value::as_str).map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

/// Opens (and migrates) the SQLite database.
#[cfg(feature = "sqlite")]
fn open_sqlite(path: &str) -> Result<rusqlite::Connection> {
    let mut connection = rusqlite::Connection::open(path)?;
    connection.execute_batch(
        "CREATE TABLE IF NOT EXISTS profiles (
            user_id INTEGER PRIMARY KEY,
            commons_username TEXT,
            credential_ciphertext TEXT,
            oauth_ciphertext TEXT,
            oauth_pending_ciphertext TEXT,
            default_author TEXT,
            default_description TEXT,
            default_lang TEXT,
            license_override TEXT,
            license TEXT NOT NULL DEFAULT 'cc-by-4.0',
            filename_prefix TEXT NOT NULL DEFAULT '',
            onboarding_step TEXT NOT NULL DEFAULT 'awaiting_username',
            default_categories TEXT NOT NULL DEFAULT '[]',
            return_upload_links INTEGER NOT NULL DEFAULT 1,
            return_category_links INTEGER NOT NULL DEFAULT 0,
            return_missing_category_links INTEGER NOT NULL DEFAULT 0,
            return_archive_file_list INTEGER NOT NULL DEFAULT 0,
            archive_confirm INTEGER NOT NULL DEFAULT 1,
            uploads_count INTEGER NOT NULL DEFAULT 0,
            created_at INTEGER NOT NULL DEFAULT 0,
            updated_at INTEGER NOT NULL DEFAULT 0
        );
        CREATE TABLE IF NOT EXISTS group_captions (
            group_id TEXT PRIMARY KEY,
            caption TEXT NOT NULL,
            expires_at INTEGER NOT NULL
        );
        CREATE TABLE IF NOT EXISTS idempotency (
            key TEXT PRIMARY KEY,
            expires_at INTEGER NOT NULL
        );
        CREATE TABLE IF NOT EXISTS schema_migrations (
            name TEXT PRIMARY KEY,
            applied_at INTEGER NOT NULL DEFAULT 0
        );",
    )?;
    run_sqlite_migrations(&mut connection)?;
    Ok(connection)
}

/// Applies one-time SQLite data migrations.
#[cfg(feature = "sqlite")]
fn run_sqlite_migrations(connection: &mut rusqlite::Connection) -> Result<()> {
    let transaction = connection.transaction()?;
    let inserted = transaction.execute(
        "INSERT OR IGNORE INTO schema_migrations (name, applied_at) VALUES ('return_upload_links_default_on', strftime('%s', 'now'))",
        [],
    )?;
    if inserted > 0 {
        transaction.execute("UPDATE profiles SET return_upload_links = 1", [])?;
    }
    transaction.commit()?;
    Ok(())
}

/// Loads a profile from SQLite (returns default when absent).
#[cfg(feature = "sqlite")]
fn sqlite_load_profile(
    connection: &std::sync::Mutex<rusqlite::Connection>,
    user_id: i64,
) -> Result<Profile> {
    let connection = connection.lock().expect("sqlite mutex poisoned");
    let result = connection.query_row(
        "SELECT commons_username, credential_ciphertext, license, filename_prefix, onboarding_step, default_categories, return_upload_links, return_category_links, return_missing_category_links, uploads_count, created_at, updated_at, default_author, default_description, default_lang, license_override, return_archive_file_list, archive_confirm, oauth_ciphertext, oauth_pending_ciphertext FROM profiles WHERE user_id = ?1",
        rusqlite::params![user_id],
        |row| {
            let categories: String = row.get(5)?;
            Ok(Profile {
                commons_username: row.get(0)?,
                credential_ciphertext: row.get(1)?,
                oauth_ciphertext: row.get(18)?,
                oauth_pending_ciphertext: row.get(19)?,
                license: License::parse(&row.get::<_, String>(2)?).unwrap_or_default(),
                filename_prefix: row.get(3)?,
                onboarding_step: OnboardingStep::parse(&row.get::<_, String>(4)?).unwrap_or_default(),
                default_categories: serde_json::from_str(&categories).unwrap_or_default(),
                return_upload_links: row.get::<_, i64>(6)? != 0,
                return_category_links: row.get::<_, i64>(7)? != 0,
                return_missing_category_links: row.get::<_, i64>(8)? != 0,
                uploads_count: row.get::<_, i64>(9)?.max(0) as u64,
                created_at: row.get(10)?,
                updated_at: row.get(11)?,
                default_author: row.get(12)?,
                default_description: row.get(13)?,
                default_lang: row.get(14)?,
                license_override: row.get(15)?,
                return_archive_file_list: row.get::<_, i64>(16)? != 0,
                archive_confirm: row.get::<_, i64>(17)? != 0,
            })
        },
    );
    match result {
        Ok(profile) => Ok(profile),
        Err(rusqlite::Error::QueryReturnedNoRows) => Ok(Profile::default()),
        Err(error) => Err(error.into()),
    }
}

/// Writes a profile to SQLite.
#[cfg(feature = "sqlite")]
fn sqlite_put_profile(
    connection: &std::sync::Mutex<rusqlite::Connection>,
    user_id: i64,
    profile: &Profile,
) -> Result<()> {
    let categories =
        serde_json::to_string(&profile.default_categories).unwrap_or_else(|_| "[]".into());
    connection.lock().expect("sqlite mutex poisoned").execute(
        "INSERT OR REPLACE INTO profiles (user_id, commons_username, credential_ciphertext, license, filename_prefix, onboarding_step, default_categories, return_upload_links, return_category_links, return_missing_category_links, uploads_count, created_at, updated_at, default_author, default_description, default_lang, license_override, return_archive_file_list, archive_confirm, oauth_ciphertext, oauth_pending_ciphertext) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19, ?20, ?21)",
        rusqlite::params![
            user_id,
            profile.commons_username,
            profile.credential_ciphertext,
            profile.license.as_key(),
            profile.filename_prefix,
            profile.onboarding_step.as_str(),
            categories,
            profile.return_upload_links as i64,
            profile.return_category_links as i64,
            profile.return_missing_category_links as i64,
            profile.uploads_count as i64,
            profile.created_at,
            profile.updated_at,
            profile.default_author,
            profile.default_description,
            profile.default_lang,
            profile.license_override,
            profile.return_archive_file_list as i64,
            profile.archive_confirm as i64,
            profile.oauth_ciphertext,
            profile.oauth_pending_ciphertext,
        ],
    )?;
    Ok(())
}

/// Reads an album caption from SQLite if present and unexpired.
#[cfg(feature = "sqlite")]
fn sqlite_get_group_caption(
    connection: &std::sync::Mutex<rusqlite::Connection>,
    group_id: &str,
    now: i64,
) -> Result<Option<String>> {
    let connection = connection.lock().expect("sqlite mutex poisoned");
    match connection.query_row(
        "SELECT caption FROM group_captions WHERE group_id = ?1 AND expires_at >= ?2",
        rusqlite::params![group_id, now],
        |row| row.get::<_, String>(0),
    ) {
        Ok(caption) => Ok(Some(caption)),
        Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
        Err(error) => Err(error.into()),
    }
}

/// Reserves an idempotency key in SQLite (deletes expired rows first).
#[cfg(feature = "sqlite")]
fn sqlite_reserve(
    connection: &std::sync::Mutex<rusqlite::Connection>,
    key: &str,
    now: i64,
    expires_at: i64,
) -> Result<bool> {
    let connection = connection.lock().expect("sqlite mutex poisoned");
    connection.execute(
        "DELETE FROM idempotency WHERE expires_at < ?1",
        rusqlite::params![now],
    )?;
    match connection.execute(
        "INSERT INTO idempotency(key, expires_at) VALUES (?1, ?2)",
        rusqlite::params![key, expires_at],
    ) {
        Ok(_) => Ok(true),
        Err(rusqlite::Error::SqliteFailure(error, _))
            if error.code == rusqlite::ErrorCode::ConstraintViolation =>
        {
            Ok(false)
        }
        Err(error) => Err(error.into()),
    }
}

/// Totals profiles and uploads from SQLite.
#[cfg(feature = "sqlite")]
fn sqlite_aggregate_stats(
    connection: &std::sync::Mutex<rusqlite::Connection>,
) -> Result<UploadStats> {
    let connection = connection.lock().expect("sqlite mutex poisoned");
    let (users, uploads): (i64, i64) = connection.query_row(
        "SELECT COUNT(*), COALESCE(SUM(uploads_count), 0) FROM profiles",
        [],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    Ok(UploadStats {
        users: users.max(0) as u64,
        uploads: uploads.max(0) as u64,
    })
}

#[cfg(test)]
mod tests {
    use super::{
        IDEMPOTENCY_RAM, is_conditional_check_failed, item_to_profile, profile_to_item,
        reserve_in_ram,
    };
    use crate::models::{License, OnboardingStep, Profile};

    #[test]
    fn profile_round_trips_through_dynamodb_json() {
        let profile = Profile {
            commons_username: Some("Example@uploader".into()),
            credential_ciphertext: Some("base64ciphertext".into()),
            oauth_ciphertext: Some("oauthct".into()),
            oauth_pending_ciphertext: None,
            license: License::Cc0,
            filename_prefix: "Minsk trip".into(),
            onboarding_step: OnboardingStep::Done,
            default_categories: vec!["Minsk".into(), "Belarus".into()],
            default_author: Some("Jane Doe".into()),
            default_description: Some("A trip".into()),
            default_lang: Some("en".into()),
            license_override: None,
            return_upload_links: true,
            return_category_links: true,
            return_missing_category_links: false,
            return_archive_file_list: false,
            archive_confirm: true,
            uploads_count: 12,
            created_at: 1_700_000_000,
            updated_at: 1_700_000_500,
        };
        let item = profile_to_item(42, &profile);
        assert_eq!(item["pk"]["S"], "USER#42");
        assert_eq!(item_to_profile(&item), profile);
    }

    #[test]
    fn missing_optional_fields_default_safely() {
        let item = profile_to_item(1, &Profile::default());
        let profile = item_to_profile(&item);
        assert_eq!(profile.commons_username, None);
        assert_eq!(profile.license, License::CcBy40);
        assert!(profile.return_upload_links);
        assert!(!profile.is_ready());

        let mut legacy_item = item;
        legacy_item
            .as_object_mut()
            .expect("profile item is an object")
            .remove("return_upload_links");
        let legacy_profile = item_to_profile(&legacy_item);
        assert!(legacy_profile.return_upload_links);
    }

    #[tokio::test]
    async fn ram_reservation_suppresses_unexpired_duplicates() {
        IDEMPOTENCY_RAM.write().await.clear();
        assert!(reserve_in_ram("update:1", 100, 200).await.unwrap());
        assert!(!reserve_in_ram("update:1", 101, 201).await.unwrap());
        assert!(reserve_in_ram("update:1", 201, 301).await.unwrap());
    }

    #[test]
    fn detects_conditional_check_errors() {
        let error = anyhow::anyhow!("...ConditionalCheckFailedException...");
        assert!(is_conditional_check_failed(&error));
        assert!(!is_conditional_check_failed(&anyhow::anyhow!("other")));
    }

    #[cfg(feature = "sqlite")]
    #[test]
    fn sqlite_migration_enables_upload_links_once() {
        let mut connection = rusqlite::Connection::open_in_memory().unwrap();
        connection
            .execute_batch(
                "CREATE TABLE profiles (
                    user_id INTEGER PRIMARY KEY,
                    return_upload_links INTEGER NOT NULL DEFAULT 0
                );
                CREATE TABLE schema_migrations (
                    name TEXT PRIMARY KEY,
                    applied_at INTEGER NOT NULL DEFAULT 0
                );",
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO profiles (user_id, return_upload_links) VALUES (1, 0)",
                [],
            )
            .unwrap();

        super::run_sqlite_migrations(&mut connection).unwrap();
        let enabled: i64 = connection
            .query_row(
                "SELECT return_upload_links FROM profiles WHERE user_id = 1",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(enabled, 1);

        connection
            .execute(
                "UPDATE profiles SET return_upload_links = 0 WHERE user_id = 1",
                [],
            )
            .unwrap();
        super::run_sqlite_migrations(&mut connection).unwrap();
        let disabled: i64 = connection
            .query_row(
                "SELECT return_upload_links FROM profiles WHERE user_id = 1",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(disabled, 0);
    }

    #[cfg(feature = "sqlite")]
    #[test]
    fn sqlite_round_trips_profile_and_idempotency() {
        let connection = std::sync::Mutex::new(super::open_sqlite(":memory:").unwrap());
        let profile = Profile {
            commons_username: Some("Example@uploader".into()),
            credential_ciphertext: Some("ct".into()),
            oauth_ciphertext: None,
            oauth_pending_ciphertext: Some("pendingct".into()),
            license: License::CcBySa40,
            filename_prefix: "Trip".into(),
            onboarding_step: OnboardingStep::Done,
            default_categories: vec!["Minsk".into()],
            default_author: Some("Jane".into()),
            default_description: Some("Trip".into()),
            default_lang: None,
            license_override: Some("{{PD-RU-exempt}}".into()),
            return_upload_links: true,
            return_category_links: false,
            return_missing_category_links: true,
            return_archive_file_list: true,
            archive_confirm: false,
            uploads_count: 5,
            created_at: 1,
            updated_at: 2,
        };
        super::sqlite_put_profile(&connection, 7, &profile).unwrap();
        assert_eq!(super::sqlite_load_profile(&connection, 7).unwrap(), profile);
        assert!(super::sqlite_reserve(&connection, "k", 100, 200).unwrap());
        assert!(!super::sqlite_reserve(&connection, "k", 101, 201).unwrap());
        let stats = super::sqlite_aggregate_stats(&connection).unwrap();
        assert_eq!(stats.users, 1);
        assert_eq!(stats.uploads, 5);
    }
}
