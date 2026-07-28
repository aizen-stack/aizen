//! LSP (Language Server Protocol) subsystem — gives the agent IDE-grade, type-aware code
//! navigation (find-references / go-to-definition / document & workspace symbols) plus on-demand
//! diagnostics by talking to a per-language language server (rust-analyzer, pyright,
//! typescript-language-server, …) over JSON-RPC/stdio, instead of relying on text/grep matching.
//!
//! Design (see `.claude/plans/lsp-integration-plan.md`):
//! - **Default ON + lazy.** The manager is armed at session start (runtime ready); nothing spawns
//!   until a query actually needs a language server. `/lsp off` still reclaims RAM; config can
//!   force-off via `enable_lsp = false`.
//! - **Robust ("không crash").** A missing / slow / hung / crashing server never aborts the agent
//!   turn — worst case it degrades to "lsp unavailable" and the agent falls back to grep. A server
//!   that dies is respawned at most [`MAX_RESPAWNS`] times, then marked disabled for the session
//!   (`/lsp restart` resets).
//! - **Language-neutral core + a per-language server table** ([`discovery`]). Adding a language is
//!   one row.
//! - **async→sync bridge.** [`LspManager`] owns its OWN dedicated tokio runtime; the `MainLoop`s and
//!   all LSP requests run there, and the synchronous `Tool::execute` dispatches a request onto that
//!   runtime and blocks on a std channel ([`std::sync::mpsc`]) — never `block_on` on a runtime
//!   worker (which would panic), so it's safe on any thread the tool happens to run on.
//! - **Symbolic edit.** `symbol_replace` / `symbol_insert` rewrite a whole named item by outline
//!   range (Serena-style), so the model never dumps the file or thrash-matches `old_string`.

pub mod discovery;
pub mod jobobject;
pub mod server;
pub mod tools;
pub mod uri;

pub use server::InsertWhere;

use anyhow::{anyhow, bail, Result};
use once_cell::sync::Lazy;
use server::{DefHit, DiagItem, DocSym, HoverHit, LspServer, RefHit, SymBody, WsSym};
use std::collections::HashMap;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::runtime::{Builder, Handle, Runtime};

/// What kind of symbolic edit was planned (tools apply the write).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SymbolEditKind {
    Replace,
    InsertBefore,
    InsertAfter,
}

/// Plan produced by a symbolic edit query — the tool writes `new_content` atomically.
#[derive(Debug, Clone)]
pub struct SymbolEditPlan {
    pub path: PathBuf,
    pub start_line: usize,
    pub end_line: usize,
    pub old_body: String,
    pub new_content: String,
    pub base_fingerprint: crate::core::persist::FileFingerprint,
    pub kind: SymbolEditKind,
    pub symbol: String,
}

/// The process-global LSP manager. One per CLI process; servers are spawned lazily and reused.
pub static LSP: Lazy<LspManager> = Lazy::new(LspManager::new);

/// A crashed server is respawned at most this many times per session before being marked disabled
/// (never a tight restart loop — plan §2 decision 5). `/lsp restart` resets the counters.
const MAX_RESPAWNS: u32 = 2;

/// Status snapshot for the `/lsp status` command.
pub struct LspStatus {
    pub enabled: bool,
    pub servers: Vec<ServerStatus>,
}

pub struct ServerStatus {
    pub lang: &'static str,
    pub root: PathBuf,
    pub indexed: bool,
    pub alive: bool,
}

impl LspStatus {
    /// Human-readable summary for `/lsp status`.
    pub fn render(&self) -> String {
        if !self.enabled {
            return "LSP: off — `/lsp on` to enable type-aware code navigation + symbolic edit."
                .to_string();
        }
        if self.servers.is_empty() {
            return "LSP: on — no server running yet (starts lazily on the first symbol query)."
                .to_string();
        }
        let mut s = String::from("LSP: on\n");
        for sv in &self.servers {
            let state = if !sv.alive {
                "dead"
            } else if sv.indexed {
                "ready"
            } else {
                "indexing…"
            };
            s.push_str(&format!(
                "  {} [{}]  {}\n",
                sv.lang,
                state,
                sv.root.display()
            ));
        }
        s.trim_end().to_string()
    }
}

pub struct LspManager {
    enabled: AtomicBool,
    /// Fold NEW diagnostics into edit-tool results (see [`edit_feedback`](Self::edit_feedback)).
    /// Default ON (only matters while `enabled`); `/lsp edits off` / config kill it.
    edit_feedback: AtomicBool,
    /// Per-request wall-clock cap (seconds); set from `AgentConfig.lsp_request_timeout_secs`.
    request_timeout_secs: AtomicU64,
    /// The dedicated runtime the servers + requests run on. Built on [`enable`](Self::enable).
    runtime: Mutex<Option<Runtime>>,
    /// Live servers, keyed by `"<lang>\0<root>"`. `Arc` so query tasks can hold one independently.
    servers: Mutex<HashMap<String, Arc<LspServer>>>,
    /// Respawns-after-death per server key; capped at [`MAX_RESPAWNS`] per session.
    restarts: Mutex<HashMap<String, u32>>,
    /// Session baseline of diagnostic FINGERPRINTS per file (line-number-free — edits shift
    /// lines), refreshed on every fetch. Post-edit feedback reports `now − baseline` only, so a
    /// pre-existing warning wall never spams every edit result.
    diag_baseline: Mutex<HashMap<PathBuf, std::collections::HashSet<String>>>,
}

impl LspManager {
    fn new() -> Self {
        Self {
            enabled: AtomicBool::new(false),
            edit_feedback: AtomicBool::new(true),
            request_timeout_secs: AtomicU64::new(20),
            runtime: Mutex::new(None),
            servers: Mutex::new(HashMap::new()),
            restarts: Mutex::new(HashMap::new()),
            diag_baseline: Mutex::new(HashMap::new()),
        }
    }

    pub fn is_enabled(&self) -> bool {
        self.enabled.load(Ordering::Relaxed)
    }

    /// Is the post-edit diagnostics fold on? (Only meaningful while LSP itself is enabled.)
    pub fn edit_feedback_enabled(&self) -> bool {
        self.edit_feedback.load(Ordering::Relaxed)
    }

    pub fn set_edit_feedback(&self, on: bool) {
        self.edit_feedback.store(on, Ordering::Relaxed);
    }

    /// Set the per-request timeout (seconds, min 1). Called from config at startup / `/lsp on`.
    pub fn set_request_timeout(&self, secs: u64) {
        self.request_timeout_secs
            .store(secs.max(1), Ordering::Relaxed);
    }

    fn request_timeout(&self) -> Duration {
        Duration::from_secs(self.request_timeout_secs.load(Ordering::Relaxed).max(1))
    }

    /// Turn LSP on for this session: build the dedicated runtime if needed. Idempotent. Does NOT
    /// spawn any server yet — servers start lazily on first use.
    pub fn enable(&self) -> Result<()> {
        let mut guard = self.runtime.lock().unwrap_or_else(|e| e.into_inner());
        if guard.is_none() {
            let rt = Builder::new_multi_thread()
                .worker_threads(2)
                .enable_all()
                .thread_name("aizen-lsp")
                .build()
                .map_err(|e| anyhow!("failed to start the LSP runtime: {e}"))?;
            *guard = Some(rt);
        }
        // Fresh session semantics: `/lsp restart` (disable+enable) also resets the give-up counters.
        self.restarts
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clear();
        self.enabled.store(true, Ordering::Relaxed);
        Ok(())
    }

    /// Turn LSP off: drop all servers (each `Drop` aborts its mainloop → `kill_on_drop` + the
    /// Windows Job Object reap the child tree) and shut the runtime down. Reclaims the servers' RAM.
    pub fn disable(&self) {
        self.enabled.store(false, Ordering::Relaxed);
        let servers: Vec<Arc<LspServer>> = self
            .servers
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .drain()
            .map(|(_, v)| v)
            .collect();
        self.restarts
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clear();
        let rt = self
            .runtime
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take();
        // Teardown blocks (the graceful shutdown handshake + dropping the runtime waits on its
        // workers) and must not run inside another runtime — hand it to a plain OS thread.
        std::thread::spawn(move || {
            if let Some(rt) = rt {
                rt.block_on(async {
                    for s in &servers {
                        // best-effort graceful shutdown→exit; `Drop` is the hard backstop.
                        let _ = tokio::time::timeout(Duration::from_secs(5), s.shutdown()).await;
                    }
                });
                drop(rt); // drop the runtime explicitly (joins its workers)
            }
            // Drop the server Arcs → `LspServer::drop` aborts each mainloop task → `kill_on_drop`
            // (+ Job Object on Windows) reaps the child tree.
            drop(servers);
        });
    }

    pub fn status(&self) -> LspStatus {
        let servers = self
            .servers
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .values()
            .map(|s| ServerStatus {
                lang: s.spec.lang,
                root: s.root.clone(),
                indexed: s.is_indexed(),
                alive: s.is_alive(),
            })
            .collect();
        LspStatus {
            enabled: self.is_enabled(),
            servers,
        }
    }

    /// Find references to `symbol` (by NAME) within the project that owns `anchor` (a file in the
    /// project, or the project directory). Returns a formatted, capped result string for the model,
    /// or an `Err` the tool surfaces as a clean "unavailable" message (never aborting the turn).
    pub fn references(&self, anchor: &Path, symbol: &str, include_decl: bool) -> Result<String> {
        let sym = symbol.to_string();
        let hint = anchor.is_file().then(|| anchor.to_path_buf());
        self.run_query(anchor, "references", move |s| async move {
            let hits = s
                .references_by_name(hint.as_deref(), &sym, include_decl)
                .await?;
            Ok(format_hits(&s.root, &sym, &hits))
        })
    }

    /// Resolve `symbol` (by NAME) to its definition and return the definition source text inline.
    pub fn definition(&self, anchor: &Path, symbol: &str) -> Result<String> {
        let sym = symbol.to_string();
        let hint = anchor.is_file().then(|| anchor.to_path_buf());
        self.run_query(anchor, "definition", move |s| async move {
            let def = s.definition_by_name(hint.as_deref(), &sym).await?;
            Ok(format_def(&s.root, &sym, &def))
        })
    }

    /// Resolve `symbol` (by NAME) to the language server's hover (type/signature + doc), capped.
    pub fn hover(&self, anchor: &Path, symbol: &str) -> Result<String> {
        let sym = symbol.to_string();
        let hint = anchor.is_file().then(|| anchor.to_path_buf());
        self.run_query(anchor, "hover", move |s| async move {
            let hit = s.hover_by_name(hint.as_deref(), &sym).await?;
            Ok(format_hover(&s.root, &sym, &hit))
        })
    }

    /// Structural outline (symbols, no bodies) of one file.
    pub fn document_symbols(&self, file: &Path) -> Result<String> {
        let f = file.to_path_buf();
        self.run_query(file, "documentSymbol", move |s| async move {
            let syms = s.document_symbols(&f).await?;
            Ok(format_doc_symbols(&s.root, &f, &syms))
        })
    }

    /// [`document_symbols`](Self::document_symbols) returning the STRUCTS (for `repo_map`, which
    /// renders its own compact skeleton instead of the tool's outline).
    pub fn document_symbols_items(&self, file: &Path) -> Result<Vec<DocSym>> {
        let f = file.to_path_buf();
        self.run_query(file, "documentSymbol", move |s| async move {
            s.document_symbols(&f).await
        })
    }

    /// Project-wide fuzzy symbol search by name (`max` caps the rendered hits).
    pub fn workspace_symbols(&self, anchor: &Path, query: &str, max: usize) -> Result<String> {
        let q = query.to_string();
        self.run_query(anchor, "workspace/symbol", move |s| async move {
            let syms = s.workspace_symbols(&q).await?;
            Ok(format_ws_symbols(&s.root, &q, &syms, max))
        })
    }

    /// Current diagnostics for one file (pull-preferred, push-fallback — see `server::diagnostics`).
    pub fn diagnostics(&self, file: &Path) -> Result<String> {
        let f = file.to_path_buf();
        let out = self.run_query(file, "diagnostics", move |s| async move {
            let items = s.diagnostics(&f).await?;
            Ok((format_diagnostics(&s.root, &f, &items), items))
        });
        match out {
            Ok((rendered, items)) => {
                self.update_baseline(file, &items);
                Ok(rendered)
            }
            Err(e) => Err(e),
        }
    }

    /// Resolve a named symbol to its full body text (for token-lean reads before a symbolic edit).
    pub fn symbol_body(&self, anchor: &Path, symbol: &str) -> Result<String> {
        let sym = symbol.to_string();
        let hint = anchor.is_file().then(|| anchor.to_path_buf());
        self.run_query(anchor, "symbolBody", move |s| async move {
            let body = s.symbol_body(hint.as_deref(), &sym).await?;
            Ok(format_sym_body(&s.root, &body))
        })
    }

    /// Replace a named symbol's full body. Returns a formatted edit result; caller is responsible
    /// for writing the new file contents via the returned path+text (tools do the write + fold).
    /// The manager itself does **not** write — tools own the atomic write + verify-gate arming.
    pub fn replace_symbol(
        &self,
        anchor: &Path,
        symbol: &str,
        new_body: &str,
    ) -> Result<SymbolEditPlan> {
        let sym = symbol.to_string();
        let body = new_body.to_string();
        let hint = anchor.is_file().then(|| anchor.to_path_buf());
        self.run_query(anchor, "symbolReplace", move |s| async move {
            let (path, start, end, old, new_text, base_fingerprint) =
                s.replace_symbol_body(hint.as_deref(), &sym, &body).await?;
            Ok(SymbolEditPlan {
                path,
                start_line: start,
                end_line: end,
                old_body: old,
                new_content: new_text,
                base_fingerprint,
                kind: SymbolEditKind::Replace,
                symbol: sym,
            })
        })
    }

    /// Insert text immediately before or after a named symbol's full range.
    pub fn insert_at_symbol(
        &self,
        anchor: &Path,
        symbol: &str,
        where_: InsertWhere,
        text: &str,
    ) -> Result<SymbolEditPlan> {
        let sym = symbol.to_string();
        let body = text.to_string();
        let hint = anchor.is_file().then(|| anchor.to_path_buf());
        self.run_query(anchor, "symbolInsert", move |s| async move {
            let (path, at, new_text, base_fingerprint) = s
                .insert_relative_to_symbol(hint.as_deref(), &sym, where_, &body)
                .await?;
            Ok(SymbolEditPlan {
                path,
                start_line: at,
                end_line: at,
                old_body: String::new(),
                new_content: new_text,
                base_fingerprint,
                kind: match where_ {
                    InsertWhere::Before => SymbolEditKind::InsertBefore,
                    InsertWhere::After => SymbolEditKind::InsertAfter,
                },
                symbol: sym,
            })
        })
    }

    /// Post-edit diagnostics FOLD: NEW diagnostics for `file` (vs the session baseline), appended
    /// to the edit tool's own result — the model learns of breakage in the same round-trip that
    /// caused it, no separate diagnostics call, no failed-build round-trip later.
    ///
    /// Fail-soft and edit-latency-safe by construction: `None` on any miss (LSP off, fold off, no
    /// project, server not RUNNING yet — never spawned from the edit path, only warmed in the
    /// background for next time — or the 3s hard cap elapsing). An edit's success is never held
    /// hostage by a slow analysis.
    pub fn edit_feedback(&self, file: &Path) -> Option<String> {
        if !self.is_enabled() || !self.edit_feedback_enabled() {
            return None;
        }
        let file = file.canonicalize().unwrap_or_else(|_| file.to_path_buf());
        let (spec, root) = discovery::detect(&file)?;
        let key = format!("{}\0{}", spec.lang, root.display());
        let server = self
            .servers
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&key)
            .cloned();
        let Some(server) = server else {
            self.warm_spawn(spec, root); // folds activate from the NEXT edit onward
            return None;
        };
        if !server.is_alive() {
            return None;
        }
        let handle = self.handle().ok()?;
        let f = file.clone();
        let (tx, rx) = std::sync::mpsc::channel();
        handle.spawn(async move {
            // Hard 3s wall-clock cap around re-open + re-analysis + (bounded) settle.
            let out = tokio::time::timeout(
                Duration::from_secs(3),
                server.diagnostics_bounded(&f, Duration::from_millis(1500)),
            )
            .await;
            let _ = tx.send(match out {
                Ok(Ok(items)) => Some(items),
                _ => None,
            });
        });
        let items = rx
            .recv_timeout(Duration::from_millis(3_500))
            .ok()
            .flatten()?;

        let fingerprints: std::collections::HashSet<String> =
            items.iter().map(diag_fingerprint).collect();
        let had_baseline;
        let new_items: Vec<&DiagItem> = {
            let mut base = self.diag_baseline.lock().unwrap_or_else(|e| e.into_inner());
            let prev = base.get(&file);
            had_baseline = prev.is_some();
            let fresh: Vec<&DiagItem> = match prev {
                Some(prev) => items
                    .iter()
                    .filter(|d| !prev.contains(&diag_fingerprint(d)))
                    .collect(),
                // First edit with no baseline: report only ERRORS (a pre-existing warning wall
                // must not spam), labeled `current` not `new`.
                None => items.iter().filter(|d| d.severity == "error").collect(),
            };
            base.insert(file.clone(), fingerprints);
            fresh
        };
        Some(format_edit_feedback(&new_items, had_baseline))
    }

    /// Refresh the per-file fingerprint baseline (every fetch is the new truth).
    fn update_baseline(&self, file: &Path, items: &[DiagItem]) {
        let file = file.canonicalize().unwrap_or_else(|_| file.to_path_buf());
        let fp = items.iter().map(diag_fingerprint).collect();
        self.diag_baseline
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(file, fp);
    }

    /// Fire-and-forget server spawn (a throwaway OS thread carries the blocking handshake) so the
    /// NEXT edit's fold finds a live server. Never blocks the caller.
    fn warm_spawn(&self, spec: &'static discovery::ServerSpec, root: PathBuf) {
        let Ok(handle) = self.handle() else { return };
        let timeout = self.request_timeout();
        std::thread::spawn(move || {
            let _ = LSP.get_or_spawn(spec, &root, &handle, timeout);
        });
    }

    /// The shared async→sync bridge: detect the project for `anchor`, get/spawn its server, run
    /// `f(server)` on the dedicated runtime under the request timeout, and block the calling (tool)
    /// thread on a std channel until the result lands. Panic-free on any thread.
    fn run_query<T, F, Fut>(&self, anchor: &Path, op: &'static str, f: F) -> Result<T>
    where
        T: Send + 'static,
        F: FnOnce(Arc<LspServer>) -> Fut + Send + 'static,
        Fut: Future<Output = Result<T>> + Send + 'static,
    {
        if !self.is_enabled() {
            bail!("LSP is off — enable it with `/lsp on`");
        }
        let (spec, root) = discovery::detect(anchor).ok_or_else(|| {
            anyhow!(
                "no supported language project found at/above {}",
                anchor.display()
            )
        })?;
        let timeout = self.request_timeout();
        let handle = self.handle()?;
        let server = self.get_or_spawn(spec, &root, &handle, timeout)?;

        let (tx, rx) = std::sync::mpsc::channel();
        handle.spawn(async move {
            let r = tokio::time::timeout(timeout, f(server)).await;
            let _ = tx.send(match r {
                Ok(inner) => inner,
                Err(_) => Err(anyhow!(
                    "LSP {op} timed out after {timeout:?} (the server may still be indexing — try again)"
                )),
            });
        });
        rx.recv_timeout(timeout + Duration::from_secs(3))
            .map_err(|_| anyhow!("LSP request did not return"))?
    }

    fn handle(&self) -> Result<Handle> {
        self.runtime
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            .map(|rt| rt.handle().clone())
            .ok_or_else(|| anyhow!("LSP runtime is not running"))
    }

    /// Get the live server for `(lang, root)`, spawning + initializing it on the runtime if needed.
    /// A dead server (crashed / EOF) is evicted and respawned up to [`MAX_RESPAWNS`] times, then the
    /// key is disabled for the session. Blocks the caller via a std channel while the (possibly
    /// slow, cold-index) handshake runs.
    fn get_or_spawn(
        &self,
        spec: &'static discovery::ServerSpec,
        root: &Path,
        handle: &Handle,
        timeout: Duration,
    ) -> Result<Arc<LspServer>> {
        let key = format!("{}\0{}", spec.lang, root.display());
        {
            let mut servers = self.servers.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(s) = servers.get(&key) {
                if s.is_alive() {
                    return Ok(Arc::clone(s));
                }
                // Dead server: evict + count the respawn (bounded — never a restart storm).
                servers.remove(&key);
                *self
                    .restarts
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .entry(key.clone())
                    .or_insert(0) += 1;
            }
        }
        let respawns = self
            .restarts
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&key)
            .copied()
            .unwrap_or(0);
        if respawns > MAX_RESPAWNS {
            bail!(
                "the {} language server crashed repeatedly — disabled for this session (`/lsp restart` to reset)",
                spec.lang
            );
        }
        // Not installed → graceful error (caller turns it into "unavailable", never a crash).
        let bin = discovery::resolve_server_binary(spec)?;
        let init_timeout = timeout.max(Duration::from_secs(30)); // generous for cold indexing

        let (tx, rx) = std::sync::mpsc::channel();
        let root_owned = root.to_path_buf();
        handle.spawn(async move {
            let r = LspServer::spawn(spec, &bin, &root_owned, init_timeout).await;
            let _ = tx.send(r);
        });
        let server = rx
            .recv_timeout(init_timeout + Duration::from_secs(5))
            .map_err(|_| anyhow!("LSP server did not start in time"))??;
        let server = Arc::new(server);
        self.servers
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(key, Arc::clone(&server));
        Ok(server)
    }
}

/// Path relative to the workspace root, for display. `strip_prefix` alone misses on Windows: the
/// root is canonicalized (`\\?\C:\…`) while server URIs decode to plain, lowercase-drive paths
/// (`c:\…`) — so fall back to a verbatim-prefix-stripped, case-insensitive prefix match.
fn rel_display(root: &Path, path: &Path) -> PathBuf {
    if let Ok(r) = path.strip_prefix(root) {
        return r.to_path_buf();
    }
    let norm = |p: &Path| {
        p.to_string_lossy()
            .trim_start_matches(r"\\?\")
            .replace('/', r"\")
    };
    let (r, p) = (norm(root), norm(path));
    if cfg!(windows) && p.len() > r.len() && p[..r.len()].eq_ignore_ascii_case(&r) {
        return PathBuf::from(p[r.len()..].trim_start_matches('\\'));
    }
    path.to_path_buf()
}

/// Render reference hits as a compact, capped block: `relpath:line:col  [in kind name]  snippet`
/// (1-based display; the `[in …]` note is omitted for top-level hits with no enclosing symbol).
fn format_hits(root: &Path, symbol: &str, hits: &[RefHit]) -> String {
    if hits.is_empty() {
        return format!("no references found for '{symbol}'");
    }
    const MAX: usize = 100;
    let mut out = format!("{} reference(s) to '{}':\n", hits.len(), symbol);
    for h in hits.iter().take(MAX) {
        let rel = rel_display(root, &h.path);
        let ctx = match &h.enclosing {
            Some((name, kind)) => format!("[in {kind} {name}]  "),
            None => String::new(),
        };
        out.push_str(&format!(
            "  {}:{}:{}  {}{}\n",
            rel.display(),
            h.line + 1,
            h.col + 1,
            ctx,
            h.snippet
        ));
    }
    if hits.len() > MAX {
        out.push_str(&format!("  … (+{} more)\n", hits.len() - MAX));
    }
    out.trim_end().to_string()
}

/// Render a definition: header line + the definition source, fenced for readability.
fn format_def(root: &Path, symbol: &str, def: &DefHit) -> String {
    let rel = rel_display(root, &def.path);
    let mut out = format!(
        "definition of '{}' — {}:{}:{}\n",
        symbol,
        rel.display(),
        def.line + 1,
        def.col + 1
    );
    out.push_str(&def.source);
    if def.truncated {
        out.push_str("\n  … (truncated — read the file for the rest)");
    }
    out
}

/// Render a full symbol body (uncapped definition range) for token-lean symbol reads.
fn format_sym_body(root: &Path, body: &SymBody) -> String {
    let rel = rel_display(root, &body.path);
    format!(
        "{} '{}' — {}:{}-{}\n{}",
        body.kind,
        body.name,
        rel.display(),
        body.start_line + 1,
        body.end_line + 1,
        body.text
    )
}

/// Cap on hover text lines (rust-analyzer embeds the full doc-comment as multi-line markdown).
const HOVER_MAX_LINES: usize = 24;

/// Render a hover result — type/signature + doc-comment, capped to [`HOVER_MAX_LINES`] lines.
fn format_hover(root: &Path, symbol: &str, hit: &HoverHit) -> String {
    let rel = rel_display(root, &hit.path);
    if hit.text.is_empty() {
        return format!(
            "no hover info for '{symbol}' ({}:{})",
            rel.display(),
            hit.line + 1
        );
    }
    let mut lines: Vec<&str> = hit.text.lines().collect();
    let truncated = lines.len() > HOVER_MAX_LINES;
    lines.truncate(HOVER_MAX_LINES);
    let mut body = lines.join("\n");
    if truncated {
        body.push_str("\n  … (truncated — use read_symbol for the full body)");
    }
    format!(
        "hover '{}' — {}:{}\n{}",
        hit.name,
        rel.display(),
        hit.line + 1,
        body
    )
}

/// Render a file outline with indentation showing nesting.
fn format_doc_symbols(root: &Path, file: &Path, syms: &[DocSym]) -> String {
    let rel = rel_display(root, file);
    if syms.is_empty() {
        return format!(
            "no symbols found in {} (empty file or unsupported)",
            rel.display()
        );
    }
    const MAX: usize = 200;
    let mut out = format!("{} symbol(s) in {}:\n", syms.len(), rel.display());
    for s in syms.iter().take(MAX) {
        out.push_str(&format!(
            "  {}{} {}  :{}\n",
            "  ".repeat(s.depth),
            s.kind,
            s.name,
            s.line + 1
        ));
    }
    if syms.len() > MAX {
        out.push_str(&format!("  … (+{} more)\n", syms.len() - MAX));
    }
    out.trim_end().to_string()
}

/// Render workspace-symbol hits: `kind name  relpath:line`.
fn format_ws_symbols(root: &Path, query: &str, syms: &[WsSym], max: usize) -> String {
    if syms.is_empty() {
        return format!("no symbols matching '{query}'");
    }
    let max = max.clamp(1, 100);
    let mut out = format!("{} symbol(s) matching '{}':\n", syms.len(), query);
    for s in syms.iter().take(max) {
        let rel = rel_display(root, &s.path);
        match s.line {
            Some(l) => out.push_str(&format!(
                "  {} {}  {}:{}\n",
                s.kind,
                s.name,
                rel.display(),
                l + 1
            )),
            None => out.push_str(&format!("  {} {}  {}\n", s.kind, s.name, rel.display())),
        }
    }
    if syms.len() > max {
        out.push_str(&format!(
            "  … (+{} more — narrow the query)\n",
            syms.len() - max
        ));
    }
    out.trim_end().to_string()
}

/// Render diagnostics: severity counts header + `severity line:col  message [code]` rows.
/// LINE-NUMBER-FREE identity of one diagnostic (`severity|code|first-120-chars-of-message`): an
/// edit shifts every line below it, so a positional identity would mark every pre-existing
/// diagnostic "new" after each edit.
fn diag_fingerprint(d: &DiagItem) -> String {
    let first: String = d
        .message
        .lines()
        .next()
        .unwrap_or("")
        .chars()
        .take(120)
        .collect();
    format!(
        "{}|{}|{}",
        d.severity,
        d.code.as_deref().unwrap_or(""),
        first
    )
}

/// The post-edit fold block appended to an edit result. `had_baseline` picks the honest label:
/// diffed against a prior snapshot ⇒ "new"; first sighting ⇒ "current" (errors only).
fn format_edit_feedback(items: &[&DiagItem], had_baseline: bool) -> String {
    if items.is_empty() {
        return "[lsp] no new diagnostics".to_string();
    }
    let label = if had_baseline { "new" } else { "current" };
    let mut s = format!("[lsp] {} {label} diagnostic(s) after edit:", items.len());
    for d in items.iter().take(5) {
        let first: String = d
            .message
            .lines()
            .next()
            .unwrap_or("")
            .chars()
            .take(140)
            .collect();
        let code = d
            .code
            .as_deref()
            .map(|c| format!(" [{c}]"))
            .unwrap_or_default();
        s.push_str(&format!(
            "\n  {} {}:{}  {first}{code}",
            d.severity, d.line, d.col
        ));
    }
    if items.len() > 5 {
        s.push_str(&format!(
            "\n  (+{} more — run lsp_diagnostics for the full list)",
            items.len() - 5
        ));
    }
    s
}

fn format_diagnostics(root: &Path, file: &Path, items: &[DiagItem]) -> String {
    let rel = rel_display(root, file);
    if items.is_empty() {
        return format!("no diagnostics for {} — clean", rel.display());
    }
    const MAX: usize = 50;
    let errors = items.iter().filter(|d| d.severity == "error").count();
    let warnings = items.iter().filter(|d| d.severity == "warning").count();
    let mut out = format!(
        "{} diagnostic(s) for {} ({} error, {} warning):\n",
        items.len(),
        rel.display(),
        errors,
        warnings
    );
    for d in items.iter().take(MAX) {
        let first_line = d.message.lines().next().unwrap_or("");
        let msg: String = first_line.chars().take(300).collect();
        let more = if d.message.lines().count() > 1 {
            " …"
        } else {
            ""
        };
        let code = d
            .code
            .as_deref()
            .map(|c| format!(" [{c}]"))
            .unwrap_or_default();
        out.push_str(&format!(
            "  {} {}:{}  {}{}{}\n",
            d.severity,
            d.line + 1,
            d.col + 1,
            msg,
            more,
            code
        ));
    }
    if items.len() > MAX {
        out.push_str(&format!("  … (+{} more)\n", items.len() - MAX));
    }
    out.trim_end().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An absolute path spelled for the HOST platform: a drive path on Windows, POSIX elsewhere.
    ///
    /// Not cosmetic. `rel_display` strips the root by prefix, and a Windows literal like
    /// `C:\p\a.rs` is ONE opaque component on Linux — nothing strips, so every formatter prints the
    /// full path and the `a.rs`-shaped assertions below fail on the unix CI jobs while staying green
    /// locally. Build both sides of the comparison the same way and the tests hold everywhere.
    fn tpath(root: &str, rest: &str) -> PathBuf {
        let mut p = PathBuf::from(if cfg!(windows) {
            format!(r"C:\{root}")
        } else {
            format!("/{root}")
        });
        p.extend(rest.split('/').filter(|s| !s.is_empty()));
        p
    }

    fn diag(sev: &'static str, line: usize, msg: &str, code: Option<&str>) -> DiagItem {
        DiagItem {
            line,
            col: 1,
            severity: sev,
            message: msg.into(),
            code: code.map(String::from),
        }
    }

    #[test]
    fn diag_fingerprint_ignores_line_numbers() {
        let a = diag("error", 10, "cannot find value `x`", Some("E0425"));
        let b = diag("error", 99, "cannot find value `x`", Some("E0425"));
        assert_eq!(
            diag_fingerprint(&a),
            diag_fingerprint(&b),
            "same diagnostic, shifted lines"
        );
        let c = diag("warning", 10, "cannot find value `x`", Some("E0425"));
        assert_ne!(
            diag_fingerprint(&a),
            diag_fingerprint(&c),
            "severity is identity"
        );
    }

    #[test]
    fn edit_feedback_formats_cap_and_labels() {
        let items: Vec<DiagItem> = (0..8)
            .map(|i| diag("error", i, &format!("problem {i}"), None))
            .collect();
        let refs: Vec<&DiagItem> = items.iter().collect();
        let s = format_edit_feedback(&refs, true);
        assert!(
            s.starts_with("[lsp] 8 new diagnostic(s) after edit:"),
            "{s}"
        );
        assert!(s.contains("(+3 more"), "capped at 5: {s}");
        let s2 = format_edit_feedback(&refs[..1], false);
        assert!(
            s2.contains("1 current diagnostic"),
            "no baseline → honest 'current' label: {s2}"
        );
        assert_eq!(format_edit_feedback(&[], true), "[lsp] no new diagnostics");
    }

    #[test]
    fn edit_feedback_is_none_when_off() {
        // LSP disabled (the default in tests) → the fold must be a hard no-op.
        assert!(LSP.edit_feedback(Path::new("src/main.rs")).is_none());
    }

    #[test]
    fn hits_formatting_shows_enclosing_symbol() {
        let hits = vec![
            RefHit {
                path: PathBuf::from(r"C:\proj\src\a.rs"),
                line: 41,
                col: 8,
                snippet: "foo();".into(),
                enclosing: Some(("run_loop".into(), "fn")),
            },
            // Top-level reference (use-statement, etc.) → no enclosing note.
            RefHit {
                path: PathBuf::from(r"C:\proj\src\b.rs"),
                line: 2,
                col: 4,
                snippet: "use crate::foo;".into(),
                enclosing: None,
            },
        ];
        let out = format_hits(Path::new(r"C:\proj"), "foo", &hits);
        assert!(out.contains("2 reference(s) to 'foo'"), "{out}");
        assert!(
            out.contains(r"a.rs:42:9  [in fn run_loop]  foo();"),
            "enclosing shown: {out}"
        );
        assert!(
            out.contains(r"b.rs:3:5  use crate::foo;"),
            "no bracket when None: {out}"
        );
        assert!(!out.contains("[in  ]"), "no empty bracket: {out}");
    }

    #[test]
    fn rel_display_handles_verbatim_and_drive_case() {
        // Plain prefix → strip_prefix path.
        assert_eq!(
            rel_display(&tpath("proj", ""), &tpath("proj", "src/a.rs")),
            ["src", "a.rs"].iter().collect::<PathBuf>()
        );
        if cfg!(windows) {
            // Canonicalized root (`\\?\C:\…`) vs lowercase-drive server path (`c:\…`). Windows-only
            // by nature: there is no verbatim prefix or drive-letter casing to reconcile elsewhere.
            assert_eq!(
                rel_display(Path::new(r"\\?\C:\proj"), Path::new(r"c:\proj\src\a.rs")),
                PathBuf::from(r"src\a.rs")
            );
        }
        // Unrelated path → unchanged.
        let unrelated = tpath("other", "b.rs");
        assert_eq!(rel_display(&tpath("proj", ""), &unrelated), unrelated);
    }

    #[test]
    fn def_formatting() {
        let def = DefHit {
            path: PathBuf::from(r"C:\proj\src\lib.rs"),
            line: 9,
            col: 4,
            source: "pub struct Foo {\n    x: u32,\n}".into(),
            truncated: false,
        };
        let out = format_def(Path::new(r"C:\proj"), "Foo", &def);
        assert!(out.starts_with("definition of 'Foo' — "), "{out}");
        assert!(out.contains(":10:5"), "1-based position: {out}");
        assert!(out.contains("pub struct Foo"), "{out}");
        assert!(!out.contains("truncated"), "{out}");
        let def_t = DefHit {
            truncated: true,
            ..def
        };
        assert!(format_def(Path::new(r"C:\proj"), "Foo", &def_t).contains("truncated"));
    }

    #[test]
    fn sym_body_formatting() {
        // read_symbol renders: "{kind} '{name}' — rel:start-end\n{text}" (1-based, uncapped body).
        let body = SymBody {
            path: PathBuf::from(r"C:\proj\src\lib.rs"),
            name: "do_thing".into(),
            kind: "function",
            start_line: 41,
            end_line: 48,
            text: "fn do_thing() {\n    // body\n}".into(),
        };
        let out = format_sym_body(Path::new(r"C:\proj"), &body);
        assert!(out.starts_with("function 'do_thing' — "), "{out}");
        assert!(out.contains(":42-49"), "1-based inclusive range: {out}");
        assert!(out.contains("fn do_thing() {"), "full body present: {out}");
    }

    #[test]
    fn hover_formatting_caps_and_labels() {
        // lsp_hover renders: "hover '{name}' — rel:line\n{markdown}" (1-based line, capped).
        let hit = HoverHit {
            path: PathBuf::from(r"C:\proj\src\lib.rs"),
            name: "Foo".into(),
            line: 9,
            text: "pub struct Foo\n/// docs".into(),
        };
        let out = format_hover(Path::new(r"C:\proj"), "Foo", &hit);
        assert!(out.starts_with("hover 'Foo' — "), "{out}");
        assert!(out.contains(":10\n"), "1-based line: {out}");
        assert!(out.contains("pub struct Foo"), "signature present: {out}");
        assert!(
            !out.contains("truncated"),
            "short hover not truncated: {out}"
        );
        // Over-cap hover → truncated marker pointing at read_symbol.
        let long = (0..HOVER_MAX_LINES + 5)
            .map(|i| format!("line{i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let hit_long = HoverHit {
            text: long,
            ..hit.clone()
        };
        let out_long = format_hover(Path::new(r"C:\proj"), "Foo", &hit_long);
        assert!(
            out_long.contains("truncated — use read_symbol"),
            "{out_long}"
        );
        // Empty hover → honest "no hover info" with location.
        let hit_empty = HoverHit {
            text: String::new(),
            ..hit
        };
        let out_empty = format_hover(Path::new(r"C:\proj"), "Foo", &hit_empty);
        assert!(
            out_empty.starts_with("no hover info for 'Foo'"),
            "{out_empty}"
        );
    }

    #[test]
    fn outline_formatting_indents_by_depth() {
        let syms = vec![
            DocSym {
                name: "Outer".into(),
                kind: "struct",
                line: 0,
                depth: 0,
            },
            DocSym {
                name: "field".into(),
                kind: "field",
                line: 1,
                depth: 1,
            },
        ];
        let (root, file) = (tpath("p", ""), tpath("p", "a.rs"));
        let out = format_doc_symbols(&root, &file, &syms);
        assert!(out.contains("2 symbol(s) in a.rs"), "{out}");
        assert!(out.contains("  struct Outer  :1"), "{out}");
        assert!(
            out.contains("    field field  :2"),
            "depth-1 gets deeper indent: {out}"
        );
        let empty = format_doc_symbols(&root, &file, &[]);
        assert!(empty.contains("no symbols"), "{empty}");
    }

    #[test]
    fn ws_symbol_formatting_caps() {
        let syms: Vec<WsSym> = (0..5)
            .map(|i| WsSym {
                name: format!("sym{i}"),
                kind: "fn",
                path: tpath("p", "a.rs"),
                line: Some(i),
            })
            .collect();
        let root = tpath("p", "");
        let out = format_ws_symbols(&root, "sym", &syms, 3);
        assert!(out.contains("5 symbol(s) matching 'sym'"), "{out}");
        assert!(out.contains("fn sym0  a.rs:1"), "{out}");
        assert!(out.contains("(+2 more"), "cap marker: {out}");
        assert!(format_ws_symbols(&root, "zzz", &[], 3).contains("no symbols matching"));
    }

    #[test]
    fn diagnostics_formatting() {
        let items = vec![
            DiagItem {
                line: 2,
                col: 0,
                severity: "error",
                message: "mismatched types\nexpected u32".into(),
                code: Some("E0308".into()),
            },
            DiagItem {
                line: 5,
                col: 3,
                severity: "warning",
                message: "unused variable".into(),
                code: None,
            },
        ];
        let (root, file) = (tpath("p", ""), tpath("p", "a.rs"));
        let out = format_diagnostics(&root, &file, &items);
        assert!(
            out.contains("2 diagnostic(s) for a.rs (1 error, 1 warning)"),
            "{out}"
        );
        assert!(
            out.contains("error 3:1  mismatched types … [E0308]"),
            "first line only + code: {out}"
        );
        assert!(out.contains("warning 6:4  unused variable"), "{out}");
        assert!(format_diagnostics(&root, &file, &[]).contains("clean"));
    }
}

#[cfg(test)]
mod itest {
    use super::*;
    use std::path::Path;

    /// End-to-end against THIS repo. Spawns a real rust-analyzer (slow; ~indexing) and requires it
    /// installed — so it's `#[ignore]`d and skips cleanly when absent. Exercises the WHOLE v1 tool
    /// surface in one server session (references, definition, outline, workspace symbols,
    /// diagnostics). Run explicitly:
    ///   `cargo test --bin aizen lsp::itest -- --ignored --nocapture`
    #[test]
    #[ignore = "spawns rust-analyzer (slow; requires it installed)"]
    fn navigation_end_to_end() {
        if discovery::resolve_server_binary(&discovery::SERVERS[0]).is_err() {
            eprintln!("rust-analyzer not installed — skipping LSP end-to-end test");
            return;
        }
        LSP.set_request_timeout(90);
        LSP.enable().expect("enable lsp");
        // `AgentConfig` is referenced across many files of this crate → exercises cross-file refs.
        let anchor = Path::new("src/agent/mod.rs");

        let refs = LSP
            .references(anchor, "AgentConfig", true)
            .expect("references query failed");
        eprintln!("--- lsp_references(AgentConfig) ---\n{refs}\n---");
        assert!(
            refs.contains("reference(s) to 'AgentConfig'"),
            "unexpected output:\n{refs}"
        );

        let def = LSP
            .definition(anchor, "AgentConfig")
            .expect("definition query failed");
        eprintln!("--- lsp_definition(AgentConfig) ---\n{def}\n---");
        assert!(
            def.contains("definition of 'AgentConfig'"),
            "unexpected output:\n{def}"
        );
        assert!(
            def.contains("pub struct AgentConfig"),
            "definition source inline:\n{def}"
        );

        let outline = LSP
            .document_symbols(Path::new("src/agent/lsp/mod.rs"))
            .expect("outline failed");
        eprintln!("--- lsp_document_symbols(lsp/mod.rs) ---\n{outline}\n---");
        assert!(
            outline.contains("LspManager"),
            "unexpected outline:\n{outline}"
        );

        let ws = LSP
            .workspace_symbols(anchor, "LspManager", 30)
            .expect("workspace symbols failed");
        eprintln!("--- lsp_workspace_symbol(LspManager) ---\n{ws}\n---");
        assert!(ws.contains("LspManager"), "unexpected output:\n{ws}");

        let diags = LSP
            .diagnostics(Path::new("src/agent/lsp/mod.rs"))
            .expect("diagnostics failed");
        eprintln!("--- lsp_diagnostics(lsp/mod.rs) ---\n{diags}\n---");
        assert!(
            diags.contains("diagnostic(s) for") || diags.contains("clean"),
            "unexpected output:\n{diags}"
        );

        LSP.disable();
    }
}
