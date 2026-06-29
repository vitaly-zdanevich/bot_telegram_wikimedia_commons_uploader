use crate::models::DngMode;
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

/// Returns true when an archive member is uploadable (accepted as-is, or convertible).
pub fn is_uploadable_archive_member(name: &str) -> bool {
    match name.rsplit_once('.') {
        Some((_, ext)) => {
            let ext = ext.to_ascii_lowercase();
            is_commons_accepted(&ext) || matches!(ext.as_str(), "dng" | "heic" | "heif" | "bmp")
        }
        None => false,
    }
}

/// Renders a small JPEG thumbnail for an archive preview, or `None` if undecodable.
#[cfg(feature = "archive")]
pub fn make_thumbnail(bytes: &[u8], max_edge: u32) -> Option<Vec<u8>> {
    let image = image::load_from_memory(bytes).ok()?;
    let thumb = image.thumbnail(max_edge, max_edge).to_rgb8();
    let mut out = std::io::Cursor::new(Vec::new());
    image::DynamicImage::ImageRgb8(thumb)
        .write_to(&mut out, image::ImageFormat::Jpeg)
        .ok()?;
    Some(out.into_inner())
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

/// Converts a file into an uploadable form, returning `(bytes, extension)`.
///
/// DNG develops to WebP; if imagepipe can't develop the raw, ImageMagick is used as a
/// second raw decoder. HEIC and BMP become WebP.
/// Pass-through inputs are rejected.
pub fn convert(
    bytes: &[u8],
    source: SourceFormat,
    quality: f32,
    dng_mode: DngMode,
) -> Result<(Vec<u8>, &'static str)> {
    match source {
        SourceFormat::Dng => convert_dng(bytes, quality, dng_mode),
        SourceFormat::Heic => Ok((encode_webp_lossy(&decode_heic(bytes)?, quality)?, "webp")),
        SourceFormat::Bmp => Ok((encode_webp_lossless(&decode_bmp(bytes)?)?, "webp")),
        SourceFormat::PassThrough => bail!("pass-through files must not be converted"),
    }
}

/// Converts a DNG according to the user's mode: raw development to WebP, or embedded JPEG.
fn convert_dng(bytes: &[u8], quality: f32, dng_mode: DngMode) -> Result<(Vec<u8>, &'static str)> {
    match dng_mode {
        DngMode::ConvertToWebp => match develop_raw(bytes) {
            Ok(image) => Ok((encode_webp_lossy(&image, quality)?, "webp")),
            Err(develop_error) => match develop_dng_with_imagemagick(bytes, quality) {
                Ok(webp) => Ok((webp, "webp")),
                Err(magick_error) => bail!(
                    "could not decode DNG with imagepipe ({develop_error}); ImageMagick fallback failed ({magick_error})"
                ),
            },
        },
        DngMode::ExtractEmbeddedJpeg => match extract_largest_valid_jpeg(bytes) {
            Some(jpeg) => Ok((jpeg.to_vec(), "jpg")),
            None => bail!("no usable embedded JPEG preview found in DNG"),
        },
    }
}

/// Returns the upload extension for a pass-through file.
pub fn passthrough_extension(file_name: Option<&str>, mime: Option<&str>) -> String {
    if let Some(extension) = file_name.and_then(file_extension) {
        return extension;
    }
    mime_extension(mime).unwrap_or("bin").to_string()
}

/// Develops a raw file into an sRGB 8-bit image via imagepipe (camera-dependent).
fn develop_raw(bytes: &[u8]) -> Result<RawRgb> {
    use std::io::Write;
    let mut file = tempfile::NamedTempFile::new().context("failed to create temp file for raw")?;
    file.write_all(bytes)
        .context("failed to buffer raw to disk")?;
    file.flush().ok();
    let mut pipeline =
        imagepipe::Pipeline::new_from_file(file.path()).map_err(|error| anyhow!("{error}"))?;
    let decoded = pipeline
        .output_8bit(None)
        .map_err(|error| anyhow!("{error}"))?;
    Ok(RawRgb {
        width: decoded.width as u32,
        height: decoded.height as u32,
        data: decoded.data,
    })
}

/// Develops a DNG through ImageMagick's raw decoder and returns WebP bytes.
fn develop_dng_with_imagemagick(bytes: &[u8], quality: f32) -> Result<Vec<u8>> {
    use std::io::Write;
    let dir = tempfile::tempdir().context("failed to create temp directory for DNG conversion")?;
    let input_path = dir.path().join("input.dng");
    let output_path = dir.path().join("output.webp");
    let mut input =
        std::fs::File::create(&input_path).context("failed to create temporary DNG file")?;
    input
        .write_all(bytes)
        .context("failed to write temporary DNG file")?;
    drop(input);

    let quality = format!("{:.0}", quality.clamp(1.0, 100.0));
    let mut errors = Vec::new();
    for program in ["magick", "convert"] {
        let output = match std::process::Command::new(program)
            .arg(&input_path)
            .arg("-auto-orient")
            .arg("-colorspace")
            .arg("sRGB")
            .arg("-quality")
            .arg(&quality)
            .arg(&output_path)
            .output()
        {
            Ok(output) => output,
            Err(error) => {
                errors.push(format!("{program}: failed to launch: {error}"));
                continue;
            }
        };
        if output.status.success() {
            let webp =
                std::fs::read(&output_path).context("ImageMagick did not write WebP output")?;
            if webp.is_empty() {
                bail!("ImageMagick wrote empty WebP output");
            }
            return Ok(webp);
        }
        let stderr = String::from_utf8_lossy(&output.stderr);
        errors.push(format!(
            "{program}: exited with {}: {}",
            output.status,
            stderr.trim()
        ));
    }
    bail!("{}", errors.join("; "))
}

/// Returns the bytes of the largest embedded JPEG that decodes cleanly.
fn extract_largest_valid_jpeg(bytes: &[u8]) -> Option<&[u8]> {
    jpeg_spans_largest_first(bytes)
        .into_iter()
        .find(|span| image::load_from_memory_with_format(span, image::ImageFormat::Jpeg).is_ok())
}

/// Returns byte ranges that look like complete JPEG streams (SOI...EOI), largest first.
fn jpeg_spans_largest_first(bytes: &[u8]) -> Vec<&[u8]> {
    let mut spans: Vec<(usize, usize)> = Vec::new();
    let mut i = 0;
    while i + 2 < bytes.len() {
        if bytes[i] == 0xFF && bytes[i + 1] == 0xD8 && bytes[i + 2] == 0xFF {
            let mut end = None;
            let mut j = i + 2;
            while j + 1 < bytes.len() {
                if bytes[j] == 0xFF && bytes[j + 1] == 0xD9 {
                    end = Some(j + 2);
                    break;
                }
                j += 1;
            }
            if let Some(found) = end {
                spans.push((i, found));
                i = found;
                continue;
            }
        }
        i += 1;
    }
    spans.sort_by_key(|&(start, end)| std::cmp::Reverse(end - start));
    spans
        .into_iter()
        .map(|(start, end)| &bytes[start..end])
        .collect()
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

    #[test]
    fn finds_largest_jpeg_span_first() {
        let mut data = vec![0x49, 0x49]; // TIFF-ish leading bytes
        data.extend_from_slice(&[0xFF, 0xD8, 0xFF, 0x10, 0xFF, 0xD9]); // small span
        data.extend_from_slice(&[0xFF, 0xD8, 0xFF, 1, 2, 3, 4, 5, 6, 0xFF, 0xD9]); // larger span
        let spans = super::jpeg_spans_largest_first(&data);
        assert_eq!(spans.len(), 2);
        assert!(spans[0].len() > spans[1].len());
        assert_eq!(&spans[0][0..3], &[0xFF, 0xD8, 0xFF]);
    }
}
