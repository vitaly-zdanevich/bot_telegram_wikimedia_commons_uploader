//! Finds nearby Commons categories from Wikidata coordinates and P373 values.

use anyhow::Context;
use reqwest::header::{ACCEPT, USER_AGENT};
use serde::Deserialize;
use std::collections::HashSet;

/// Geographic coordinate pair in decimal degrees.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Coordinates {
    /// Latitude in decimal degrees.
    pub latitude: f64,
    /// Longitude in decimal degrees.
    pub longitude: f64,
}

impl Coordinates {
    /// Returns validated coordinates.
    pub fn new(latitude: f64, longitude: f64) -> Option<Self> {
        let valid_latitude = latitude.is_finite() && (-90.0..=90.0).contains(&latitude);
        let valid_longitude = longitude.is_finite() && (-180.0..=180.0).contains(&longitude);
        (valid_latitude && valid_longitude).then_some(Self {
            latitude,
            longitude,
        })
    }
}

/// One nearby Commons category candidate.
#[derive(Clone, Debug, PartialEq)]
pub struct NearbyCategory {
    /// Commons category name without the `Category:` namespace prefix.
    pub category: String,
    /// Nearby Wikidata item label, useful context when different from the category.
    pub label: Option<String>,
    /// Distance from the user-provided point, in kilometers.
    pub distance_km: Option<f64>,
}

/// Client for Wikidata nearby category lookups.
#[derive(Clone)]
pub struct NearbyCategoryClient {
    client: reqwest::Client,
    user_agent: String,
    sparql_url: String,
    radius_meters: u32,
    limit: u32,
}

impl NearbyCategoryClient {
    /// Creates a client for nearby category lookups.
    pub fn new(
        user_agent: impl Into<String>,
        sparql_url: impl Into<String>,
        radius_meters: u32,
        limit: u32,
    ) -> Self {
        Self {
            client: reqwest::Client::new(),
            user_agent: user_agent.into(),
            sparql_url: sparql_url.into(),
            radius_meters,
            limit,
        }
    }

    /// Returns nearby Commons categories, ordered by distance from the point.
    pub async fn nearby_categories(
        &self,
        coordinates: Coordinates,
    ) -> anyhow::Result<Vec<NearbyCategory>> {
        let query = self.wikidata_query(coordinates);
        let response = self
            .client
            .get(&self.sparql_url)
            .header(USER_AGENT, &self.user_agent)
            .header("Api-User-Agent", &self.user_agent)
            .header(ACCEPT, "application/sparql-results+json")
            .query(&[("query", query.as_str()), ("format", "json")])
            .send()
            .await
            .context("Wikidata nearby Commons category request failed")?
            .error_for_status()
            .context("Wikidata nearby Commons category request returned an error status")?
            .json::<SparqlResponse>()
            .await
            .context("failed to parse Wikidata nearby Commons category response")?;

        Ok(response.into_categories(self.limit))
    }

    /// Builds the SPARQL query for nearby Wikidata items with Commons categories.
    fn wikidata_query(&self, coordinates: Coordinates) -> String {
        let longitude = decimal(coordinates.longitude);
        let latitude = decimal(coordinates.latitude);
        let radius_km = decimal(self.radius_meters as f64 / 1000.0);
        let item_limit = self.limit.saturating_mul(4).max(self.limit);
        format!(
            r#"SELECT ?item ?itemLabel ?commonsCategory ?distance WHERE {{
	SERVICE wikibase:around {{
		?item wdt:P625 ?location .
		bd:serviceParam wikibase:center "Point({longitude} {latitude})"^^geo:wktLiteral .
		bd:serviceParam wikibase:radius "{radius_km}" .
		bd:serviceParam wikibase:distance ?distance .
	}}
	?item wdt:P373 ?commonsCategory .
	SERVICE wikibase:label {{ bd:serviceParam wikibase:language "en,ru,be,uk,pl,de,fr,mul" . }}
}}
ORDER BY ASC(?distance)
LIMIT {item_limit}"#
        )
    }
}

#[derive(Debug, Deserialize)]
struct SparqlResponse {
    results: SparqlResults,
}

impl SparqlResponse {
    /// Converts SPARQL bindings into de-duplicated category candidates.
    fn into_categories(self, limit: u32) -> Vec<NearbyCategory> {
        let mut seen = HashSet::new();
        let mut categories = self
            .results
            .bindings
            .into_iter()
            .filter_map(SparqlBinding::into_category)
            .filter(|candidate| seen.insert(candidate.category.clone()))
            .collect::<Vec<_>>();
        categories
            .sort_by(|left, right| optional_distance_order(left.distance_km, right.distance_km));
        categories.truncate(limit as usize);
        categories
    }
}

#[derive(Debug, Deserialize)]
struct SparqlResults {
    bindings: Vec<SparqlBinding>,
}

#[derive(Debug, Deserialize)]
struct SparqlBinding {
    #[serde(rename = "itemLabel")]
    item_label: Option<SparqlValue>,
    #[serde(rename = "commonsCategory")]
    commons_category: SparqlValue,
    distance: Option<SparqlValue>,
}

impl SparqlBinding {
    /// Converts one SPARQL binding into a nearby Commons category.
    fn into_category(self) -> Option<NearbyCategory> {
        let category = self.commons_category.value.trim();
        if category.is_empty() {
            return None;
        }
        Some(NearbyCategory {
            category: category.to_string(),
            label: self
                .item_label
                .map(|value| value.value.trim().to_string())
                .filter(|value| !value.is_empty() && value != category),
            distance_km: self
                .distance
                .and_then(|value| value.value.parse::<f64>().ok()),
        })
    }
}

#[derive(Debug, Deserialize)]
struct SparqlValue {
    value: String,
}

/// Formats a finite decimal for SPARQL without unnecessary trailing zeroes.
fn decimal(value: f64) -> String {
    let mut text = format!("{value:.6}");
    while text.contains('.') && text.ends_with('0') {
        text.pop();
    }
    if text.ends_with('.') {
        text.pop();
    }
    text
}

/// Orders optional distances, keeping unknown distances last.
fn optional_distance_order(left: Option<f64>, right: Option<f64>) -> std::cmp::Ordering {
    match (left, right) {
        (Some(left), Some(right)) => left
            .partial_cmp(&right)
            .unwrap_or(std::cmp::Ordering::Equal),
        (Some(_), None) => std::cmp::Ordering::Less,
        (None, Some(_)) => std::cmp::Ordering::Greater,
        (None, None) => std::cmp::Ordering::Equal,
    }
}

#[cfg(test)]
mod tests {
    use super::{Coordinates, NearbyCategoryClient, SparqlResponse};

    #[test]
    fn validates_coordinates() {
        assert!(Coordinates::new(53.9, 27.5667).is_some());
        assert!(Coordinates::new(91.0, 27.5667).is_none());
        assert!(Coordinates::new(53.9, 181.0).is_none());
    }

    #[test]
    fn query_uses_longitude_then_latitude() {
        let client =
            NearbyCategoryClient::new("ua", "https://query.wikidata.org/sparql", 10_000, 8);
        let query = client.wikidata_query(Coordinates::new(53.9, 27.5667).unwrap());

        assert!(query.contains("Point(27.5667 53.9)"));
        assert!(query.contains("?item wdt:P373 ?commonsCategory"));
        assert!(query.contains("wikibase:radius \"10\""));
    }

    #[test]
    fn parses_and_deduplicates_sparql_categories() {
        let response: SparqlResponse = serde_json::from_str(
            r#"{
                "results": {
                    "bindings": [
                        {
                            "itemLabel": {"type": "literal", "value": "Minsk"},
                            "commonsCategory": {"type": "literal", "value": "Minsk"},
                            "distance": {"type": "literal", "value": "0.4"}
                        },
                        {
                            "itemLabel": {"type": "literal", "value": "Minsk again"},
                            "commonsCategory": {"type": "literal", "value": "Minsk"},
                            "distance": {"type": "literal", "value": "0.5"}
                        },
                        {
                            "itemLabel": {"type": "literal", "value": "Church"},
                            "commonsCategory": {"type": "literal", "value": "Churches in Minsk"},
                            "distance": {"type": "literal", "value": "0.2"}
                        }
                    ]
                }
            }"#,
        )
        .unwrap();

        let categories = response.into_categories(10);

        assert_eq!(
            categories
                .iter()
                .map(|item| item.category.as_str())
                .collect::<Vec<_>>(),
            vec!["Churches in Minsk", "Minsk"]
        );
        assert_eq!(categories[0].label.as_deref(), Some("Church"));
    }
}
