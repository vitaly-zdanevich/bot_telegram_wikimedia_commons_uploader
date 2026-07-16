//! Parses geographic coordinates from map links and from plain `lat, lon` text.

#[derive(Clone, Copy)]
enum CoordinateSystem {
    Wgs84,
    Gcj02,
    Bd09,
}

/// Parses `(latitude, longitude)` from a map URL or plain `lat, lon` text.
pub fn parse_coordinates(input: &str) -> Option<(f64, f64)> {
    let trimmed = input.trim();
    if (trimmed.starts_with("http://") || trimmed.starts_with("https://"))
        && let Some(coords) = parse_map_url(trimmed)
    {
        return Some(coords);
    }
    if trimmed
        .get(..4)
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case("geo:"))
        && let Some(coords) = pair_at(&trimmed[4..]).and_then(|(lat, lon)| valid(lat, lon))
    {
        return Some(coords);
    }
    parse_lat_lon(trimmed)
}

/// Validates latitude/longitude ranges.
fn valid(lat: f64, lon: f64) -> Option<(f64, f64)> {
    ((-90.0..=90.0).contains(&lat) && (-180.0..=180.0).contains(&lon)).then_some((lat, lon))
}

/// Validates and converts provider-native coordinates to WGS84.
fn normalize_valid(lat: f64, lon: f64, coordinate_system: CoordinateSystem) -> Option<(f64, f64)> {
    let (lat, lon) = match coordinate_system {
        CoordinateSystem::Wgs84 => (lat, lon),
        CoordinateSystem::Gcj02 if outside_mainland_china(lat, lon) => (lat, lon),
        CoordinateSystem::Gcj02 => lon_lat_to_lat_lon(coordtransform::gcj02_to_wgs84(lon, lat)),
        CoordinateSystem::Bd09 if outside_mainland_china(lat, lon) => (lat, lon),
        CoordinateSystem::Bd09 => lon_lat_to_lat_lon(coordtransform::bd09_to_wgs84(lon, lat)),
    };
    valid(lat, lon)
}

/// Returns true where Chinese map-provider coordinate offsets do not apply.
fn outside_mainland_china(lat: f64, lon: f64) -> bool {
    !(72.004..=137.8347).contains(&lon) || !(0.8293..=55.8271).contains(&lat)
}

/// Converts crate-style `(longitude, latitude)` into bot-style `(latitude, longitude)`.
fn lon_lat_to_lat_lon((lon, lat): (f64, f64)) -> (f64, f64) {
    (lat, lon)
}

/// Returns the coordinate system commonly used by the map provider.
fn coordinate_system_for_url(lower: &str) -> CoordinateSystem {
    if lower.contains("baidu.") {
        CoordinateSystem::Bd09
    } else if lower.contains("amap.")
        || lower.contains("autonavi.")
        || lower.contains("gaode.")
        || lower.contains("map.qq.")
        || lower.contains("maps.qq.")
        || lower.contains("sogou.")
    {
        CoordinateSystem::Gcj02
    } else {
        CoordinateSystem::Wgs84
    }
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
    let decoded = urlencoding::decode(url)
        .map(|value| value.into_owned())
        .unwrap_or_else(|_| url.to_string());
    let lower = decoded.to_ascii_lowercase();
    let coordinate_system = coordinate_system_for_url(&lower);
    let baidu = matches!(coordinate_system, CoordinateSystem::Bd09);

    // OpenStreetMap explicit `?mlat=&mlon=`.
    if let (Some(lat), Some(lon)) = (param_value(&lower, "mlat"), param_value(&lower, "mlon"))
        && let Some(coords) = normalize_valid(lat, lon, CoordinateSystem::Wgs84)
    {
        return Some(coords);
    }
    // Explicit latitude/longitude parameters used by many map providers and embed widgets.
    for (lat_key, lon_key) in [
        ("lat", "lon"),
        ("lat", "lng"),
        ("latitude", "longitude"),
        ("marker_lat", "marker_lon"),
    ] {
        if let (Some(lat), Some(lon)) = (param_value(&lower, lat_key), param_value(&lower, lon_key))
            && let Some(coords) = normalize_valid(lat, lon, coordinate_system)
        {
            return Some(coords);
        }
    }
    // Wikimapia often uses `#lat=&lon=`.
    if let (Some(lat), Some(lon)) = (
        fragment_param_value(&lower, "lat"),
        fragment_param_value(&lower, "lon"),
    ) && let Some(coords) = normalize_valid(lat, lon, CoordinateSystem::Wgs84)
    {
        return Some(coords);
    }
    // OpenStreetMap `#map=zoom/lat/lon`.
    if let Some(index) = lower.find("#map=") {
        let segments: Vec<&str> = decoded[index + 5..].split('/').collect();
        if let [_, lat, lon, ..] = segments.as_slice()
            && let (Some(lat), Some(lon)) = (leading_f64(lat), leading_f64(lon))
            && let Some(coords) = normalize_valid(lat, lon, CoordinateSystem::Wgs84)
        {
            return Some(coords);
        }
    }
    // GeoHack links commonly encode decimal coordinates in the `params=` value.
    if lower.contains("geohack.toolforge.org")
        && let Some(coords) = geohack_params(&lower)
    {
        return Some(coords);
    }

    // Bing Maps `cp=lat~lon`.
    if let Some((lat, lon)) = pair_after_with_separator(&lower, "cp=", '~')
        && let Some(coords) = normalize_valid(lat, lon, CoordinateSystem::Wgs84)
    {
        return Some(coords);
    }
    // Bing Maps `sp=point.lat_lon_Label`.
    if let Some((lat, lon)) = pair_after_with_separator(&lower, "sp=point.", '_')
        && let Some(coords) = normalize_valid(lat, lon, CoordinateSystem::Wgs84)
    {
        return Some(coords);
    }
    // Tencent/QQ Maps URI `marker=coord:lat,lon;title:...`.
    if let Some((lat, lon)) = pair_after(&lower, "coord:")
        && let Some(coords) = normalize_valid(lat, lon, coordinate_system)
    {
        return Some(coords);
    }
    // Baidu static/share URLs often use longitude first for `center=`.
    if baidu {
        for key in ["center=", "c="] {
            if let Some((lon, lat)) = pair_after(&lower, key)
                && let Some(coords) = normalize_valid(lat, lon, coordinate_system)
            {
                return Some(coords);
            }
        }
    }

    // Yandex, 2GIS, Amap/Gaode and Mapy.cz put longitude first in common pair params.
    let lon_first = lower.contains("yandex.")
        || lower.contains("2gis.")
        || lower.contains("amap.")
        || lower.contains("autonavi.")
        || lower.contains("gaode.")
        || lower.contains("mapy.cz")
        || lower.contains("sogou.");
    for key in [
        "ll=",
        "sll=",
        "m=",
        "q=",
        "query=",
        "center=",
        "location=",
        "latlng=",
        "coord=",
        "coordinates=",
        "coords=",
        "pin=",
        "map=",
        "position=",
        "lnglat=",
        "c=",
    ] {
        if let Some((first, second)) = pair_after(&lower, key) {
            let (lat, lon) = if lon_first {
                (second, first)
            } else {
                (first, second)
            };
            if let Some(coords) = normalize_valid(lat, lon, coordinate_system) {
                return Some(coords);
            }
        }
    }
    // Mapy.cz and some map widgets use `x=lon&y=lat`.
    if lower.contains("mapy.cz")
        && let (Some(lon), Some(lat)) = (param_value(&lower, "x"), param_value(&lower, "y"))
        && let Some(coords) = normalize_valid(lat, lon, CoordinateSystem::Wgs84)
    {
        return Some(coords);
    }
    // 2GIS share URLs can put `longitude,latitude` in the path.
    if lower.contains("2gis.")
        && let Some((lon, lat)) = path_pair(&lower)
        && let Some(coords) = normalize_valid(lat, lon, CoordinateSystem::Wgs84)
    {
        return Some(coords);
    }

    // Google `/@lat,lon`.
    if let Some(index) = decoded.find("/@")
        && let Some((lat, lon)) = pair_at(&decoded[index + 2..])
        && let Some(coords) = normalize_valid(lat, lon, CoordinateSystem::Wgs84)
    {
        return Some(coords);
    }
    // Google `!3dLAT!4dLON`.
    if let (Some(lat), Some(lon)) = (marker_f64(&decoded, "!3d"), marker_f64(&decoded, "!4d"))
        && let Some(coords) = normalize_valid(lat, lon, CoordinateSystem::Wgs84)
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

/// Reads a `key=<number>` value from the URL fragment.
fn fragment_param_value(lower: &str, key: &str) -> Option<f64> {
    let fragment = lower.split_once('#')?.1;
    param_value(fragment, key)
}

/// Reads the `A,B` number pair right after `key` (which already includes `=`).
fn pair_after(haystack: &str, key: &str) -> Option<(f64, f64)> {
    let start = haystack.find(key)? + key.len();
    pair_at(&haystack[start..])
}

/// Reads a coordinate pair from any URL path segment.
fn path_pair(lower: &str) -> Option<(f64, f64)> {
    let path = lower
        .split_once("://")
        .and_then(|(_, rest)| rest.split_once('/').map(|(_, path)| path))
        .unwrap_or(lower);
    path.split('/').find_map(pair_at)
}

/// Reads an `A<separator>B` pair right after `key`.
fn pair_after_with_separator(haystack: &str, key: &str, separator: char) -> Option<(f64, f64)> {
    let start = haystack.find(key)? + key.len();
    let mut parts = haystack[start..].split(separator);
    let first = leading_f64(parts.next()?)?;
    let second = leading_f64(parts.next()?)?;
    Some((first, second))
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

/// Parses GeoHack `params=lat_lon_region:lon_lon_region` values.
fn geohack_params(lower: &str) -> Option<(f64, f64)> {
    let value = string_param_value(lower, "params")?;
    let parts: Vec<&str> = value.split('_').collect();
    if let [lat, lat_region, lon, lon_region, ..] = parts.as_slice() {
        let mut latitude = lat.parse::<f64>().ok()?;
        let mut longitude = lon.parse::<f64>().ok()?;
        if *lat_region == "s" {
            latitude = -latitude;
        }
        if *lon_region == "w" {
            longitude = -longitude;
        }
        return valid(latitude, longitude);
    }
    None
}

/// Reads a string query value until the next parameter separator.
fn string_param_value(lower: &str, key: &str) -> Option<String> {
    let needle = format!("{key}=");
    let start = lower.find(&needle)? + needle.len();
    let rest = &lower[start..];
    let end = rest.find(['&', '#']).unwrap_or(rest.len());
    Some(rest[..end].to_string())
}

#[cfg(test)]
mod tests {
    use super::parse_coordinates;

    fn assert_close(actual: Option<(f64, f64)>, expected: (f64, f64)) {
        let actual = actual.expect("coordinates should parse");
        assert!(
            (actual.0 - expected.0).abs() < 0.0002,
            "latitude {:?} differs from {:?}",
            actual.0,
            expected.0
        );
        assert!(
            (actual.1 - expected.1).abs() < 0.0002,
            "longitude {:?} differs from {:?}",
            actual.1,
            expected.1
        );
    }

    #[test]
    fn plain_lat_lon() {
        assert_eq!(parse_coordinates("55.75, 37.61"), Some((55.75, 37.61)));
        assert_eq!(parse_coordinates("55.75 37.61"), Some((55.75, 37.61)));
        assert_eq!(parse_coordinates("geo:55.75,37.61"), Some((55.75, 37.61)));
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
    fn apple_maps_links() {
        assert_eq!(
            parse_coordinates("https://maps.apple.com/?ll=55.75,37.61&q=Pin"),
            Some((55.75, 37.61))
        );
        assert_eq!(
            parse_coordinates("https://maps.apple.com/?sll=55.75,37.61"),
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

    #[test]
    fn more_map_provider_links() {
        assert_eq!(
            parse_coordinates("https://wikimapia.org/#lat=55.75&lon=37.61&z=15"),
            Some((55.75, 37.61))
        );
        assert_eq!(
            parse_coordinates("https://www.bing.com/maps?cp=55.75~37.61&lvl=15"),
            Some((55.75, 37.61))
        );
        assert_eq!(
            parse_coordinates("https://wego.here.com/?map=55.75,37.61,15"),
            Some((55.75, 37.61))
        );
        assert_eq!(
            parse_coordinates("https://mapy.cz/?x=37.61&y=55.75&z=15"),
            Some((55.75, 37.61))
        );
        assert_eq!(
            parse_coordinates("https://osmand.net/map?pin=55.75,37.61#15/55.75/37.61"),
            Some((55.75, 37.61))
        );
        assert_eq!(
            parse_coordinates("https://map.baidu.com/?location=55.75%2C37.61"),
            Some((55.75, 37.61))
        );
        assert_eq!(
            parse_coordinates("https://map.qq.com/?marker=coord:55.75,37.61;title:Pin"),
            Some((55.75, 37.61))
        );
        assert_eq!(
            parse_coordinates("https://uri.amap.com/marker?position=37.61,55.75"),
            Some((55.75, 37.61))
        );
    }

    #[test]
    fn chinese_provider_coordinates_are_converted_to_wgs84_inside_china() {
        assert_close(
            parse_coordinates("https://uri.amap.com/marker?position=116.397428,39.909230"),
            (39.907826, 116.391186),
        );
        assert_close(
            parse_coordinates("https://map.baidu.com/?center=116.403963,39.915119"),
            (39.907372, 116.391347),
        );
    }
}
