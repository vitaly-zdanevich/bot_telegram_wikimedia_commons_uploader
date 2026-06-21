//! Archive (zip / rar) extraction. Built only with the `archive` feature (VM-only).

use anyhow::{Result, bail};

/// An image extracted from an archive.
pub struct ArchiveEntry {
    /// Base file name inside the archive.
    pub name: String,
    /// File bytes.
    pub bytes: Vec<u8>,
}

/// Returns true when the file looks like a supported archive (by name or magic bytes).
pub fn is_archive(file_name: Option<&str>, bytes: &[u8]) -> bool {
    let ext = file_name
        .and_then(|name| name.rsplit_once('.'))
        .map(|(_, ext)| ext.to_ascii_lowercase());
    matches!(ext.as_deref(), Some("zip" | "cbz" | "rar" | "cbr"))
        || bytes.starts_with(b"PK\x03\x04")
        || bytes.starts_with(b"Rar!")
}

/// Extracts uploadable image entries from a zip or rar archive.
pub fn extract_images(bytes: &[u8], file_name: Option<&str>) -> Result<Vec<ArchiveEntry>> {
    let is_rar = bytes.starts_with(b"Rar!")
        || file_name.is_some_and(|name| {
            let lower = name.to_ascii_lowercase();
            lower.ends_with(".rar") || lower.ends_with(".cbr")
        });
    let entries = if is_rar {
        extract_rar(bytes)?
    } else {
        extract_zip(bytes)?
    };
    if entries.is_empty() {
        bail!("no uploadable images found in the archive");
    }
    Ok(entries)
}

/// Extracts images from a zip archive (pure Rust).
fn extract_zip(bytes: &[u8]) -> Result<Vec<ArchiveEntry>> {
    let mut archive = zip::ZipArchive::new(std::io::Cursor::new(bytes))?;
    let mut entries = Vec::new();
    for index in 0..archive.len() {
        let mut file = archive.by_index(index)?;
        if !file.is_file() {
            continue;
        }
        let name = file.name().to_string();
        if !crate::convert::is_uploadable_archive_member(&name) {
            continue;
        }
        let mut buffer = Vec::new();
        std::io::Read::read_to_end(&mut file, &mut buffer)?;
        entries.push(ArchiveEntry {
            name: basename(&name),
            bytes: buffer,
        });
    }
    Ok(entries)
}

/// Extracts images from a rar archive via the system libunrar (`rar` feature).
#[cfg(feature = "rar")]
fn extract_rar(bytes: &[u8]) -> Result<Vec<ArchiveEntry>> {
    use std::io::Write;
    let mut temp = tempfile::NamedTempFile::new()?;
    temp.write_all(bytes)?;
    temp.flush().ok();

    let mut entries = Vec::new();
    let mut cursor = unrar::Archive::new(temp.path()).open_for_processing()?;
    while let Some(header) = cursor.read_header()? {
        let name = header.entry().filename.to_string_lossy().to_string();
        if header.entry().is_file() && crate::convert::is_uploadable_archive_member(&name) {
            let (data, next) = header.read()?;
            entries.push(ArchiveEntry {
                name: basename(&name),
                bytes: data,
            });
            cursor = next;
        } else {
            cursor = header.skip()?;
        }
    }
    Ok(entries)
}

/// RAR stub used when the `rar` feature is disabled.
#[cfg(not(feature = "rar"))]
fn extract_rar(_bytes: &[u8]) -> Result<Vec<ArchiveEntry>> {
    bail!("RAR needs the `rar` build (system libunrar). Please send a ZIP instead.")
}

/// Returns the base name of an archive entry path.
fn basename(name: &str) -> String {
    name.rsplit(['/', '\\']).next().unwrap_or(name).to_string()
}

#[cfg(test)]
mod tests {
    use super::{extract_images, is_archive};

    #[test]
    fn detects_archives() {
        assert!(is_archive(Some("photos.zip"), &[]));
        assert!(is_archive(Some("comic.cbr"), &[]));
        assert!(is_archive(None, b"PK\x03\x04rest"));
        assert!(is_archive(None, b"Rar!\x1a\x07\x00"));
        assert!(!is_archive(Some("photo.jpg"), b"\xff\xd8\xff"));
    }

    #[test]
    fn extracts_zip_images_only() {
        use std::io::Write;
        let mut zip = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
        let options = zip::write::SimpleFileOptions::default();
        zip.start_file("album/photo.jpg", options).unwrap();
        zip.write_all(b"pretend jpeg bytes").unwrap();
        zip.start_file("notes.txt", options).unwrap();
        zip.write_all(b"ignore me").unwrap();
        let bytes = zip.finish().unwrap().into_inner();

        let entries = extract_images(&bytes, Some("album.zip")).unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].name, "photo.jpg");
        assert_eq!(entries[0].bytes, b"pretend jpeg bytes");
    }

    #[test]
    fn errors_when_no_images() {
        use std::io::Write;
        let mut zip = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
        let options = zip::write::SimpleFileOptions::default();
        zip.start_file("readme.txt", options).unwrap();
        zip.write_all(b"text only").unwrap();
        let bytes = zip.finish().unwrap().into_inner();
        assert!(extract_images(&bytes, Some("a.zip")).is_err());
    }
}
