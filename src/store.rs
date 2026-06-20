use crate::aws::AwsJsonClient;
use crate::config::Config;
use crate::models::{License, OnboardingStep, Profile};
use anyhow::Result;
use once_cell::sync::Lazy;
use serde_json::{Value, json};
use std::collections::HashMap;
use time::OffsetDateTime;
use tokio::sync::RwLock;

/// Per-user profiles cached in warm Lambda instances to avoid repeat DynamoDB reads.
static PROFILE_CACHE: Lazy<RwLock<HashMap<i64, Profile>>> =
    Lazy::new(|| RwLock::new(HashMap::new()));
/// Media-group captions cached by `media_group_id`, with a unix expiry timestamp.
static GROUP_CAPTION_CACHE: Lazy<RwLock<HashMap<String, (String, i64)>>> =
    Lazy::new(|| RwLock::new(HashMap::new()));
/// Idempotency reservations cached in RAM when DynamoDB is unavailable.
static IDEMPOTENCY_RAM: Lazy<RwLock<HashMap<String, i64>>> =
    Lazy::new(|| RwLock::new(HashMap::new()));

/// How long an album's caption is remembered for its later photos.
const GROUP_CAPTION_TTL_SECONDS: i64 = 10 * 60;

/// DynamoDB-backed storage for user profiles, album captions, and webhook idempotency.
///
/// Every read consults a process-wide RAM cache first, so warm Lambda containers serve
/// repeat lookups (including the photos of one album) without touching DynamoDB.
#[derive(Clone)]
pub struct Store {
    table_name: Option<String>,
    aws: AwsJsonClient,
}

impl Store {
    /// Creates a store from runtime configuration.
    pub fn new(config: &Config) -> Self {
        Self {
            table_name: config.dynamodb_table.clone(),
            aws: AwsJsonClient::new(config.aws_region.clone()),
        }
    }

    /// Returns true when DynamoDB can be used (table configured and credentials present).
    fn dynamodb_available(&self) -> bool {
        self.table_name.is_some() && self.aws.has_credentials()
    }

    /// Returns the configured table name, panicking only when the caller mis-checks.
    fn table(&self) -> &str {
        self.table_name.as_deref().expect("checked by caller")
    }

    /// Loads a user profile, using the RAM cache before DynamoDB.
    pub async fn get_profile(&self, user_id: i64) -> Profile {
        if let Some(cached) = PROFILE_CACHE.read().await.get(&user_id).cloned() {
            return cached;
        }
        if !self.dynamodb_available() {
            return Profile::default();
        }
        let profile = self.load_profile(user_id).await.unwrap_or_default();
        PROFILE_CACHE.write().await.insert(user_id, profile.clone());
        profile
    }

    /// Saves a user profile and refreshes the RAM cache.
    pub async fn put_profile(&self, user_id: i64, profile: &Profile) -> Result<()> {
        PROFILE_CACHE.write().await.insert(user_id, profile.clone());
        if !self.dynamodb_available() {
            return Ok(());
        }
        self.aws
            .post_json(
                "dynamodb",
                "DynamoDB_20120810.PutItem",
                json!({
                    "TableName": self.table(),
                    "Item": profile_to_item(user_id, profile),
                }),
            )
            .await?;
        Ok(())
    }

    /// Deletes a user profile and its cached copy (used by `/forget`).
    pub async fn delete_profile(&self, user_id: i64) -> Result<()> {
        PROFILE_CACHE.write().await.remove(&user_id);
        if !self.dynamodb_available() {
            return Ok(());
        }
        self.aws
            .post_json(
                "dynamodb",
                "DynamoDB_20120810.DeleteItem",
                json!({
                    "TableName": self.table(),
                    "Key": {
                        "pk": {"S": format!("USER#{user_id}")},
                        "sk": {"S": "PROFILE"},
                    },
                }),
            )
            .await?;
        Ok(())
    }

    /// Loads a profile item directly from DynamoDB.
    async fn load_profile(&self, user_id: i64) -> Result<Profile> {
        let response = self
            .aws
            .post_json(
                "dynamodb",
                "DynamoDB_20120810.GetItem",
                json!({
                    "TableName": self.table(),
                    "Key": {
                        "pk": {"S": format!("USER#{user_id}")},
                        "sk": {"S": "PROFILE"},
                    },
                }),
            )
            .await?;
        Ok(response
            .get("Item")
            .map(item_to_profile)
            .unwrap_or_default())
    }

    /// Remembers an album's caption so the album's later photos can reuse it.
    pub async fn put_group_caption(&self, group_id: &str, caption: &str) -> Result<()> {
        let expires_at = now() + GROUP_CAPTION_TTL_SECONDS;
        GROUP_CAPTION_CACHE
            .write()
            .await
            .insert(group_id.to_string(), (caption.to_string(), expires_at));
        if !self.dynamodb_available() {
            return Ok(());
        }
        self.aws
            .post_json(
                "dynamodb",
                "DynamoDB_20120810.PutItem",
                json!({
                    "TableName": self.table(),
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

    /// Returns an album's caption from the RAM cache, then DynamoDB.
    pub async fn get_group_caption(&self, group_id: &str) -> Option<String> {
        let now = now();
        {
            let mut cache = GROUP_CAPTION_CACHE.write().await;
            cache.retain(|_, (_, expiry)| *expiry >= now);
            if let Some((caption, _)) = cache.get(group_id) {
                return Some(caption.clone());
            }
        }
        if !self.dynamodb_available() {
            return None;
        }
        let response = self
            .aws
            .post_json(
                "dynamodb",
                "DynamoDB_20120810.GetItem",
                json!({
                    "TableName": self.table(),
                    "Key": {
                        "pk": {"S": format!("MEDIAGROUP#{group_id}")},
                        "sk": {"S": "CAPTION"},
                    },
                }),
            )
            .await
            .ok()?;
        let item = response.get("Item")?;
        if attr_i64(item, "expires_at").is_some_and(|expiry| expiry < now) {
            return None;
        }
        let caption = attr_string(item, "caption")?;
        GROUP_CAPTION_CACHE.write().await.insert(
            group_id.to_string(),
            (caption.clone(), now + GROUP_CAPTION_TTL_SECONDS),
        );
        Some(caption)
    }

    /// Reserves an idempotency key, returning false when it was already seen.
    pub async fn reserve_idempotency(&self, key: &str, retention_seconds: i64) -> Result<bool> {
        let now = now();
        let expires_at = now.saturating_add(retention_seconds.max(1));
        if !self.dynamodb_available() {
            return reserve_in_ram(key, now, expires_at).await;
        }
        let result = self
            .aws
            .post_json(
                "dynamodb",
                "DynamoDB_20120810.PutItem",
                json!({
                    "TableName": self.table(),
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

    /// Scans the table to total onboarded profiles and uploads (admin `/stat`).
    pub async fn aggregate_stats(&self) -> Result<UploadStats> {
        if !self.dynamodb_available() {
            return Ok(UploadStats::default());
        }
        let mut stats = UploadStats::default();
        let mut start_key: Option<Value> = None;
        loop {
            let mut request = json!({
                "TableName": self.table(),
                "FilterExpression": "sk = :profile",
                "ExpressionAttributeValues": {":profile": {"S": "PROFILE"}},
                "ProjectionExpression": "uploads_count",
            });
            if let Some(key) = &start_key {
                request["ExclusiveStartKey"] = key.clone();
            }
            let response = self
                .aws
                .post_json("dynamodb", "DynamoDB_20120810.Scan", request)
                .await?;
            if let Some(items) = response.get("Items").and_then(Value::as_array) {
                for item in items {
                    stats.users += 1;
                    stats.uploads += attr_i64(item, "uploads_count").unwrap_or(0).max(0) as u64;
                }
            }
            match response.get("LastEvaluatedKey") {
                Some(key) if !key.is_null() => start_key = Some(key.clone()),
                _ => break,
            }
        }
        Ok(stats)
    }
}

/// Aggregate upload statistics for the admin `/stat` command.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct UploadStats {
    /// Number of onboarded user profiles.
    pub users: u64,
    /// Sum of successful uploads across all users.
    pub uploads: u64,
}

/// Returns the current unix timestamp in seconds.
fn now() -> i64 {
    OffsetDateTime::now_utc().unix_timestamp()
}

/// Reserves a key in the warm Lambda RAM cache.
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
    item
}

/// Converts a DynamoDB item into a profile.
fn item_to_profile(item: &Value) -> Profile {
    Profile {
        commons_username: attr_string(item, "commons_username"),
        credential_ciphertext: attr_string(item, "credential_ciphertext"),
        license: attr_string(item, "license")
            .and_then(|value| License::parse(&value))
            .unwrap_or_default(),
        filename_prefix: attr_string(item, "filename_prefix").unwrap_or_default(),
        onboarding_step: attr_string(item, "onboarding_step")
            .and_then(|value| OnboardingStep::parse(&value))
            .unwrap_or_default(),
        default_categories: attr_string_list(item, "default_categories"),
        return_upload_links: attr_bool(item, "return_upload_links").unwrap_or(false),
        return_category_links: attr_bool(item, "return_category_links").unwrap_or(false),
        return_missing_category_links: attr_bool(item, "return_missing_category_links")
            .unwrap_or(false),
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
            license: License::Cc0,
            filename_prefix: "Minsk trip".into(),
            onboarding_step: OnboardingStep::Done,
            default_categories: vec!["Minsk".into(), "Belarus".into()],
            return_upload_links: true,
            return_category_links: true,
            return_missing_category_links: false,
            uploads_count: 12,
            created_at: 1_700_000_000,
            updated_at: 1_700_000_500,
        };
        let item = profile_to_item(42, &profile);
        assert_eq!(item["pk"]["S"], "USER#42");
        assert_eq!(item["sk"]["S"], "PROFILE");
        assert_eq!(item_to_profile(&item), profile);
    }

    #[test]
    fn missing_optional_fields_default_safely() {
        let item = profile_to_item(1, &Profile::default());
        let profile = item_to_profile(&item);
        assert_eq!(profile.commons_username, None);
        assert_eq!(profile.credential_ciphertext, None);
        assert_eq!(profile.license, License::CcBy40);
        assert_eq!(profile.onboarding_step, OnboardingStep::AwaitingUsername);
        assert!(!profile.is_ready());
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
}
