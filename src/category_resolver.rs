//! Resolves wiki links and Wikidata item ids to Commons categories.

use anyhow::Context;
use reqwest::header::{ACCEPT, USER_AGENT};
use serde_json::Value;
use std::collections::HashSet;
use url::Url;

const WIKIDATA_API_URL: &str = "https://www.wikidata.org/w/api.php";

/// A user-provided reference that can imply Commons categories.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CategoryReference {
    /// A Wikidata item id such as `Q42`.
    WikidataItem { qid: String },
    /// A Wikimedia Commons page title such as `Category:Minsk` or `File:Example.jpg`.
    CommonsPage { title: String },
    /// A Wikipedia article title and matching Action API endpoint.
    WikipediaPage {
        api_url: String,
        title: String,
        project_label: String,
    },
}

impl CategoryReference {
    /// Parses a reference from an exact `Q123` text value.
    pub fn from_qid_text(text: &str) -> Option<Self> {
        let qid = normalize_qid(text.trim_matches(|ch: char| {
            matches!(
                ch,
                '<' | '>' | '"' | '\'' | '`' | '(' | ')' | '[' | ']' | '{' | '}'
            ) || ch.is_ascii_punctuation() && ch != 'Q' && ch != 'q'
        }))?;
        Some(Self::WikidataItem { qid })
    }

    /// Parses a reference from a supported wiki URL.
    pub fn from_url(url: &Url) -> Option<Self> {
        let host = url.host_str()?.to_ascii_lowercase();
        if host == "www.wikidata.org" || host == "wikidata.org" {
            let qid = wikidata_qid_from_url(url)?;
            return Some(Self::WikidataItem { qid });
        }
        if host == "commons.wikimedia.org" || host == "commons.m.wikimedia.org" {
            let title = wiki_title_from_url(url)?;
            return Some(Self::CommonsPage { title });
        }
        if wikipedia_host(&host) {
            let title = wiki_title_from_url(url)?;
            let api_host = host.replace(".m.wikipedia.org", ".wikipedia.org");
            return Some(Self::WikipediaPage {
                api_url: format!("https://{api_host}/w/api.php"),
                title,
                project_label: api_host,
            });
        }
        None
    }

    /// Human-readable label for a Telegram reply.
    pub fn label(&self) -> String {
        match self {
            CategoryReference::WikidataItem { qid } => format!("Wikidata item {qid}"),
            CategoryReference::CommonsPage { title } => format!("Commons page {title}"),
            CategoryReference::WikipediaPage {
                title,
                project_label,
                ..
            } => format!("{project_label} article {title}"),
        }
    }
}

/// Result of resolving a link or item to Commons categories.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ResolvedCategories {
    /// Short source label for a user-facing reply.
    pub source_label: String,
    /// Commons category names without `Category:`.
    pub categories: Vec<String>,
}

/// HTTP client for MediaWiki/Wikidata category resolution.
#[derive(Clone)]
pub struct CategoryResolver {
    client: reqwest::Client,
    user_agent: String,
    commons_api_url: String,
    max_categories: usize,
}

impl CategoryResolver {
    /// Creates a resolver for article/QID/Commons category links.
    pub fn new(
        user_agent: impl Into<String>,
        commons_api_url: impl Into<String>,
        max_categories: usize,
    ) -> Self {
        Self {
            client: reqwest::Client::new(),
            user_agent: user_agent.into(),
            commons_api_url: commons_api_url.into(),
            max_categories,
        }
    }

    /// Resolves a user reference to zero or more Commons categories.
    pub async fn categories_for_reference(
        &self,
        reference: &CategoryReference,
    ) -> anyhow::Result<ResolvedCategories> {
        let mut categories = Vec::new();
        match reference {
            CategoryReference::WikidataItem { qid } => {
                self.categories_from_wikidata_item(qid, &mut categories)
                    .await?;
            }
            CategoryReference::CommonsPage { title } => {
                self.categories_from_commons_page(title, &mut categories)
                    .await?;
            }
            CategoryReference::WikipediaPage { api_url, title, .. } => {
                if let Some(qid) = self.wikidata_item_from_page(api_url, title).await? {
                    self.categories_from_wikidata_item(&qid, &mut categories)
                        .await?;
                }
            }
        }
        Ok(ResolvedCategories {
            source_label: reference.label(),
            categories: limit_categories(categories, self.max_categories),
        })
    }

    /// Resolves a Wikidata item through P373, Commons sitelinks, and P910 main-category items.
    async fn categories_from_wikidata_item(
        &self,
        qid: &str,
        categories: &mut Vec<String>,
    ) -> anyhow::Result<()> {
        let entity = self.wikidata_entity(qid).await?;
        append_unique_categories(categories, &categories_from_wikidata_entity(&entity));
        let main_category_qids = entity_claim_entity_ids(&entity, "P910");
        for main_category_qid in main_category_qids.into_iter().take(4) {
            let entity = self.wikidata_entity(&main_category_qid).await?;
            append_unique_categories(categories, &categories_from_wikidata_entity(&entity));
            if categories.len() >= self.max_categories {
                break;
            }
        }
        Ok(())
    }

    /// Reads one Wikidata entity JSON object.
    async fn wikidata_entity(&self, qid: &str) -> anyhow::Result<Value> {
        let value = self
            .get_json(
                WIKIDATA_API_URL,
                &[
                    ("action", "wbgetentities"),
                    ("ids", qid),
                    ("props", "claims|sitelinks"),
                    ("format", "json"),
                ],
            )
            .await
            .with_context(|| format!("failed to fetch Wikidata entity {qid}"))?;
        value
            .get("entities")
            .and_then(|entities| entities.get(qid))
            .cloned()
            .with_context(|| format!("Wikidata response is missing entity {qid}"))
    }

    /// Resolves a wiki page to its Wikidata item id via `pageprops.wikibase_item`.
    async fn wikidata_item_from_page(
        &self,
        api_url: &str,
        title: &str,
    ) -> anyhow::Result<Option<String>> {
        let value = self
            .get_json(
                api_url,
                &[
                    ("action", "query"),
                    ("prop", "pageprops"),
                    ("titles", title),
                    ("redirects", "1"),
                    ("format", "json"),
                    ("formatversion", "2"),
                ],
            )
            .await
            .with_context(|| format!("failed to fetch pageprops for {title}"))?;
        Ok(value
            .pointer("/query/pages/0/pageprops/wikibase_item")
            .and_then(Value::as_str)
            .and_then(normalize_qid))
    }

    /// Resolves categories directly from a Commons page and its linked Wikidata item.
    async fn categories_from_commons_page(
        &self,
        title: &str,
        categories: &mut Vec<String>,
    ) -> anyhow::Result<()> {
        if let Some(category) = category_from_title(title) {
            append_unique_category(categories, &category);
            return Ok(());
        }

        let value = self
            .get_json(
                &self.commons_api_url,
                &[
                    ("action", "query"),
                    ("prop", "pageprops|categories"),
                    ("titles", title),
                    ("cllimit", "max"),
                    ("clshow", "!hidden"),
                    ("redirects", "1"),
                    ("format", "json"),
                    ("formatversion", "2"),
                ],
            )
            .await
            .with_context(|| format!("failed to fetch Commons page categories for {title}"))?;

        if let Some(page) = value.pointer("/query/pages/0") {
            append_unique_categories(categories, &categories_from_commons_page_json(page));
            if let Some(qid) = page
                .pointer("/pageprops/wikibase_item")
                .and_then(Value::as_str)
                .and_then(normalize_qid)
            {
                self.categories_from_wikidata_item(&qid, categories).await?;
            }
        }
        Ok(())
    }

    /// Performs one JSON GET request to a MediaWiki API.
    async fn get_json(&self, api_url: &str, params: &[(&str, &str)]) -> anyhow::Result<Value> {
        self.client
            .get(api_url)
            .header(USER_AGENT, &self.user_agent)
            .header("Api-User-Agent", &self.user_agent)
            .header(ACCEPT, "application/json")
            .query(params)
            .send()
            .await
            .context("MediaWiki category lookup request failed")?
            .error_for_status()
            .context("MediaWiki category lookup returned an error status")?
            .json::<Value>()
            .await
            .context("failed to parse MediaWiki category lookup response")
    }
}

/// Returns true when a URL can be resolved to category suggestions.
pub fn category_reference_looks_supported(url: &Url) -> bool {
    CategoryReference::from_url(url).is_some()
}

/// Parses a wiki title from `/wiki/Title` or `?title=Title`.
fn wiki_title_from_url(url: &Url) -> Option<String> {
    if let Some((_, title)) = url.path().split_once("/wiki/") {
        return decode_wiki_title(title);
    }
    url.query_pairs()
        .find(|(key, _)| key == "title")
        .and_then(|(_, title)| decode_wiki_title(&title))
}

/// Parses a Wikidata QID from supported Wikidata URL forms.
fn wikidata_qid_from_url(url: &Url) -> Option<String> {
    let title = wiki_title_from_url(url)?;
    title
        .rsplit('/')
        .next()
        .and_then(|part| {
            part.split_once('#')
                .map(|(before, _)| before)
                .or(Some(part))
        })
        .and_then(normalize_qid)
}

/// Decodes a MediaWiki title path/query component.
fn decode_wiki_title(raw: &str) -> Option<String> {
    let title = raw.split('#').next().unwrap_or(raw);
    let decoded = urlencoding::decode(title).ok()?.replace('_', " ");
    let decoded = decoded.trim();
    (!decoded.is_empty()).then(|| decoded.to_string())
}

/// Returns true for desktop or mobile Wikipedia hosts.
fn wikipedia_host(host: &str) -> bool {
    host.ends_with(".wikipedia.org")
        || host.ends_with(".m.wikipedia.org")
        || host == "wikipedia.org"
        || host == "www.wikipedia.org"
}

/// Normalizes a QID and rejects non-item text.
fn normalize_qid(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    let rest = trimmed
        .strip_prefix('Q')
        .or_else(|| trimmed.strip_prefix('q'))?;
    (!rest.is_empty() && rest.chars().all(|ch| ch.is_ascii_digit())).then(|| format!("Q{rest}"))
}

/// Extracts categories from P373 and Commons category sitelinks.
fn categories_from_wikidata_entity(entity: &Value) -> Vec<String> {
    let mut categories = Vec::new();
    for category in entity_claim_strings(entity, "P373") {
        append_unique_category(&mut categories, &category);
    }
    if let Some(title) = entity
        .pointer("/sitelinks/commonswiki/title")
        .and_then(Value::as_str)
        && let Some(category) = category_from_title(title)
    {
        append_unique_category(&mut categories, &category);
    }
    categories
}

/// Extracts string-valued Wikidata claims.
fn entity_claim_strings(entity: &Value, property: &str) -> Vec<String> {
    entity
        .pointer(&format!("/claims/{property}"))
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|claim| {
            claim
                .pointer("/mainsnak/datavalue/value")
                .and_then(Value::as_str)
        })
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
        .collect()
}

/// Extracts entity-id claims such as P910.
fn entity_claim_entity_ids(entity: &Value, property: &str) -> Vec<String> {
    entity
        .pointer(&format!("/claims/{property}"))
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|claim| {
            if let Some(id) = claim
                .pointer("/mainsnak/datavalue/value/id")
                .and_then(Value::as_str)
                .and_then(normalize_qid)
            {
                return Some(id);
            }
            claim
                .pointer("/mainsnak/datavalue/value/numeric-id")
                .and_then(Value::as_u64)
                .map(|id| format!("Q{id}"))
        })
        .collect()
}

/// Extracts visible category links from a Commons page JSON object.
fn categories_from_commons_page_json(page: &Value) -> Vec<String> {
    page.pointer("/categories")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|category| category.get("title").and_then(Value::as_str))
        .filter_map(category_from_title)
        .collect()
}

/// Converts a `Category:Name` title to a bare category name.
fn category_from_title(title: &str) -> Option<String> {
    title
        .trim()
        .strip_prefix("Category:")
        .map(crate::commons::sanitize_title)
        .filter(|category| !category.is_empty())
}

/// Appends categories while preserving insertion order.
fn append_unique_categories(target: &mut Vec<String>, categories: &[String]) {
    for category in categories {
        append_unique_category(target, category);
    }
}

/// Appends one category when not already present.
fn append_unique_category(target: &mut Vec<String>, category: &str) {
    let category = crate::commons::sanitize_title(category);
    if !category.is_empty() && !target.contains(&category) {
        target.push(category);
    }
}

/// Removes duplicates and applies the configured display limit.
fn limit_categories(categories: Vec<String>, max_categories: usize) -> Vec<String> {
    let mut seen = HashSet::new();
    categories
        .into_iter()
        .filter(|category| seen.insert(category.clone()))
        .take(max_categories)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::{
        CategoryReference, categories_from_commons_page_json, categories_from_wikidata_entity,
        category_reference_looks_supported, entity_claim_entity_ids,
    };
    use serde_json::json;
    use url::Url;

    #[test]
    fn parses_wikidata_qids_and_links() {
        assert_eq!(
            CategoryReference::from_qid_text("Q42"),
            Some(CategoryReference::WikidataItem { qid: "Q42".into() })
        );
        assert_eq!(CategoryReference::from_qid_text("Q42 text"), None);
        let url =
            Url::parse("https://www.wikidata.org/wiki/Special:EntityPage/Q140382791").unwrap();
        assert_eq!(
            CategoryReference::from_url(&url),
            Some(CategoryReference::WikidataItem {
                qid: "Q140382791".into()
            })
        );
    }

    #[test]
    fn parses_commons_and_wikipedia_links() {
        let commons =
            Url::parse("https://commons.wikimedia.org/wiki/Category:Churches_in_Minsk").unwrap();
        assert_eq!(
            CategoryReference::from_url(&commons),
            Some(CategoryReference::CommonsPage {
                title: "Category:Churches in Minsk".into()
            })
        );
        let wikipedia = Url::parse("https://en.m.wikipedia.org/wiki/Minsk?oldformat=true").unwrap();
        assert_eq!(
            CategoryReference::from_url(&wikipedia),
            Some(CategoryReference::WikipediaPage {
                api_url: "https://en.wikipedia.org/w/api.php".into(),
                title: "Minsk".into(),
                project_label: "en.wikipedia.org".into(),
            })
        );
        assert!(category_reference_looks_supported(&wikipedia));
    }

    #[test]
    fn extracts_categories_from_wikidata_entity_json() {
        let entity = json!({
            "claims": {
                "P373": [{
                    "mainsnak": {"datavalue": {"value": "Minsk"}}
                }],
                "P910": [{
                    "mainsnak": {"datavalue": {"value": {"id": "Q123"}}}
                }]
            },
            "sitelinks": {
                "commonswiki": {"title": "Category:Minsk buildings"}
            }
        });

        assert_eq!(
            categories_from_wikidata_entity(&entity),
            vec!["Minsk", "Minsk buildings"]
        );
        assert_eq!(entity_claim_entity_ids(&entity, "P910"), vec!["Q123"]);
    }

    #[test]
    fn extracts_categories_from_commons_page_json() {
        let page = json!({
            "categories": [
                {"title": "Category:Minsk"},
                {"title": "Category:Churches in Minsk"}
            ]
        });

        assert_eq!(
            categories_from_commons_page_json(&page),
            vec!["Minsk", "Churches in Minsk"]
        );
    }
}
