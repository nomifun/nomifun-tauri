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

/// Per-entry decompressed-size ceiling — no single archive member may inflate
/// beyond this (matches the asset upload cap).
const MAX_ENTRY_BYTES: u64 = crate::MAX_ASSET_BYTES as u64;
/// Cumulative decompressed-size ceiling across ALL entries — the zip-bomb guard.
const MAX_TOTAL_BYTES: u64 = 512 * 1024 * 1024;

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
    extract_zip_bounded(bytes, MAX_ENTRY_BYTES, MAX_TOTAL_BYTES)
}

/// [`extract_zip`] with explicit per-entry / cumulative decompression budgets
/// (parameterized so tests can drive the overflow paths with small caps).
///
/// Crucially it never pre-allocates from the untrusted header-declared size
/// (`file.size()`), which is attacker-controlled and can trigger an
/// unsatisfiable allocation → `abort()`. Instead each entry is read through a
/// `Read::take` bounded by the smaller of the per-entry cap and the remaining
/// cumulative budget, so neither a lying `size` field nor a compression bomb
/// can exhaust memory — an oversized entry returns an error (mapped to a
/// `BadRequest` by the caller).
fn extract_zip_bounded(
    bytes: &[u8],
    max_entry: u64,
    max_total: u64,
) -> std::io::Result<HashMap<String, Vec<u8>>> {
    let mut archive = zip::ZipArchive::new(Cursor::new(bytes))
        .map_err(|e| std::io::Error::other(format!("open zip: {e}")))?;
    let mut out = HashMap::new();
    let mut total: u64 = 0;
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
        // Bound this entry by min(per-entry cap, remaining cumulative budget),
        // reading one byte past the cap so an overflow is detectable rather than
        // silently truncated.
        let cap = max_entry.min(max_total.saturating_sub(total));
        let mut buf = Vec::new();
        (&mut file).take(cap.saturating_add(1)).read_to_end(&mut buf)?;
        if buf.len() as u64 > cap {
            return Err(std::io::Error::other(format!(
                "archive entry '{name}' exceeds the decompression budget \
                 ({max_entry} bytes/entry, {max_total} bytes total)"
            )));
        }
        total = total.saturating_add(buf.len() as u64);
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

    #[test]
    fn extract_rejects_entry_over_per_entry_budget() {
        // A single 50-byte entry against a 10-byte per-entry cap → rejected
        // (the take-based reader stops at cap+1 and reports the overflow).
        let zip = build_zip(vec![("assets/wsa_a.bin".to_string(), vec![7u8; 50])]).unwrap();
        let err = extract_zip_bounded(&zip, 10, 1024).unwrap_err();
        assert!(err.to_string().contains("decompression budget"), "got: {err}");
    }

    #[test]
    fn extract_rejects_cumulative_bomb() {
        // Three 40-byte entries (120 total) against a 100-byte cumulative cap:
        // the entry that crosses the running budget is rejected.
        let zip = build_zip(vec![
            ("assets/a.bin".to_string(), vec![1u8; 40]),
            ("assets/b.bin".to_string(), vec![2u8; 40]),
            ("assets/c.bin".to_string(), vec![3u8; 40]),
        ])
        .unwrap();
        let err = extract_zip_bounded(&zip, 1000, 100).unwrap_err();
        assert!(err.to_string().contains("decompression budget"), "got: {err}");
    }

    #[test]
    fn extract_within_budget_succeeds() {
        let zip = build_zip(vec![
            (CANVAS_ENTRY.to_string(), b"{\"schema\":1}".to_vec()),
            ("assets/wsa_a.png".to_string(), vec![1, 2, 3, 4]),
        ])
        .unwrap();
        let extracted = extract_zip_bounded(&zip, 1024, 4096).unwrap();
        assert_eq!(extracted.len(), 2);
        assert_eq!(extracted.get("assets/wsa_a.png").unwrap(), &[1, 2, 3, 4]);
    }
}
