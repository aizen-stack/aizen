//! Image (vision) input — turn a clipboard image (Ctrl-O) or a referenced/dragged image FILE into
//! an OpenAI-compatible `image_url` data URL the chat layer attaches to a user message.
//!
//! Two ways in, because terminals can't deliver Ctrl-V image paste (Windows Terminal intercepts
//! Ctrl-V for its own paste):
//!  1. **File path** — drag an image file onto the window (the terminal pastes its path), or type/
//!     paste a path; `extract_image_attachments` pulls it off the input line on Enter. Pure +
//!     cross-platform, no clipboard.
//!  2. **Clipboard image** — press Ctrl-O to grab a copied screenshot (Win+Shift+S / "Copy image").
//!     DESKTOP-ONLY (Windows/macOS via `arboard`): on Linux `arboard` needs X11/Wayland libs at
//!     runtime, which would break the headless static binary `ng serve` ships as — so it's
//!     `cfg`-gated and a no-op stub elsewhere.

use anyhow::{Context, Result};

/// Hard ceiling on the image bytes (before base64). A larger image is rejected, not sent.
const MAX_BYTES: usize = 8 * 1024 * 1024;
/// Longest-side pixel cap for a clipboard screenshot (downscaled before encoding). Files pass
/// through as-is (only size-capped) since we don't decode them.
#[cfg(any(windows, target_os = "macos"))]
const MAX_DIM: u32 = 1568;

/// Image extensions we accept as drag-drop / path attachments.
const IMAGE_EXTS: &[&str] = &["png", "jpg", "jpeg", "gif", "webp"];

/// Base64-encode (standard alphabet, padded). Hand-rolled (encode-only) to avoid a dependency.
pub fn base64_encode(input: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(input.len().div_ceil(3) * 4);
    for chunk in input.chunks(3) {
        let b0 = chunk[0] as usize;
        let b1 = chunk.get(1).copied().unwrap_or(0) as usize;
        let b2 = chunk.get(2).copied().unwrap_or(0) as usize;
        out.push(T[b0 >> 2] as char);
        out.push(T[((b0 & 0x03) << 4) | (b1 >> 4)] as char);
        out.push(if chunk.len() > 1 {
            T[((b1 & 0x0f) << 2) | (b2 >> 6)] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            T[b2 & 0x3f] as char
        } else {
            '='
        });
    }
    out
}

/// Build a `data:<mime>;base64,<…>` URL.
fn data_url(mime: &str, bytes: &[u8]) -> String {
    format!("data:{mime};base64,{}", base64_encode(bytes))
}

/// Detect an image mime from leading magic bytes. `None` if not a recognized image.
fn sniff_mime(b: &[u8]) -> Option<&'static str> {
    if b.starts_with(&[0x89, b'P', b'N', b'G']) {
        Some("image/png")
    } else if b.starts_with(&[0xFF, 0xD8, 0xFF]) {
        Some("image/jpeg")
    } else if b.starts_with(b"GIF87a") || b.starts_with(b"GIF89a") {
        Some("image/gif")
    } else if b.len() >= 12 && &b[0..4] == b"RIFF" && &b[8..12] == b"WEBP" {
        Some("image/webp")
    } else {
        None
    }
}

/// Read an image FILE → a data URL (cross-platform). Mime sniffed from magic bytes; non-image or
/// oversize files are rejected.
pub fn image_file_to_data_url(path: &str) -> Result<String> {
    let bytes = std::fs::read(path).with_context(|| format!("reading image {path}"))?;
    anyhow::ensure!(
        bytes.len() <= MAX_BYTES,
        "{path} is too large ({} MB; max {} MB)",
        bytes.len() / 1_048_576,
        MAX_BYTES / 1_048_576
    );
    let mime =
        sniff_mime(&bytes).with_context(|| format!("{path} is not a PNG/JPEG/GIF/WebP image"))?;
    Ok(data_url(mime, &bytes))
}

/// Pull image-file attachments off an input line (drag a file onto the terminal → it pastes the
/// path; a typed/pasted path works too). Returns (text with those paths removed, data URLs). A
/// token becomes an attachment ONLY when it's an existing file with an image extension that decodes
/// — so mentioning a name that isn't a real image stays as text (no false positives). Quote-aware
/// (drag-drop quotes paths containing spaces).
pub fn extract_image_attachments(line: &str) -> (String, Vec<String>) {
    let mut text: Vec<String> = Vec::new();
    let mut images: Vec<String> = Vec::new();
    for tok in tokenize_quoted(line) {
        if has_image_ext(&tok) {
            if let Ok(url) = image_file_to_data_url(&tok) {
                images.push(url);
                continue; // consumed as an attachment → drop from the text
            }
        }
        text.push(tok);
    }
    (text.join(" ").trim().to_string(), images)
}

/// True when `path` ends in an image extension AND is an existing file.
fn has_image_ext(path: &str) -> bool {
    let ext_ok = std::path::Path::new(path)
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| IMAGE_EXTS.contains(&e.to_ascii_lowercase().as_str()))
        .unwrap_or(false);
    ext_ok && std::path::Path::new(path).is_file()
}

/// Whitespace-split, keeping double-quoted spans together and stripping the quotes (drag-drop
/// quotes paths that contain spaces, e.g. `"C:\…\my shot.png"`).
fn tokenize_quoted(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut in_q = false;
    for c in s.chars() {
        match c {
            '"' => in_q = !in_q,
            c if c.is_whitespace() && !in_q => {
                if !cur.is_empty() {
                    out.push(std::mem::take(&mut cur));
                }
            }
            c => cur.push(c),
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

/// Grab an image from the OS clipboard (a copied screenshot) and return it as a PNG data URL —
/// the Ctrl-O action. `Ok(None)` when the clipboard holds no image (it's text / empty / a file
/// reference). Desktop-only — a no-op stub elsewhere so the caller compiles everywhere.
#[cfg(any(windows, target_os = "macos"))]
pub fn clipboard_image_data_url() -> Result<Option<String>> {
    let mut cb = arboard::Clipboard::new().context("opening the clipboard")?;
    let img = match cb.get_image() {
        Ok(i) => i,
        Err(_) => return Ok(None), // no image on the clipboard
    };
    let (w, h) = (img.width as u32, img.height as u32);
    if w == 0 || h == 0 || img.bytes.len() < (w * h * 4) as usize {
        return Ok(None);
    }
    let (rgba, w, h) = downscale_rgba(&img.bytes, w, h, MAX_DIM);
    let png = encode_png(&rgba, w, h).context("encoding the pasted image to PNG")?;
    anyhow::ensure!(
        png.len() <= MAX_BYTES,
        "pasted image is too large ({} MB after encoding; max {} MB)",
        png.len() / 1_048_576,
        MAX_BYTES / 1_048_576
    );
    Ok(Some(data_url("image/png", &png)))
}

#[cfg(not(any(windows, target_os = "macos")))]
pub fn clipboard_image_data_url() -> Result<Option<String>> {
    Ok(None)
}

/// Nearest-neighbour downscale of an RGBA8 buffer so the longest side ≤ `max_dim` (returns the
/// source unchanged when already within bounds). Cheap + dependency-free.
#[cfg(any(windows, target_os = "macos"))]
fn downscale_rgba(src: &[u8], w: u32, h: u32, max_dim: u32) -> (Vec<u8>, u32, u32) {
    if w.max(h) <= max_dim {
        return (src.to_vec(), w, h);
    }
    let scale = max_dim as f64 / w.max(h) as f64;
    let nw = ((w as f64 * scale).round() as u32).max(1);
    let nh = ((h as f64 * scale).round() as u32).max(1);
    let mut out = vec![0u8; (nw * nh * 4) as usize];
    for y in 0..nh {
        let sy = (y * h / nh).min(h - 1);
        for x in 0..nw {
            let sx = (x * w / nw).min(w - 1);
            let si = ((sy * w + sx) * 4) as usize;
            let di = ((y * nw + x) * 4) as usize;
            out[di..di + 4].copy_from_slice(&src[si..si + 4]);
        }
    }
    (out, nw, nh)
}

/// Encode an RGBA8 buffer to PNG bytes (pure-Rust `png` crate).
#[cfg(any(windows, target_os = "macos"))]
fn encode_png(rgba: &[u8], w: u32, h: u32) -> Result<Vec<u8>> {
    let mut buf = Vec::new();
    {
        let mut enc = png::Encoder::new(&mut buf, w, h);
        enc.set_color(png::ColorType::Rgba);
        enc.set_depth(png::BitDepth::Eight);
        let mut writer = enc.write_header().context("png header")?;
        writer.write_image_data(rgba).context("png data")?;
    }
    Ok(buf)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_matches_known_vectors() {
        // RFC 4648 test vectors.
        assert_eq!(base64_encode(b""), "");
        assert_eq!(base64_encode(b"f"), "Zg==");
        assert_eq!(base64_encode(b"fo"), "Zm8=");
        assert_eq!(base64_encode(b"foo"), "Zm9v");
        assert_eq!(base64_encode(b"foob"), "Zm9vYg==");
        assert_eq!(base64_encode(b"fooba"), "Zm9vYmE=");
        assert_eq!(base64_encode(b"foobar"), "Zm9vYmFy");
    }

    #[test]
    fn base64_encodes_high_bytes() {
        assert_eq!(base64_encode(&[0xff, 0xff, 0xff]), "////");
        assert_eq!(base64_encode(&[0x00]), "AA==");
    }

    #[test]
    fn data_url_shape() {
        assert_eq!(data_url("image/png", b"foo"), "data:image/png;base64,Zm9v");
    }

    #[test]
    fn sniff_mime_detects_formats() {
        assert_eq!(
            sniff_mime(&[0x89, b'P', b'N', b'G', 0, 0]),
            Some("image/png")
        );
        assert_eq!(sniff_mime(&[0xFF, 0xD8, 0xFF, 0xE0]), Some("image/jpeg"));
        assert_eq!(sniff_mime(b"GIF89a...."), Some("image/gif"));
        assert_eq!(sniff_mime(b"hello not an image"), None);
    }

    #[test]
    fn tokenize_keeps_quoted_paths_together() {
        let toks = tokenize_quoted(r#"look at "C:\a b\shot.png" please"#);
        assert_eq!(toks, vec!["look", "at", r"C:\a b\shot.png", "please"]);
    }

    fn temp_png(tag: &str) -> std::path::PathBuf {
        // A minimal valid PNG (header + bytes good enough for the magic-byte sniff).
        let p = std::env::temp_dir().join(format!("ng-img-{tag}-{}.png", std::process::id()));
        let mut bytes = vec![0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a];
        bytes.extend_from_slice(b"rest-of-a-tiny-png");
        std::fs::write(&p, bytes).unwrap();
        p
    }

    #[test]
    fn image_file_to_data_url_reads_png() {
        let p = temp_png("read");
        let url = image_file_to_data_url(p.to_str().unwrap()).unwrap();
        assert!(url.starts_with("data:image/png;base64,"));
        std::fs::remove_file(&p).ok();
        // a non-image file is rejected
        let txt = std::env::temp_dir().join(format!("ng-not-img-{}.png", std::process::id()));
        std::fs::write(&txt, b"i am plain text").unwrap();
        assert!(
            image_file_to_data_url(txt.to_str().unwrap()).is_err(),
            "not a real image"
        );
        std::fs::remove_file(&txt).ok();
    }

    #[test]
    fn extract_pulls_existing_image_path_and_keeps_prose() {
        let p = temp_png("extract");
        let path = p.to_str().unwrap();
        let line = format!("what is in {path} ?");
        let (text, imgs) = extract_image_attachments(&line);
        assert_eq!(imgs.len(), 1, "the real image file is attached");
        assert!(imgs[0].starts_with("data:image/png;base64,"));
        assert_eq!(text, "what is in ?", "the path is removed from the prose");
        std::fs::remove_file(&p).ok();
    }

    #[test]
    fn extract_ignores_nonexistent_or_nonimage_tokens() {
        // A path-looking token that isn't a real file stays as text (no false positive).
        let (text, imgs) = extract_image_attachments("describe nope.png and run main.rs");
        assert!(imgs.is_empty());
        assert_eq!(text, "describe nope.png and run main.rs");
    }

    #[cfg(any(windows, target_os = "macos"))]
    #[test]
    fn downscale_caps_longest_side_and_passes_small_through() {
        let src = vec![0u8; 4 * 2 * 4]; // 4×2 RGBA
        let (out, w, h) = downscale_rgba(&src, 4, 2, 2);
        assert_eq!((w, h), (2, 1));
        assert_eq!(out.len(), (w * h * 4) as usize);
        let (out2, w2, h2) = downscale_rgba(&src, 4, 2, 8);
        assert_eq!((w2, h2), (4, 2));
        assert_eq!(out2.len(), src.len());
    }

    #[cfg(any(windows, target_os = "macos"))]
    #[test]
    fn png_round_trips_through_data_url() {
        let png = encode_png(&[255, 0, 0, 255], 1, 1).unwrap();
        assert!(
            png.starts_with(&[0x89, b'P', b'N', b'G']),
            "valid PNG signature"
        );
        assert!(data_url("image/png", &png).starts_with("data:image/png;base64,"));
    }
}
