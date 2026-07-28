//! P6 — one-time local download of a Model2Vec model into `~/.aizen/models/<name>/`.
//!
//! The dense tier loads its model from a local dir (`local-only` — no network at *retrieval*
//! time). This module is the one place that reaches the network, and only on an explicit
//! `aizen memory model-download`: it fetches the three files `model2vec-rs::from_pretrained`
//! reads (`config.json`, `tokenizer.json`, `model.safetensors`) from the Hugging Face CDN and
//! streams them to disk. It is deliberately NOT gated behind the `dense` feature — the download
//! is pure HTTP + file IO, so a user can pre-provision the model before switching to a
//! `--features dense` build. Only *loading* the weights needs the feature.
//!
//! Posture: reuses the repo's rustls-only reqwest stack (no OpenSSL), streams the body chunk by
//! chunk so the ~30 MB safetensors never sits in RAM twice, and writes to a `.part` file renamed
//! on success so an interrupted download never leaves a half file that looks complete.

use crate::core::config;
use anyhow::{bail, Context, Result};
use futures_util::StreamExt;
use std::time::Duration;
use tokio::io::AsyncWriteExt;

/// The three files `model2vec-rs::from_pretrained` reads from a local model folder.
const FILES: [&str; 3] = ["config.json", "tokenizer.json", "model.safetensors"];

/// Hard ceiling per file — the largest legitimate asset is the multilingual safetensors (~530 MB);
/// this bounds a tarpit/redirect-loop from filling the disk. potion-base-8M is ~30 MB.
const MAX_FILE_BYTES: u64 = 700 * 1024 * 1024;

/// Download `name` (default: the configured embed model) from Hugging Face into
/// `models_dir()/<name>/`. Skips files already present so a re-run only fetches what's missing
/// (resumable at file granularity). Returns the resolved model dir.
pub async fn download(name: Option<&str>) -> Result<std::path::PathBuf> {
    let name = name
        .map(str::to_string)
        .unwrap_or_else(config::embed_model_name);
    if name.trim().is_empty() {
        bail!("empty model name");
    }
    let dir = config::models_dir().join(&name);
    std::fs::create_dir_all(&dir)
        .with_context(|| format!("creating model dir {}", dir.display()))?;

    // A dedicated client: unlike the reach layer's, this one FOLLOWS redirects (HF `resolve/`
    // 302s to its CDN) and has no 8 MiB body cap. rustls-only (repo default) → no OpenSSL.
    let client = reqwest::Client::builder()
        .user_agent(concat!("aizen/", env!("CARGO_PKG_VERSION")))
        .timeout(Duration::from_secs(600)) // a 30 MB file on a slow link
        .build()
        .context("building download client")?;

    println!("Downloading model '{name}' → {}", dir.display());
    for file in FILES {
        let dest = dir.join(file);
        if dest.exists() {
            println!("  {file}: already present, skipping");
            continue;
        }
        let url =
            format!("https://huggingface.co/minishlab/{name}/resolve/main/{file}?download=true");
        fetch_to_file(&client, &url, &dest)
            .await
            .with_context(|| format!("downloading {file}"))?;
    }
    println!("Model '{name}' ready. Enable the dense tier with a `--features dense` build.");
    Ok(dir)
}

/// Stream one URL to `dest`, via a `.part` temp renamed on success (an interrupted download never
/// leaves a truncated file that `exists()` would treat as complete).
async fn fetch_to_file(client: &reqwest::Client, url: &str, dest: &std::path::Path) -> Result<()> {
    let resp = client
        .get(url)
        .send()
        .await
        .with_context(|| format!("GET {url}"))?;
    if !resp.status().is_success() {
        bail!("HTTP {} from {url}", resp.status().as_u16());
    }
    if let Some(len) = resp.content_length() {
        if len > MAX_FILE_BYTES {
            bail!("file too large ({len} bytes > {MAX_FILE_BYTES})");
        }
    }
    let part = dest.with_extension("part");
    let mut file = tokio::fs::File::create(&part)
        .await
        .with_context(|| format!("creating {}", part.display()))?;
    let mut written: u64 = 0;
    let mut stream = resp.bytes_stream();
    let name = dest.file_name().and_then(|s| s.to_str()).unwrap_or("file");
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.context("reading response chunk")?;
        written += chunk.len() as u64;
        if written > MAX_FILE_BYTES {
            let _ = tokio::fs::remove_file(&part).await;
            bail!("file exceeded {MAX_FILE_BYTES} bytes mid-stream");
        }
        file.write_all(&chunk)
            .await
            .context("writing chunk to disk")?;
    }
    file.flush().await.context("flushing file")?;
    drop(file);
    tokio::fs::rename(&part, dest)
        .await
        .with_context(|| format!("finalizing {}", dest.display()))?;
    println!("  {name}: {} KB", written / 1024);
    Ok(())
}
