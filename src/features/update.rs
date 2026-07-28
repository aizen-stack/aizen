//! Self-update and rollback support.
//!
//! Aizen installs a downloaded release beside the running executable. It never re-execs or kills the
//! current process: the current terminal keeps its loaded image, while a new terminal resolves the
//! replaced path and starts the selected version.

use anyhow::{bail, Context, Result};
use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use std::cmp::Ordering;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::io::AsyncWriteExt;

const DEFAULT_REPO: &str = "dawnofcd/aizen";
const CHECK_TTL_SECS: u64 = 24 * 60 * 60;
const MAX_BINARY_BYTES: u64 = 300 * 1024 * 1024;
const HTTP_TIMEOUT_SECS: u64 = 120;

#[derive(Debug, Clone, PartialEq, Eq)]
struct Version {
    major: u64,
    minor: u64,
    patch: u64,
    pre: Option<String>,
}

impl Ord for Version {
    fn cmp(&self, other: &Self) -> Ordering {
        (self.major, self.minor, self.patch)
            .cmp(&(other.major, other.minor, other.patch))
            .then_with(|| match (&self.pre, &other.pre) {
                (None, None) => Ordering::Equal,
                (None, Some(_)) => Ordering::Greater,
                (Some(_), None) => Ordering::Less,
                (Some(a), Some(b)) => a.cmp(b),
            })
    }
}

impl PartialOrd for Version {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ReleaseInfo {
    tag: String,
    name: String,
    published: String,
    prerelease: bool,
    asset_url: String,
    asset_name: String,
    asset_size: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct UpdateCache {
    checked_unix: u64,
    latest_tag: String,
    for_version: String,
}

fn current_version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

/// Print one line to whichever surface owns the screen.
///
/// `/update` runs both from the CLI and from inside the REPL, where the retained render thread
/// repaints over anything written straight to the terminal. `tui::emit_line` routes into that
/// thread's buffer when it is running and falls back to stdout otherwise, so it is correct on both.
fn emit(line: &str) {
    crate::ui::tui::emit_line(line);
}

fn parse_version(raw: &str) -> Option<Version> {
    let raw = raw.trim().trim_start_matches('v');
    let (core, pre) = raw
        .split_once('-')
        .map_or((raw, None), |(a, b)| (a, Some(b.to_string())));
    let mut parts = core.split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next()?.parse().ok()?;
    let patch = parts.next()?.parse().ok()?;
    if parts.next().is_some() || core.is_empty() {
        return None;
    }
    Some(Version {
        major,
        minor,
        patch,
        pre,
    })
}

fn is_newer(candidate: &str, current: &str) -> bool {
    match (parse_version(candidate), parse_version(current)) {
        (Some(a), Some(b)) => a > b,
        _ => false,
    }
}

fn asset_suffix_for(os: &str, arch: &str) -> Option<&'static str> {
    match (os, arch) {
        ("windows", "x86_64") => Some("windows-x86_64.exe"),
        ("linux", "x86_64") => Some("linux-x86_64"),
        ("macos", "aarch64") => Some("macos-aarch64"),
        _ => None,
    }
}

fn asset_suffix() -> Option<&'static str> {
    asset_suffix_for(std::env::consts::OS, std::env::consts::ARCH)
}

fn repo() -> String {
    std::env::var("AIZEN_UPDATE_REPO")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| DEFAULT_REPO.to_string())
}

fn parse_releases(json: &str, suffix: &str) -> Vec<ReleaseInfo> {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(json) else {
        return Vec::new();
    };
    let Some(items) = value.as_array() else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for item in items {
        if item
            .get("draft")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false)
        {
            continue;
        }
        let tag = item
            .get("tag_name")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("");
        if parse_version(tag).is_none() {
            continue;
        }
        let Some(assets) = item.get("assets").and_then(serde_json::Value::as_array) else {
            continue;
        };
        let Some(asset) = assets.iter().find(|a| {
            a.get("name")
                .and_then(serde_json::Value::as_str)
                .is_some_and(|n| n.ends_with(suffix))
        }) else {
            continue;
        };
        let Some(asset_name) = asset.get("name").and_then(serde_json::Value::as_str) else {
            continue;
        };
        let Some(asset_url) = asset
            .get("browser_download_url")
            .and_then(serde_json::Value::as_str)
        else {
            continue;
        };
        out.push(ReleaseInfo {
            tag: tag.to_string(),
            name: item
                .get("name")
                .and_then(serde_json::Value::as_str)
                .unwrap_or(tag)
                .to_string(),
            published: item
                .get("published_at")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("")
                .to_string(),
            prerelease: item
                .get("prerelease")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false),
            asset_url: asset_url.to_string(),
            asset_name: asset_name.to_string(),
            asset_size: asset
                .get("size")
                .and_then(serde_json::Value::as_u64)
                .unwrap_or(0),
        });
    }
    out.sort_by(|a, b| parse_version(&b.tag).cmp(&parse_version(&a.tag)));
    out
}

fn http_client() -> Result<reqwest::Client> {
    reqwest::Client::builder()
        .user_agent(concat!("aizen/", env!("CARGO_PKG_VERSION")))
        .timeout(Duration::from_secs(HTTP_TIMEOUT_SECS))
        .build()
        .context("building update client")
}

async fn list_releases(limit: usize) -> Result<Vec<ReleaseInfo>> {
    let suffix = asset_suffix().ok_or_else(|| {
        anyhow::anyhow!(
            "no published Aizen asset for this platform ({} / {})",
            std::env::consts::OS,
            std::env::consts::ARCH
        )
    })?;
    let url = format!(
        "https://api.github.com/repos/{}/releases?per_page={}",
        repo(),
        limit.clamp(1, 100)
    );
    crate::core::net_guard::guard_url_async(&url).await?;
    let client = http_client()?;
    let mut req = client
        .get(&url)
        .header("Accept", "application/vnd.github+json")
        .header("X-GitHub-Api-Version", "2022-11-28");
    if let Some(token) = crate::core::cli_config::load()
        .reach
        .and_then(|r| r.resolved_github_token())
    {
        req = req.bearer_auth(token);
    }
    let response = req.send().await.context("fetching GitHub releases")?;
    let status = response.status();
    let body = response.text().await.context("reading GitHub releases")?;
    if !status.is_success() {
        bail!(
            "GitHub releases returned HTTP {}: {}",
            status.as_u16(),
            body.chars().take(200).collect::<String>()
        );
    }
    Ok(parse_releases(&body, suffix))
}

async fn download_to(url: &str, part: &Path, expected_size: u64) -> Result<u64> {
    let client = http_client()?;
    let response = client
        .get(url)
        .send()
        .await
        .with_context(|| format!("downloading {url}"))?;
    if !response.status().is_success() {
        bail!("download returned HTTP {}", response.status().as_u16());
    }
    if response
        .content_length()
        .is_some_and(|n| n > MAX_BINARY_BYTES)
    {
        bail!("release asset is too large");
    }
    let mut file = tokio::fs::File::create(part)
        .await
        .with_context(|| format!("creating {}", part.display()))?;
    let mut stream = response.bytes_stream();
    let mut total = 0u64;
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.context("reading release asset")?;
        total = total.saturating_add(chunk.len() as u64);
        if total > MAX_BINARY_BYTES {
            let _ = tokio::fs::remove_file(part).await;
            bail!(
                "release asset exceeds {} MiB",
                MAX_BINARY_BYTES / 1024 / 1024
            );
        }
        tokio::io::AsyncWriteExt::write_all(&mut file, &chunk)
            .await
            .context("writing release asset")?;
    }
    file.flush().await.context("flushing release asset")?;
    drop(file);
    if expected_size > 0 && expected_size != total {
        let _ = tokio::fs::remove_file(part).await;
        bail!("release asset size mismatch (expected {expected_size}, downloaded {total})");
    }
    Ok(total)
}

fn backup_path(target: &Path) -> PathBuf {
    let name = target
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("aizen");
    target.with_file_name(format!(
        "{name}.old-{}-{}",
        current_version(),
        std::process::id()
    ))
}

fn swap_in_place(staged: &Path, target: &Path) -> Result<PathBuf> {
    let backup = backup_path(target);
    if backup.exists() {
        let _ = fs::remove_file(&backup);
    }
    fs::rename(target, &backup)
        .with_context(|| format!("staging current executable {}", target.display()))?;
    if let Err(error) = fs::rename(staged, target) {
        let _ = fs::rename(&backup, target);
        return Err(error)
            .with_context(|| format!("installing new executable {}", target.display()));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(target, fs::Permissions::from_mode(0o755));
    }
    Ok(backup)
}

pub fn cleanup_stale_backups(dir: &Path, live_exe: &Path) {
    let Some(live_name) = live_exe.file_name().and_then(|s| s.to_str()) else {
        return;
    };
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Some(name) = path.file_name().and_then(|s| s.to_str()) else {
            continue;
        };
        if name != live_name && name.starts_with("aizen") && name.contains(".old-") {
            let _ = fs::remove_file(path);
        }
    }
}

fn cache_path() -> PathBuf {
    crate::core::config::aizen_home().join("update-check.json")
}

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn should_check(last: Option<u64>, now: u64) -> bool {
    last.map_or(true, |last| now.saturating_sub(last) >= CHECK_TTL_SECS)
}

fn read_cache() -> Option<UpdateCache> {
    serde_json::from_slice(&fs::read(cache_path()).ok()?).ok()
}

fn write_cache(latest_tag: &str) -> Result<()> {
    let cache = UpdateCache {
        checked_unix: now_unix(),
        latest_tag: latest_tag.to_string(),
        for_version: current_version().to_string(),
    };
    let bytes = serde_json::to_vec_pretty(&cache)?;
    crate::core::persist::atomic_write(&cache_path(), &bytes)
}

pub fn cached_notice() -> Option<String> {
    let cache = read_cache()?;
    (cache.for_version == current_version() && is_newer(&cache.latest_tag, current_version()))
        .then(|| format!("⬆ aizen {} is available — run /update", cache.latest_tag))
}

pub fn spawn_background_check() {
    if !crate::core::cli_config::update_check_enabled(&crate::core::cli_config::load()) {
        return;
    }
    if !should_check(read_cache().map(|c| c.checked_unix), now_unix()) {
        return;
    }
    tokio::spawn(async {
        if let Ok(releases) = list_releases(10).await {
            if let Some(latest) = releases
                .iter()
                .filter(|r| !r.prerelease)
                .max_by(|a, b| parse_version(&a.tag).cmp(&parse_version(&b.tag)))
            {
                let _ = write_cache(&latest.tag);
            }
        }
    });
}

/// Human-readable asset size. `0` means GitHub did not report one.
fn fmt_size(bytes: u64) -> String {
    if bytes == 0 {
        return "—".to_string();
    }
    format!("{:.1} MiB", bytes as f64 / 1024.0 / 1024.0)
}

/// The `YYYY-MM-DD` head of an ISO-8601 `published_at`, or empty when absent.
fn fmt_published(raw: &str) -> &str {
    raw.split('T').next().unwrap_or("")
}

/// True when `tag` names the build this process is running.
fn is_running_tag(tag: &str) -> bool {
    tag.trim_start_matches('v') == current_version()
}

/// Newest non-prerelease build — the picker's default landing row and what the silent check compares
/// against, so a pre-release never becomes the suggested upgrade.
fn newest_stable(releases: &[ReleaseInfo]) -> Option<&ReleaseInfo> {
    releases
        .iter()
        .filter(|r| !r.prerelease)
        .max_by(|a, b| parse_version(&a.tag).cmp(&parse_version(&b.tag)))
}

/// One row of the picker: the tag, how it relates to the running build, when it shipped, how big.
///
/// The running build is marked instead of hidden, so the list doubles as the version check — there
/// is no separate "which version am I on" command to remember.
fn label_for(release: &ReleaseInfo) -> String {
    let kind = if is_running_tag(&release.tag) {
        "● installed now"
    } else if release.prerelease {
        "pre-release"
    } else if is_newer(&release.tag, current_version()) {
        "newer"
    } else {
        "older (roll back)"
    };
    format!(
        "{:<10} {:<18} {:<11} {}",
        release.tag,
        kind,
        fmt_published(&release.published),
        fmt_size(release.asset_size)
    )
}

/// The whole feature: show the versions, let one be picked, install it.
///
/// This is the only entry point — `aizen update` and `/update` both land here with no arguments.
/// It owns stdin through dialoguer, so callers must have suspended the retained frame (see
/// `ui::tui::slash_takes_stdin`).
pub async fn run() -> Result<()> {
    let target = std::env::current_exe().context("resolving the aizen executable path")?;
    let releases = list_releases(50).await?;
    if releases.is_empty() {
        emit(&format!(
            "no published release carries an asset for {} / {}",
            std::env::consts::OS,
            std::env::consts::ARCH
        ));
        return Ok(());
    }
    if let Some(latest) = newest_stable(&releases) {
        let _ = write_cache(&latest.tag);
    }

    let labels: Vec<String> = releases.iter().map(label_for).collect();
    // The list is newest-first; land the cursor on the newest stable build, which is what most runs
    // want, while leaving every older tag one keypress away for a rollback.
    let default = releases.iter().position(|r| !r.prerelease).unwrap_or(0);
    emit(&format!(
        "aizen {} · pick a version to install (Esc to cancel)",
        current_version()
    ));
    let Some(index) = dialoguer::Select::new()
        .with_prompt("Version")
        .items(&labels)
        .default(default)
        .interact_opt()
        .unwrap_or(None)
    else {
        emit("cancelled");
        return Ok(());
    };
    let selected = releases[index].clone();
    if is_running_tag(&selected.tag) {
        emit(&format!(
            "aizen {} is already running — nothing to install",
            current_version()
        ));
        return Ok(());
    }
    install(&selected, &target).await
}

/// Download `release` and put it where the running executable lives.
///
/// Never re-execs and never signals the current process: the running image is renamed aside as a
/// backup and the download takes its path, so this session keeps the build it started with and the
/// next terminal picks up the installed one.
async fn install(release: &ReleaseInfo, target: &Path) -> Result<()> {
    let version = release.tag.trim_start_matches('v');
    emit(&format!(
        "downloading {} ({})",
        release.asset_name,
        fmt_size(release.asset_size)
    ));
    let part = target.with_file_name(format!(".aizen-update-{}.part", std::process::id()));
    let size = download_to(&release.asset_url, &part, release.asset_size).await?;
    let backup = match swap_in_place(&part, target) {
        Ok(path) => path,
        Err(error) => {
            let _ = fs::remove_file(&part);
            return Err(error.context(format!(
                "could not replace {} — check write access, or set AIZEN_INSTALL to an install dir you own",
                target.display()
            )));
        }
    };
    emit(&format!(
        "installed aizen {} ({}) to {}",
        version,
        fmt_size(size),
        target.display()
    ));
    emit(&format!("previous build kept at {}", backup.display()));
    emit(&format!(
        "this session keeps running {}; close this terminal and open a new one to use {}",
        current_version(),
        version
    ));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions_compare_numerically_and_release_beats_pre() {
        assert!(is_newer("0.4.10", "0.4.9"));
        assert!(is_newer("v0.5.0", "0.4.8"));
        assert!(is_newer("0.4.8", "0.4.8-rc1"));
        assert!(!is_newer("0.4.8", "0.4.8"));
    }

    #[test]
    fn assets_match_published_matrix() {
        assert_eq!(
            asset_suffix_for("windows", "x86_64"),
            Some("windows-x86_64.exe")
        );
        assert_eq!(asset_suffix_for("linux", "x86_64"), Some("linux-x86_64"));
        assert_eq!(asset_suffix_for("macos", "aarch64"), Some("macos-aarch64"));
        assert_eq!(asset_suffix_for("macos", "x86_64"), None);
    }

    #[test]
    fn release_parser_filters_drafts_and_missing_assets() {
        let json = r#"[
          {"tag_name":"v0.5.0","name":"five","draft":false,"prerelease":false,"published_at":"today","assets":[{"name":"aizen-v0.5.0-windows-x86_64.exe","browser_download_url":"https://x/a","size":12}]},
          {"tag_name":"v0.4.9","draft":true,"assets":[{"name":"aizen-v0.4.9-windows-x86_64.exe","browser_download_url":"https://x/b","size":10}]},
          {"tag_name":"v0.4.8","draft":false,"assets":[]}
        ]"#;
        let out = parse_releases(json, "windows-x86_64.exe");
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].tag, "v0.5.0");
    }

    #[test]
    fn check_ttl_is_twenty_four_hours() {
        assert!(!should_check(Some(100), 100 + CHECK_TTL_SECS - 1));
        assert!(should_check(Some(100), 100 + CHECK_TTL_SECS));
        assert!(should_check(None, 100));
    }

    /// A scratch directory unique per test, so two swaps never race on the same backup name.
    fn scratch(tag: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!("aizen-update-{}-{tag}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        root
    }

    #[test]
    fn swap_preserves_old_bytes_in_backup() {
        let root = scratch("swap");
        let target = root.join("aizen");
        let staged = root.join("new.part");
        fs::write(&target, b"old").unwrap();
        fs::write(&staged, b"new").unwrap();
        let backup = swap_in_place(&staged, &target).unwrap();
        assert_eq!(fs::read(&target).unwrap(), b"new");
        assert_eq!(fs::read(backup).unwrap(), b"old");
        let _ = fs::remove_dir_all(root);
    }

    /// The invariant the whole feature rests on: a process holding the old image keeps reading the
    /// old bytes through its existing handle, and the swap still succeeds. On Windows this is the
    /// difference between rename-aside (works on a running `.exe`) and replace-in-place (fails).
    #[test]
    fn swap_succeeds_while_a_handle_is_open_and_that_handle_still_sees_old_bytes() {
        use std::io::Read;
        let root = scratch("live");
        let target = root.join("aizen");
        let staged = root.join("new.part");
        fs::write(&target, b"old-image").unwrap();
        fs::write(&staged, b"new-image").unwrap();

        let mut live = fs::File::open(&target).unwrap();
        let backup = swap_in_place(&staged, &target).expect("swap must work with the image open");

        let mut seen = Vec::new();
        live.read_to_end(&mut seen).unwrap();
        assert_eq!(
            seen, b"old-image",
            "the running image must be unaffected by the swap"
        );
        assert_eq!(fs::read(&target).unwrap(), b"new-image");
        assert_eq!(fs::read(&backup).unwrap(), b"old-image");
        drop(live);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn cleanup_removes_stale_backups_but_never_the_live_binary() {
        let root = scratch("sweep");
        let live = root.join("aizen.exe");
        let stale = root.join("aizen.exe.old-0.4.7-123");
        let unrelated = root.join("notes.txt");
        fs::write(&live, b"live").unwrap();
        fs::write(&stale, b"stale").unwrap();
        fs::write(&unrelated, b"keep").unwrap();

        cleanup_stale_backups(&root, &live);

        assert!(live.exists(), "the running binary must survive the sweep");
        assert!(!stale.exists(), "a previous update's backup must be swept");
        assert!(unrelated.exists(), "unrelated files must be left alone");
        let _ = fs::remove_dir_all(root);
    }
}
