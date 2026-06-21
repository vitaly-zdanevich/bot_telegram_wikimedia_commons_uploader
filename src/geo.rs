//! Parses geographic coordinates from map links (Google, Yandex, 2GIS, OpenStreetMap)
//! and from plain `lat, lon` text.

/// Parses `(latitude, longitude)` from a map URL or plain `lat, lon` text.
pub fn parse_coordinates(input: &str) -> Option<(f64, f64)> {
    let trimmed = input.trim();
    if (trimmed.starts_with("http://") || trimmed.starts_with("https://"))
        && let Some(coords) = parse_map_url(trimmed)
    {
        return Some(coords);
    }
    parse_lat_lon(trimmed)
}

/// Validates latitude/longitude ranges.
fn valid(lat: f64, lon: f64) -> Option<(f64, f64)> {
    ((-90.0..=90.0).contains(&lat) && (-180.0..=180.0).contains(&lon)).then_some((lat, lon))
}

/// Parses plain `lat, lon` / `lat lon` / `lat; lon`.
fn parse_lat_lon(text: &str) -> Option<(f64, f64)> {
    let numbers: Vec<f64> = text
        .split(|c: char| c == ',' || c == ';' || c.is_whitespace())
        .filter(|part| !part.is_empty())
        .map(str::parse::<f64>)
        .collect::<Result<Vec<_>, _>>()
        .ok()?;
    match numbers.as_slice() {
        [lat, lon] => valid(*lat, *lon),
        _ => None,
    }
}

/// Extracts coordinates from a known map-service URL.
fn parse_map_url(url: &str) -> Option<(f64, f64)> {
    let lower = url.to_ascii_lowercase();

    // OpenStreetMap explicit `?mlat=&mlon=`.
    if let (Some(lat), Some(lon)) = (param_value(&lower, "mlat"), param_value(&lower, "mlon"))
        && let Some(coords) = valid(lat, lon)
    {
        return Some(coords);
    }
    // OpenStreetMap `#map=zoom/lat/lon`.
    if let Some(index) = lower.find("#map=") {
        let segments: Vec<&str> = url[index + 5..].split('/').collect();
        if let [_, lat, lon, ..] = segments.as_slice()
            && let (Some(lat), Some(lon)) = (leading_f64(lat), leading_f64(lon))
            && let Some(coords) = valid(lat, lon)
        {
            return Some(coords);
        }
    }

    // Yandex and 2GIS put longitude first in `ll=`/`m=`.
    let lon_first = lower.contains("yandex.") || lower.contains("2gis.");
    for key in ["ll=", "m=", "q=", "query=", "center="] {
        if let Some((first, second)) = pair_after(&lower, key) {
            let (lat, lon) = if lon_first {
                (second, first)
            } else {
                (first, second)
            };
            if let Some(coords) = valid(lat, lon) {
                return Some(coords);
            }
        }
    }

    // Google `/@lat,lon`.
    if let Some(index) = url.find("/@")
        && let Some((lat, lon)) = pair_at(&url[index + 2..])
        && let Some(coords) = valid(lat, lon)
    {
        return Some(coords);
    }
    // Google `!3dLAT!4dLON`.
    if let (Some(lat), Some(lon)) = (marker_f64(url, "!3d"), marker_f64(url, "!4d"))
        && let Some(coords) = valid(lat, lon)
    {
        return Some(coords);
    }
    None
}

/// Reads a `key=<number>` query-parameter value.
fn param_value(lower: &str, key: &str) -> Option<f64> {
    let needle = format!("{key}=");
    let start = lower.find(&needle)? + needle.len();
    leading_f64(&lower[start..])
}

/// Reads the `A,B` number pair right after `key` (which already includes `=`).
fn pair_after(haystack: &str, key: &str) -> Option<(f64, f64)> {
    let start = haystack.find(key)? + key.len();
    pair_at(&haystack[start..])
}

/// Parses a leading `A,B` number pair.
fn pair_at(text: &str) -> Option<(f64, f64)> {
    let mut parts = text.split(',');
    let first = leading_f64(parts.next()?)?;
    let second = leading_f64(parts.next()?)?;
    Some((first, second))
}

/// Reads a number that follows `marker` in `url`.
fn marker_f64(url: &str, marker: &str) -> Option<f64> {
    let start = url.find(marker)? + marker.len();
    leading_f64(&url[start..])
}

/// Parses the leading numeric portion of a string (ignores trailing characters).
fn leading_f64(text: &str) -> Option<f64> {
    let text = text.trim();
    let end = text
        .find(|c: char| !(c.is_ascii_digit() || c == '.' || c == '-' || c == '+'))
        .unwrap_or(text.len());
    text[..end].parse().ok()
}

#[cfg(test)]
mod tests {
    use super::parse_coordinates;

    #[test]
    fn plain_lat_lon() {
        assert_eq!(parse_coordinates("55.75, 37.61"), Some((55.75, 37.61)));
        assert_eq!(parse_coordinates("55.75 37.61"), Some((55.75, 37.61)));
        assert_eq!(parse_coordinates("not coords"), None);
        assert_eq!(parse_coordinates("200, 0"), None);
    }

    #[test]
    fn google_links() {
        assert_eq!(
            parse_coordinates("https://www.google.com/maps/@55.75,37.61,15z"),
            Some((55.75, 37.61))
        );
        assert_eq!(
            parse_coordinates("https://maps.google.com/?q=55.75,37.61"),
            Some((55.75, 37.61))
        );
    }

    #[test]
    fn openstreetmap_links() {
        assert_eq!(
            parse_coordinates("https://www.openstreetmap.org/?mlat=55.75&mlon=37.61"),
            Some((55.75, 37.61))
        );
        assert_eq!(
            parse_coordinates("https://www.openstreetmap.org/#map=15/55.75/37.61"),
            Some((55.75, 37.61))
        );
    }

    #[test]
    fn yandex_and_2gis_are_lon_first() {
        assert_eq!(
            parse_coordinates("https://yandex.ru/maps/?ll=37.61,55.75&z=15"),
            Some((55.75, 37.61))
        );
        assert_eq!(
            parse_coordinates("https://2gis.ru/moscow?m=37.61,55.75/15"),
            Some((55.75, 37.61))
        );
    }
}
