//! Self-host as a systemd service so the bot stays alive on a Linux VPS (auto-restart on crash,
//! auto-start on reboot). `aizen serve --install` writes the unit; `--uninstall` removes it. On
//! non-Linux there's no systemd, so we print platform-appropriate guidance (NSSM / launchd) and stop.
//!
//! "Always alive" here means "a service on a running VPS that self-recovers + starts on boot" — NOT
//! surviving a powered-off VPS (nothing does). `Restart=always` handles crashes + graceful exits;
//! `network-online.target` makes it wait for the network after a reboot.

#[cfg(target_os = "linux")]
use anyhow::Context;
use anyhow::Result;
#[cfg(target_os = "linux")]
use console::style;

#[cfg(target_os = "linux")]
use crate::ui::splash;

/// Render the systemd unit that runs `aizen serve` as an always-on service. `exec` is the absolute
/// path to this binary; `user` picks the install target (`default.target` for a `--user` unit vs
/// `multi-user.target` for a system unit). Pure — tested without touching systemctl.
#[cfg(any(target_os = "linux", test))]
fn systemd_unit_text(exec: &std::path::Path, workdir: &std::path::Path, user: bool) -> String {
    let wanted_by = if user {
        "default.target"
    } else {
        "multi-user.target"
    };
    format!(
        "[Unit]\n\
         Description=aizen — Telegram control daemon\n\
         After=network-online.target\n\
         Wants=network-online.target\n\
         \n\
         [Service]\n\
         Type=simple\n\
         ExecStart={exec} serve\n\
         WorkingDirectory={workdir}\n\
         Restart=always\n\
         RestartSec=3\n\
         \n\
         [Install]\n\
         WantedBy={wanted_by}\n",
        exec = exec.display(),
        workdir = workdir.display(),
    )
}

/// `aizen serve --install` / `--uninstall`: wire (or remove) the systemd service. On non-Linux we
/// can't do systemd, so we print equivalent guidance and stop. A user-mode unit is installed directly
/// (no root); a system unit needs root, so we print the `sudo` steps unless we already are root.
#[cfg(target_os = "linux")]
pub async fn run_serve_service(
    install: bool,
    uninstall: bool,
    user: bool,
    now: bool,
) -> Result<()> {
    let exec = std::env::current_exe().context("finding the aizen binary path")?;
    let home = std::env::var("HOME")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| std::path::PathBuf::from("/"));
    // Root check without a libc dep: `id -u` prints 0 for root.
    let is_root = std::process::Command::new("id")
        .arg("-u")
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim() == "0")
        .unwrap_or(false);

    if uninstall {
        if user {
            let path = home.join(".config/systemd/user/aizen-serve.service");
            let _ = std::process::Command::new("systemctl")
                .args(["--user", "disable", "--now", "aizen-serve"])
                .status();
            let _ = std::fs::remove_file(&path);
            let _ = std::process::Command::new("systemctl")
                .args(["--user", "daemon-reload"])
                .status();
            println!(
                "{}",
                style(format!("✓ removed user service ({})", path.display()))
                    .color256(splash::ACCENT)
            );
        } else if is_root {
            let path = std::path::PathBuf::from("/etc/systemd/system/aizen-serve.service");
            let _ = std::process::Command::new("systemctl")
                .args(["disable", "--now", "aizen-serve"])
                .status();
            let _ = std::fs::remove_file(&path);
            let _ = std::process::Command::new("systemctl")
                .arg("daemon-reload")
                .status();
            println!(
                "{}",
                style("✓ removed system service").color256(splash::ACCENT)
            );
        } else {
            println!("Run these as root to remove the system service:");
            println!("  sudo systemctl disable --now aizen-serve");
            println!("  sudo rm /etc/systemd/system/aizen-serve.service");
            println!("  sudo systemctl daemon-reload");
        }
        return Ok(());
    }

    if install {
        if user {
            let dir = home.join(".config/systemd/user");
            std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
            let path = dir.join("aizen-serve.service");
            std::fs::write(&path, systemd_unit_text(&exec, &home, true))
                .with_context(|| format!("writing {}", path.display()))?;
            println!(
                "{}",
                style(format!("✓ wrote {}", path.display())).color256(splash::ACCENT)
            );
            if now {
                let _ = std::process::Command::new("systemctl")
                    .args(["--user", "daemon-reload"])
                    .status();
                let st = std::process::Command::new("systemctl")
                    .args(["--user", "enable", "--now", "aizen-serve"])
                    .status();
                // Linger keeps the user service alive when you're not logged in (the whole point on a VPS).
                let _ = std::process::Command::new("loginctl")
                    .args(["enable-linger"])
                    .arg(whoami_user())
                    .status();
                match st {
                    Ok(s) if s.success() => println!("{}", style("✓ enabled + started (survives logout + reboot via linger)").color256(splash::ACCENT)),
                    _ => println!("{}", style("wrote the unit, but `systemctl --user enable --now` failed — run it manually.").yellow()),
                }
            } else {
                println!("Then enable it (survives reboot):");
                println!("  systemctl --user daemon-reload");
                println!("  systemctl --user enable --now aizen-serve");
                println!("  loginctl enable-linger $USER   # keep it alive when logged out");
            }
        } else if is_root {
            let path = std::path::PathBuf::from("/etc/systemd/system/aizen-serve.service");
            std::fs::write(&path, systemd_unit_text(&exec, &home, false))
                .with_context(|| format!("writing {}", path.display()))?;
            println!(
                "{}",
                style(format!("✓ wrote {}", path.display())).color256(splash::ACCENT)
            );
            if now {
                let _ = std::process::Command::new("systemctl")
                    .arg("daemon-reload")
                    .status();
                let _ = std::process::Command::new("systemctl")
                    .args(["enable", "--now", "aizen-serve"])
                    .status();
                println!(
                    "{}",
                    style("✓ enabled + started (auto-starts on reboot)").color256(splash::ACCENT)
                );
            } else {
                println!("Then: systemctl daemon-reload && systemctl enable --now aizen-serve");
            }
        } else {
            // System install needs root — print the unit + the sudo steps rather than silently failing.
            let path = "/etc/systemd/system/aizen-serve.service";
            println!(
                "System install needs root. Either re-run with sudo, or use --user (no root):"
            );
            println!("  aizen serve --install --user --now");
            println!("\nOr write this unit as root at {path}:\n");
            println!("{}", systemd_unit_text(&exec, &home, false));
            println!(
                "Then: sudo systemctl daemon-reload && sudo systemctl enable --now aizen-serve"
            );
        }
        return Ok(());
    }

    // Neither flag → nothing to do (the caller only routes here when one is set).
    Ok(())
}

/// Best-effort current username for `loginctl enable-linger` (falls back to `$USER`).
#[cfg(target_os = "linux")]
fn whoami_user() -> String {
    std::env::var("USER").unwrap_or_else(|_| "root".to_string())
}

/// Non-Linux: no systemd. Explain the platform-appropriate path and stop.
#[cfg(not(target_os = "linux"))]
pub async fn run_serve_service(
    _install: bool,
    _uninstall: bool,
    _user: bool,
    _now: bool,
) -> Result<()> {
    println!("`aizen serve --install` wires a systemd service — Linux only.");
    if cfg!(target_os = "windows") {
        println!("On Windows, run the daemon as a service with NSSM (https://nssm.cc):");
        println!("  nssm install aizen \"<path>\\aizen.exe\" serve");
        println!("or as a Scheduled Task set to run at logon with 'Restart on failure'.");
    } else {
        println!("On macOS, wrap `aizen serve` in a launchd plist under ~/Library/LaunchAgents.");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn systemd_unit_has_restart_and_exec() {
        let exec = std::path::Path::new("/home/vps/.cargo/bin/aizen");
        let workdir = std::path::Path::new("/home/vps");
        let user_unit = systemd_unit_text(exec, workdir, true);
        assert!(
            user_unit.contains("Restart=always"),
            "must auto-restart on crash/exit"
        );
        assert!(
            user_unit.contains("ExecStart=/home/vps/.cargo/bin/aizen serve"),
            "runs `serve`"
        );
        assert!(
            user_unit.contains("WantedBy=default.target"),
            "--user targets default.target"
        );
        assert!(
            user_unit.contains("network-online.target"),
            "waits for network after reboot"
        );

        let sys_unit = systemd_unit_text(exec, workdir, false);
        assert!(
            sys_unit.contains("WantedBy=multi-user.target"),
            "system unit targets multi-user.target"
        );
    }
}
