//! Windows-safe conversion between filesystem paths and `file://` URIs for LSP.
//!
//! LSP identifies every document by a URI (RFC 3986), never by a raw path: a Windows path like
//! `C:\src\main.rs` is not a valid URI and conformant servers reject it for type-aware operations.
//! We route through the `url` crate (already a dependency) so drive letters, backslash→`/`, the
//! `file:///` prefix, and percent-encoding of reserved characters (spaces, `#`, …) are all handled
//! correctly rather than hand-rolled.
//!
//! A [`normalize_uri`] is provided for comparison / dedup / keying because servers — notably
//! rust-analyzer — may echo a URI back with a different drive-letter case (`c:` vs `C:`) or colon
//! encoding than we sent. Keying must compare a canonical form, not raw bytes, or the same file
//! looks like two.

use anyhow::{anyhow, Result};
use std::path::{Path, PathBuf};

/// Convert an ABSOLUTE filesystem path to a `file://` URI string (e.g. `file:///C:/src/main.rs`).
/// Errors on a relative path or one the `url` crate can't represent (returns `Err(())` there).
pub fn path_to_uri(path: &Path) -> Result<String> {
    url::Url::from_file_path(path)
        .map(|u| u.to_string())
        .map_err(|()| {
            anyhow!(
                "cannot build a file:// URI from path (must be absolute): {}",
                path.display()
            )
        })
}

/// Parse a `file://` URI back into a local filesystem path.
pub fn uri_to_path(uri: &str) -> Result<PathBuf> {
    let url = url::Url::parse(uri).map_err(|e| anyhow!("invalid URI {uri:?}: {e}"))?;
    if url.scheme() != "file" {
        return Err(anyhow!("not a file:// URI: {uri:?}"));
    }
    url.to_file_path()
        .map_err(|()| anyhow!("file:// URI has no local path: {uri:?}"))
}

/// A canonical form of a `file://` URI for comparison / dedup / keying. Round-trips through the path
/// (normalizing slashes + percent-encoding to whatever `url` emits) and lowercases a Windows drive
/// letter so `C:` and `c:` (rust-analyzer emits lowercase) compare equal. Falls back to a trimmed
/// copy when the input isn't a parseable local `file://` URI (e.g. on a non-Windows host fed a
/// Windows path), so it is always total and never panics.
pub fn normalize_uri(uri: &str) -> String {
    match uri_to_path(uri).ok().and_then(|p| path_to_uri(&p).ok()) {
        Some(canon) => lower_drive_letter(&canon),
        None => uri.trim().to_string(),
    }
}

/// Lowercase the drive letter in a `file:///X:/…` URI (Windows). No-op for any other shape.
fn lower_drive_letter(uri: &str) -> String {
    const PREFIX: &str = "file:///";
    let bytes = uri.as_bytes();
    let drive = PREFIX.len(); // index of the drive letter, if present
    if uri.starts_with(PREFIX)
        && bytes.len() > drive + 1
        && bytes[drive].is_ascii_alphabetic()
        && bytes[drive + 1] == b':'
        && bytes[drive].is_ascii_uppercase()
    {
        let mut s = uri.to_string();
        s.replace_range(
            drive..drive + 1,
            &uri[drive..drive + 1].to_ascii_lowercase(),
        );
        s
    } else {
        uri.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_relative_path() {
        assert!(path_to_uri(Path::new("relative/path.rs")).is_err());
    }

    #[test]
    fn rejects_non_file_uri() {
        assert!(uri_to_path("http://example.com/x").is_err());
        assert!(uri_to_path("not a uri").is_err());
    }

    #[test]
    fn normalize_is_total_on_garbage() {
        // Never panics; returns a trimmed fallback for unparseable input.
        assert_eq!(normalize_uri("  not a uri  "), "not a uri");
    }

    #[cfg(windows)]
    #[test]
    fn windows_path_roundtrip_and_encoding() {
        let p = Path::new(r"C:\src\my project\main.rs");
        let uri = path_to_uri(p).expect("absolute windows path → uri");
        // forward slashes, drive letter kept, space percent-encoded, three leading slashes.
        assert!(uri.starts_with("file:///C:/"), "got {uri}");
        assert!(uri.contains("my%20project"), "space must be %20: {uri}");
        let back = uri_to_path(&uri).expect("uri → path");
        assert_eq!(back, p);
    }

    #[cfg(windows)]
    #[test]
    fn windows_normalize_lowercases_drive_and_unifies_case() {
        let upper = "file:///C:/src/main.rs";
        let lower = "file:///c:/src/main.rs";
        assert_eq!(
            normalize_uri(upper),
            normalize_uri(lower),
            "C: and c: must key equal"
        );
        assert!(normalize_uri(upper).starts_with("file:///c:/"));
    }

    #[cfg(not(windows))]
    #[test]
    fn unix_path_roundtrip_and_encoding() {
        let p = Path::new("/src/my project/main.rs");
        let uri = path_to_uri(p).expect("absolute unix path → uri");
        assert!(uri.starts_with("file:///src/"), "got {uri}");
        assert!(uri.contains("my%20project"), "space must be %20: {uri}");
        let back = uri_to_path(&uri).expect("uri → path");
        assert_eq!(back, p);
    }
}
