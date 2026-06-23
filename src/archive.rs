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

/// Extracts images from a rar archive via a system extractor (`rar` feature).
///
/// Prefers `unar` (The Unarchiver — free, in Debian main, handles RAR5) and falls back to
/// the non-free `unrar`. Avoids a fragile vendored C++ decoder at compile time, and lets the
/// same build run on Toolforge (Aptfile: `unar`) and on a Cloud VPS.
#[cfg(feature = "rar")]
fn extract_rar(bytes: &[u8]) -> Result<Vec<ArchiveEntry>> {
    let dir = tempfile::tempdir()?;
    let archive_path = dir.path().join("archive.rar");
    std::fs::write(&archive_path, bytes)?;
    let out_dir = dir.path().join("out");
    std::fs::create_dir_all(&out_dir)?;

    let out = out_dir.display().to_string();
    let archive = archive_path.display().to_string();
    run_extractor(
        "unar",
        &[
            "-force-overwrite",
            "-quiet",
            "-output-directory",
            &out,
            &archive,
        ],
    )
    .or_else(|unar_error| {
        run_extractor("unrar", &["x", "-o+", "-idq", &archive, &format!("{out}/")]).map_err(
            |unrar_error| {
                anyhow::anyhow!(
                    "no RAR extractor worked — unar: {unar_error}; unrar: {unrar_error}"
                )
            },
        )
    })?;

    let mut entries = Vec::new();
    collect_images(&out_dir, &mut entries)?;
    Ok(entries)
}

/// Runs an extractor command, erroring if it cannot start or exits non-zero.
#[cfg(feature = "rar")]
fn run_extractor(program: &str, args: &[&str]) -> Result<()> {
    use anyhow::Context;
    let output = std::process::Command::new(program)
        .args(args)
        .output()
        .with_context(|| format!("failed to launch `{program}`"))?;
    if !output.status.success() {
        bail!(
            "`{program}` failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(())
}

/// Recursively collects uploadable images from an extracted directory tree.
#[cfg(feature = "rar")]
fn collect_images(dir: &std::path::Path, entries: &mut Vec<ArchiveEntry>) -> Result<()> {
    for entry in std::fs::read_dir(dir)? {
        let path = entry?.path();
        if path.is_dir() {
            collect_images(&path, entries)?;
        } else if let Some(name) = path.file_name().and_then(|name| name.to_str())
            && crate::convert::is_uploadable_archive_member(name)
        {
            entries.push(ArchiveEntry {
                name: name.to_string(),
                bytes: std::fs::read(&path)?,
            });
        }
    }
    Ok(())
}

/// RAR stub used when the `rar` feature is disabled.
#[cfg(not(feature = "rar"))]
fn extract_rar(_bytes: &[u8]) -> Result<Vec<ArchiveEntry>> {
    bail!("RAR support is not enabled in this build. Please send a ZIP instead.")
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
