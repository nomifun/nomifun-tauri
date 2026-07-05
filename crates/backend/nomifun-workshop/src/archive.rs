//! Canvas export/import archive (.zip) assembly + extraction.
//!
//! Layout of an exported canvas archive:
//! - `canvas.json`   — the opaque canvas doc (verbatim).
//! - `manifest.json` — `{ version, app, exported_at, canvas: {title}, assets: [...] }`
//!   where each asset entry carries the metadata needed to re-register it on
//!   import plus a `file` path (`assets/{id}.{ext}`) or `null` for text assets.
//! - `assets/{id}.{ext}` — the binary of every doc-referenced asset that has one.
//!
//! Both functions are synchronous and CPU-bound (deflate + copies); the service
//! runs them inside `tokio::task::spawn_blocking`.

use std::collections::HashMap;
use std::io::{Cursor, Read, Write};

use zip::write::SimpleFileOptions;

/// App tag stamped into (and verified from) a manifest.
pub(crate) const ARCHIVE_APP: &str = "nomifun-workshop";
/// Archive schema version.
pub(crate) const ARCHIVE_VERSION: u32 = 1;

pub(crate) const CANVAS_ENTRY: &str = "canvas.json";
pub(crate) const MANIFEST_ENTRY: &str = "manifest.json";

/// Build a zip from `(entry_name, bytes)` pairs. Deflate-compressed.
pub(crate) fn build_zip(entries: Vec<(String, Vec<u8>)>) -> std::io::Result<Vec<u8>> {
    let mut writer = zip::ZipWriter::new(Cursor::new(Vec::new()));
    let opts = SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated);
    for (name, bytes) in entries {
        writer
            .start_file(name, opts)
            .map_err(|e| std::io::Error::other(format!("zip start_file: {e}")))?;
        writer.write_all(&bytes)?;
    }
    let cursor = writer
        .finish()
        .map_err(|e| std::io::Error::other(format!("zip finish: {e}")))?;
    Ok(cursor.into_inner())
}

/// Extract every entry of a zip into `name → bytes`. Uses `enclosed_name` to
/// reject zip-slip (`../`, absolute) entries. Directory entries are skipped.
pub(crate) fn extract_zip(bytes: &[u8]) -> std::io::Result<HashMap<String, Vec<u8>>> {
    let mut archive = zip::ZipArchive::new(Cursor::new(bytes))
        .map_err(|e| std::io::Error::other(format!("open zip: {e}")))?;
    let mut out = HashMap::new();
    for i in 0..archive.len() {
        let mut file = archive
            .by_index(i)
            .map_err(|e| std::io::Error::other(format!("zip entry {i}: {e}")))?;
        if file.is_dir() {
            continue;
        }
        // `enclosed_name` normalizes + rejects traversal / absolute paths.
        let Some(path) = file.enclosed_name() else {
            return Err(std::io::Error::other("zip entry escapes archive root"));
        };
        let name = path.to_string_lossy().replace('\\', "/");
        let mut buf = Vec::with_capacity(file.size() as usize);
        file.read_to_end(&mut buf)?;
        out.insert(name, buf);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zip_roundtrip_preserves_entries() {
        let entries = vec![
            (CANVAS_ENTRY.to_string(), b"{\"schema\":1}".to_vec()),
            ("assets/wsa_a.png".to_string(), vec![1, 2, 3, 4]),
        ];
        let zip = build_zip(entries).unwrap();
        let extracted = extract_zip(&zip).unwrap();
        assert_eq!(extracted.get(CANVAS_ENTRY).unwrap(), b"{\"schema\":1}");
        assert_eq!(extracted.get("assets/wsa_a.png").unwrap(), &[1, 2, 3, 4]);
    }

    #[test]
    fn extract_rejects_garbage() {
        assert!(extract_zip(b"not a zip").is_err());
    }
}
