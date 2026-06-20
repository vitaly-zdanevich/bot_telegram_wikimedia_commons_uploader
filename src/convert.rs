use anyhow::{Context, Result, anyhow, bail};

/// File extensions Wikimedia Commons accepts for upload (lower-case, without dot).
///
/// DNG and HEIC are deliberately absent: they are converted to WebP first.
const COMMONS_ACCEPTED_EXTENSIONS: &[&str] = &[
    // Images
    "jpg", "jpeg", "png", "gif", "svg", "tif", "tiff", "webp", "xcf", // Documents
    "pdf", "djvu", // 3D
    "stl",  // Audio
    "wav", "mp3", "oga", "ogg", "opus", "flac", "mid", "midi", // Video
    "ogv", "webm", "mpg", "mpeg",
];

/// How the bot should handle an incoming file before upload.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SourceFormat {
    /// Format already accepted by Commons; upload the original bytes unchanged.
    PassThrough,
    /// Adobe DNG raw photo; convert to lossy WebP.
    Dng,
    /// HEIC/HEIF image; convert to lossy WebP.
    Heic,
    /// Windows BMP; convert to lossless WebP (BMP is usually graphics/screenshots).
    Bmp,
}

impl SourceFormat {
    /// Returns true when the format must be converted to WebP before upload.
    pub fn needs_conversion(self) -> bool {
        !matches!(self, SourceFormat::PassThrough)
    }
}

/// Decoded 8-bit RGB image buffer (interleaved, no row padding).
struct RawRgb {
    /// Width in pixels.
    width: u32,
    /// Height in pixels.
    height: u32,
    /// Interleaved RGB bytes (`width * height * 3`).
    data: Vec<u8>,
}

/// Returns true when Commons accepts files with this extension as-is.
pub fn is_commons_accepted(extension: &str) -> bool {
    let extension = extension.trim_start_matches('.').to_ascii_lowercase();
    COMMONS_ACCEPTED_EXTENSIONS.contains(&extension.as_str())
}

/// Classifies an incoming file by extension, MIME type, and magic bytes.
pub fn classify(file_name: Option<&str>, mime: Option<&str>, bytes: &[u8]) -> SourceFormat {
    let extension = file_name.and_then(file_extension);
    let mime = mime.map(str::to_ascii_lowercase);
    let mime = mime.as_deref();

    if extension.as_deref() == Some("dng") || mime == Some("image/x-adobe-dng") {
        return SourceFormat::Dng;
    }
    if matches!(extension.as_deref(), Some("heic") | Some("heif"))
        || matches!(mime, Some("image/heic") | Some("image/heif"))
        || is_heic_magic(bytes)
    {
        return SourceFormat::Heic;
    }
    if extension.as_deref() == Some("bmp")
        || matches!(mime, Some("image/bmp") | Some("image/x-ms-bmp"))
        || bytes.starts_with(b"BM")
    {
        return SourceFormat::Bmp;
    }
    SourceFormat::PassThrough
}

/// Converts DNG or HEIC bytes into WebP; pass-through inputs are rejected.
pub fn to_webp(bytes: &[u8], source: SourceFormat, quality: f32) -> Result<Vec<u8>> {
    match source {
        SourceFormat::Dng => encode_webp_lossy(&decode_dng(bytes)?, quality),
        SourceFormat::Heic => encode_webp_lossy(&decode_heic(bytes)?, quality),
        SourceFormat::Bmp => encode_webp_lossless(&decode_bmp(bytes)?),
        SourceFormat::PassThrough => bail!("pass-through files must not be converted"),
    }
}

/// Returns the upload extension for a pass-through file.
pub fn passthrough_extension(file_name: Option<&str>, mime: Option<&str>) -> String {
    if let Some(extension) = file_name.and_then(file_extension) {
        return extension;
    }
    mime_extension(mime).unwrap_or("bin").to_string()
}

/// Decodes and develops a raw DNG into an sRGB 8-bit image via imagepipe.
fn decode_dng(bytes: &[u8]) -> Result<RawRgb> {
    use std::io::Write;
    let mut file = tempfile::NamedTempFile::new().context("failed to create temp file for DNG")?;
    file.write_all(bytes)
        .context("failed to buffer DNG to disk")?;
    file.flush().ok();
    let mut pipeline = imagepipe::Pipeline::new_from_file(file.path())
        .map_err(|error| anyhow!("failed to read DNG: {error}"))?;
    let decoded = pipeline
        .output_8bit(None)
        .map_err(|error| anyhow!("failed to develop DNG: {error}"))?;
    Ok(RawRgb {
        width: decoded.width as u32,
        height: decoded.height as u32,
        data: decoded.data,
    })
}

/// Decodes a HEIC/HEIF image into an sRGB 8-bit buffer via libheif.
#[cfg(feature = "heic")]
fn decode_heic(bytes: &[u8]) -> Result<RawRgb> {
    use libheif_rs::{ColorSpace, HeifContext, LibHeif, RgbChroma};

    let lib_heif = LibHeif::new();
    let context = HeifContext::read_from_bytes(bytes)
        .map_err(|error| anyhow!("failed to read HEIC: {error}"))?;
    let handle = context
        .primary_image_handle()
        .map_err(|error| anyhow!("failed to read HEIC image handle: {error}"))?;
    let image = lib_heif
        .decode(&handle, ColorSpace::Rgb(RgbChroma::Rgb), None)
        .map_err(|error| anyhow!("failed to decode HEIC: {error}"))?;
    let width = image.width();
    let height = image.height();
    let plane = image
        .planes()
        .interleaved
        .context("HEIC image is missing an interleaved RGB plane")?;
    let row_bytes = width as usize * 3;
    let mut data = Vec::with_capacity(row_bytes * height as usize);
    for row in 0..height as usize {
        let start = row * plane.stride;
        data.extend_from_slice(&plane.data[start..start + row_bytes]);
    }
    Ok(RawRgb {
        width,
        height,
        data,
    })
}

/// HEIC stub used when the `heic` feature is disabled.
#[cfg(not(feature = "heic"))]
fn decode_heic(_bytes: &[u8]) -> Result<RawRgb> {
    bail!("HEIC support is not built into this binary (enable the `heic` Cargo feature)")
}

/// Decodes a BMP into an sRGB 8-bit buffer.
fn decode_bmp(bytes: &[u8]) -> Result<RawRgb> {
    let decoded = image::load_from_memory_with_format(bytes, image::ImageFormat::Bmp)
        .map_err(|error| anyhow!("failed to decode BMP: {error}"))?
        .to_rgb8();
    Ok(RawRgb {
        width: decoded.width(),
        height: decoded.height(),
        data: decoded.into_raw(),
    })
}

/// Encodes an RGB buffer to lossy WebP at the given quality (1-100).
fn encode_webp_lossy(image: &RawRgb, quality: f32) -> Result<Vec<u8>> {
    ensure_non_empty(image)?;
    let encoder = webp::Encoder::from_rgb(&image.data, image.width, image.height);
    Ok(encoder.encode(quality.clamp(1.0, 100.0)).to_vec())
}

/// Encodes an RGB buffer to lossless WebP (used for BMP graphics).
fn encode_webp_lossless(image: &RawRgb) -> Result<Vec<u8>> {
    ensure_non_empty(image)?;
    let encoder = webp::Encoder::from_rgb(&image.data, image.width, image.height);
    Ok(encoder.encode_lossless().to_vec())
}

/// Validates that a decoded image has non-zero dimensions.
fn ensure_non_empty(image: &RawRgb) -> Result<()> {
    if image.width == 0 || image.height == 0 {
        bail!("decoded image has zero dimensions");
    }
    Ok(())
}

/// Returns the lower-case extension of a filename when it looks like a real extension.
fn file_extension(file_name: &str) -> Option<String> {
    file_name
        .rsplit_once('.')
        .map(|(_, extension)| extension.to_ascii_lowercase())
        .filter(|extension| {
            !extension.is_empty()
                && extension.len() <= 5
                && extension.chars().all(|ch| ch.is_ascii_alphanumeric())
        })
}

/// Maps a MIME type to a Commons-friendly extension (used when no filename is present).
fn mime_extension(mime: Option<&str>) -> Option<&'static str> {
    match mime?.to_ascii_lowercase().as_str() {
        "image/jpeg" => Some("jpg"),
        "image/png" => Some("png"),
        "image/gif" => Some("gif"),
        "image/tiff" => Some("tif"),
        "image/webp" => Some("webp"),
        "image/svg+xml" => Some("svg"),
        "application/pdf" => Some("pdf"),
        "image/vnd.djvu" | "image/x-djvu" => Some("djvu"),
        "audio/ogg" | "application/ogg" => Some("ogg"),
        "audio/opus" => Some("opus"),
        "audio/mpeg" => Some("mp3"),
        "audio/wav" | "audio/x-wav" => Some("wav"),
        "audio/flac" | "audio/x-flac" => Some("flac"),
        "video/webm" => Some("webm"),
        "video/ogg" => Some("ogv"),
        "video/mpeg" => Some("mpg"),
        _ => None,
    }
}

/// Returns true when bytes start with an ISO-BMFF `ftyp` box of a HEIC brand.
fn is_heic_magic(bytes: &[u8]) -> bool {
    bytes.len() >= 12
        && &bytes[4..8] == b"ftyp"
        && matches!(
            &bytes[8..12],
            b"heic" | b"heix" | b"hevc" | b"heim" | b"heis" | b"hevm" | b"hevs"
        )
}

#[cfg(test)]
mod tests {
    use super::{
        SourceFormat, classify, file_extension, is_commons_accepted, is_heic_magic, mime_extension,
        passthrough_extension,
    };

    #[test]
    fn classifies_dng() {
        assert_eq!(classify(Some("IMG_1.DNG"), None, &[]), SourceFormat::Dng);
        assert_eq!(
            classify(None, Some("image/x-adobe-dng"), &[]),
            SourceFormat::Dng
        );
    }

    #[test]
    fn classifies_heic_by_extension_mime_and_magic() {
        assert_eq!(classify(Some("a.heif"), None, &[]), SourceFormat::Heic);
        assert_eq!(classify(None, Some("image/heic"), &[]), SourceFormat::Heic);
        let mut magic = vec![0, 0, 0, 0];
        magic.extend_from_slice(b"ftypheic");
        assert_eq!(classify(None, None, &magic), SourceFormat::Heic);
    }

    #[test]
    fn classifies_bmp_by_extension_and_magic() {
        assert_eq!(classify(Some("a.bmp"), None, &[]), SourceFormat::Bmp);
        assert_eq!(
            classify(None, None, b"BM\x00\x00\x00\x00"),
            SourceFormat::Bmp
        );
        assert!(SourceFormat::Bmp.needs_conversion());
        assert!(!SourceFormat::PassThrough.needs_conversion());
    }

    #[test]
    fn other_formats_pass_through() {
        assert_eq!(
            classify(Some("p.jpg"), Some("image/jpeg"), b"\xff\xd8\xff"),
            SourceFormat::PassThrough
        );
        assert_eq!(
            classify(Some("doc.pdf"), None, &[]),
            SourceFormat::PassThrough
        );
        assert_eq!(
            classify(Some("clip.webm"), None, &[]),
            SourceFormat::PassThrough
        );
    }

    #[test]
    fn commons_allowlist_covers_requested_media() {
        for extension in [
            "jpg", "png", "webp", "pdf", "djvu", "wav", "mp3", "oga", "ogg", "ogv", "opus", "webm",
        ] {
            assert!(
                is_commons_accepted(extension),
                "{extension} should be accepted"
            );
        }
        assert!(is_commons_accepted(".JPG"));
        assert!(!is_commons_accepted("dng"));
        assert!(!is_commons_accepted("heic"));
        assert!(!is_commons_accepted("exe"));
    }

    #[test]
    fn passthrough_extension_prefers_filename_then_mime() {
        assert_eq!(
            passthrough_extension(Some("a.PNG"), Some("image/jpeg")),
            "png"
        );
        assert_eq!(passthrough_extension(None, Some("audio/ogg")), "ogg");
        assert_eq!(passthrough_extension(None, None), "bin");
    }

    #[test]
    fn maps_voice_and_document_mimes() {
        assert_eq!(mime_extension(Some("audio/ogg")), Some("ogg"));
        assert_eq!(mime_extension(Some("application/pdf")), Some("pdf"));
        assert_eq!(mime_extension(Some("video/webm")), Some("webm"));
        assert_eq!(mime_extension(Some("application/x-unknown")), None);
    }

    #[test]
    fn file_extension_is_sane() {
        assert_eq!(file_extension("a.JpG"), Some("jpg".to_string()));
        assert_eq!(file_extension("noext"), None);
        assert_eq!(file_extension("archive.verylong"), None);
    }

    #[test]
    fn detects_heic_magic() {
        let mut heic = vec![1, 2, 3, 4];
        heic.extend_from_slice(b"ftypheic");
        assert!(is_heic_magic(&heic));
        assert!(!is_heic_magic(b"\xff\xd8\xff\xe0jpeg-data"));
    }
}
