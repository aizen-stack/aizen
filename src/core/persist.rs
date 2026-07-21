//! Crash-safe byte persistence shared by metadata stores.
//!
//! The writer never truncates the destination in place: it writes a uniquely named sibling,
//! flushes it, and only then replaces the destination. Callers can therefore recover the last
//! complete generation after a process kill or a disk-full error.

use anyhow::{Context, Result};
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

static SEQ: AtomicU64 = AtomicU64::new(0);

fn temp_path(path: &Path) -> PathBuf {
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("state");
    parent.join(format!(
        ".{name}.aizen-tmp-{}-{}",
        std::process::id(),
        SEQ.fetch_add(1, Ordering::Relaxed)
    ))
}

/// Atomically replace `path` with `bytes`.
///
/// The temporary file is created with `create_new`, so concurrent writers never share a staging
/// path. Existing Unix mode bits are copied before the rename. The final path is never truncated.
pub fn atomic_write(path: &Path, bytes: &[u8]) -> Result<()> {
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;

    let tmp = temp_path(path);

    let result = (|| -> Result<()> {
        let mut opts = OpenOptions::new();
        opts.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600);
        }
        let mut file = opts
            .open(&tmp)
            .with_context(|| format!("creating temporary {}", tmp.display()))?;
        file.write_all(bytes)
            .with_context(|| format!("writing temporary {}", tmp.display()))?;
        file.sync_all()
            .with_context(|| format!("flushing temporary {}", tmp.display()))?;

        // Keep the existing mode on Unix. This is intentionally best-effort: the data durability
        // path must not turn a recoverable write into a data-loss path because chmod failed.
        #[cfg(unix)]
        if let Ok(meta) = fs::metadata(path) {
            let _ = fs::set_permissions(&tmp, meta.permissions());
        }

        replace_file(&tmp, path)
            .with_context(|| format!("replacing {}", path.display()))?;
        Ok(())
    })();

    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result
}

fn replace_file(tmp: &Path, dst: &Path) -> std::io::Result<()> {
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        use windows_sys::Win32::Storage::FileSystem::{MoveFileExW, MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH};
        let from: Vec<u16> = tmp.as_os_str().encode_wide().chain(std::iter::once(0)).collect();
        let to: Vec<u16> = dst.as_os_str().encode_wide().chain(std::iter::once(0)).collect();
        let ok = unsafe {
            MoveFileExW(from.as_ptr(), to.as_ptr(), MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH)
        };
        if ok == 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(())
    }
    #[cfg(not(windows))]
    {
        fs::rename(tmp, dst)
    }
}

/// Atomically replace a sensitive file, hardening the staged bytes before they become visible.
/// The final path is checked again after replacement so callers never silently commit a transcript
/// or credential with inherited broad permissions.
pub fn atomic_write_owner_only(path: &Path, bytes: &[u8]) -> Result<()> {
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
    let tmp = temp_path(path);
    let result = (|| -> Result<()> {
        let mut opts = OpenOptions::new();
        opts.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600);
        }
        let mut file = opts
            .open(&tmp)
            .with_context(|| format!("creating temporary {}", tmp.display()))?;
        file.write_all(bytes)
            .with_context(|| format!("writing temporary {}", tmp.display()))?;
        file.sync_all()
            .with_context(|| format!("flushing temporary {}", tmp.display()))?;
        drop(file);
        harden_owner_only_checked(&tmp)?;
        replace_file(&tmp, path).with_context(|| format!("replacing {}", path.display()))?;
        harden_owner_only_checked(path)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result
}

/// Owner-only hardening for a newly created metadata file. Unix applies mode 0600. Windows replaces
/// the inherited DACL with an SDDL owner-rights ACL (`OW`: object owner) and protects it from parent
/// inheritance. Callers that store transcripts/secrets should propagate this result.
pub fn harden_owner_only_checked(path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))
            .with_context(|| format!("hardening {}", path.display()))?;
    }
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        use std::ptr::null_mut;
        use windows_sys::Win32::Foundation::LocalFree;
        use windows_sys::Win32::Security::Authorization::{
            ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
        };
        use windows_sys::Win32::Security::{
            SetFileSecurityW, DACL_SECURITY_INFORMATION, PROTECTED_DACL_SECURITY_INFORMATION,
            PSECURITY_DESCRIPTOR,
        };

        let sddl: Vec<u16> = "D:P(A;;FA;;;OW)"
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect();
        let mut descriptor: PSECURITY_DESCRIPTOR = null_mut();
        let ok = unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                sddl.as_ptr(),
                SDDL_REVISION_1,
                &mut descriptor,
                null_mut(),
            )
        };
        if ok == 0 {
            return Err(std::io::Error::last_os_error())
                .with_context(|| format!("building owner-only ACL for {}", path.display()));
        }
        let wide: Vec<u16> = path.as_os_str().encode_wide().chain(std::iter::once(0)).collect();
        let applied = unsafe {
            SetFileSecurityW(
                wide.as_ptr(),
                DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
                descriptor,
            )
        };
        unsafe { LocalFree(descriptor) };
        if applied == 0 {
            return Err(std::io::Error::last_os_error())
                .with_context(|| format!("applying owner-only ACL to {}", path.display()));
        }
    }
    Ok(())
}

/// Read a file while distinguishing a missing file from a present-but-invalid/unreadable file.
pub fn read_optional(path: &Path) -> std::io::Result<Option<Vec<u8>>> {
    match fs::read(path) {
        Ok(bytes) => Ok(Some(bytes)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e),
    }
}

/// Remove a file only when it exists; other errors are returned.
pub fn remove_if_exists(path: &Path) -> std::io::Result<bool> {
    match fs::remove_file(path) {
        Ok(()) => Ok(true),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e),
    }
}
