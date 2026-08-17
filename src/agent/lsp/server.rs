//! One running language server: spawn → `initialize`/`initialized` handshake → typed queries →
//! teardown. Runs entirely on the [`LspManager`](super::LspManager)'s dedicated tokio runtime; the
//! synchronous tool layer reaches it through a blocking channel (see `super`), never `block_on` on a
//! runtime worker.
//!
//! Built on `async-lsp`'s client `MainLoop` (request/response-id correlation + JSON-RPC framing are
//! the crate's job, not ours). The child process's stdio is adapted from tokio to futures-io via
//! `tokio_util::compat` so `run_buffered` accepts it.
//!
//! Robustness: the router tolerates chatty servers (any unhandled notification is absorbed —
//! pyright/typescript-language-server stream `window/logMessage`, which would otherwise BREAK the
//! MainLoop via async-lsp's strict default), answers the server→client requests real servers make
//! (`workspace/configuration`, `client/registerCapability`, `window/workDoneProgress/create`), and
//! is wrapped in async-lsp's `CatchUnwind` layer so a panic in a handler becomes an error response
//! instead of killing the loop. `kill_on_drop` + the mainloop task being `abort()`ed on [`Drop`]
//! always reaps the child — on Windows a Job Object (see `jobobject`) extends that to the whole
//! `cmd.exe`→`node` tree the node-based servers spawn. Every query is wrapped in a timeout by the
//! manager; a dead server surfaces as an `Err` (the tool degrades to "unavailable", never crashes
//! the turn).
//!
//! Document sync is FULL-text (plan §2 decision 6): [`LspServer::ensure_open`] fingerprints the
//! on-disk content and re-sends it (`didChange`, full replacement) whenever the agent's file edits
//! made the server's buffer stale — without this, references/diagnostics would drift after edits.

use super::discovery::{self, ServerSpec};
use super::uri;
use anyhow::{anyhow, Context, Result};
use async_lsp::lsp_types::{
    notification::{LogMessage, Progress, PublishDiagnostics, ShowMessage},
    request::{
        RegisterCapability, UnregisterCapability, WorkDoneProgressCreate, WorkspaceConfiguration,
    },
    ClientCapabilities, Diagnostic, DiagnosticClientCapabilities, DiagnosticSeverity,
    DidChangeTextDocumentParams, DidOpenTextDocumentParams, DocumentDiagnosticParams,
    DocumentDiagnosticReport, DocumentDiagnosticReportResult, DocumentSymbol,
    DocumentSymbolClientCapabilities, DocumentSymbolParams, DocumentSymbolResponse, Hover,
    HoverContents, HoverParams, InitializeParams, InitializedParams, Location, MarkedString,
    NumberOrString, OneOf, PartialResultParams, Position, PublishDiagnosticsClientCapabilities,
    Range, ReferenceContext, ReferenceParams, ServerCapabilities, SymbolKind,
    TextDocumentClientCapabilities, TextDocumentContentChangeEvent, TextDocumentIdentifier,
    TextDocumentItem, TextDocumentPositionParams, Url, VersionedTextDocumentIdentifier,
    WindowClientCapabilities, WorkDoneProgress, WorkDoneProgressParams, WorkspaceFolder,
    WorkspaceSymbolParams, WorkspaceSymbolResponse,
};
use async_lsp::panic::CatchUnwindLayer;
use async_lsp::router::Router;
use async_lsp::{LanguageServer, MainLoop, ServerSocket};
use std::collections::{HashMap, VecDeque};
use std::future::ready;
use std::ops::ControlFlow;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::{Duration, Instant};
use tokio::sync::Mutex as AsyncMutex;
use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};
use tower_layer::Layer as _;

/// rust-analyzer's work-done progress tokens that signal indexing is complete.
const RA_INDEXING_TOKENS: &[&str] = &["rustAnalyzer/Indexing", "rustAnalyzer/cachePriming"];

/// Cap on the inline source text returned by a definition lookup (lines).
const MAX_DEF_LINES: usize = 120;
/// Window used when the document outline can't give the definition's exact full range.
const DEF_FALLBACK_LINES: usize = 40;

/// A single reference hit, shaped for the model: 0-based `line`/`col` (rendered 1-based) + a trimmed
/// one-line snippet so the agent doesn't have to re-read the file. `enclosing` is the containing
/// symbol `(name, kind)` when the file's outline resolves it — so impact analysis reads "in fn X"
/// per call site instead of forcing a file_read of every referencing file.
#[derive(Debug, Clone)]
pub struct RefHit {
    pub path: PathBuf,
    pub line: usize,
    pub col: usize,
    pub snippet: String,
    pub enclosing: Option<(String, &'static str)>,
}

/// A resolved definition: where it is + its source text inline (kills the navigate-then-read
/// round-trip — the mcp-language-server design the plan adopted).
#[derive(Debug, Clone)]
pub struct DefHit {
    pub path: PathBuf,
    pub line: usize,
    pub col: usize,
    pub source: String,
    pub truncated: bool,
}

/// A resolved hover: where the symbol is + the language server's type/signature/doc markdown
/// (uncapped here; the tool layer caps line count when rendering).
#[derive(Debug, Clone)]
pub struct HoverHit {
    pub path: PathBuf,
    pub name: String,
    pub line: usize,
    pub text: String,
}

/// One entry of a file's structural outline (0-based `line`, `depth` = nesting level).
#[derive(Debug, Clone)]
pub struct DocSym {
    pub name: String,
    pub kind: &'static str,
    pub line: usize,
    pub depth: usize,
}

/// Resolved full body of a named symbol (for symbolic edit / token-lean replace).
/// Line numbers are 0-based inclusive, matching LSP `DocumentSymbol.range` after line-rounding.
#[derive(Debug, Clone)]
pub struct SymBody {
    pub path: PathBuf,
    pub name: String,
    pub kind: &'static str,
    pub start_line: usize,
    pub end_line: usize,
    pub text: String,
}

/// Where to place text relative to a symbol's full range.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InsertWhere {
    Before,
    After,
}

/// One project-wide symbol-search hit (`line` is `None` for lazy server entries that carry only a
/// file, no position).
#[derive(Debug, Clone)]
pub struct WsSym {
    pub name: String,
    pub kind: &'static str,
    pub path: PathBuf,
    pub line: Option<usize>,
}

/// One diagnostic, flattened for rendering (0-based `line`/`col`).
#[derive(Debug, Clone)]
pub struct DiagItem {
    pub line: usize,
    pub col: usize,
    pub severity: &'static str,
    pub message: String,
    pub code: Option<String>,
}

/// Latest `publishDiagnostics` snapshot for one document (`seq` orders publishes across documents so
/// the settle loop can detect "something new arrived").
struct PushDiags {
    seq: u64,
    items: Vec<Diagnostic>,
}

/// Sync bookkeeping for one `didOpen`ed document: its LSP version + a fingerprint of the text we
/// last sent, so [`LspServer::ensure_open`] can detect on-disk edits and re-sync.
struct OpenDoc {
    version: i32,
    fingerprint: u64,
}

/// Client-side state for the LSP router: indexing progress + the push-diagnostics store.
struct ClientState {
    indexed: Arc<AtomicBool>,
    diags: Arc<StdMutex<HashMap<String, PushDiags>>>,
    diag_seq: Arc<AtomicU64>,
}

/// Custom event emitted to break the `MainLoop` for a graceful shutdown.
struct Stop;

impl ClientState {
    fn new_router(
        indexed: Arc<AtomicBool>,
        diags: Arc<StdMutex<HashMap<String, PushDiags>>>,
        diag_seq: Arc<AtomicU64>,
    ) -> Router<Self> {
        let mut router = Router::new(ClientState {
            indexed,
            diags,
            diag_seq,
        });
        router
            .notification::<Progress>(|this, prog| {
                if matches!(&prog.token, NumberOrString::String(s) if RA_INDEXING_TOKENS.contains(&s.as_str()))
                    && matches!(prog.value, async_lsp::lsp_types::ProgressParamsValue::WorkDone(WorkDoneProgress::End(_)))
                {
                    this.indexed.store(true, Ordering::Relaxed);
                }
                ControlFlow::Continue(())
            })
            // Keep the latest push diagnostics per document — the Phase-3 fallback when a server
            // doesn't support pull diagnostics (typescript-language-server).
            .notification::<PublishDiagnostics>(|this, params| {
                let key = uri::normalize_uri(params.uri.as_str());
                let seq = this.diag_seq.fetch_add(1, Ordering::Relaxed) + 1;
                this.diags
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .insert(key, PushDiags { seq, items: params.diagnostics });
                ControlFlow::Continue(())
            })
            .notification::<ShowMessage>(|_, _| ControlFlow::Continue(()))
            .notification::<LogMessage>(|_, _| ControlFlow::Continue(()))
            // Servers we don't hand-configure ask for configuration at startup (pyright does);
            // "no specific settings" (nulls) makes them fall back to their defaults.
            .request::<WorkspaceConfiguration, _>(|_, params| {
                ready(Ok(vec![serde_json::Value::Null; params.items.len()]))
            })
            .request::<RegisterCapability, _>(|_, _| ready(Ok(())))
            .request::<UnregisterCapability, _>(|_, _| ready(Ok(())))
            .request::<WorkDoneProgressCreate, _>(|_, _| ready(Ok(())))
            .event(|_, _: Stop| ControlFlow::Break(Ok(())))
            // Chatty servers must never kill the loop: async-lsp's default BREAKS on any unhandled
            // non-`$/` notification, and the node servers routinely send extras. Absorb instead.
            .unhandled_notification(|_, _| ControlFlow::Continue(()));
        router
    }
}

/// A symbol candidate from `workspace/symbol` (lazy entries carry a URI but no range).
struct SymCand {
    name: String,
    kind: SymbolKind,
    uri: Url,
    range: Option<Range>,
    /// Containing symbol name reported by the server (e.g. the `impl`/class/module a method lives
    /// in). Used to disambiguate a `Container/method` name-path; `None`/empty when the server omits
    /// it (some do), in which case name-path targeting falls back to the file hint.
    container: Option<String>,
}

/// A picked, position-resolved symbol (definition site).
struct SymHit {
    name: String,
    uri: Url,
    range: Range,
}

pub struct LspServer {
    pub spec: &'static ServerSpec,
    pub root: PathBuf,
    socket: ServerSocket,
    /// What the server said it can do (`initialize` result) — gates pull diagnostics.
    caps: ServerCapabilities,
    indexed: Arc<AtomicBool>,
    diags: Arc<StdMutex<HashMap<String, PushDiags>>>,
    /// Documents we've synced (`didOpen`/`didChange`), keyed by normalized URI.
    opened: AsyncMutex<HashMap<String, OpenDoc>>,
    /// The `run_buffered` task; `abort()`ed on drop → its owned `Child` (+ Job Object on Windows)
    /// drops → the server process (tree) is reaped.
    mainloop_handle: tokio::task::JoinHandle<()>,
}

impl LspServer {
    /// Spawn the server binary, run its `MainLoop`, and complete the initialize handshake. Must be
    /// awaited on the manager's runtime (it uses `tokio::spawn` for the mainloop). `init_timeout`
    /// bounds the (possibly slow, cold-index) handshake.
    pub async fn spawn(
        spec: &'static ServerSpec,
        bin: &Path,
        root: &Path,
        init_timeout: Duration,
    ) -> Result<Self> {
        let indexed = Arc::new(AtomicBool::new(false));
        let diags: Arc<StdMutex<HashMap<String, PushDiags>>> =
            Arc::new(StdMutex::new(HashMap::new()));
        let diag_seq = Arc::new(AtomicU64::new(0));
        let router = ClientState::new_router(
            Arc::clone(&indexed),
            Arc::clone(&diags),
            Arc::clone(&diag_seq),
        );
        // A panic inside any handler becomes an error response instead of killing the MainLoop.
        let service = CatchUnwindLayer::default().layer(router);
        let (mainloop, socket) = MainLoop::new_client(move |_server| service);

        // Through the sandbox runner: a language server's INPUT is repository content, so it runs
        // env-scrubbed and (where the platform enforces it) without network — no LSP server we
        // launch needs a socket. The runner also applies `CREATE_NO_WINDOW`/`setsid`.
        let mut sbx =
            crate::sandbox::runner::prepare_tokio(crate::sandbox::request::SandboxRequest::exec(
                crate::sandbox::CommandOrigin::Lsp,
                bin.to_path_buf(),
                spec.args.iter().map(|a| a.to_string()).collect(),
                root.to_path_buf(),
                root.to_path_buf(),
            ))?;
        sbx.command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null()) // the server's own logs aren't useful to the agent
            .kill_on_drop(true);

        let mut child = match sbx.command.spawn() {
            Ok(c) => c,
            Err(e) => {
                sbx.finish(crate::sandbox::runner::Outcome::SpawnFailed);
                return Err(e)
                    .with_context(|| format!("spawning language server {}", bin.display()));
            }
        };
        let mut sandbox_guard = sbx.into_guard();
        sandbox_guard.finish(crate::sandbox::runner::Outcome::Spawned);
        let stdout = child.stdout.take().context("child has no stdout")?.compat();
        let stdin = child
            .stdin
            .take()
            .context("child has no stdin")?
            .compat_write();

        // Node servers are `.cmd` shims: the direct child is cmd.exe and the real node.exe is a
        // grandchild `kill_on_drop` can't reach. A kill-on-close Job Object reaps the whole tree;
        // best-effort — `None` just means Phase-1 direct-child kill semantics.
        #[cfg(windows)]
        let job = super::jobobject::contain(&child);

        // The mainloop task OWNS the child (and its Job Object): when it ends (graceful Stop or
        // `abort()`), both drop and the server process tree is reaped. A transport error just ends
        // the loop → the next query sees a dead server (`is_alive`) and the manager respawns or
        // degrades cleanly. The sandbox guard rides along so the server's private temp lives
        // exactly as long as the server does.
        let mainloop_handle = tokio::spawn(async move {
            #[cfg(windows)]
            let _job = job;
            let _child = child;
            let _sandbox = sandbox_guard;
            let _ = mainloop.run_buffered(stdout, stdin).await;
        });

        let mut server = LspServer {
            spec,
            root: root.to_path_buf(),
            socket,
            caps: ServerCapabilities::default(),
            indexed,
            diags,
            opened: AsyncMutex::new(HashMap::new()),
            mainloop_handle,
        };
        server.caps = server.initialize(root, init_timeout).await?;
        // tsserver-style servers only build a project once a file is open; seed one so project-wide
        // queries (workspace/symbol) work even before any file-anchored query happens.
        if !server.has_indexing_signal() {
            server.seed_open().await;
        }
        Ok(server)
    }

    async fn initialize(&self, root: &Path, init_timeout: Duration) -> Result<ServerCapabilities> {
        let root_uri = Url::from_file_path(root)
            .map_err(|()| anyhow!("non-absolute root: {}", root.display()))?;
        let mut sock = self.socket.clone();
        let params = InitializeParams {
            workspace_folders: Some(vec![WorkspaceFolder {
                uri: root_uri,
                name: "root".into(),
            }]),
            capabilities: ClientCapabilities {
                window: Some(WindowClientCapabilities {
                    work_done_progress: Some(true),
                    ..Default::default()
                }),
                text_document: Some(TextDocumentClientCapabilities {
                    // Ask for the nested outline (servers return Flat without this).
                    document_symbol: Some(DocumentSymbolClientCapabilities {
                        hierarchical_document_symbol_support: Some(true),
                        ..Default::default()
                    }),
                    // Advertise pull diagnostics so servers that gate `diagnosticProvider` on the
                    // client capability (rust-analyzer, pyright) actually offer it.
                    diagnostic: Some(DiagnosticClientCapabilities::default()),
                    publish_diagnostics: Some(PublishDiagnosticsClientCapabilities::default()),
                    ..Default::default()
                }),
                ..Default::default()
            },
            ..Default::default()
        };
        let init = tokio::time::timeout(init_timeout, sock.initialize(params))
            .await
            .map_err(|_| anyhow!("lsp initialize timed out after {init_timeout:?}"))?
            .map_err(|e| anyhow!("lsp initialize failed: {e}"))?;
        sock.initialized(InitializedParams {})
            .map_err(|e| anyhow!("lsp initialized notify failed: {e}"))?;
        Ok(init.capabilities)
    }

    pub fn is_indexed(&self) -> bool {
        self.indexed.load(Ordering::Relaxed)
    }

    /// The mainloop task ends when the server dies / the transport closes; a finished task = a dead
    /// server. The manager evicts dead servers and respawns (bounded).
    pub fn is_alive(&self) -> bool {
        !self.mainloop_handle.is_finished()
    }

    /// Whether this server emits an explicit "indexing done" signal ([`RA_INDEXING_TOKENS`]). For
    /// servers without one, symbol polling uses a short fixed warm-up instead of waiting on it.
    fn has_indexing_signal(&self) -> bool {
        self.spec.lang == "rust"
    }

    /// Ensure the server's view of `file` matches the disk: `didOpen` on first touch, and a
    /// FULL-text `didChange` re-sync whenever the on-disk content changed since we last sent it
    /// (the agent edits files on disk, not through the server). Returns the canonical URI and
    /// whether anything was (re)sent — the diagnostics settle loop uses that to decide if a fresh
    /// publish should be expected.
    async fn ensure_open(&self, file: &Path) -> Result<(Url, bool)> {
        // Absolutize: callers may hand a cwd-relative path; `Url::from_file_path` needs absolute
        // (canonicalize also verifies existence — it only succeeds for real files).
        let file = &file.canonicalize().unwrap_or_else(|_| file.to_path_buf());
        let url = Url::from_file_path(file)
            .map_err(|()| anyhow!("non-absolute path: {}", file.display()))?;
        let key = uri::normalize_uri(url.as_str());
        let text = tokio::fs::read_to_string(file)
            .await
            .with_context(|| format!("reading {}", file.display()))?;
        let fingerprint = fnv1a(text.as_bytes());
        let mut opened = self.opened.lock().await;
        let mut sock = self.socket.clone();
        match opened.get_mut(&key) {
            None => {
                sock.did_open(DidOpenTextDocumentParams {
                    text_document: TextDocumentItem {
                        uri: url.clone(),
                        language_id: discovery::language_id_for(file, self.spec.lang),
                        version: 1,
                        text,
                    },
                })
                .map_err(|e| anyhow!("didOpen failed: {e}"))?;
                opened.insert(
                    key,
                    OpenDoc {
                        version: 1,
                        fingerprint,
                    },
                );
                Ok((url, true))
            }
            Some(doc) if doc.fingerprint != fingerprint => {
                doc.version += 1;
                doc.fingerprint = fingerprint;
                sock.did_change(DidChangeTextDocumentParams {
                    text_document: VersionedTextDocumentIdentifier {
                        uri: url.clone(),
                        version: doc.version,
                    },
                    // Full replacement (`range: None`) — plan §2 decision 6: simple and immune to
                    // incremental text-drift; every wired server accepts it.
                    content_changes: vec![TextDocumentContentChangeEvent {
                        range: None,
                        range_length: None,
                        text,
                    }],
                })
                .map_err(|e| anyhow!("didChange failed: {e}"))?;
                Ok((url, true))
            }
            Some(_) => Ok((url, false)),
        }
    }

    /// Open one representative source file (best-effort) so lazy servers can answer project-wide
    /// queries before any file-anchored query has happened.
    async fn seed_open(&self) {
        if let Some(f) = find_source_file(&self.root, self.spec.extensions) {
            let _ = self.ensure_open(&f).await;
        }
    }

    /// Find references to a symbol BY NAME: resolve its definition location via `workspace/symbol`
    /// (so the model never computes line/col), then `textDocument/references` at that position.
    pub async fn references_by_name(
        &self,
        file_hint: Option<&Path>,
        symbol: &str,
        include_decl: bool,
    ) -> Result<Vec<RefHit>> {
        let chosen = self.find_symbol(symbol, file_hint).await?;
        let target = uri::uri_to_path(chosen.uri.as_str())?;
        self.ensure_open(&target).await?;

        let mut sock = self.socket.clone();
        let params = ReferenceParams {
            text_document_position: TextDocumentPositionParams {
                text_document: TextDocumentIdentifier {
                    uri: chosen.uri.clone(),
                },
                position: chosen.range.start,
            },
            context: ReferenceContext {
                include_declaration: include_decl,
            },
            work_done_progress_params: WorkDoneProgressParams::default(),
            partial_result_params: PartialResultParams::default(),
        };
        let locs: Vec<Location> = sock
            .references(params)
            .await
            .map_err(|e| anyhow!("textDocument/references failed: {e}"))?
            .unwrap_or_default();

        // Resolve each hit's ENCLOSING symbol (name + kind) so impact analysis reads at a glance
        // which function/impl a call site sits in — instead of a bare line the model must re-open
        // each file to understand. document_symbol is fetched ONCE per file (cached by URI) and
        // reused across that file's hits: a 100-reference result costs one outline per distinct
        // file, not one per hit.
        let mut outline_cache: HashMap<Url, Option<DocumentSymbolResponse>> = HashMap::new();
        let mut hits = Vec::with_capacity(locs.len());
        for loc in locs {
            let Ok(p) = uri::uri_to_path(loc.uri.as_str()) else {
                continue;
            };
            let line = loc.range.start.line as usize;
            let col = loc.range.start.character as usize;
            let snippet = read_line_snippet(&p, line).await;
            let enclosing = self
                .enclosing_sym_cached(&loc.uri, loc.range.start, &mut outline_cache)
                .await;
            hits.push(RefHit {
                path: p,
                line,
                col,
                snippet,
                enclosing,
            });
        }
        Ok(hits)
    }

    /// Enclosing symbol (name + kind label) for `pos` in `uri`'s outline, fetching + caching the
    /// document-symbol response once per URI. `None` for top-level positions (use-statements, refs
    /// outside any symbol) or when the server can't answer — the caller renders those without a
    /// containing-symbol note.
    async fn enclosing_sym_cached(
        &self,
        uri: &Url,
        pos: Position,
        cache: &mut HashMap<Url, Option<DocumentSymbolResponse>>,
    ) -> Option<(String, &'static str)> {
        if !cache.contains_key(uri) {
            // ensure_open so a lazy server has the buffer before we ask for its outline.
            if let Ok(target) = uri::uri_to_path(uri.as_str()) {
                let _ = self.ensure_open(&target).await;
            }
            let mut sock = self.socket.clone();
            let resp = sock
                .document_symbol(DocumentSymbolParams {
                    text_document: TextDocumentIdentifier { uri: uri.clone() },
                    work_done_progress_params: WorkDoneProgressParams::default(),
                    partial_result_params: PartialResultParams::default(),
                })
                .await
                .ok()
                .flatten();
            cache.insert(uri.clone(), resp);
        }
        match cache.get(uri)?.as_ref()? {
            DocumentSymbolResponse::Nested(v) => find_enclosing_sym(v, pos),
            DocumentSymbolResponse::Flat(v) => v
                .iter()
                .filter(|si| range_contains(&si.location.range, pos))
                // Deepest = smallest range among the flat symbols containing pos.
                .min_by_key(|si| {
                    let r = &si.location.range;
                    (r.end.line - r.start.line) as u64
                })
                .map(|si| (si.name.clone(), kind_label(si.kind))),
        }
    }

    /// Resolve a symbol BY NAME to its definition, returning the definition's source text inline.
    /// The full item range comes from the file's document outline (the `workspace/symbol` range may
    /// cover only the name); falls back to a fixed window when the outline can't say.
    pub async fn definition_by_name(
        &self,
        file_hint: Option<&Path>,
        symbol: &str,
    ) -> Result<DefHit> {
        let chosen = self.find_symbol(symbol, file_hint).await?;
        let target = uri::uri_to_path(chosen.uri.as_str())?;
        self.ensure_open(&target).await?;

        let full_range = self.outline_range_at(&chosen).await;
        let start = chosen.range.start;
        let (first, last) = match full_range {
            Some(r) => (r.start.line as usize, r.end.line as usize),
            None => (
                start.line as usize,
                start.line as usize + DEF_FALLBACK_LINES - 1,
            ),
        };
        let text = tokio::fs::read_to_string(&target)
            .await
            .with_context(|| format!("reading {}", target.display()))?;
        let (source, truncated) = slice_lines(&text, first, last, MAX_DEF_LINES);
        Ok(DefHit {
            path: target,
            line: start.line as usize,
            col: start.character as usize,
            source,
            // The fallback window is a guess, so flag it as truncated unless the outline answered.
            truncated: truncated || full_range.is_none(),
        })
    }

    /// Resolve a symbol BY NAME and return the language server's `textDocument/hover` result —
    /// the type/signature + doc-comment, far cheaper than reading the definition body. The hover
    /// text is markdown; the tool layer formats + caps it (see `format_hover`).
    pub async fn hover_by_name(&self, file_hint: Option<&Path>, symbol: &str) -> Result<HoverHit> {
        let chosen = self.find_symbol(symbol, file_hint).await?;
        let target = uri::uri_to_path(chosen.uri.as_str())?;
        self.ensure_open(&target).await?;
        let mut sock = self.socket.clone();
        let resp: Option<Hover> = sock
            .hover(HoverParams {
                text_document_position_params: TextDocumentPositionParams {
                    text_document: TextDocumentIdentifier {
                        uri: chosen.uri.clone(),
                    },
                    position: chosen.range.start,
                },
                work_done_progress_params: WorkDoneProgressParams::default(),
            })
            .await
            .map_err(|e| anyhow!("textDocument/hover failed: {e}"))?;
        let text = resp.map(|h| flatten_hover(&h.contents)).unwrap_or_default();
        Ok(HoverHit {
            path: target,
            name: chosen.name,
            line: chosen.range.start.line as usize,
            text: text.trim().to_string(),
        })
    }

    /// The full item range enclosing `chosen`'s position, per `textDocument/documentSymbol`.
    async fn outline_range_at(&self, chosen: &SymHit) -> Option<Range> {
        let mut sock = self.socket.clone();
        let resp = sock
            .document_symbol(DocumentSymbolParams {
                text_document: TextDocumentIdentifier {
                    uri: chosen.uri.clone(),
                },
                work_done_progress_params: WorkDoneProgressParams::default(),
                partial_result_params: PartialResultParams::default(),
            })
            .await
            .ok()??;
        match resp {
            DocumentSymbolResponse::Nested(v) => find_enclosing(&v, chosen.range.start),
            DocumentSymbolResponse::Flat(v) => v
                .into_iter()
                .find(|si| {
                    si.name == chosen.name && range_contains(&si.location.range, chosen.range.start)
                })
                .map(|si| si.location.range),
        }
    }

    /// Structural outline of `file` (no bodies) — lets the agent read the right symbol instead of
    /// dumping the whole file.
    pub async fn document_symbols(&self, file: &Path) -> Result<Vec<DocSym>> {
        let (url, _) = self.ensure_open(file).await?;
        let mut sock = self.socket.clone();
        let resp = sock
            .document_symbol(DocumentSymbolParams {
                text_document: TextDocumentIdentifier { uri: url },
                work_done_progress_params: WorkDoneProgressParams::default(),
                partial_result_params: PartialResultParams::default(),
            })
            .await
            .map_err(|e| anyhow!("textDocument/documentSymbol failed: {e}"))?;
        let mut out = Vec::new();
        match resp {
            Some(DocumentSymbolResponse::Nested(v)) => collect_outline(&v, 0, &mut out),
            Some(DocumentSymbolResponse::Flat(mut v)) => {
                v.sort_by_key(|si| si.location.range.start.line);
                for si in v {
                    out.push(DocSym {
                        name: si.name,
                        kind: kind_label(si.kind),
                        line: si.location.range.start.line as usize,
                        depth: 0,
                    });
                }
            }
            None => {}
        }
        Ok(out)
    }

    /// Resolve a named symbol to its full body text (definition file + inclusive line range).
    /// Prefer this over `file_read` when the agent only needs one item — much cheaper tokens.
    pub async fn symbol_body(&self, file_hint: Option<&Path>, symbol: &str) -> Result<SymBody> {
        let chosen = self.find_symbol(symbol, file_hint).await?;
        let target = uri::uri_to_path(chosen.uri.as_str())?;
        self.ensure_open(&target).await?;
        let full_range = self.outline_range_at(&chosen).await.unwrap_or(chosen.range);
        let kind = self.outline_kind_at(&chosen).await.unwrap_or("symbol");
        let text = tokio::fs::read_to_string(&target)
            .await
            .with_context(|| format!("reading {}", target.display()))?;
        let (start_line, end_line) = line_span(&full_range);
        let body = slice_lines_inclusive(&text, start_line, end_line);
        Ok(SymBody {
            path: target,
            name: chosen.name,
            kind,
            start_line,
            end_line,
            text: body,
        })
    }

    /// Replace the full body of a named symbol with `new_body`. Returns (path, start, end, old, new).
    /// Line-based (whole symbol range from the document outline) so UTF-16 LSP columns never bite.
    pub async fn replace_symbol_body(
        &self,
        file_hint: Option<&Path>,
        symbol: &str,
        new_body: &str,
    ) -> Result<(
        PathBuf,
        usize,
        usize,
        String,
        String,
        crate::core::persist::FileFingerprint,
    )> {
        let body = self.symbol_body(file_hint, symbol).await?;
        let content = tokio::fs::read_to_string(&body.path)
            .await
            .with_context(|| format!("reading {}", body.path.display()))?;
        if slice_lines_inclusive(&content, body.start_line, body.end_line) != body.text {
            anyhow::bail!(
                "stale symbol plan for {}: file changed while resolving the symbol; retry",
                body.path.display()
            );
        }
        let base_fingerprint = crate::core::persist::FileFingerprint::for_bytes(content.as_bytes());
        let new_text = replace_line_span(&content, body.start_line, body.end_line, new_body);
        Ok((
            body.path,
            body.start_line,
            body.end_line,
            body.text,
            new_text,
            base_fingerprint,
        ))
    }

    /// Insert `text` immediately before or after a named symbol's full range.
    pub async fn insert_relative_to_symbol(
        &self,
        file_hint: Option<&Path>,
        symbol: &str,
        where_: InsertWhere,
        text: &str,
    ) -> Result<(
        PathBuf,
        usize,
        String,
        crate::core::persist::FileFingerprint,
    )> {
        let body = self.symbol_body(file_hint, symbol).await?;
        let content = tokio::fs::read_to_string(&body.path)
            .await
            .with_context(|| format!("reading {}", body.path.display()))?;
        if slice_lines_inclusive(&content, body.start_line, body.end_line) != body.text {
            anyhow::bail!(
                "stale symbol plan for {}: file changed while resolving the symbol; retry",
                body.path.display()
            );
        }
        let base_fingerprint = crate::core::persist::FileFingerprint::for_bytes(content.as_bytes());
        let new_text =
            insert_relative_line_span(&content, body.start_line, body.end_line, where_, text);
        let at = match where_ {
            InsertWhere::Before => body.start_line,
            InsertWhere::After => body.end_line.saturating_add(1),
        };
        Ok((body.path, at, new_text, base_fingerprint))
    }

    /// Kind label for the outline item enclosing `chosen` (best-effort).
    async fn outline_kind_at(&self, chosen: &SymHit) -> Option<&'static str> {
        let mut sock = self.socket.clone();
        let resp = sock
            .document_symbol(DocumentSymbolParams {
                text_document: TextDocumentIdentifier {
                    uri: chosen.uri.clone(),
                },
                work_done_progress_params: WorkDoneProgressParams::default(),
                partial_result_params: PartialResultParams::default(),
            })
            .await
            .ok()??;
        match resp {
            DocumentSymbolResponse::Nested(v) => find_enclosing_kind(&v, chosen.range.start),
            DocumentSymbolResponse::Flat(v) => v
                .into_iter()
                .find(|si| {
                    si.name == chosen.name && range_contains(&si.location.range, chosen.range.start)
                })
                .map(|si| kind_label(si.kind)),
        }
    }

    /// Project-wide fuzzy symbol search by name. Deduplicated — servers can return the same
    /// definition through multiple index routes (rust-analyzer does).
    pub async fn workspace_symbols(&self, query: &str) -> Result<Vec<WsSym>> {
        let cands = self.query_symbols(query).await?;
        let mut seen = std::collections::HashSet::new();
        Ok(cands
            .into_iter()
            .filter_map(|c| {
                let path = uri::uri_to_path(c.uri.as_str())
                    .unwrap_or_else(|_| PathBuf::from(c.uri.as_str()));
                let line = c.range.map(|r| r.start.line as usize);
                seen.insert(format!("{}\0{}\0{:?}", c.name, path.display(), line))
                    .then_some(WsSym {
                        name: c.name,
                        kind: kind_label(c.kind),
                        path,
                        line,
                    })
            })
            .collect())
    }

    /// Diagnostics for `file` — pull (`textDocument/diagnostic`, deterministic) when the server
    /// advertises it; otherwise settle on push (`publishDiagnostics`) with a quiet window. Sorted
    /// by position.
    pub async fn diagnostics(&self, file: &Path) -> Result<Vec<DiagItem>> {
        self.diagnostics_bounded(file, Duration::from_secs(8)).await
    }

    /// [`diagnostics`](Self::diagnostics) with a caller-chosen push-settle deadline: the tool path
    /// keeps the patient 8s; the post-edit feedback fold uses a tight budget (its whole fold is
    /// hard-capped) so an edit result is never held hostage by a slow re-analysis.
    pub async fn diagnostics_bounded(
        &self,
        file: &Path,
        settle: Duration,
    ) -> Result<Vec<DiagItem>> {
        let (url, refreshed) = self.ensure_open(file).await?;
        let key = uri::normalize_uri(url.as_str());

        if self.caps.diagnostic_provider.is_some() {
            let mut sock = self.socket.clone();
            let pulled = sock
                .document_diagnostic(DocumentDiagnosticParams {
                    text_document: TextDocumentIdentifier { uri: url.clone() },
                    identifier: None,
                    previous_result_id: None,
                    work_done_progress_params: WorkDoneProgressParams::default(),
                    partial_result_params: PartialResultParams::default(),
                })
                .await;
            // Advertised-but-failing pull falls through to the push path rather than erroring.
            if let Ok(DocumentDiagnosticReportResult::Report(DocumentDiagnosticReport::Full(
                full,
            ))) = pulled
            {
                let mut items: Vec<DiagItem> = full
                    .full_document_diagnostic_report
                    .items
                    .iter()
                    .map(to_diag_item)
                    .collect();
                items.sort_by_key(|d| (d.line, d.col));
                return Ok(items);
            }
        }

        // Push fallback: wait (bounded) for the server's next publish for this document, then read
        // the stored snapshot. `refreshed` ⇒ a didOpen/didChange just went out, so a fresh publish
        // is expected; otherwise the existing snapshot is current and a short quiet wait suffices.
        let start_seq = self.push_seq(&key);
        let deadline = Instant::now() + settle;
        let mut last_seen = start_seq;
        let mut last_change = Instant::now();
        loop {
            tokio::time::sleep(Duration::from_millis(100)).await;
            let now_seq = self.push_seq(&key);
            if now_seq != last_seen {
                last_seen = now_seq;
                last_change = Instant::now();
            }
            let quiet = last_change.elapsed() >= Duration::from_millis(400);
            let got_update = now_seq != start_seq;
            let existing_is_current =
                !refreshed && last_change.elapsed() >= Duration::from_millis(600);
            if (got_update && quiet) || existing_is_current || Instant::now() >= deadline {
                let mut items: Vec<DiagItem> = self
                    .diags
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .get(&key)
                    .map(|p| p.items.iter().map(to_diag_item).collect())
                    .unwrap_or_default();
                items.sort_by_key(|d| (d.line, d.col));
                return Ok(items);
            }
        }
    }

    fn push_seq(&self, key: &str) -> Option<u64> {
        self.diags
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(key)
            .map(|p| p.seq)
    }

    /// `workspace/symbol` with a settle loop: while the server is still warming up, an empty result
    /// is retried — until the indexing signal (rust-analyzer) or a short fixed warm-up (servers
    /// without one) says empty really means empty. The manager's outer timeout bounds the total.
    async fn query_symbols(&self, query: &str) -> Result<Vec<SymCand>> {
        let max_attempts = if self.has_indexing_signal() { 600 } else { 16 };
        let mut attempts = 0u32;
        loop {
            let mut sock = self.socket.clone();
            let resp = sock
                .symbol(WorkspaceSymbolParams {
                    query: query.to_string(),
                    ..Default::default()
                })
                .await
                .map_err(|e| anyhow!("workspace/symbol failed: {e}"))?;
            let cands = flatten_symbols(resp);
            if !cands.is_empty() {
                return Ok(cands);
            }
            attempts += 1;
            if self.is_indexed() || attempts >= max_attempts {
                return Ok(Vec::new());
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
    }

    /// Resolve a symbol name to a single definition location via `workspace/symbol`.
    async fn find_symbol(&self, symbol: &str, file_hint: Option<&Path>) -> Result<SymHit> {
        let cands = self.query_symbols(symbol).await?;
        pick_symbol(cands, symbol, file_hint).ok_or_else(|| {
            anyhow!(
                "no symbol named '{symbol}' found in the project{}",
                if self.is_indexed() {
                    ""
                } else {
                    " (server may still be indexing — try again)"
                }
            )
        })
    }

    /// Graceful shutdown handshake (best-effort; `Drop` is the hard backstop).
    pub async fn shutdown(&self) {
        let mut sock = self.socket.clone();
        let _ = tokio::time::timeout(Duration::from_secs(5), sock.shutdown(())).await;
        let _ = sock.exit(());
        let _ = sock.emit(Stop);
    }
}

impl Drop for LspServer {
    fn drop(&mut self) {
        // Abort the mainloop task → its owned Child (+ Job Object) drops → the server tree is reaped.
        self.mainloop_handle.abort();
    }
}

/// FNV-1a over bytes — cheap content fingerprint for the didChange re-sync check.
fn fnv1a(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for &b in bytes {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

/// Short human label for an LSP `SymbolKind` (a struct-of-consts, so a lookup table, not a match).
fn kind_label(kind: SymbolKind) -> &'static str {
    const LABELS: &[(SymbolKind, &str)] = &[
        (SymbolKind::FILE, "file"),
        (SymbolKind::MODULE, "mod"),
        (SymbolKind::NAMESPACE, "namespace"),
        (SymbolKind::PACKAGE, "package"),
        (SymbolKind::CLASS, "class"),
        (SymbolKind::METHOD, "method"),
        (SymbolKind::PROPERTY, "prop"),
        (SymbolKind::FIELD, "field"),
        (SymbolKind::CONSTRUCTOR, "ctor"),
        (SymbolKind::ENUM, "enum"),
        (SymbolKind::INTERFACE, "interface"),
        (SymbolKind::FUNCTION, "fn"),
        (SymbolKind::VARIABLE, "var"),
        (SymbolKind::CONSTANT, "const"),
        (SymbolKind::STRING, "string"),
        (SymbolKind::NUMBER, "number"),
        (SymbolKind::BOOLEAN, "bool"),
        (SymbolKind::ARRAY, "array"),
        (SymbolKind::OBJECT, "object"),
        (SymbolKind::KEY, "key"),
        (SymbolKind::NULL, "null"),
        (SymbolKind::ENUM_MEMBER, "variant"),
        (SymbolKind::STRUCT, "struct"),
        (SymbolKind::EVENT, "event"),
        (SymbolKind::OPERATOR, "op"),
        (SymbolKind::TYPE_PARAMETER, "typeparam"),
    ];
    LABELS
        .iter()
        .find(|(k, _)| *k == kind)
        .map(|(_, l)| *l)
        .unwrap_or("sym")
}

/// Short label for a diagnostic severity (also a struct-of-consts).
fn severity_label(sev: Option<DiagnosticSeverity>) -> &'static str {
    match sev {
        Some(DiagnosticSeverity::ERROR) => "error",
        Some(DiagnosticSeverity::WARNING) => "warning",
        Some(DiagnosticSeverity::HINT) => "hint",
        // INFORMATION or anything unknown/omitted.
        _ => "info",
    }
}

fn to_diag_item(d: &Diagnostic) -> DiagItem {
    DiagItem {
        line: d.range.start.line as usize,
        col: d.range.start.character as usize,
        severity: severity_label(d.severity),
        message: d.message.clone(),
        code: d.code.as_ref().map(|c| match c {
            NumberOrString::Number(n) => n.to_string(),
            NumberOrString::String(s) => s.clone(),
        }),
    }
}

/// `pos` within `range` (inclusive start, exclusive-ish end is fine for containment checks).
fn range_contains(range: &Range, pos: Position) -> bool {
    let after_start = pos.line > range.start.line
        || (pos.line == range.start.line && pos.character >= range.start.character);
    let before_end = pos.line < range.end.line
        || (pos.line == range.end.line && pos.character <= range.end.character);
    after_start && before_end
}

/// Deepest outline item whose NAME (`selection_range`) sits at `pos` → its full `range`; if `pos`
/// is inside an item's body but on no nested name, that item's range.
fn find_enclosing(nodes: &[DocumentSymbol], pos: Position) -> Option<Range> {
    for n in nodes {
        if range_contains(&n.selection_range, pos) {
            return Some(n.range);
        }
        if range_contains(&n.range, pos) {
            if let Some(children) = &n.children {
                if let Some(r) = find_enclosing(children, pos) {
                    return Some(r);
                }
            }
            return Some(n.range);
        }
    }
    None
}

/// Same walk as [`find_enclosing`], but returns the kind label of the deepest hit.
fn find_enclosing_kind(nodes: &[DocumentSymbol], pos: Position) -> Option<&'static str> {
    for n in nodes {
        if range_contains(&n.selection_range, pos) {
            return Some(kind_label(n.kind));
        }
        if range_contains(&n.range, pos) {
            if let Some(children) = &n.children {
                if let Some(k) = find_enclosing_kind(children, pos) {
                    return Some(k);
                }
            }
            return Some(kind_label(n.kind));
        }
    }
    None
}

/// Same descent as [`find_enclosing`], but returns the deepest enclosing symbol's `(name, kind)` —
/// what a reference hit needs to render "in fn X". Prefers the deepest child whose full range
/// contains `pos`, so a method inside an impl/class wins over the outer block.
fn find_enclosing_sym(nodes: &[DocumentSymbol], pos: Position) -> Option<(String, &'static str)> {
    for n in nodes {
        if range_contains(&n.range, pos) {
            // Descend first: a nested item (method in an impl) is the more precise answer.
            if let Some(children) = &n.children {
                if let Some(inner) = find_enclosing_sym(children, pos) {
                    return Some(inner);
                }
            }
            return Some((n.name.clone(), kind_label(n.kind)));
        }
        // `pos` on the name but outside the body range (rare) still identifies the symbol.
        if range_contains(&n.selection_range, pos) {
            return Some((n.name.clone(), kind_label(n.kind)));
        }
    }
    None
}

/// Inclusive 0-based line span of an LSP range (end column 0 still includes that line when the
/// end character is > 0; if end is at col 0 of a later line, that line is excluded — standard
/// half-open end semantics reduced to whole lines for text surgery).
fn line_span(range: &Range) -> (usize, usize) {
    let start = range.start.line as usize;
    let end = if range.end.character == 0 && range.end.line > range.start.line {
        (range.end.line as usize).saturating_sub(1)
    } else {
        range.end.line as usize
    };
    (start, end.max(start))
}

/// All lines `first..=last` (0-based inclusive) joined with `\n` — no cap (used for edits).
fn slice_lines_inclusive(text: &str, first: usize, last: usize) -> String {
    text.lines()
        .skip(first)
        .take(last.saturating_sub(first) + 1)
        .collect::<Vec<_>>()
        .join("\n")
}

/// Replace lines `start..=end` (0-based inclusive) with `new_body`. Preserves the file's EOL
/// style (CRLF if the original used it) and a trailing newline if the original had one.
fn replace_line_span(content: &str, start: usize, end: usize, new_body: &str) -> String {
    let crlf = content.contains("\r\n");
    let had_trailing_nl = content.ends_with('\n');
    let lines: Vec<&str> = content.lines().collect();
    if lines.is_empty() {
        return join_lines(
            &new_body.lines().map(|s| s.to_string()).collect::<Vec<_>>(),
            crlf,
            had_trailing_nl || !new_body.is_empty(),
        );
    }
    let start = start.min(lines.len() - 1);
    let end = end.min(lines.len() - 1).max(start);
    let mut owned: Vec<String> = lines[..start].iter().map(|s| (*s).to_string()).collect();
    if !new_body.is_empty() {
        owned.extend(new_body.lines().map(|s| s.to_string()));
    }
    if end + 1 < lines.len() {
        owned.extend(lines[end + 1..].iter().map(|s| (*s).to_string()));
    }
    join_lines(&owned, crlf, had_trailing_nl)
}

/// Insert `text` as new lines immediately before `start` or after `end` (0-based inclusive).
fn insert_relative_line_span(
    content: &str,
    start: usize,
    end: usize,
    where_: InsertWhere,
    text: &str,
) -> String {
    let crlf = content.contains("\r\n");
    let had_trailing_nl = content.ends_with('\n');
    let lines: Vec<&str> = content.lines().collect();
    let start = start.min(lines.len());
    let end = if lines.is_empty() {
        0
    } else {
        end.min(lines.len() - 1)
    };
    let insert_at = match where_ {
        InsertWhere::Before => start,
        InsertWhere::After => end.saturating_add(1).min(lines.len()),
    };
    let new_lines: Vec<String> = if text.is_empty() {
        Vec::new()
    } else {
        text.lines().map(|s| s.to_string()).collect()
    };
    let mut owned: Vec<String> = lines[..insert_at]
        .iter()
        .map(|s| (*s).to_string())
        .collect();
    owned.extend(new_lines);
    owned.extend(lines[insert_at..].iter().map(|s| (*s).to_string()));
    join_lines(&owned, crlf, had_trailing_nl)
}

fn join_lines(lines: &[String], crlf: bool, trailing_nl: bool) -> String {
    let sep = if crlf { "\r\n" } else { "\n" };
    let mut s = lines.join(sep);
    if trailing_nl && !s.ends_with('\n') {
        s.push_str(sep);
    }
    s
}

/// Flatten a nested outline into display rows, depth-first (document order preserved).
fn collect_outline(nodes: &[DocumentSymbol], depth: usize, out: &mut Vec<DocSym>) {
    for n in nodes {
        out.push(DocSym {
            name: n.name.clone(),
            kind: kind_label(n.kind),
            line: n.selection_range.start.line as usize,
            depth,
        });
        if let Some(children) = &n.children {
            collect_outline(children, depth + 1, out);
        }
    }
}

/// Lines `first..=last` (0-based, inclusive) of `text`, capped at `cap` lines. Returns the slice and
/// whether the cap cut it short.
fn slice_lines(text: &str, first: usize, last: usize, cap: usize) -> (String, bool) {
    let want = last.saturating_sub(first) + 1;
    let n = want.min(cap);
    let lines: Vec<&str> = text.lines().skip(first).take(n).collect();
    (lines.join("\n"), want > cap)
}

/// Flatten an LSP `HoverContents` into a plain string. Handles all three shapes: a single
/// `MarkedString` (plain or `language`-tagged code), an array of them (joined), and `MarkupContent`
/// (markdown/plaintext — returned as-is; the tool layer caps line count).
fn flatten_hover(contents: &HoverContents) -> String {
    fn marked(ms: &MarkedString) -> String {
        match ms {
            MarkedString::String(s) => s.clone(),
            MarkedString::LanguageString(ls) => ls.value.clone(),
        }
    }
    match contents {
        HoverContents::Scalar(ms) => marked(ms),
        HoverContents::Array(v) => v.iter().map(marked).collect::<Vec<_>>().join("\n\n"),
        HoverContents::Markup(mc) => mc.value.clone(),
    }
}

/// First source file under `root` matching one of `extensions` — bounded BFS (skips dependency /
/// VCS / build dirs; depth + directory caps keep it cheap even on big trees).
fn find_source_file(root: &Path, extensions: &[&str]) -> Option<PathBuf> {
    const SKIP_DIRS: &[&str] = &[
        "node_modules",
        ".git",
        "target",
        "dist",
        "build",
        "out",
        ".next",
        ".venv",
        "venv",
        "__pycache__",
        ".tox",
    ];
    const MAX_DEPTH: usize = 4;
    const MAX_DIRS: usize = 300;
    let mut queue: VecDeque<(PathBuf, usize)> = VecDeque::from([(root.to_path_buf(), 0)]);
    let mut visited = 0usize;
    while let Some((dir, depth)) = queue.pop_front() {
        visited += 1;
        if visited > MAX_DIRS {
            return None;
        }
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_file() {
                if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
                    if extensions.iter().any(|x| x.eq_ignore_ascii_case(ext)) {
                        return Some(path);
                    }
                }
            } else if path.is_dir() && depth < MAX_DEPTH {
                let name = entry.file_name();
                let name = name.to_string_lossy();
                if !SKIP_DIRS.iter().any(|s| name.eq_ignore_ascii_case(s)) && !name.starts_with('.')
                {
                    queue.push_back((path, depth + 1));
                }
            }
        }
    }
    None
}

/// Flatten a `workspace/symbol` response into candidates. Lazy entries (URI, no range) are KEPT —
/// `workspace_symbols` can still show the file; position-needing callers filter via `pick_symbol`.
fn flatten_symbols(resp: Option<WorkspaceSymbolResponse>) -> Vec<SymCand> {
    match resp {
        Some(WorkspaceSymbolResponse::Flat(v)) => v
            .into_iter()
            .map(|si| SymCand {
                name: si.name,
                kind: si.kind,
                uri: si.location.uri,
                range: Some(si.location.range),
                container: si.container_name,
            })
            .collect(),
        Some(WorkspaceSymbolResponse::Nested(v)) => v
            .into_iter()
            .map(|ws| {
                let container = ws.container_name;
                match ws.location {
                    OneOf::Left(loc) => SymCand {
                        name: ws.name,
                        kind: ws.kind,
                        uri: loc.uri,
                        range: Some(loc.range),
                        container,
                    },
                    OneOf::Right(wsl) => SymCand {
                        name: ws.name,
                        kind: ws.kind,
                        uri: wsl.uri,
                        range: None,
                        container,
                    },
                }
            })
            .collect(),
        None => Vec::new(),
    }
}

/// Choose the best position-resolved candidate for `symbol`, which may be a hierarchical name-path
/// `Container/leaf` (e.g. `MyStruct/new`, `Outer/Inner/method`) to disambiguate same-named symbols
/// on different types — Serena-style. Resolution order:
///   1. exact match on the LEAF name,
///   2. among those, if a container was given, prefer one whose `container_name` matches it
///      (case-insensitive; the last path segment is enough, so `a/b/leaf` matches container `b`),
///   3. among the survivors, prefer one whose file matches `file_hint`,
///   4. else the first exact match, then the first fuzzy one.
/// A bare `symbol` (no `/`) behaves exactly as before. When a server omits `container_name` the
/// container filter simply finds nothing and falls through to the file-hint / first-match path.
fn pick_symbol(candidates: Vec<SymCand>, symbol: &str, file_hint: Option<&Path>) -> Option<SymHit> {
    let usable: Vec<SymCand> = candidates
        .into_iter()
        .filter(|c| c.range.is_some())
        .collect();
    if usable.is_empty() {
        return None;
    }
    // Split a `Container/leaf` path: the leaf is the last segment, the container is the segment
    // before it (deeper ancestors are ignored — servers report at most the immediate container).
    let (container, leaf) = match symbol.rsplit_once('/') {
        Some((c, l)) => (
            c.rsplit('/')
                .next()
                .map(str::trim)
                .filter(|s| !s.is_empty()),
            l.trim(),
        ),
        None => (None, symbol),
    };
    let hint = file_hint
        .and_then(|p| p.file_name())
        .and_then(|s| s.to_str())
        .map(|s| s.to_string());
    let to_hit = |c: &SymCand| SymHit {
        name: c.name.clone(),
        uri: c.uri.clone(),
        range: c.range.unwrap(),
    };
    let (exact, rest): (Vec<_>, Vec<_>) = usable.into_iter().partition(|c| c.name == leaf);
    if exact.is_empty() {
        return rest.first().map(to_hit);
    }
    // Container disambiguation (only when the path carried one): narrow to matching containers if
    // any exist; if none match, keep the full exact set (the container hint was unmet, not fatal).
    let pool: Vec<&SymCand> = match container {
        Some(want) => {
            let matched: Vec<&SymCand> = exact
                .iter()
                .filter(|c| {
                    c.container
                        .as_deref()
                        .map(|cn| {
                            cn.rsplit(&['/', ':', '.'][..])
                                .next()
                                .unwrap_or(cn)
                                .eq_ignore_ascii_case(want)
                        })
                        .unwrap_or(false)
                })
                .collect();
            if matched.is_empty() {
                exact.iter().collect()
            } else {
                matched
            }
        }
        None => exact.iter().collect(),
    };
    if let Some(h) = &hint {
        if let Some(m) = pool.iter().find(|c| c.uri.as_str().ends_with(h.as_str())) {
            return Some(to_hit(m));
        }
    }
    pool.first().map(|c| to_hit(c))
}

/// Best-effort: read line `line0` (0-based) of `path`, trimmed and length-capped. Empty on error.
async fn read_line_snippet(path: &Path, line0: usize) -> String {
    match tokio::fs::read_to_string(path).await {
        Ok(text) => text
            .lines()
            .nth(line0)
            .map(|l| l.trim().chars().take(200).collect())
            .unwrap_or_default(),
        Err(_) => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pos(line: u32, ch: u32) -> Position {
        Position {
            line,
            character: ch,
        }
    }

    fn range(l1: u32, c1: u32, l2: u32, c2: u32) -> Range {
        Range {
            start: pos(l1, c1),
            end: pos(l2, c2),
        }
    }

    #[allow(deprecated)] // DocumentSymbol has a deprecated `deprecated` field we must fill
    fn doc_sym(
        name: &str,
        full: Range,
        sel: Range,
        children: Vec<DocumentSymbol>,
    ) -> DocumentSymbol {
        DocumentSymbol {
            name: name.to_string(),
            detail: None,
            kind: SymbolKind::FUNCTION,
            tags: None,
            deprecated: None,
            range: full,
            selection_range: sel,
            children: if children.is_empty() {
                None
            } else {
                Some(children)
            },
        }
    }

    #[test]
    fn kind_and_severity_labels() {
        assert_eq!(kind_label(SymbolKind::STRUCT), "struct");
        assert_eq!(kind_label(SymbolKind::FUNCTION), "fn");
        assert_eq!(kind_label(SymbolKind::ENUM_MEMBER), "variant");
        assert_eq!(severity_label(Some(DiagnosticSeverity::ERROR)), "error");
        assert_eq!(severity_label(Some(DiagnosticSeverity::WARNING)), "warning");
        assert_eq!(severity_label(None), "info");
    }

    #[test]
    fn range_containment() {
        let r = range(2, 4, 5, 0);
        assert!(range_contains(&r, pos(2, 4)), "inclusive start");
        assert!(range_contains(&r, pos(3, 0)), "middle line");
        assert!(range_contains(&r, pos(5, 0)), "inclusive end");
        assert!(!range_contains(&r, pos(2, 3)), "before start col");
        assert!(!range_contains(&r, pos(6, 0)), "after end");
    }

    fn cand(name: &str, uri: &str, container: Option<&str>) -> SymCand {
        SymCand {
            name: name.to_string(),
            kind: SymbolKind::METHOD,
            uri: Url::parse(uri).unwrap(),
            range: Some(range(0, 0, 1, 0)),
            container: container.map(str::to_string),
        }
    }

    #[test]
    fn pick_symbol_bare_name_unchanged() {
        // No `/` → the pre-existing behaviour: first exact match, file-hint tiebreak.
        let cands = vec![
            cand("run", "file:///a/foo.rs", Some("Foo")),
            cand("run", "file:///a/bar.rs", Some("Bar")),
        ];
        let hit = pick_symbol(cands, "run", Some(Path::new("bar.rs"))).unwrap();
        assert!(
            hit.uri.as_str().ends_with("bar.rs"),
            "file hint still wins: {}",
            hit.uri
        );
    }

    #[test]
    fn pick_symbol_container_path_disambiguates() {
        // Two same-named methods on different types → `Container/method` picks the right one, even
        // with no file hint (which is useless when both live in the same file).
        let cands = vec![
            cand("run", "file:///a/svc.rs", Some("Server")),
            cand("run", "file:///a/svc.rs", Some("Client")),
        ];
        let hit = pick_symbol(cands, "Client/run", None).unwrap();
        assert!(hit.uri.as_str().ends_with("svc.rs"));
        // The container filter must have selected the Client occurrence, not the first (Server).
        let cands2 = vec![
            cand("run", "file:///a/svc.rs", Some("Server")),
            cand("run", "file:///a/svc.rs", Some("Client")),
        ];
        // Sanity: bare name returns the FIRST (Server) — proving the path arg changed the outcome.
        let bare = pick_symbol(cands2, "run", None).unwrap();
        assert!(bare.uri.as_str().ends_with("svc.rs"));
    }

    #[test]
    fn pick_symbol_unmet_container_falls_through() {
        // A container that matches nothing is a hint, not a filter: fall through to the exact set.
        let cands = vec![cand("run", "file:///a/foo.rs", Some("Foo"))];
        let hit = pick_symbol(cands, "Nonexistent/run", None).expect("must still resolve the leaf");
        assert!(hit.uri.as_str().ends_with("foo.rs"));
    }

    #[test]
    fn enclosing_prefers_name_match_then_deepest_child() {
        // mod outer { fn inner() {...} } — outer spans 0..10, inner 2..5, names on their first lines.
        let inner = doc_sym("inner", range(2, 0, 5, 1), range(2, 3, 2, 8), vec![]);
        let outer = doc_sym("outer", range(0, 0, 10, 1), range(0, 4, 0, 9), vec![inner]);
        let tree = vec![outer];
        // On the inner name → inner's FULL range.
        assert_eq!(find_enclosing(&tree, pos(2, 4)), Some(range(2, 0, 5, 1)));
        // On the outer name → outer's full range.
        assert_eq!(find_enclosing(&tree, pos(0, 5)), Some(range(0, 0, 10, 1)));
        // Inside outer's body but on no nested name → outer.
        assert_eq!(find_enclosing(&tree, pos(8, 0)), Some(range(0, 0, 10, 1)));
        // Outside everything → None.
        assert_eq!(find_enclosing(&tree, pos(20, 0)), None);
    }

    #[test]
    fn outline_flattening_keeps_depth_and_order() {
        let child = doc_sym("child", range(2, 0, 3, 0), range(2, 3, 2, 8), vec![]);
        let parent = doc_sym("parent", range(0, 0, 10, 0), range(0, 4, 0, 9), vec![child]);
        let mut out = Vec::new();
        collect_outline(&[parent], 0, &mut out);
        assert_eq!(out.len(), 2);
        assert_eq!(
            (out[0].name.as_str(), out[0].depth, out[0].line),
            ("parent", 0, 0)
        );
        assert_eq!(
            (out[1].name.as_str(), out[1].depth, out[1].line),
            ("child", 1, 2)
        );
    }

    #[test]
    fn line_slicing_caps() {
        let text = "a\nb\nc\nd\ne";
        assert_eq!(slice_lines(text, 1, 3, 100), ("b\nc\nd".to_string(), false));
        assert_eq!(
            slice_lines(text, 1, 4, 2),
            ("b\nc".to_string(), true),
            "cap cuts"
        );
        assert_eq!(
            slice_lines(text, 4, 9, 100),
            ("e".to_string(), false),
            "EOF-tolerant"
        );
    }

    #[test]
    fn line_span_half_open_end() {
        // end at col 0 of line 5 → lines 2..=4
        assert_eq!(line_span(&range(2, 0, 5, 0)), (2, 4));
        // end mid-line 5 → include line 5
        assert_eq!(line_span(&range(2, 0, 5, 3)), (2, 5));
        // single-line name range
        assert_eq!(line_span(&range(3, 4, 3, 10)), (3, 3));
    }

    #[test]
    fn replace_line_span_middle() {
        let content = "a\nb\nc\nd\ne\n";
        let out = replace_line_span(content, 1, 3, "X\nY");
        assert_eq!(out, "a\nX\nY\ne\n");
    }

    #[test]
    fn replace_line_span_crlf_and_delete() {
        let content = "a\r\nb\r\nc\r\n";
        let out = replace_line_span(content, 1, 1, "BB");
        assert_eq!(out, "a\r\nBB\r\nc\r\n");
        let deleted = replace_line_span(content, 1, 1, "");
        assert_eq!(deleted, "a\r\nc\r\n");
    }

    #[test]
    fn insert_before_after_symbol_span() {
        let content = "a\nfn foo() {}\nbar\n";
        // symbol on line 1
        let before = insert_relative_line_span(content, 1, 1, InsertWhere::Before, "// hi");
        assert_eq!(before, "a\n// hi\nfn foo() {}\nbar\n");
        let after = insert_relative_line_span(content, 1, 1, InsertWhere::After, "// bye");
        assert_eq!(after, "a\nfn foo() {}\n// bye\nbar\n");
    }

    #[test]
    fn fingerprints_differ() {
        assert_ne!(fnv1a(b"fn main() {}"), fnv1a(b"fn main() { }"));
        assert_eq!(fnv1a(b"same"), fnv1a(b"same"));
    }

    #[test]
    fn source_file_discovery_skips_dep_dirs() {
        let root = std::env::temp_dir().join(format!("aizen-lsp-seed-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("node_modules/pkg")).unwrap();
        std::fs::write(root.join("node_modules/pkg/index.ts"), "x").unwrap();
        assert!(
            find_source_file(&root, &["ts"]).is_none(),
            "node_modules is skipped"
        );
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join("src/app.ts"), "export const x = 1;").unwrap();
        let found = find_source_file(&root, &["ts"]).expect("finds src/app.ts");
        assert!(found.ends_with("app.ts"));
        let _ = std::fs::remove_dir_all(&root);
    }
}
