//! Language-neutral query normalization for codebase retrieval.
//!
//! # The problem this solves
//!
//! Code identifiers are English. Almost always — `login`, `save`, `retry`, `parse`. But the person
//! asking about that code may use any language. When they type "chỗ nào xử lý đăng nhập", the
//! lexical retrieval path tokenizes it into Vietnamese tokens that appear in ZERO chunks, so:
//!
//! - `rank_chunks` scores every chunk 0.0 and returns nothing, and
//! - `gate_passes` sees `covered == 0` and blocks the per-turn auto-injection.
//!
//! The index is fine. The tokenizer is fine (it is Unicode-aware and preserves diacritics). The
//! mismatch is purely that the QUERY is in one language and the CORPUS is in another.
//!
//! # Why normalize the query instead of the index
//!
//! The tempting fix is a multilingual embedding model. Two independent reasons that is the wrong
//! lever here:
//!
//! 1. **Order of operations defeats it.** The dense tier (`fuse_dense`) only RERANKS candidates that
//!    the lexical pass already produced, and `gate_passes` is lexical regardless. A query with zero
//!    lexical candidates gives dense nothing to rerank, so the gate still closes. Dense would have to
//!    become a candidate GENERATOR first — a much larger change.
//! 2. **This project already measured the opposite.** The P6 CI bench found the small English
//!    embedder lifted paraphrase recall MORE than a ~18x-larger multilingual one, on bilingual
//!    fixtures (see `core::config::embed_model_name`).
//!
//! Normalizing the query needs no index rebuild, no embedding model, and no `--features dense` — so
//! it keeps the single-static-binary and startup-time constraints intact.
//!
//! # What this module does
//!
//! `expand_query` APPENDS English identifier terms; it never replaces the original. An
//! English-language query is left semantically untouched (nothing matches the glossary), so this is
//! a no-op on the existing path rather than a behavior change — the reason it is safe to run
//! unconditionally on every turn.
//!
//! Two passes, longest-match-first:
//!
//! - **Phrase pass** over the raw lowercased text, so multi-word concepts map as a unit
//!   ("đăng nhập" → login/auth/signin) before their parts are considered separately.
//! - **Token pass** over the tokenizer's output, catching single words the phrase pass missed.
//!
//! Diacritic-folded keys are matched too ("dang nhap" — how people type without an IME).
//!
//! The glossary is intentionally a static table: zero tokens, zero latency, deterministic, testable.
//! It covers the common programming vocabulary of several languages, not every word of any. The
//! LLM fallback (Phase 2) is what generalizes to arbitrary phrasing in arbitrary languages; this
//! table is the free path that makes the fallback unnecessary most of the time.

use once_cell::sync::Lazy;
use std::collections::{BTreeSet, HashMap};
use std::sync::RwLock;

use crate::memory::tokenize::tokenize;

/// Multi-word concepts, matched against the raw lowercased query before the token pass.
///
/// Longest-first is enforced at match time, not by this table's order: "đăng nhập" must win over a
/// hypothetical "đăng" entry, otherwise the compound concept is lost to its parts.
const PHRASES: &[(&str, &str)] = &[
    // ── Vietnamese ────────────────────────────────────────────────────────────────────────────
    ("đăng nhập", "login auth signin authenticate"),
    ("dang nhap", "login auth signin authenticate"),
    ("đăng ký", "signup register registration"),
    ("dang ky", "signup register registration"),
    ("đăng xuất", "logout signout"),
    ("dang xuat", "logout signout"),
    ("mật khẩu", "password credential"),
    ("mat khau", "password credential"),
    ("quên mật khẩu", "password reset forgot"),
    ("phân quyền", "authorization permission role access"),
    ("phan quyen", "authorization permission role access"),
    ("xác thực", "authentication verify validate token"),
    ("xac thuc", "authentication verify validate token"),
    ("cơ sở dữ liệu", "database db sql query connection"),
    ("co so du lieu", "database db sql query connection"),
    ("kết nối", "connect connection client socket"),
    ("ket noi", "connect connection client socket"),
    ("cửa sổ", "window session terminal pane"),
    ("cua so", "window session terminal pane"),
    ("giao diện", "ui interface render view frontend"),
    ("giao dien", "ui interface render view frontend"),
    ("bộ nhớ", "memory cache store buffer"),
    ("bo nho", "memory cache store buffer"),
    ("xử lý lỗi", "error handling exception result"),
    ("xu ly loi", "error handling exception result"),
    ("ghi log", "log logging trace"),
    ("tệp tin", "file path fs"),
    ("tep tin", "file path fs"),
    ("thư mục", "directory dir folder path"),
    ("thu muc", "directory dir folder path"),
    ("cấu hình", "config configuration settings options"),
    ("cau hinh", "config configuration settings options"),
    ("kiểm thử", "test testing assert spec"),
    ("kiem thu", "test testing assert spec"),
    ("khởi động", "startup start init boot launch"),
    ("khoi dong", "startup start init boot launch"),
    ("gửi yêu cầu", "request send http post fetch"),
    ("máy chủ", "server host backend daemon"),
    ("may chu", "server host backend daemon"),
    ("người dùng", "user account profile"),
    ("nguoi dung", "user account profile"),
    ("thanh toán", "payment billing checkout charge"),
    ("thanh toan", "payment billing checkout charge"),
    ("giỏ hàng", "cart basket checkout"),
    ("gio hang", "cart basket checkout"),
    ("tìm kiếm", "search query find index"),
    ("tim kiem", "search query find index"),
    ("phiên bản", "version release semver"),
    ("phien ban", "version release semver"),
    ("hàng đợi", "queue channel worker job"),
    ("hang doi", "queue channel worker job"),
    ("luồng", "thread task spawn concurrency"),
    ("bất đồng bộ", "async await future concurrency"),
    ("bat dong bo", "async await future concurrency"),
    ("biến môi trường", "env environment variable"),
    ("bien moi truong", "env environment variable"),
    ("mã hóa", "encrypt encryption cipher hash"),
    ("ma hoa", "encrypt encryption cipher hash"),
    ("giới hạn tốc độ", "rate limit throttle backoff"),
    ("phụ thuộc", "dependency dependencies import"),
    ("phu thuoc", "dependency dependencies import"),
    ("triển khai", "deploy deployment release build"),
    ("trien khai", "deploy deployment release build"),
    // ── Spanish / Portuguese ──────────────────────────────────────────────────────────────────
    ("iniciar sesión", "login auth signin"),
    ("iniciar sesion", "login auth signin"),
    ("contraseña", "password credential"),
    ("base de datos", "database db sql"),
    ("banco de dados", "database db sql"),
    ("usuário", "user account"),
    ("carrinho", "cart checkout"),
    // ── French / German ───────────────────────────────────────────────────────────────────────
    ("connexion", "login auth signin connection"),
    ("mot de passe", "password credential"),
    ("base de données", "database db sql"),
    ("utilisateur", "user account"),
    ("anmeldung", "login auth signin"),
    ("benutzer", "user account"),
    ("passwort", "password credential"),
    ("datenbank", "database db sql"),
    ("einstellungen", "config settings options"),
    // ── Chinese / Japanese / Korean ───────────────────────────────────────────────────────────
    ("登录", "login auth signin"),
    ("登入", "login auth signin"),
    ("注册", "signup register"),
    ("密码", "password credential"),
    ("数据库", "database db sql"),
    ("用户", "user account"),
    ("配置", "config settings options"),
    ("错误", "error exception fail"),
    ("内存", "memory cache buffer"),
    ("窗口", "window session pane"),
    ("ログイン", "login auth signin"),
    ("パスワード", "password credential"),
    ("データベース", "database db sql"),
    ("ユーザー", "user account"),
    ("設定", "config settings options"),
    ("로그인", "login auth signin"),
    ("비밀번호", "password credential"),
    ("사용자", "user account"),
    // ── Russian ───────────────────────────────────────────────────────────────────────────────
    ("вход", "login auth signin"),
    ("пароль", "password credential"),
    ("пользователь", "user account"),
    ("настройки", "config settings options"),
    ("ошибка", "error exception fail"),
    ("база данных", "database db sql"),
];

/// Single-word terms, matched against tokenizer output.
///
/// Deliberately excludes words whose English mapping is so generic it would match half the corpus
/// and defeat the relevance gate's purpose (e.g. bare "làm"/"cái"/"này").
const WORDS: &[(&str, &str)] = &[
    // ── Vietnamese ────────────────────────────────────────────────────────────────────────────
    ("lỗi", "error fail panic exception bug"),
    ("loi", "error fail panic exception bug"),
    ("sửa", "fix patch repair"),
    ("sua", "fix patch repair"),
    ("lưu", "save persist write store"),
    ("luu", "save persist write store"),
    ("đọc", "read load parse"),
    ("doc", "read load parse"),
    ("ghi", "write save persist"),
    ("xóa", "delete remove drop clear"),
    ("xoa", "delete remove drop clear"),
    ("thêm", "add insert append create"),
    ("them", "add insert append create"),
    ("sửa đổi", "update modify edit"),
    ("cập nhật", "update refresh sync"),
    ("cap nhat", "update refresh sync"),
    ("tạo", "create new init build"),
    ("tao", "create new init build"),
    ("gửi", "send post publish emit"),
    ("gui", "send post publish emit"),
    ("nhận", "receive recv handle consume"),
    ("nhan", "receive recv handle consume"),
    ("chạy", "run exec execute spawn"),
    ("chay", "run exec execute spawn"),
    ("dừng", "stop cancel abort halt"),
    ("dung", "stop cancel abort halt"),
    ("hủy", "cancel abort revert"),
    ("huy", "cancel abort revert"),
    ("thử", "retry attempt test"),
    ("thu", "retry attempt test"),
    ("kiểm tra", "check validate verify test"),
    ("kiem tra", "check validate verify test"),
    ("mô hình", "model"),
    ("mo hinh", "model"),
    ("phiên", "session"),
    ("phien", "session"),
    ("tin nhắn", "message msg"),
    ("tin nhan", "message msg"),
    ("hình ảnh", "image img picture"),
    ("hinh anh", "image img picture"),
    ("nút", "button"),
    ("nut", "button"),
    ("biểu mẫu", "form input field"),
    ("bảng", "table schema grid"),
    ("bang", "table schema grid"),
    ("khóa", "key lock"),
    ("khoa", "key lock"),
    ("quyền", "permission role access"),
    ("quyen", "permission role access"),
    ("mạng", "network http socket"),
    ("mang", "network http socket"),
    ("đường dẫn", "path route url"),
    ("duong dan", "path route url"),
    ("tuyến", "route routing"),
    ("hàm", "function fn method"),
    ("ham", "function fn method"),
    ("lớp", "class struct type"),
    ("lop", "class struct type"),
    ("biến", "variable var field"),
    ("bien", "variable var field"),
    ("thông báo", "notification notify alert message"),
    ("thong bao", "notification notify alert message"),
    ("tải", "load download fetch"),
    ("tai", "load download fetch"),
    ("nhanh", "fast performance speed"),
    ("chậm", "slow performance latency"),
    ("cham", "slow performance latency"),
    ("an toàn", "safe safety security"),
    ("bảo mật", "security secure auth"),
    ("bao mat", "security secure auth"),
    // ── other languages (single words) ────────────────────────────────────────────────────────
    ("archivo", "file path"),
    ("configuración", "config settings"),
    ("configuracion", "config settings"),
    ("erreur", "error fail exception"),
    ("fichier", "file path"),
    ("datei", "file path"),
    ("fehler", "error fail exception"),
    ("文件", "file path"),
    ("函数", "function method"),
    ("测试", "test testing"),
    ("ファイル", "file path"),
    ("関数", "function method"),
    ("파일", "file path"),
    ("файл", "file path"),
    ("функция", "function method"),
];

/// Fold Vietnamese (and other Latin-script) diacritics to ASCII so a query typed without an IME
/// still matches the accented glossary keys. Non-Latin scripts pass through untouched — folding is
/// meaningless for them and their keys are stored verbatim.
fn fold_diacritics(s: &str) -> String {
    s.chars()
        .map(|c| match c {
            'à' | 'á' | 'ả' | 'ã' | 'ạ' | 'ă' | 'ằ' | 'ắ' | 'ẳ' | 'ẵ' | 'ặ' | 'â' | 'ầ' | 'ấ'
            | 'ẩ' | 'ẫ' | 'ậ' => 'a',
            'è' | 'é' | 'ẻ' | 'ẽ' | 'ẹ' | 'ê' | 'ề' | 'ế' | 'ể' | 'ễ' | 'ệ' => {
                'e'
            }
            'ì' | 'í' | 'ỉ' | 'ĩ' | 'ị' => 'i',
            'ò' | 'ó' | 'ỏ' | 'õ' | 'ọ' | 'ô' | 'ồ' | 'ố' | 'ổ' | 'ỗ' | 'ộ' | 'ơ' | 'ờ' | 'ớ'
            | 'ở' | 'ỡ' | 'ợ' => 'o',
            'ù' | 'ú' | 'ủ' | 'ũ' | 'ụ' | 'ư' | 'ừ' | 'ứ' | 'ử' | 'ữ' | 'ự' => {
                'u'
            }
            'ỳ' | 'ý' | 'ỷ' | 'ỹ' | 'ỵ' => 'y',
            'đ' => 'd',
            'ñ' => 'n',
            'ç' => 'c',
            other => other,
        })
        .collect()
}

/// Single-word keys whose no-IME ASCII spelling is ALSO an ordinary English word.
///
/// `doc` (đọc), `gui` (gửi), `bang` (bảng), `ham` (hàm), `nut` (nút) — every one of these appears in
/// English developer text on its own terms, so matching them unconditionally would expand English
/// queries with unrelated identifiers ("where are the doc comments" → `read load parse`), polluting
/// the ranking and potentially opening the relevance gate on the wrong chunk.
///
/// These keys therefore require CORROBORATION: at least one unambiguous glossary key must also have
/// matched, which is what tells us the query really is in another language. "doc file cau hinh" gets
/// its expansion (corroborated by "cau hinh"); "the doc comments" does not.
const AMBIGUOUS_ASCII: &[&str] = &[
    "doc", "gui", "bang", "ham", "nut", "dung", "thu", "tai", "mo", "bien", "lop", "can", "com",
    "con", "cha", "ba", "no", "so", "ma", "sang", "tang", "hang", "long", "loi", "man", "pin",
];

/// English identifier terms implied by `query`, in stable sorted order, or empty when the query is
/// already English (or uses vocabulary outside the table).
///
/// Kept separate from [`expand_query`] so callers that need only the added terms — the LLM-fallback
/// decision in particular — can ask without string-building the merged query.
pub fn glossary_terms(query: &str) -> Vec<String> {
    if query.trim().is_empty() {
        return Vec::new();
    }
    let lower = query.to_lowercase();
    let folded = fold_diacritics(&lower);
    let mut out: BTreeSet<String> = BTreeSet::new();
    // Did anything match that could NOT be an English word? That is the evidence the ambiguous
    // ASCII keys need before they are allowed to contribute.
    let mut corroborated = false;

    // Phrase pass, longest key first so a compound concept wins over its parts. Multi-word keys are
    // never ambiguous — an English sentence does not contain "mat khau" — so these all corroborate.
    let mut phrases: Vec<&(&str, &str)> = PHRASES.iter().collect();
    phrases.sort_by_key(|(k, _)| std::cmp::Reverse(k.chars().count()));
    for (key, terms) in phrases {
        if lower.contains(key) || folded.contains(&fold_diacritics(key)) {
            out.extend(terms.split_whitespace().map(str::to_string));
            corroborated = true;
        }
    }

    // Token pass for single words the phrase pass did not cover. Ambiguous ASCII keys are held back
    // until we know whether anything else in the query proves it is not English.
    let toks = tokenize(query);
    let mut deferred: Vec<&str> = Vec::new();
    for (key, terms) in WORDS {
        let folded_key = fold_diacritics(key);
        // Multi-token keys ("kiểm tra") cannot match a single token — check the raw text for those.
        // Ambiguity is a property of what the USER TYPED, not of the key: the bare ASCII token
        // `doc` reaches the accented key `đọc` through folding, so keying the check on the entry
        // would wave it straight through. Track which spelling actually matched.
        let (hit, matched_bare_ascii) = if key.contains(' ') {
            // Multi-token keys ("kiểm tra") cannot match a single token — check the raw text. A
            // multi-word key is never an English word, so it is never ambiguous.
            (lower.contains(key) || folded.contains(&folded_key), false)
        } else {
            // An ACCENTED token ("đọc") proves the user meant the Vietnamese word. A plain-ASCII
            // token ("doc") is the ambiguous spelling no matter which entry it matched.
            let accented = toks
                .iter()
                .any(|t| !t.is_ascii() && fold_diacritics(t) == folded_key);
            let ascii_only = toks
                .iter()
                .any(|t| t.is_ascii() && fold_diacritics(t) == folded_key);
            (accented || ascii_only, ascii_only && !accented)
        };
        if !hit {
            continue;
        }
        let ambiguous = matched_bare_ascii && AMBIGUOUS_ASCII.contains(&folded_key.as_str());
        if ambiguous {
            deferred.push(terms);
        } else {
            out.extend(terms.split_whitespace().map(str::to_string));
            corroborated = true;
        }
    }
    if corroborated {
        for terms in deferred {
            out.extend(terms.split_whitespace().map(str::to_string));
        }
    }
    out.into_iter().collect()
}

/// `query` with English identifier terms appended.
///
/// Append, never replace: the original wording keeps matching whatever it already matched (comments,
/// string literals, non-English identifiers), and an English query comes back semantically
/// unchanged. Returns an owned `String` on the unchanged path too — the callers feed it straight to
/// `tokenize`, so borrowing would only move the clone.
pub fn expand_query(query: &str) -> String {
    let terms = glossary_terms(query);
    if terms.is_empty() {
        return query.to_string();
    }
    format!("{} {}", query, terms.join(" "))
}

// ── LLM fallback (tier 2) ─────────────────────────────────────────────────────────────────────
//
// The static table above is free but finite: it knows the vocabulary someone wrote down, not every
// way a question can be phrased in every language. This tier covers the rest by asking a cheap
// model to name the English identifiers a query implies.
//
// Three properties keep it from becoming a tax on every turn:
//   1. It only runs when the glossary produced NOTHING and the query is plausibly non-English —
//      an English query, or one the table already handled, never reaches it.
//   2. Results are memoized per query, INCLUDING empty ones, so a repeated question (or the same
//      question in a later turn of the same session) costs zero extra calls.
//   3. It is strictly best-effort: no endpoint seeded, no runtime, a timeout, a refusal, or a
//      garbage reply all degrade to "no extra terms" — retrieval then behaves exactly as it does
//      today rather than failing.

/// The endpoint the expansion call uses, seeded once by the REPL.
///
/// `retrieval_block` and `search` are synchronous and take only `(query, budget)` — there is no
/// endpoint on either path to thread one through, and adding a parameter would ripple into every
/// caller including the `Tool` trait. So the REPL deposits its resolved endpoint here at startup,
/// mirroring the `SESSION_MODEL` / `EFFORT_OVERRIDE` precedent in `cli_config`. `None` ⇒ this tier
/// is simply off, which is the correct state for `aizen -p`, cron jobs, and tests.
static EXPANSION_ENDPOINT: Lazy<RwLock<Option<crate::core::cli_config::ResolvedEndpoint>>> =
    Lazy::new(|| RwLock::new(None));

/// Memoized expansions: query → the English terms it implies (empty vec = asked, got nothing).
///
/// Process-local and unbounded-by-session on purpose: the key space is "distinct questions the user
/// typed this session", which is small, and persisting it would mean invalidating on model change.
static EXPANSION_CACHE: Lazy<RwLock<HashMap<String, Vec<String>>>> =
    Lazy::new(|| RwLock::new(HashMap::new()));

/// Seed the endpoint used for query expansion. Called by both REPLs once the model is resolved.
///
/// Routes through the `summarizer` role so this chore lands on whatever cheap model the user already
/// configured for chores rather than the expensive model they are coding with.
pub fn set_expansion_endpoint(main: &crate::core::cli_config::ResolvedEndpoint) {
    let routed = crate::core::cli_config::resolve_role("summarizer", main);
    *EXPANSION_ENDPOINT
        .write()
        .unwrap_or_else(|e| e.into_inner()) = Some(routed);
}

/// Is the LLM fallback allowed to run? Off by default — opt in with `AIZEN_QUERY_EXPAND=1`.
///
/// Default-off because it spends a (small) model call on the user's behalf at retrieval time, and
/// this project treats unrequested spending as the user's decision, not the agent's. The static
/// glossary needs no flag and already covers the common cases.
fn llm_expansion_enabled() -> bool {
    std::env::var("AIZEN_QUERY_EXPAND")
        .map(|v| {
            matches!(
                v.trim().to_ascii_lowercase().as_str(),
                "1" | "true" | "yes" | "on"
            )
        })
        .unwrap_or(false)
}

/// Does this query look like it needs translating at all?
///
/// True when it carries a non-ASCII letter (Vietnamese, CJK, Cyrillic, accented Latin). A purely
/// ASCII query is either English or an identifier — in both cases the corpus already speaks its
/// language and a model call would buy nothing. This is a cheap, deterministic pre-filter, not a
/// language detector; being wrong in the conservative direction just means no expansion.
fn looks_non_english(query: &str) -> bool {
    query.chars().any(|c| !c.is_ascii() && c.is_alphabetic())
}

/// Keep only plausible English identifier terms from a model reply.
///
/// The reply is untrusted text, so this is a whitelist, not a cleanup: ASCII alphanumeric/underscore
/// tokens of 2..=32 chars, at most 12 of them. Anything else — prose, quotes, markdown, a refusal,
/// an attempted instruction — is dropped rather than interpreted. Terms then flow into a BM25 query,
/// never into a prompt or a shell, so the blast radius of a bad reply is a worse ranking.
fn sanitize_terms(reply: &str) -> Vec<String> {
    let mut out: BTreeSet<String> = BTreeSet::new();
    for raw in reply.split(|c: char| !(c.is_ascii_alphanumeric() || c == '_')) {
        let t = raw.trim().to_ascii_lowercase();
        let n = t.chars().count();
        if !(2..=32).contains(&n) {
            continue;
        }
        // A bare number carries no lexical signal and matches noise across a code corpus.
        if t.chars().all(|c| c.is_ascii_digit()) {
            continue;
        }
        out.insert(t);
        if out.len() >= 12 {
            break;
        }
    }
    out.into_iter().collect()
}

/// Ask the chore model which English identifiers `query` implies. Best-effort: `None` on any failure.
fn llm_terms(query: &str) -> Option<Vec<String>> {
    let endpoint = EXPANSION_ENDPOINT
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .clone()?;
    let http = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(20))
        .build()
        .ok()?;
    let sys = crate::core::types::Message::system(
        "You translate a developer's question into the English identifiers a codebase would use. \
         Reply with ONLY lowercase English keywords separated by spaces — no prose, no punctuation, \
         no explanation, at most 8 words. Prefer the words that would literally appear in code: \
         function names, types, domain nouns. Example: input `chỗ nào xử lý đăng nhập` → output \
         `login auth session credential handler`.",
    );
    let user = crate::core::types::Message::user(query);
    // The sync retrieval paths run on a tool/blocking thread, so this is the same verified bridge
    // the tool layer uses. A missing runtime returns Err rather than panicking the turn.
    let turn = crate::agent::tools::block_for_tool(async {
        crate::llm::client::chat_with_tools(
            &http,
            &endpoint.base_url,
            &endpoint.api_key,
            &endpoint.model,
            &[sys, user],
            &[],
        )
        .await
    })
    .ok()?;
    let reply = turn.content.unwrap_or_default();
    let terms = sanitize_terms(&reply);
    if terms.is_empty() {
        return None;
    }
    Some(terms)
}

/// `expand_query`, plus the LLM fallback when the static glossary came up empty.
///
/// Use this on paths that can afford a (cached, opt-in) model call; [`expand_query`] stays the pure
/// function for everything else. The original query is always preserved at the front.
pub fn expand_query_with_fallback(query: &str) -> String {
    let terms = glossary_terms(query);
    if !terms.is_empty() {
        return format!("{} {}", query, terms.join(" "));
    }
    if query.trim().is_empty() || !llm_expansion_enabled() || !looks_non_english(query) {
        return query.to_string();
    }
    let key = query.trim().to_string();
    // Cache hit (including a remembered miss) — no call.
    if let Some(cached) = EXPANSION_CACHE
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .get(&key)
    {
        return if cached.is_empty() {
            query.to_string()
        } else {
            format!("{} {}", query, cached.join(" "))
        };
    }
    let fetched = llm_terms(query).unwrap_or_default();
    EXPANSION_CACHE
        .write()
        .unwrap_or_else(|e| e.into_inner())
        .insert(key, fetched.clone());
    if fetched.is_empty() {
        query.to_string()
    } else {
        format!("{} {}", query, fetched.join(" "))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn english_queries_are_left_alone() {
        // The no-op guarantee that makes this safe to run on every turn.
        assert_eq!(
            expand_query("where is auth handled"),
            "where is auth handled"
        );
        assert!(glossary_terms("database connection setup").is_empty());
        assert_eq!(expand_query(""), "");
    }

    #[test]
    fn vietnamese_query_gains_english_identifiers() {
        let terms = glossary_terms("chỗ nào xử lý đăng nhập");
        assert!(
            terms.iter().any(|t| t == "login"),
            "expected login in {terms:?}"
        );
        assert!(terms.iter().any(|t| t == "auth"));
        // The original wording must survive — it still matches comments and Vietnamese literals.
        let expanded = expand_query("chỗ nào xử lý đăng nhập");
        assert!(expanded.starts_with("chỗ nào xử lý đăng nhập"));
        assert!(expanded.contains("login"));
    }

    #[test]
    fn works_without_an_ime() {
        // Typed without diacritics, which is how a lot of Vietnamese is actually typed.
        let terms = glossary_terms("sua loi dang nhap");
        assert!(terms.iter().any(|t| t == "login"), "{terms:?}");
        assert!(terms.iter().any(|t| t == "error"), "{terms:?}");
        assert!(terms.iter().any(|t| t == "fix"), "{terms:?}");
    }

    #[test]
    fn covers_non_latin_scripts() {
        // "any language" is the requirement, so CJK/Cyrillic must land too, not just Vietnamese.
        for (q, want) in [
            ("登录在哪里处理", "login"),
            ("ログインの処理", "login"),
            ("로그인 처리", "login"),
            ("где обрабатывается вход", "login"),
            ("¿dónde está la contraseña?", "password"),
            ("wo ist die datenbank", "database"),
        ] {
            let terms = glossary_terms(q);
            assert!(
                terms.iter().any(|t| t == want),
                "query {q:?} should imply {want:?}, got {terms:?}"
            );
        }
    }

    #[test]
    fn compound_phrase_beats_its_parts() {
        // "cơ sở dữ liệu" is one concept; matching only "dữ liệu" would lose `sql`/`db`.
        let terms = glossary_terms("kết nối cơ sở dữ liệu ở đâu");
        assert!(terms.iter().any(|t| t == "database"), "{terms:?}");
        assert!(terms.iter().any(|t| t == "connection"), "{terms:?}");
    }

    #[test]
    fn ascii_keys_that_are_also_english_words_need_corroboration() {
        // These no-IME spellings are real English words. On their own they must NOT expand, or every
        // English question about docs/tables/bindings drags in unrelated identifiers and can open the
        // relevance gate on the wrong chunk.
        for q in [
            "where are the doc comments",
            "the bang operator",
            "gui layer rendering",
            "long running task",
            "how do i mo the file",
        ] {
            assert!(
                glossary_terms(q).is_empty(),
                "{q:?} is English — expected no expansion, got {:?}",
                glossary_terms(q)
            );
        }

        // With an unambiguous non-English key present, the same word DOES contribute: "cau hinh"
        // proves the query is Vietnamese, so "doc" is safe to read as đọc.
        let terms = glossary_terms("doc file cau hinh");
        assert!(terms.iter().any(|t| t == "config"), "{terms:?}");
        assert!(
            terms.iter().any(|t| t == "read"),
            "corroborated ambiguous key should contribute: {terms:?}"
        );

        // An accented key is unambiguous on its own — no English word carries those diacritics.
        let terms = glossary_terms("đọc");
        assert!(terms.iter().any(|t| t == "read"), "{terms:?}");
    }

    #[test]
    fn non_english_detection_gates_the_llm_tier() {
        // ASCII → never worth a model call: the corpus already speaks that language.
        assert!(!looks_non_english("where is auth handled"));
        assert!(!looks_non_english("charge_card"));
        assert!(!looks_non_english(""));
        // Non-ASCII letters in any script → a candidate for translation.
        assert!(looks_non_english("đăng nhập ở đâu"));
        assert!(looks_non_english("登录在哪里"));
        assert!(looks_non_english("где вход"));
    }

    #[test]
    fn model_reply_is_whitelisted_not_trusted() {
        // The happy path: plain keywords survive.
        let terms = sanitize_terms("login auth session handler");
        assert!(terms.contains(&"login".to_string()), "{terms:?}");
        assert_eq!(terms.len(), 4);

        // Prose, markdown, and punctuation are split into their word parts; nothing is executed or
        // interpreted, and an injection attempt is just more words that will rank badly.
        let terms = sanitize_terms("Sure! Here you go: `login`, **auth**.\n- session");
        assert!(terms.contains(&"login".to_string()), "{terms:?}");
        assert!(terms.contains(&"auth".to_string()), "{terms:?}");

        // Bare numbers carry no lexical signal; single chars are below the tokenizer's floor.
        let terms = sanitize_terms("1 22 333 a login");
        assert_eq!(terms, vec!["login".to_string()], "got {terms:?}");

        // Hard cap so a runaway reply cannot flood the BM25 query.
        let many: String = (0..50).map(|i| format!("term{i} ")).collect();
        assert!(sanitize_terms(&many).len() <= 12);

        // A refusal contributes only harmless words, never an error.
        assert!(sanitize_terms("I cannot help with that.").len() <= 12);
    }

    #[test]
    fn fallback_is_a_passthrough_when_unseeded() {
        // No endpoint seeded (the state for tests, `aizen -p`, and cron) and no opt-in env ⇒ the
        // fallback must degrade to plain glossary behavior rather than erroring or blocking.
        assert_eq!(
            expand_query_with_fallback("where is auth handled"),
            "where is auth handled"
        );
        // A glossary hit still works through the fallback entry point — the static tier runs first
        // and short-circuits before any endpoint or runtime is needed.
        let expanded = expand_query_with_fallback("sửa lỗi đăng nhập");
        assert!(expanded.starts_with("sửa lỗi đăng nhập"));
        assert!(expanded.contains("login"), "{expanded}");
    }

    #[test]
    fn terms_are_deduplicated_and_stable() {
        // Two keys map to overlapping terms; the result must be a stable sorted set so the injected
        // context (and therefore the prompt cache) does not churn between identical queries.
        let a = glossary_terms("đăng nhập và xác thực");
        let b = glossary_terms("đăng nhập và xác thực");
        assert_eq!(a, b, "expansion must be deterministic");
        let mut sorted = a.clone();
        sorted.sort();
        sorted.dedup();
        assert_eq!(a, sorted, "expected a deduplicated sorted set");
    }
}
