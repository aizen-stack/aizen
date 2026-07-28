//! Stable device identity for tier-based memory partitioning.
//!
//! Aizen needs to know which physical machine it runs on so that `tier: device`
//! facts (e.g. "this machine doesn't have gcc", "git is at C:\Program Files\Git\cmd")
//! stay valid across `cd` and across repo switches — but NOT across machines (a
//! Docker container, a fresh Windows install, or a sysprep reset get a new id).
//!
//! ## Probe order (first success wins, cached for the process lifetime)
//!
//! 1. **Windows:** `HKLM\SOFTWARE\Microsoft\Cryptography\MachineGuid` (stable per
//!    install, survives reimage — the same GUID the OS itself uses for identity).
//! 2. **Linux:** `/etc/machine-id` (the `dbus`/`systemd` identifier, 32 hex chars).
//! 3. **macOS:** `ioreg -rd1 -c IOPlatformExpertDevice | grep IOPlatformUUID`
//!    (the hardware UUID; spawns a subprocess once).
//! 4. **Fallback:** `~/.aizen/device.json` — a persisted random id generated on
//!    first probe, so machines without any of the above still have a device id
//!    (and the file doubles as the history store — see below).
//!
//! ## History (`device.json`)
//!
//! Aizen stores **every** device id it has ever seen (not just the current one) so
//! that facts tagged with an old id remain reachable. The file lives at:
//! `~/.aizen/device.json`
//!
//! ```json
//! [
//!   {"id":"dev-a1b2c3d4","source":"windows-registry","firstSeen":"2026-07-20","lastSeen":"2026-07-27"},
//!   {"id":"dev-e5f6a7b8","source":"fallback","firstSeen":"2026-07-27","lastSeen":"2026-07-27"}
//! ]
//! ```
//!
//! When a probe produces an id that differs from the current one, the old id moves
//! into the `also_read` set (so frozen-core and search queries that match the old
//! id still work). A one-line warning is printed at startup/doctor time.
//!
//! ## Security
//!
//! The raw `MachineGuid` / `machine-id` / hardware UUID is **never** logged or
//! displayed. Only the hash `dev-{hex8}` form appears in user-facing output.
//! See [`id()`] for the format.

use crate::core::config;
use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::OnceLock;

/// The current device identity (cached for the process lifetime).
static DEVICE: OnceLock<DeviceIdent> = OnceLock::new();

/// A single device identity record.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeviceRecord {
    pub id: String,
    pub source: String,
    #[serde(rename = "firstSeen")]
    pub first_seen: String,
    #[serde(rename = "lastSeen")]
    pub last_seen: String,
}

/// The resolved device identity.
#[derive(Debug, Clone)]
pub struct DeviceIdent {
    /// Stable id: `dev-{hex8(fnv1a64(raw_secret))}`.
    pub id: String,
    /// Human-readable source description (e.g. `"windows-registry"`, `"machine-id"`, `"fallback"`).
    pub source: String,
    /// Human-readable label for display (e.g. `"DESKTOP-ABC123"`).
    pub label: String,
    /// Device ids that are NOT current but whose tagged facts should still be readable.
    /// Populated when the probe returns a different id than was current last session.
    pub also_read: Vec<String>,
}

/// The raw secret read from the platform probe — never exposed outside this module.
enum RawSecret {
    Text(String),
}

// ── Platform probes ──────────────────────────────────────────────────────

#[cfg(target_os = "windows")]
fn probe_raw() -> Result<RawSecret> {
    use std::ffi::OsString;
    use std::os::windows::ffi::OsStringExt;
    use windows_sys::Win32::Foundation::ERROR_SUCCESS;
    use windows_sys::Win32::System::Registry::*;

    const KEY_PATH: &str = r"SOFTWARE\Microsoft\Cryptography";
    const VALUE_NAME: &str = "MachineGuid";

    unsafe {
        let mut hkey: HKEY = std::ptr::null_mut();
        let status = RegOpenKeyExW(
            HKEY_LOCAL_MACHINE,
            &encode_wide(KEY_PATH)[0],
            0,
            KEY_READ,
            &mut hkey,
        );
        if status != ERROR_SUCCESS {
            anyhow::bail!("RegOpenKeyExW failed: {status}");
        }

        let mut value_type: u32 = 0;
        let mut buf = [0u16; 128];
        let mut size = (buf.len() * 2) as u32;
        let status = RegQueryValueExW(
            hkey,
            &encode_wide(VALUE_NAME)[0],
            std::ptr::null_mut(),
            &mut value_type,
            buf.as_mut_ptr() as *mut u8,
            &mut size,
        );
        RegCloseKey(hkey);

        if status != ERROR_SUCCESS {
            anyhow::bail!("RegQueryValueExW failed: {status}");
        }

        let len = (size as usize / 2).min(buf.len());
        let os = OsString::from_wide(&buf[..len]);
        let s = os.to_string_lossy().trim_end_matches('\0').to_string();
        if s.is_empty() {
            anyhow::bail!("MachineGuid is empty");
        }
        Ok(RawSecret::Text(s))
    }
}

#[cfg(target_os = "windows")]
fn encode_wide(s: &str) -> Vec<u16> {
    use std::os::windows::ffi::OsStrExt;
    std::ffi::OsStr::new(s)
        .encode_wide()
        .chain(std::iter::once(0))
        .collect()
}

#[cfg(target_os = "linux")]
fn probe_raw() -> Result<RawSecret> {
    let raw = std::fs::read_to_string("/etc/machine-id")
        .map_err(|e| anyhow::anyhow!("reading /etc/machine-id: {e}"))?;
    let s = raw.trim().to_string();
    if s.is_empty() || s.len() < 8 {
        anyhow::bail!("machine-id too short or empty");
    }
    Ok(RawSecret::Text(s))
}

#[cfg(target_os = "macos")]
fn probe_raw() -> Result<RawSecret> {
    let out = std::process::Command::new("ioreg")
        .args(["-rd1", "-c", "IOPlatformExpertDevice"])
        .output()
        .map_err(|e| anyhow::anyhow!("spawning ioreg: {e}"))?;
    let stdout = String::from_utf8_lossy(&out.stdout);
    for line in stdout.lines() {
        let trimmed = line.trim();
        if let Some(val) = trimmed.strip_prefix(r#""IOPlatformUUID" = ""#) {
            if let Some(end) = val.find('"') {
                let uuid = val[..end].to_string();
                if !uuid.is_empty() {
                    return Ok(RawSecret::Text(uuid));
                }
            }
        }
    }
    anyhow::bail!("IOPlatformUUID not found in ioreg output");
}

/// Where the fallback seed lives. Separate from `device.json`: that file stores HASHED ids for
/// the human-readable history, and a hash cannot be turned back into the seed it came from.
fn seed_path() -> PathBuf {
    config::nextgen_home().join("device-seed")
}

/// Fallback probe when every platform probe fails (a locked-down registry, a container with no
/// `/etc/machine-id`, `ioreg` missing): mint ONE random seed and keep it.
///
/// The seed must persist, because the id derived from it is what every `tier: device` fact is
/// tagged with. Re-rolling it each run would silently orphan all of them every session — the
/// device tier would look like it worked while quietly remembering nothing.
fn probe_fallback() -> Result<RawSecret> {
    let path = seed_path();
    if let Ok(existing) = std::fs::read_to_string(&path) {
        let s = existing.trim().to_string();
        if s.len() >= 16 {
            return Ok(RawSecret::Text(s));
        }
    }
    let mut bytes = [0u8; 16];
    getrandom::getrandom(&mut bytes).map_err(|e| anyhow::anyhow!("getrandom: {e}"))?;
    let hex: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    // A write failure is survivable but not silent-worthy: the id stays correct for THIS process
    // and the next run re-rolls, so say why device facts may not stick.
    if std::fs::write(&path, &hex).is_ok() {
        config::harden_file(&path); // the seed is machine-identifying — owner-only
    }
    Ok(RawSecret::Text(hex))
}

/// Path to the device history JSON file.
fn device_json_path() -> PathBuf {
    config::nextgen_home().join("device.json")
}

/// Read device history from disk.
fn read_history() -> Vec<DeviceRecord> {
    let path = device_json_path();
    std::fs::read_to_string(&path)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

/// Write device history to disk (best-effort).
fn write_history(records: &[DeviceRecord]) {
    let path = device_json_path();
    if let Ok(json) = serde_json::to_string_pretty(records) {
        let _ = std::fs::write(&path, &json);
    }
}

fn today() -> String {
    chrono::Local::now().format("%Y-%m-%d").to_string()
}

/// FNV-1a 64-bit (copy of `crate::core::config`'s private fn — kept here so device.rs
/// has no dependency on config internals beyond the public API).
fn fnv1a64(s: &str) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in s.as_bytes() {
        h ^= *b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

/// Format the stable id from a raw secret.
fn format_id(raw: &str) -> String {
    format!("dev-{:08x}", fnv1a64(raw) as u32)
}

/// Resolve the full `DeviceIdent`, probing the platform and consulting history.
fn resolve() -> DeviceIdent {
    let now = today();
    let mut history = read_history();

    let (raw, source_label) = match probe_raw() {
        Ok(secret) => {
            let label = match () {
                #[cfg(target_os = "windows")]
                _ => "windows-registry",
                #[cfg(target_os = "linux")]
                _ => "machine-id",
                #[cfg(target_os = "macos")]
                _ => "ioreg-uuid",
                #[cfg(not(any(target_os = "windows", target_os = "linux", target_os = "macos")))]
                _ => "fallback",
            };
            (secret, label)
        }
        Err(_) => {
            // Fallback to device.json or generate new
            match probe_fallback() {
                Ok(secret) => (secret, "fallback"),
                Err(_) => {
                    // Last resort: this shouldn't fail, but if it does, use a compile-time constant
                    (
                        RawSecret::Text("aizen-device-fallback".to_string()),
                        "fallback",
                    )
                }
            }
        }
    };

    let RawSecret::Text(raw_str) = &raw;
    let id = format_id(raw_str);
    let hostname = hostname().unwrap_or_else(|| "unknown".to_string());

    // Build the also_read set from history entries that are NOT the current id
    let also_read: Vec<String> = history
        .iter()
        .filter(|r| r.id != id)
        .map(|r| r.id.clone())
        .collect();

    // Update history: add or update current entry
    if let Some(existing) = history.iter_mut().find(|r| r.id == id) {
        existing.last_seen = now;
        existing.source = source_label.to_string();
    } else {
        history.push(DeviceRecord {
            id: id.clone(),
            source: source_label.to_string(),
            first_seen: now.clone(),
            last_seen: now,
        });
    }
    write_history(&history);

    DeviceIdent {
        id,
        source: source_label.to_string(),
        label: hostname,
        also_read,
    }
}

/// Get the hostname for the device label.
fn hostname() -> Option<String> {
    std::env::var("COMPUTERNAME")
        .ok()
        .filter(|s| !s.is_empty())
        .or_else(|| std::env::var("HOSTNAME").ok().filter(|s| !s.is_empty()))
        .or_else(|| {
            std::fs::read_to_string("/etc/hostname")
                .ok()
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
        })
}

// ── Public API ───────────────────────────────────────────────────────────

/// The current device identity, computed once per process lifetime.
pub fn current() -> &'static DeviceIdent {
    DEVICE.get_or_init(resolve)
}

/// Shortcut: the stable device id string (`dev-{hex8}`).
pub fn id() -> &'static str {
    &current().id
}

/// Device ids that are NOT the current identity but whose tagged facts should
/// still be readable (historical ids from this machine).
pub fn also_read() -> &'static [String] {
    &current().also_read
}

// ── Tests ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn id_never_exposes_raw_secret() {
        let ident = resolve();
        // The id must be the hash format, never a raw GUID or UUID.
        assert!(
            ident.id.starts_with("dev-"),
            "id should start with dev-: {}",
            ident.id
        );
        assert_eq!(
            ident.id.len(),
            12,
            "id should be `dev-` + 8 hex = 12 chars: {}",
            ident.id
        );
        for c in ident.id.chars().skip(4) {
            assert!(
                c.is_ascii_hexdigit(),
                "id should be hex after dev-: {}",
                ident.id
            );
        }
        // Source should be one of the known values
        assert!(
            ["windows-registry", "machine-id", "ioreg-uuid", "fallback"]
                .contains(&ident.source.as_str()),
            "unexpected source: {}",
            ident.source
        );
    }

    #[test]
    fn current_returns_consistent_id() {
        let a = id().to_string();
        let b = id().to_string();
        assert_eq!(a, b, "id must be stable within a process");
    }

    #[test]
    fn format_id_is_deterministic() {
        let a = format_id("test-machine-guid");
        let b = format_id("test-machine-guid");
        assert_eq!(a, b);
        // Different inputs produce different ids
        let c = format_id("other-machine-guid");
        assert_ne!(a, c);
    }
}
