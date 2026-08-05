//! Server table, binary discovery, and workspace-root resolution with safety locks.
//!
//! The lifecycle/transport layer is language-neutral; this module is the only per-language part —
//! a small table mapping a language to (server command, file extensions, project manifests). Adding
//! a language is one row.
//!
//! Root resolution mirrors what the language's own build tool does: walk UP from the current file/
//! directory to the nearest ancestor that holds a manifest (`Cargo.toml`, …), exactly like
//! `cargo build` / `npm` find their project. Two safety locks (see plan §2A) ensure a server is
//! never pointed at a giant or system tree: (1) no manifest found ⇒ no root (so a server never
//! starts), and (2) the filesystem root and the user's home directory are forbidden as roots even
//! if a stray manifest sits there.

use anyhow::{anyhow, Result};
use std::path::{Path, PathBuf};

/// Max directories to climb looking for a project manifest (matches `project_context`'s cap).
const MAX_CLIMB: usize = 40;

/// A language the LSP subsystem knows how to serve.
#[derive(Debug, Clone, Copy)]
pub struct ServerSpec {
    /// Stable language id / label, e.g. `"rust"`.
    pub lang: &'static str,
    /// The server executable to resolve on PATH, e.g. `"rust-analyzer"`.
    pub command: &'static str,
    /// Arguments the server needs to speak LSP over stdio (node servers require `--stdio`).
    pub args: &'static [&'static str],
    /// File extensions this server handles (lowercase, no dot), e.g. `["rs"]`.
    pub extensions: &'static [&'static str],
    /// Project manifest/marker filenames; the workspace root is the nearest ancestor directory
    /// containing one of these, e.g. `["Cargo.toml"]`.
    pub manifests: &'static [&'static str],
}

/// Built-in server table — the chosen v1 languages (Rust + Python + JS/TS). The transport/lifecycle
/// is language-neutral; a language is one row. The node-based servers ship as `.cmd` shims on
/// Windows: `which` resolves them (PATHEXT-aware) and the spawned `cmd.exe`→`node` tree is reaped
/// via a Job Object (see `jobobject.rs`), so they tear down as cleanly as a native `.exe`.
pub const SERVERS: &[ServerSpec] = &[
    ServerSpec {
        lang: "rust",
        command: "rust-analyzer",
        args: &[],
        extensions: &["rs"],
        manifests: &["Cargo.toml"],
    },
    ServerSpec {
        lang: "python",
        command: "pyright-langserver",
        args: &["--stdio"],
        extensions: &["py", "pyi"],
        manifests: &[
            "pyproject.toml",
            "setup.py",
            "setup.cfg",
            "requirements.txt",
        ],
    },
    ServerSpec {
        lang: "typescript",
        command: "typescript-language-server",
        args: &["--stdio"],
        extensions: &["ts", "tsx", "js", "jsx", "mjs", "cjs", "mts", "cts"],
        manifests: &["package.json", "tsconfig.json", "jsconfig.json"],
    },
];

/// LSP `languageId` for a file, per the spec's well-known ids (typescript-language-server routes on
/// it: `.tsx` must be `typescriptreact`, `.js` must be `javascript`, …). Falls back to the server's
/// language label for unknown extensions.
pub fn language_id_for(path: &Path, fallback: &str) -> String {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase())
        .unwrap_or_default();
    match ext.as_str() {
        "rs" => "rust",
        "py" | "pyi" => "python",
        "ts" | "mts" | "cts" => "typescript",
        "tsx" => "typescriptreact",
        "js" | "mjs" | "cjs" => "javascript",
        "jsx" => "javascriptreact",
        _ => fallback,
    }
    .to_string()
}

/// Pick the server for a file extension (lowercase compare, leading dot tolerated). `None` =
/// unsupported language ⇒ LSP stays out of the way and the agent uses text search.
pub fn server_for_extension(ext: &str) -> Option<&'static ServerSpec> {
    let ext = ext.trim_start_matches('.').to_ascii_lowercase();
    SERVERS
        .iter()
        .find(|s| s.extensions.iter().any(|e| *e == ext))
}

/// Pick the server for a path by its file extension.
pub fn server_for_path(path: &Path) -> Option<&'static ServerSpec> {
    let ext = path.extension()?.to_str()?;
    server_for_extension(ext)
}

/// Detect the language server + workspace root for an `anchor` path (a file OR a directory). A file
/// of a known type binds to ITS language's server (a `.rs` file in a folder with only a
/// `package.json` is honestly "no rust project" — never mis-routed to the typescript server);
/// otherwise the first language whose project manifest is found at/above the anchor wins. `None` ⇒
/// no supported project here, so LSP stays out of the way.
pub fn detect(anchor: &Path) -> Option<(&'static ServerSpec, PathBuf)> {
    if anchor.is_file() {
        if let Some(spec) = server_for_path(anchor) {
            let start = anchor.parent().unwrap_or(anchor);
            let root = resolve_workspace_root(start, spec.manifests)?;
            return Some((spec, root));
        }
    }
    let start = if anchor.is_dir() {
        anchor
    } else {
        anchor.parent().unwrap_or(anchor)
    };
    for spec in SERVERS {
        if let Some(root) = resolve_workspace_root(start, spec.manifests) {
            return Some((spec, root));
        }
    }
    None
}

/// Resolve the server's executable to an absolute path, honoring Windows `PATHEXT` (so a node server
/// installed as `name.cmd` resolves — a bare-name spawn would `ENOENT`). For Rust it also tries
/// `rustup which rust-analyzer` as a fallback, since the common `rustup component add` install isn't
/// always directly on PATH. Returns `Err` when the server isn't installed — callers MUST downgrade
/// to a graceful skip, never abort the agent turn.
pub fn resolve_server_binary(spec: &ServerSpec) -> Result<PathBuf> {
    if let Some(p) = which_on_path(spec.command) {
        return Ok(p);
    }
    if spec.lang == "rust" {
        if let Some(p) = rustup_which("rust-analyzer") {
            return Ok(p);
        }
    }
    Err(anyhow!(
        "language server '{}' not found on PATH — install it to enable LSP for {} \
         (the agent falls back to text search)",
        spec.command,
        spec.lang
    ))
}

/// Hand-rolled `which`: walk `PATH`, trying `PATHEXT` extensions on Windows (std/tokio `Command`
/// don't consult PATHEXT, and node servers install as `.cmd` shims — a bare-name spawn ENOENTs).
/// Hand-rolled rather than the `which` crate because that crate's `winsafe` dependency links
/// `ktmw32`, which the windows-gnu toolchain's slim self-contained MinGW cannot resolve.
fn which_on_path(cmd: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    let dirs: Vec<PathBuf> = std::env::split_paths(&path).collect();
    let exts: Vec<String> = if cfg!(windows) {
        std::env::var("PATHEXT")
            .unwrap_or_else(|_| ".COM;.EXE;.BAT;.CMD".to_string())
            .split(';')
            .filter(|e| !e.is_empty())
            .map(str::to_string)
            .collect()
    } else {
        Vec::new()
    };
    search_dirs(&dirs, &exts, cmd)
}

/// The pure core of [`which_on_path`] (injectable for tests). On Windows (`exts` non-empty) a bare
/// name is only tried with the PATHEXT extensions — a bare name is not executable there; an
/// explicit extension in `cmd` is tried verbatim. On Unix (`exts` empty) the name is tried as-is.
fn search_dirs(dirs: &[PathBuf], exts: &[String], cmd: &str) -> Option<PathBuf> {
    let try_exact = exts.is_empty() || cmd.contains('.');
    for dir in dirs {
        if dir.as_os_str().is_empty() {
            continue;
        }
        if try_exact {
            let p = dir.join(cmd);
            if p.is_file() {
                return Some(p);
            }
        }
        for ext in exts {
            let p = dir.join(format!("{cmd}{ext}"));
            if p.is_file() {
                return Some(p);
            }
        }
    }
    None
}

/// Best-effort `rustup which <bin>` → absolute path (rust-analyzer added via `rustup component`).
fn rustup_which(bin: &str) -> Option<PathBuf> {
    let out = std::process::Command::new("rustup")
        .arg("which")
        .arg(bin)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let path = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if path.is_empty() {
        return None;
    }
    let p = PathBuf::from(path);
    p.exists().then_some(p)
}

/// Resolve the workspace root for `start` by walking UP to the nearest ancestor directory containing
/// one of `manifests`. Returns `None` (so the caller does NOT start a server) when no manifest is
/// found within [`MAX_CLIMB`], or when the would-be root is a [forbidden root](is_forbidden_root) —
/// the filesystem root or the user's home directory. This is the core "never index a giant/system
/// tree" guarantee.
pub fn resolve_workspace_root(start: &Path, manifests: &[&str]) -> Option<PathBuf> {
    let start = start.canonicalize().unwrap_or_else(|_| start.to_path_buf());
    let mut cur: &Path = &start;
    for _ in 0..MAX_CLIMB {
        if manifests.iter().any(|m| cur.join(m).is_file()) {
            return if is_forbidden_root(cur) {
                None
            } else {
                Some(cur.to_path_buf())
            };
        }
        match cur.parent() {
            Some(p) => cur = p,
            None => break, // reached the filesystem root with no manifest
        }
    }
    None
}

/// Roots a server must never be anchored at: the filesystem root (`C:\`, `/`) or the user's home
/// directory. Pointing a server here would invite it to index an enormous, unrelated tree.
fn is_forbidden_root(dir: &Path) -> bool {
    if dir.parent().is_none() {
        return true; // filesystem root (e.g. `C:\` or `/`)
    }
    match home_dir() {
        Some(home) => same_dir(dir, &home),
        None => false,
    }
}

/// The user's home directory from the environment (`USERPROFILE` on Windows, `HOME` elsewhere).
fn home_dir() -> Option<PathBuf> {
    let var = if cfg!(windows) { "USERPROFILE" } else { "HOME" };
    std::env::var_os(var)
        .map(PathBuf::from)
        .filter(|p| !p.as_os_str().is_empty())
}

/// Compare two directories by their canonical form (so `C:\x` vs `\\?\C:\x` etc. compare equal).
fn same_dir(a: &Path, b: &Path) -> bool {
    let ca = a.canonicalize().unwrap_or_else(|_| a.to_path_buf());
    let cb = b.canonicalize().unwrap_or_else(|_| b.to_path_buf());
    ca == cb
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An isolated temp dir for a test (mirrors `project_context`'s sandbox pattern).
    fn sandbox(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("aizen-lsp-disc-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn extension_mapping() {
        assert_eq!(server_for_extension("rs").map(|s| s.lang), Some("rust"));
        assert_eq!(
            server_for_extension("RS").map(|s| s.lang),
            Some("rust"),
            "case-insensitive"
        );
        assert_eq!(
            server_for_extension(".rs").map(|s| s.lang),
            Some("rust"),
            "leading dot tolerated"
        );
        assert_eq!(server_for_extension("py").map(|s| s.lang), Some("python"));
        assert_eq!(
            server_for_extension("tsx").map(|s| s.lang),
            Some("typescript")
        );
        assert_eq!(
            server_for_extension("mjs").map(|s| s.lang),
            Some("typescript")
        );
        assert!(server_for_extension("go").is_none(), "gopls is a later row");
        assert!(server_for_extension("").is_none());
    }

    #[test]
    fn language_ids() {
        assert_eq!(language_id_for(Path::new("a/m.rs"), "rust"), "rust");
        assert_eq!(language_id_for(Path::new("a/m.py"), "python"), "python");
        assert_eq!(
            language_id_for(Path::new("a/m.ts"), "typescript"),
            "typescript"
        );
        assert_eq!(
            language_id_for(Path::new("a/m.tsx"), "typescript"),
            "typescriptreact"
        );
        assert_eq!(
            language_id_for(Path::new("a/m.js"), "typescript"),
            "javascript"
        );
        assert_eq!(
            language_id_for(Path::new("a/m.jsx"), "typescript"),
            "javascriptreact"
        );
        assert_eq!(
            language_id_for(Path::new("a/Makefile"), "python"),
            "python",
            "fallback"
        );
    }

    #[test]
    fn known_ext_file_never_misroutes_to_another_language() {
        // A `.rs` file whose project has only a package.json: honestly "no rust project" (None),
        // NOT a bogus route to the typescript server.
        let root = sandbox("misroute");
        std::fs::write(root.join("package.json"), "{}\n").unwrap();
        let f = root.join("lib.rs");
        std::fs::write(&f, "fn x() {}\n").unwrap();
        assert!(detect(&f).is_none());
        // …while the directory anchor legitimately detects the typescript project.
        let (spec, _) = detect(&root).expect("dir anchor finds the js/ts project");
        assert_eq!(spec.lang, "typescript");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn path_mapping() {
        assert_eq!(
            server_for_path(Path::new("a/b/main.rs")).map(|s| s.lang),
            Some("rust")
        );
        assert!(
            server_for_path(Path::new("a/b/README")).is_none(),
            "no extension → none"
        );
    }

    #[test]
    fn resolves_nearest_manifest_from_subdir() {
        let root = sandbox("nearest");
        std::fs::write(root.join("Cargo.toml"), "[package]\n").unwrap();
        let deep = root.join("src").join("agent");
        std::fs::create_dir_all(&deep).unwrap();
        let got = resolve_workspace_root(&deep, &["Cargo.toml"]).expect("should find the root");
        assert_eq!(
            got,
            root.canonicalize().unwrap(),
            "resolves to the Cargo.toml dir"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn innermost_manifest_wins() {
        let root = sandbox("inner");
        std::fs::write(root.join("Cargo.toml"), "[workspace]\n").unwrap();
        let sub = root.join("crates").join("inner");
        std::fs::create_dir_all(&sub).unwrap();
        std::fs::write(sub.join("Cargo.toml"), "[package]\n").unwrap();
        let deep = sub.join("src");
        std::fs::create_dir_all(&deep).unwrap();
        let got = resolve_workspace_root(&deep, &["Cargo.toml"]).expect("found");
        assert_eq!(
            got,
            sub.canonicalize().unwrap(),
            "nearest (inner) manifest wins"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn no_manifest_anywhere_returns_none() {
        // A bare temp dir whose ancestors (…/Temp, …, C:\ or /) hold no Cargo.toml → no root → no
        // server. This is the "open at a generic/huge folder, index nothing" guarantee.
        // Same cross-test home hazard as `detect_by_dir_and_by_file` — serialize on the same lock.
        let _g = crate::core::config::TEST_HOME_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let dir = sandbox("none");
        assert!(resolve_workspace_root(&dir, &["Cargo.toml"]).is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn detect_by_dir_and_by_file() {
        // `is_forbidden_root` reads USERPROFILE/HOME at call time, and several suites elsewhere point
        // those at their own sandbox while they run. Without this lock `detect` here can observe
        // another test's home, mistake our temp root for it, and return None — a cross-test flake
        // that only ever appeared in the full suite, never when this test ran alone.
        let _g = crate::core::config::TEST_HOME_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let root = sandbox("detect");
        std::fs::write(root.join("Cargo.toml"), "[package]\n").unwrap();
        let src = root.join("src");
        std::fs::create_dir_all(&src).unwrap();
        let main_rs = src.join("main.rs");
        std::fs::write(&main_rs, "fn main() {}\n").unwrap();
        let canon = root.canonicalize().unwrap();
        // anchor = a directory inside the project
        let (spec, r) = detect(&src).expect("detect from dir");
        assert_eq!(spec.lang, "rust");
        assert_eq!(r, canon);
        // anchor = a .rs file
        let (spec, r) = detect(&main_rs).expect("detect from file");
        assert_eq!(spec.lang, "rust");
        assert_eq!(r, canon);
        // anchor with no project → None
        let bare = sandbox("detect-none");
        assert!(detect(&bare).is_none());
        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&bare);
    }

    #[test]
    fn path_search_honors_pathext_and_exact_names() {
        let dir = sandbox("which");
        std::fs::write(dir.join("srv.cmd"), "@echo off\n").unwrap();
        std::fs::write(dir.join("plain"), "#!/bin/sh\n").unwrap();
        let dirs = vec![dir.clone()];
        let exts = vec![".EXE".to_string(), ".cmd".to_string()];
        // Bare name + PATHEXT → finds the .cmd shim (the node-server case).
        let hit = search_dirs(&dirs, &exts, "srv").expect("resolves srv.cmd");
        assert!(hit
            .to_string_lossy()
            .to_ascii_lowercase()
            .ends_with("srv.cmd"));
        // Explicit extension → tried verbatim.
        assert!(search_dirs(&dirs, &exts, "srv.cmd").is_some());
        // Unix mode (no exts) → exact name only.
        assert!(search_dirs(&dirs, &[], "plain").is_some());
        assert!(
            search_dirs(&dirs, &exts, "plain").is_none(),
            "bare name isn't executable on Windows"
        );
        assert!(search_dirs(&dirs, &exts, "missing").is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn filesystem_root_is_forbidden() {
        let root = if cfg!(windows) {
            Path::new(r"C:\")
        } else {
            Path::new("/")
        };
        assert!(
            is_forbidden_root(root),
            "the filesystem root must never be a workspace root"
        );
    }

    #[test]
    fn home_dir_is_forbidden() {
        // Reads USERPROFILE/HOME twice — once here, once inside `is_forbidden_root` — so it must hold
        // the env lock: another suite repointing home between the two reads made this fail
        // intermittently in full runs. Same hazard as `detect_by_dir_and_by_file`.
        let _g = crate::core::config::TEST_HOME_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if let Some(home) = home_dir() {
            if home.exists() {
                assert!(
                    is_forbidden_root(&home),
                    "the home directory must never be a workspace root"
                );
            }
        }
    }
}
