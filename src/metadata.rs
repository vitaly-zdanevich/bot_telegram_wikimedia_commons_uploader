use exif::{Exif, Field, In, Tag, Value};
use std::path::Path;

/// Image metadata extracted from EXIF for the Commons description page.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ImageMetadata {
    /// Capture date as `YYYY-MM-DD`, from EXIF `DateTimeOriginal`.
    pub date: Option<String>,
    /// GPS latitude in decimal degrees (negative for the southern hemisphere).
    pub latitude: Option<f64>,
    /// GPS longitude in decimal degrees (negative for the western hemisphere).
    pub longitude: Option<f64>,
}

impl ImageMetadata {
    /// Returns the GPS coordinate pair when both latitude and longitude are present.
    pub fn coordinates(&self) -> Option<(f64, f64)> {
        Some((self.latitude?, self.longitude?))
    }
}

/// Best-effort EXIF extraction; returns empty metadata when EXIF is missing or invalid.
///
/// Works on JPEG and on DNG (a TIFF container), so it runs on the original bytes
/// before any DNG → WebP conversion.
pub fn extract(bytes: &[u8]) -> ImageMetadata {
    let Ok(exif) = exif::Reader::new().read_from_container(&mut std::io::Cursor::new(bytes)) else {
        return ImageMetadata::default();
    };
    metadata_from_exif(&exif)
}

/// Best-effort EXIF extraction from a local file path.
pub fn extract_path(path: &Path) -> ImageMetadata {
    let Ok(file) = std::fs::File::open(path) else {
        return ImageMetadata::default();
    };
    let mut reader = std::io::BufReader::new(file);
    let Ok(exif) = exif::Reader::new().read_from_container(&mut reader) else {
        return ImageMetadata::default();
    };
    metadata_from_exif(&exif)
}

/// Converts a parsed EXIF block into the fields used in Commons wikitext.
fn metadata_from_exif(exif: &Exif) -> ImageMetadata {
    ImageMetadata {
        date: capture_date(exif),
        latitude: gps_coordinate(exif, Tag::GPSLatitude, Tag::GPSLatitudeRef, 'S'),
        longitude: gps_coordinate(exif, Tag::GPSLongitude, Tag::GPSLongitudeRef, 'W'),
    }
}

/// Reads `DateTimeOriginal` (or `DateTime`) and formats it as `YYYY-MM-DD`.
fn capture_date(exif: &Exif) -> Option<String> {
    let field = exif
        .get_field(Tag::DateTimeOriginal, In::PRIMARY)
        .or_else(|| exif.get_field(Tag::DateTime, In::PRIMARY))?;
    date_to_iso(&ascii_value(field)?)
}

/// Converts an EXIF datetime (`YYYY:MM:DD HH:MM:SS`) to an ISO date (`YYYY-MM-DD`).
fn date_to_iso(raw: &str) -> Option<String> {
    let date_part = raw.split_whitespace().next()?;
    let mut parts = date_part.split(':');
    let (year, month, day) = (parts.next()?, parts.next()?, parts.next()?);
    let valid = year.len() == 4
        && month.len() == 2
        && day.len() == 2
        && [year, month, day]
            .iter()
            .all(|part| part.bytes().all(|byte| byte.is_ascii_digit()));
    valid.then(|| format!("{year}-{month}-{day}"))
}

/// Computes one decimal-degree GPS coordinate from EXIF rationals and its hemisphere ref.
fn gps_coordinate(exif: &Exif, coord_tag: Tag, ref_tag: Tag, negative_ref: char) -> Option<f64> {
    let field = exif.get_field(coord_tag, In::PRIMARY)?;
    let degrees = rationals_to_degrees(&field.value)?;
    let hemisphere = exif
        .get_field(ref_tag, In::PRIMARY)
        .and_then(ascii_value)
        .and_then(|reference| reference.chars().next());
    let sign = if hemisphere == Some(negative_ref) {
        -1.0
    } else {
        1.0
    };
    Some(sign * degrees)
}

/// Converts EXIF degree/minute/second rationals into decimal degrees.
fn rationals_to_degrees(value: &Value) -> Option<f64> {
    let Value::Rational(parts) = value else {
        return None;
    };
    if parts.len() < 3 {
        return None;
    }
    Some(parts[0].to_f64() + parts[1].to_f64() / 60.0 + parts[2].to_f64() / 3600.0)
}

/// Reads the first ASCII string of an EXIF field.
fn ascii_value(field: &Field) -> Option<String> {
    match &field.value {
        Value::Ascii(values) => values
            .first()
            .map(|bytes| String::from_utf8_lossy(bytes).trim().to_string()),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::date_to_iso;

    #[test]
    fn converts_exif_datetime_to_iso_date() {
        assert_eq!(
            date_to_iso("2026:06:20 12:34:56").as_deref(),
            Some("2026-06-20")
        );
        assert_eq!(date_to_iso("not a date"), None);
        assert_eq!(date_to_iso("2026:6:20 12:00:00"), None);
    }
}
