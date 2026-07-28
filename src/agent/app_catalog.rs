//! App catalog over the **official MCP Registry** (`registry.modelcontextprotocol.io`) — the
//! Hermes-style "connect lots of apps" surface, done WITHOUT bloating the static binary: each app is
//! a `mcp.json` entry, not new Rust code. We pull the authoritative server spec (packages / remotes /
//! env-vars / headers) from the registry live and write a correct entry, prompting for any declared
//! secret. Mirrors the `skill_registry` pattern (curated UX on top of a public registry; no Node).
//!
//! Transport reality (ng's MCP client supports stdio + Streamable-HTTP + **OAuth 2.1 sign-in**):
//! - A **package** (npm→npx / pypi→uvx / oci→docker) is preferred — LOCAL-FIRST, runs on YOUR machine
//!   with YOUR keys (never a surprise-paid third-party gateway).
//! - Else a **static-token remote** (declares a header we fill, e.g. GitHub's `Authorization`).
//! - Else an **OAuth remote** (Linear/Notion/Slack/Gmail/Atlassian — no header, needs interactive
//!   sign-in): we write `{url, auth:"oauth"}` and drive the browser PKCE flow (see `mcp_oauth`).
//! `None` only for a legacy two-endpoint `sse`-only server (our client doesn't implement that).

use anyhow::{bail, Context, Result};
use serde::Deserialize;
use serde_json::{json, Map, Value};

/// The official MCP Registry. Overridable via `NG_MCP_REGISTRY` (mirrors the skill registry knob).
pub const DEFAULT_REGISTRY: &str = "https://registry.modelcontextprotocol.io";

pub fn registry_base() -> String {
    std::env::var("NG_MCP_REGISTRY")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| DEFAULT_REGISTRY.to_string())
}

// ───────────────────────────── registry schema (the subset we use) ─────────────────────────────

#[derive(Debug, Clone, Deserialize, Default)]
pub struct RegistryServer {
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub version: Option<String>,
    #[serde(default)]
    pub repository: Option<Repository>,
    #[serde(default)]
    pub packages: Vec<Package>,
    #[serde(default)]
    pub remotes: Vec<Remote>,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct Repository {
    #[serde(default)]
    pub url: String,
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct Package {
    #[serde(default)]
    pub registry_type: String, // npm | pypi | oci | nuget | mcpb
    #[serde(default)]
    pub identifier: String,
    #[serde(default)]
    pub version: Option<String>,
    #[serde(default)]
    pub runtime_hint: Option<String>, // npx | uvx | docker | dnx
    #[serde(default)]
    pub runtime_arguments: Vec<Argument>,
    #[serde(default)]
    pub package_arguments: Vec<Argument>,
    #[serde(default)]
    pub environment_variables: Vec<KeyValueInput>,
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct Remote {
    #[serde(default, rename = "type")]
    pub kind: String, // streamable-http | sse
    #[serde(default)]
    pub url: String,
    #[serde(default)]
    pub headers: Vec<KeyValueInput>,
}

/// An env-var / header input (KeyValueInput). `value` may carry `{placeholder}` templates.
#[derive(Debug, Clone, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct KeyValueInput {
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub value: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub is_required: bool,
    #[serde(default)]
    pub is_secret: bool,
    #[serde(default)]
    pub default: Option<String>,
}

/// A package/runtime argument (positional or named), per the registry schema. `value` may carry
/// `{placeholder}` templates resolved against `variables`. A required arg with no value/default is
/// PROMPTED (else the server launches misconfigured — the filesystem/docker-path bug).
#[derive(Debug, Clone, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct Argument {
    #[serde(default, rename = "type")]
    pub kind: String, // positional | named
    #[serde(default)]
    pub name: Option<String>, // for named (e.g. "--port")
    #[serde(default)]
    pub value: Option<String>,
    #[serde(default)]
    pub default: Option<String>,
    #[serde(default)]
    pub is_required: bool,
    #[serde(default)]
    pub value_hint: Option<String>, // a human hint when the arg has no fixed value (e.g. "<directory>")
    /// Named substitution inputs for `{placeholder}` tokens inside `value` (carry is_secret/required).
    #[serde(default)]
    pub variables: std::collections::HashMap<String, KeyValueInput>,
}

impl RegistryServer {
    /// Compact transport tag for the picker: `local·npx` / `sign-in` / `hosted` / `—`.
    pub fn transport_tag(&self) -> String {
        match pick_transport(self) {
            Some(TransportChoice::Package(i)) => format!(
                "local·{}",
                self.packages.get(i).map(|p| p.runner()).unwrap_or("local")
            ),
            Some(TransportChoice::OAuthRemote(_)) => "sign-in".to_string(),
            Some(TransportChoice::Remote(_)) => "hosted".to_string(),
            None => "—".to_string(),
        }
    }

    /// Short display name for the picker — publisher TAIL + server name (drops the `io.github.`/`com.`
    /// reverse-DNS prefix noise): `io.github.n24q02m/better-notion-mcp` → `n24q02m/better-notion-mcp`.
    pub fn short_name(&self) -> String {
        match self.name.split_once('/') {
            Some((publisher, rest)) => format!(
                "{}/{rest}",
                publisher.rsplit('.').next().unwrap_or(publisher)
            ),
            None => self.name.clone(),
        }
    }

    /// A short, human one-liner for search lists.
    pub fn summary_line(&self) -> String {
        let what = if !self.title.is_empty() {
            &self.title
        } else {
            &self.name
        };
        let desc: String = self.description.chars().take(80).collect();
        let via = match pick_transport(self) {
            Some(TransportChoice::Remote(i)) => {
                format!(
                    "hosted remote @ {}",
                    self.remotes
                        .get(i)
                        .map(|r| host_of(&r.url))
                        .unwrap_or_default()
                )
            }
            Some(TransportChoice::Package(i)) => {
                format!(
                    "local · {}",
                    self.packages.get(i).map(|p| p.runner()).unwrap_or("local")
                )
            }
            Some(TransportChoice::OAuthRemote(i)) => {
                format!(
                    "sign in @ {}",
                    self.remotes
                        .get(i)
                        .map(|r| host_of(&r.url))
                        .unwrap_or_default()
                )
            }
            None => "⚠ not connectable (legacy sse only)".to_string(),
        };
        let ver = self
            .version
            .as_deref()
            .filter(|v| !v.is_empty())
            .map(|v| format!(" v{v}"))
            .unwrap_or_default();
        format!("{what}{ver} [{via}] — {desc}")
    }
}

impl Package {
    /// The local runner for this package's registry type (npx/uvx/docker/…).
    pub fn runner(&self) -> &'static str {
        match (self.runtime_hint.as_deref(), self.registry_type.as_str()) {
            (Some("npx"), _) | (_, "npm") => "npx",
            (Some("uvx"), _) | (_, "pypi") => "uvx",
            (Some("docker"), _) | (_, "oci") => "docker",
            (Some("dnx"), _) | (_, "nuget") => "dnx",
            _ => "npx",
        }
    }
}

// ───────────────────────────── transport selection ─────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransportChoice {
    /// A Streamable-HTTP remote with a static-token header we fill (e.g. GitHub's `Authorization`).
    Remote(usize),
    /// A self-hostable package run locally (npx/uvx/docker/dnx).
    Package(usize),
    /// A Streamable-HTTP remote that needs **OAuth 2.1 sign-in** (Linear/Notion/Slack/Gmail/…). We
    /// write `{url, auth:"oauth"}` and drive the browser sign-in — the apps people actually want.
    OAuthRemote(usize),
}

/// Whether a remote is fillable by our client with a STATIC token: a Streamable-HTTP endpoint that
/// declares at least one header (a token slot we prompt for). A legacy two-endpoint `sse` transport
/// isn't implemented by our client → skipped.
fn remote_is_fillable(r: &Remote) -> bool {
    let http_ok = r.kind.is_empty() || r.kind == "streamable-http";
    http_ok && !r.url.is_empty() && !r.headers.is_empty()
}

/// Whether a remote is an OAuth endpoint our client can sign into: a Streamable-HTTP endpoint with NO
/// declared static-token header (so it expects interactive OAuth). We now drive that flow, so these
/// are connectable. A legacy `sse` transport is still skipped.
fn remote_is_oauth(r: &Remote) -> bool {
    let http_ok = r.kind.is_empty() || r.kind == "streamable-http";
    http_ok && !r.url.is_empty() && r.headers.is_empty()
}

/// Pick the transport our client can use, **LOCAL-FIRST**: a self-hostable package (npm > pypi > oci
/// > nuget) is preferred so the server runs on YOUR machine with YOUR credentials — never a
/// third-party HOSTED gateway (those re-host the service behind their own domain and often their own
/// account/billing — the surprise-paid trap). Then a fillable static-token remote; then an OAuth
/// remote (the marquee SaaS apps — sign-in with the real vendor). `None` only when nothing matches
/// (e.g. a legacy `sse`-only server).
pub fn pick_transport(s: &RegistryServer) -> Option<TransportChoice> {
    for ty in ["npm", "pypi", "oci", "nuget"] {
        if let Some(i) = s.packages.iter().position(|p| p.registry_type == ty) {
            return Some(TransportChoice::Package(i));
        }
    }
    if !s.packages.is_empty() {
        return Some(TransportChoice::Package(0));
    }
    if let Some(i) = s.remotes.iter().position(remote_is_fillable) {
        return Some(TransportChoice::Remote(i));
    }
    if let Some(i) = s.remotes.iter().position(remote_is_oauth) {
        return Some(TransportChoice::OAuthRemote(i));
    }
    None
}

/// The host (authority) of a URL — shows WHO hosts a remote server (so a third-party gateway is
/// visible at a glance, e.g. `spotify.api.trendsmcp.ai`).
pub fn host_of(url: &str) -> String {
    url.split("://")
        .nth(1)
        .unwrap_or(url)
        .split('/')
        .next()
        .unwrap_or(url)
        .to_string()
}

/// Is a runner (npx/uvx/docker/dnx) actually on PATH? Windows adds the shim extensions. Used so the
/// installer doesn't pick a package the user can't run when a remote (or another package) would work.
pub fn runner_available(runner: &str) -> bool {
    let exts: &[&str] = if cfg!(windows) {
        &["", ".cmd", ".exe", ".bat"]
    } else {
        &[""]
    };
    let Some(path) = std::env::var_os("PATH") else {
        return false;
    };
    for dir in std::env::split_paths(&path) {
        for ext in exts {
            if dir.join(format!("{runner}{ext}")).is_file() {
                return true;
            }
        }
    }
    false
}

/// Like `pick_transport` but **runtime-aware**, for the actual install: a package whose runner is on
/// PATH wins; else a fillable remote (works without a local runtime); else any package (so we can
/// still write it, with the caller warning that the runner is missing). Keeps `pick_transport` pure
/// (logical viability, used by search/list) while the install never silently picks an unrunnable one.
pub fn pick_transport_for_install(s: &RegistryServer) -> Option<TransportChoice> {
    for ty in ["npm", "pypi", "oci", "nuget"] {
        if let Some(i) = s
            .packages
            .iter()
            .position(|p| p.registry_type == ty && runner_available(p.runner()))
        {
            return Some(TransportChoice::Package(i));
        }
    }
    if let Some(i) = s.packages.iter().position(|p| runner_available(p.runner())) {
        return Some(TransportChoice::Package(i));
    }
    if let Some(i) = s.remotes.iter().position(remote_is_fillable) {
        return Some(TransportChoice::Remote(i));
    }
    if let Some(i) = s.remotes.iter().position(remote_is_oauth) {
        return Some(TransportChoice::OAuthRemote(i));
    }
    // Last resort: a package whose runner is missing (the caller warns the user to install it).
    pick_transport(s)
}

/// True when the server has SOME path our client can connect (used to filter search/pick).
pub fn is_viable(s: &RegistryServer) -> bool {
    pick_transport(s).is_some()
}

/// The runtime the user must have installed for a choice (None for remote).
pub fn runtime_prereq(s: &RegistryServer, choice: TransportChoice) -> Option<&'static str> {
    match choice {
        TransportChoice::Remote(_) | TransportChoice::OAuthRemote(_) => None,
        TransportChoice::Package(i) => s.packages.get(i).map(|p| p.runner()),
    }
}

// ───────────────────────────── HTTP ─────────────────────────────

fn http() -> Result<reqwest::Client> {
    reqwest::Client::builder()
        .user_agent(concat!("aizen/", env!("CARGO_PKG_VERSION")))
        .timeout(std::time::Duration::from_secs(20))
        .build()
        .context("building MCP registry HTTP client")
}

/// Tolerantly pull server objects out of a `/v0/servers` body: items are wrapped as
/// `{ "server": {...}, "_meta": {...} }`, but accept a flat `{...}` entry too.
fn parse_servers(body: &Value) -> Vec<RegistryServer> {
    let arr = body
        .get("servers")
        .and_then(|s| s.as_array())
        .cloned()
        .unwrap_or_default();
    arr.into_iter()
        .filter_map(|item| {
            let obj = item.get("server").cloned().unwrap_or(item);
            serde_json::from_value::<RegistryServer>(obj).ok()
        })
        .filter(|s| !s.name.is_empty())
        .collect()
}

/// Search the registry by free-text. `version=latest` collapses the one-row-per-published-version
/// noise server-side (so a page of N holds N DISTINCT servers — the official entry is far likelier to
/// be on it), and we follow `metadata.nextCursor` across pages until we have `limit` results or run
/// dry. Returns parsed server entries (unfiltered).
pub async fn search(query: &str, limit: usize) -> Result<Vec<RegistryServer>> {
    if query.trim().is_empty() {
        bail!("empty search query");
    }
    let base = registry_base();
    let url = format!("{}/v0/servers", base.trim_end_matches('/'));
    let client = http()?;
    let want = limit.max(1);
    let per_page = want.clamp(1, 100).to_string();
    let mut out: Vec<RegistryServer> = Vec::new();
    let mut cursor: Option<String> = None;
    for _page in 0..6 {
        let mut q: Vec<(&str, String)> = vec![
            ("search", query.to_string()),
            ("version", "latest".to_string()),
            ("limit", per_page.clone()),
        ];
        if let Some(c) = &cursor {
            q.push(("cursor", c.clone()));
        }
        let resp = client
            .get(&url)
            .query(&q)
            .send()
            .await
            .with_context(|| format!("searching {base}"))?;
        if !resp.status().is_success() {
            bail!("registry {base} returned HTTP {}", resp.status().as_u16());
        }
        let body: Value = resp.json().await.context("parsing registry JSON")?;
        out.extend(parse_servers(&body));
        if out.len() >= want {
            break;
        }
        match body
            .get("metadata")
            .and_then(|m| m.get("nextCursor"))
            .and_then(|c| c.as_str())
        {
            Some(c) if !c.is_empty() => cursor = Some(c.to_string()),
            _ => break,
        }
    }
    Ok(out)
}

/// Collapse duplicate server NAMES (the registry returns one row per published version) down to the
/// single highest-version entry, preserving first-seen order. Keeps `apps search` / pick lists clean.
pub fn dedupe_latest(servers: Vec<RegistryServer>) -> Vec<RegistryServer> {
    let mut order: Vec<String> = Vec::new();
    let mut best: std::collections::HashMap<String, RegistryServer> =
        std::collections::HashMap::new();
    for s in servers {
        let keep = match best.get(&s.name) {
            Some(prev) => version_key(s.version.as_deref()) >= version_key(prev.version.as_deref()),
            None => {
                order.push(s.name.clone());
                true
            }
        };
        if keep {
            best.insert(s.name.clone(), s);
        }
    }
    order.into_iter().filter_map(|n| best.remove(&n)).collect()
}

/// A comparable version key. The dotted numeric CORE compares first (so `1.10.0` > `1.9.0`), then a
/// GA flag breaks ties of the same core ABOVE a pre-release (`1.2.0` > `1.2.0-rc1`) — standard SemVer
/// precedence. A non-numeric/absent version sorts lowest. The tuple derives `Ord` lexicographically.
fn version_key(v: Option<&str>) -> (Vec<u64>, u8) {
    let raw = v.unwrap_or("0");
    // Everything before the first '-' (pre-release) or '+' (build metadata) is the core.
    let core = raw.split(['-', '+']).next().unwrap_or(raw);
    let is_ga = !raw.contains('-'); // SemVer: a '-' introduces a pre-release identifier
    let nums: Vec<u64> = core
        .split('.')
        .map(|p| {
            p.chars()
                .take_while(|c| c.is_ascii_digit())
                .collect::<String>()
                .parse()
                .unwrap_or(0)
        })
        .collect();
    (nums, is_ga as u8)
}

/// The raw mcp.json entry for an installed server key (for the detail view). `None` if not present.
pub fn installed_entry(key: &str) -> Option<Value> {
    let root = read_mcp_json().ok()?;
    let skey = servers_key(&root);
    root.get(skey)?.get(key).cloned()
}

/// Search, then pick the best VIABLE match. Ranks by: (1) a name/title containing `prefer` (the
/// vendor hint), then (2) a LOCAL package transport — so when both a third-party hosted remote and a
/// self-hostable package match, the local one wins (keeps your keys on your machine). Stable by
/// registry order on ties. `None` when nothing connectable was found.
pub fn pick_best(results: &[RegistryServer], prefer: &str) -> Option<RegistryServer> {
    let pref = prefer.to_lowercase();
    let viable: Vec<&RegistryServer> = results.iter().filter(|s| is_viable(s)).collect();
    viable
        .iter()
        .enumerate()
        .max_by_key(|(i, s)| {
            let matches =
                s.name.to_lowercase().contains(&pref) || s.title.to_lowercase().contains(&pref);
            let local = matches!(pick_transport(s), Some(TransportChoice::Package(_)));
            ((matches as u8) * 2 + local as u8, std::cmp::Reverse(*i))
        })
        .map(|(_, s)| (*s).clone())
}

// ───────────────────────────── entry building (pure; asker injected) ─────────────────────────────

/// A credential the catalog must collect to wire a server.
pub struct PromptSpec {
    pub label: String,
    pub description: String,
    pub is_secret: bool,
}

/// Find `{placeholder}` tokens in a template string.
fn placeholders(t: &str) -> Vec<String> {
    let mut out = Vec::new();
    let bytes = t.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'{' {
            if let Some(end) = t[i + 1..].find('}') {
                let name = &t[i + 1..i + 1 + end];
                if !name.is_empty() && !name.contains('{') {
                    out.push(name.to_string());
                }
                i = i + 1 + end + 1;
                continue;
            }
        }
        i += 1;
    }
    out
}

/// Resolve one input's concrete value, asking the user for placeholders / required secrets.
/// `Ok(None)` = optional input the user left blank (skip — never write an empty value). `Err` = a
/// REQUIRED input left empty (so we refuse to write a broken `Authorization: ""` / empty env entry).
fn resolve_input(
    inp: &KeyValueInput,
    ask: &mut dyn FnMut(&PromptSpec) -> String,
) -> Result<Option<String>> {
    let template = inp.value.clone().or_else(|| inp.default.clone());
    let label = if inp.name.is_empty() {
        "value".to_string()
    } else {
        inp.name.clone()
    };
    match template {
        Some(t) if t.contains('{') => {
            let mut out = t.clone();
            for ph in placeholders(&t) {
                let v = ask(&PromptSpec {
                    label: ph.clone(),
                    description: inp.description.clone().unwrap_or_default(),
                    is_secret: inp.is_secret,
                });
                if inp.is_required && v.trim().is_empty() {
                    bail!("'{ph}' is required but no value was provided");
                }
                out = out.replace(&format!("{{{ph}}}"), &v);
            }
            Ok(Some(out))
        }
        Some(t) => Ok(Some(t)), // concrete, non-secret default
        None => {
            if inp.is_required || inp.is_secret {
                let v = ask(&PromptSpec {
                    label: label.clone(),
                    description: inp.description.clone().unwrap_or_default(),
                    is_secret: inp.is_secret,
                });
                if v.trim().is_empty() {
                    if inp.is_required {
                        bail!("'{label}' is required but no value was provided");
                    }
                    return Ok(None); // optional secret left blank → don't write an empty value
                }
                Ok(Some(v))
            } else {
                Ok(None)
            }
        }
    }
}

/// Substitute `{placeholder}` tokens in a template against a `variables` map (prompting for each,
/// honouring is_secret/is_required). Used for CLI args whose value carries a templated secret.
fn resolve_template(
    tpl: &str,
    vars: &std::collections::HashMap<String, KeyValueInput>,
    ask: &mut dyn FnMut(&PromptSpec) -> String,
) -> Result<String> {
    let mut out = tpl.to_string();
    for ph in placeholders(tpl) {
        let spec = vars.get(&ph);
        let v = ask(&PromptSpec {
            label: ph.clone(),
            description: spec.and_then(|s| s.description.clone()).unwrap_or_default(),
            is_secret: spec.map(|s| s.is_secret).unwrap_or(false),
        });
        if spec.map(|s| s.is_required).unwrap_or(false) && v.trim().is_empty() {
            bail!("'{ph}' is required but no value was provided");
        }
        out = out.replace(&format!("{{{ph}}}"), &v);
    }
    Ok(out)
}

/// For an `Authorization` header given a bare token (no scheme, no space), default to `Bearer <tok>`
/// (the near-universal convention; registry entries often omit the template). A value the user typed
/// with a space/scheme is left untouched.
fn normalize_auth(header_name: &str, val: String) -> String {
    if header_name.eq_ignore_ascii_case("authorization") {
        let v = val.trim();
        if !v.is_empty() && !v.contains(' ') {
            return format!("Bearer {v}");
        }
    }
    val
}

/// Concrete arg tokens to append (positional `value`, or `name value` for named). A `value` carrying
/// `{placeholder}` is filled from the arg's `variables` map (prompting). An arg with no value/default
/// that is REQUIRED (or carries a `value_hint`) is PROMPTED — previously such args were silently
/// dropped, so servers that take a required CLI arg (the official filesystem server's root path,
/// docker image paths) launched misconfigured. `Err` if a required arg is left empty.
fn arg_tokens(
    args: &[Argument],
    ask: &mut dyn FnMut(&PromptSpec) -> String,
) -> Result<Vec<String>> {
    let mut out = Vec::new();
    for a in args {
        // Resolve this arg's value.
        let val: Option<String> = if let Some(v) = &a.value {
            if v.contains('{') {
                Some(resolve_template(v, &a.variables, ask)?)
            } else {
                Some(v.clone())
            }
        } else if let Some(d) = &a.default {
            Some(d.clone())
        } else if a.is_required || a.value_hint.is_some() {
            let label = a
                .name
                .clone()
                .or_else(|| a.value_hint.clone())
                .unwrap_or_else(|| "argument".to_string());
            let v = ask(&PromptSpec {
                label,
                description: a.value_hint.clone().unwrap_or_default(),
                is_secret: false,
            });
            if a.is_required && v.trim().is_empty() {
                bail!(
                    "required argument '{}' was left empty",
                    a.name.clone().or(a.value_hint.clone()).unwrap_or_default()
                );
            }
            if v.trim().is_empty() {
                None
            } else {
                Some(v)
            }
        } else {
            None
        };

        match (a.kind.as_str(), &a.name, val) {
            ("named", Some(name), Some(v)) => {
                out.push(name.clone());
                out.push(v);
            }
            ("named", Some(name), None) => out.push(name.clone()), // a bare flag
            (_, _, Some(v)) => out.push(v),                        // positional
            _ => {}
        }
    }
    Ok(out)
}

/// Build the `mcp.json` server-config Value for the chosen transport, asking for any declared secret.
/// Pure except for the injected `ask` (so it's unit-testable with a stub). Also returns the chosen
/// transport so the caller can report runtime prereqs.
pub fn build_entry(
    s: &RegistryServer,
    choice: TransportChoice,
    ask: &mut dyn FnMut(&PromptSpec) -> String,
) -> Result<Value> {
    let mut obj = Map::new();
    match choice {
        TransportChoice::Remote(i) => {
            let r = s.remotes.get(i).context("remote index out of range")?;
            obj.insert("url".into(), json!(r.url));
            let mut headers = Map::new();
            for h in &r.headers {
                if let Some(val) = resolve_input(h, ask)? {
                    headers.insert(h.name.clone(), json!(normalize_auth(&h.name, val)));
                }
            }
            if !headers.is_empty() {
                obj.insert("headers".into(), Value::Object(headers));
            }
        }
        TransportChoice::OAuthRemote(i) => {
            // No secret to prompt — the browser sign-in mints the token. We just record the endpoint
            // and `auth: oauth`; the caller runs `mcp::login(key)` to complete the flow.
            let r = s.remotes.get(i).context("remote index out of range")?;
            obj.insert("url".into(), json!(r.url));
            obj.insert("auth".into(), json!("oauth"));
        }
        TransportChoice::Package(i) => {
            let p = s.packages.get(i).context("package index out of range")?;
            let runner = p.runner();
            let mut env = Map::new();
            for e in &p.environment_variables {
                if let Some(val) = resolve_input(e, ask)? {
                    env.insert(e.name.clone(), json!(val));
                }
            }
            let (command, args) = build_command(p, runner, &env, ask)?;
            obj.insert("command".into(), json!(command));
            obj.insert("args".into(), json!(args));
            if !env.is_empty() {
                obj.insert("env".into(), Value::Object(env));
            }
        }
    }
    Ok(Value::Object(obj))
}

/// Assemble (command, args) for a package. npx/uvx get the package id (npm pins `@version`); docker
/// runs `-i --rm` with `-e KEY` forwarding for each env var (the env map is set on the child too).
fn build_command(
    p: &Package,
    runner: &str,
    env: &Map<String, Value>,
    ask: &mut dyn FnMut(&PromptSpec) -> String,
) -> Result<(String, Vec<String>)> {
    let runtime_args = arg_tokens(&p.runtime_arguments, ask)?;
    let pkg_args = arg_tokens(&p.package_arguments, ask)?;
    let built = match runner {
        "npx" => {
            // `-y` only if the entry didn't already supply it (avoid a duplicate flag).
            let mut args = if runtime_args.iter().any(|a| a == "-y" || a == "--yes") {
                Vec::new()
            } else {
                vec!["-y".to_string()]
            };
            args.extend(runtime_args);
            let id = match &p.version {
                Some(v) if !v.is_empty() => format!("{}@{}", p.identifier, v),
                _ => p.identifier.clone(),
            };
            args.push(id);
            args.extend(pkg_args);
            ("npx".to_string(), args)
        }
        "uvx" => {
            // uvx pins via `pkg==ver`; many entries omit version → run latest.
            let mut args = runtime_args;
            let id = match &p.version {
                Some(v) if !v.is_empty() => format!("{}=={}", p.identifier, v),
                _ => p.identifier.clone(),
            };
            args.push(id);
            args.extend(pkg_args);
            ("uvx".to_string(), args)
        }
        "docker" => {
            let mut args = vec!["run".to_string(), "-i".to_string(), "--rm".to_string()];
            for k in env.keys() {
                args.push("-e".to_string());
                args.push(k.clone());
            }
            args.extend(runtime_args);
            args.push(p.identifier.clone());
            args.extend(pkg_args);
            ("docker".to_string(), args)
        }
        other => {
            // dnx / unknown: best-effort `runner <id> <args>`.
            let mut args = runtime_args;
            args.push(p.identifier.clone());
            args.extend(pkg_args);
            (other.to_string(), args)
        }
    };
    Ok(built)
}

// ───────────────────────────── mcp.json read / merge / write ─────────────────────────────

/// The servers map key in mcp.json (canonical `mcpServers`, but honor an existing `servers`).
fn servers_key(root: &Value) -> &'static str {
    if root.get("servers").is_some() && root.get("mcpServers").is_none() {
        "servers"
    } else {
        "mcpServers"
    }
}

fn read_mcp_json() -> Result<Value> {
    let path = crate::agent::mcp::config_path();
    match std::fs::read_to_string(&path) {
        Ok(s) if !s.trim().is_empty() => {
            serde_json::from_str(&s).with_context(|| format!("parsing {}", path.display()))
        }
        _ => Ok(json!({})),
    }
}

fn write_mcp_json(root: &Value) -> Result<()> {
    let path = crate::agent::mcp::config_path();
    let bytes = serde_json::to_vec_pretty(root)?;
    crate::core::persist::atomic_write_owner_only(&path, &[bytes.as_slice(), b"\n"].concat())
        .with_context(|| format!("writing {}", path.display()))
}

/// Server keys currently present in HOME mcp.json.
pub fn installed_keys() -> Vec<String> {
    let Ok(root) = read_mcp_json() else {
        return Vec::new();
    };
    let key = servers_key(&root);
    root.get(key)
        .and_then(|m| m.as_object())
        .map(|m| m.keys().cloned().collect())
        .unwrap_or_default()
}

/// Insert/overwrite a server entry under `key` in HOME mcp.json (preserves all other entries).
pub fn write_server(key: &str, entry: Value) -> Result<()> {
    let mut root = read_mcp_json()?;
    let skey = servers_key(&root);
    if !root.is_object() {
        root = json!({});
    }
    let map = root.as_object_mut().unwrap();
    let servers = map.entry(skey.to_string()).or_insert_with(|| json!({}));
    if !servers.is_object() {
        *servers = json!({});
    }
    servers
        .as_object_mut()
        .unwrap()
        .insert(key.to_string(), entry);
    write_mcp_json(&root)
}

/// Remove a server entry by key. `Ok(true)` when one was removed.
pub fn remove_server(key: &str) -> Result<bool> {
    let mut root = read_mcp_json()?;
    let skey = servers_key(&root);
    let removed = root
        .get_mut(skey)
        .and_then(|m| m.as_object_mut())
        .map(|m| m.remove(key).is_some())
        .unwrap_or(false);
    if removed {
        write_mcp_json(&root)?;
    }
    Ok(removed)
}

// ───────────────────────────── featured apps (curated shortcuts) ─────────────────────────────

/// A curated app: a friendly key + the registry search query + a vendor hint to pick the right
/// server. The actual transport/secret spec is pulled LIVE from the registry (never hardcoded).
pub struct Featured {
    pub key: &'static str,
    pub label: &'static str,
    pub icon: &'static str,
    pub blurb: &'static str,
    pub query: &'static str,
    pub prefer: &'static str,
}

pub const FEATURED: &[Featured] = &[
    Featured {
        key: "github",
        label: "GitHub",
        icon: "🐙",
        blurb: "repos, issues, PRs, workflows",
        query: "github",
        prefer: "github/github-mcp-server",
    },
    Featured {
        key: "linear",
        label: "Linear",
        icon: "📐",
        blurb: "issues & project tracking (browser sign-in)",
        query: "linear",
        prefer: "linear",
    },
    Featured {
        key: "notion",
        label: "Notion",
        icon: "📔",
        blurb: "pages, databases, notes (browser sign-in)",
        query: "notion",
        prefer: "notion",
    },
    Featured {
        key: "slack",
        label: "Slack",
        icon: "💬",
        blurb: "channels, messages, search (browser sign-in)",
        query: "slack",
        prefer: "slack",
    },
    Featured {
        key: "spotify",
        label: "Spotify",
        icon: "🎧",
        blurb: "playback & search (music while coding)",
        query: "spotify",
        prefer: "spotify",
    },
    Featured {
        key: "google",
        label: "Gmail",
        icon: "📧",
        blurb: "email (browser sign-in; may need oauth.client_id)",
        query: "gmail",
        prefer: "gmail",
    },
    Featured {
        key: "filesystem",
        label: "Filesystem",
        icon: "📁",
        blurb: "give the agent sandboxed local file access (no account, runs local)",
        query: "filesystem",
        prefer: "modelcontextprotocol",
    },
];

pub fn featured(key: &str) -> Option<&'static Featured> {
    let k = key.trim().to_lowercase();
    FEATURED.iter().find(|f| f.key == k)
}

/// A short slug for the mcp.json key from a registry name (e.g. `io.github.github/github-mcp-server`
/// → `github_mcp_server`). Used when adding a non-featured server by registry name.
pub fn slug_from_name(name: &str) -> String {
    let tail = name.rsplit('/').next().unwrap_or(name);
    let mut out = String::with_capacity(tail.len());
    for c in tail.chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c.to_ascii_lowercase());
        } else {
            out.push('_');
        }
    }
    out.trim_matches('_').to_string()
}

// ───────────────────────────── tests (pure, offline) ─────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn kv(name: &str, required: bool, secret: bool, value: Option<&str>) -> KeyValueInput {
        KeyValueInput {
            name: name.into(),
            value: value.map(|s| s.into()),
            description: None,
            is_required: required,
            is_secret: secret,
            default: None,
        }
    }

    #[test]
    fn parse_servers_unwraps_envelope_and_flat() {
        let body = json!({
            "servers": [
                {"server": {"name": "a", "description": "x"}, "_meta": {}},
                {"name": "b"}, // flat fallback
                {"server": {"description": "no name"}} // dropped (empty name)
            ],
            "metadata": {"count": 3}
        });
        let got = parse_servers(&body);
        assert_eq!(got.len(), 2);
        assert_eq!(got[0].name, "a");
        assert_eq!(got[1].name, "b");
    }

    #[test]
    fn pick_transport_is_local_first_remote_fallback() {
        // a fillable remote AND a package both exist → choose the LOCAL package (your machine/keys),
        // not the third-party hosted remote.
        let s = RegistryServer {
            remotes: vec![Remote {
                kind: "streamable-http".into(),
                url: "https://x/mcp".into(),
                headers: vec![kv("Authorization", true, true, None)],
            }],
            packages: vec![Package {
                registry_type: "npm".into(),
                identifier: "p".into(),
                ..Default::default()
            }],
            ..Default::default()
        };
        assert_eq!(pick_transport(&s), Some(TransportChoice::Package(0)));

        // no package, fillable remote → fall back to the remote
        let s2 = RegistryServer {
            remotes: vec![Remote {
                kind: "streamable-http".into(),
                url: "https://x/mcp".into(),
                headers: vec![kv("Authorization", true, true, None)],
            }],
            ..Default::default()
        };
        assert_eq!(pick_transport(&s2), Some(TransportChoice::Remote(0)));

        // OAuth-only remote (no header), no package → now connectable via sign-in (was None before).
        let s3 = RegistryServer {
            remotes: vec![Remote {
                kind: "streamable-http".into(),
                url: "https://x/mcp".into(),
                headers: vec![],
            }],
            ..Default::default()
        };
        assert_eq!(pick_transport(&s3), Some(TransportChoice::OAuthRemote(0)));
        assert!(is_viable(&s3));

        // legacy sse-only remote → still not connectable (our client doesn't implement two-endpoint sse)
        let s4 = RegistryServer {
            remotes: vec![Remote {
                kind: "sse".into(),
                url: "https://x/sse".into(),
                headers: vec![],
            }],
            ..Default::default()
        };
        assert_eq!(pick_transport(&s4), None);
        assert!(!is_viable(&s4));
    }

    #[test]
    fn build_oauth_remote_entry_writes_url_and_auth_no_prompt() {
        // An OAuth remote (Linear/Notion-style): no header to fill → build_entry must NOT prompt and
        // must record `{url, auth:"oauth"}` so the caller can run the sign-in flow.
        let s = RegistryServer {
            name: "com.linear/linear".into(),
            remotes: vec![Remote {
                kind: "streamable-http".into(),
                url: "https://mcp.linear.app/mcp".into(),
                headers: vec![],
            }],
            ..Default::default()
        };
        let mut ask =
            |_: &PromptSpec| -> String { panic!("OAuth remote must not prompt for a secret") };
        let entry = build_entry(&s, TransportChoice::OAuthRemote(0), &mut ask).unwrap();
        assert_eq!(entry["url"], json!("https://mcp.linear.app/mcp"));
        assert_eq!(entry["auth"], json!("oauth"));
        assert!(
            entry.get("headers").is_none(),
            "no headers written for an OAuth remote"
        );
    }

    #[test]
    fn pick_best_prefers_local_over_hosted_remote() {
        // a hosted-remote match (trendsmcp-style, the surprise-paid trap) + a self-hostable uvx match
        // → pick the LOCAL one even though the remote appears first in the registry results.
        let remote = RegistryServer {
            name: "ai.trendsmcp/spotify".into(),
            remotes: vec![Remote {
                kind: "streamable-http".into(),
                url: "https://spotify.api.trendsmcp.ai/mcp".into(),
                headers: vec![kv("Authorization", true, true, None)],
            }],
            ..Default::default()
        };
        let local = RegistryServer {
            name: "io.github.jamiew/spotify-mcp".into(),
            packages: vec![Package {
                registry_type: "pypi".into(),
                identifier: "spotify-mcp".into(),
                runtime_hint: Some("uvx".into()),
                ..Default::default()
            }],
            ..Default::default()
        };
        let best = pick_best(&[remote, local], "spotify").unwrap();
        assert_eq!(best.name, "io.github.jamiew/spotify-mcp");
    }

    #[test]
    fn dedupe_latest_keeps_highest_version_per_name() {
        let mk = |name: &str, ver: &str| RegistryServer {
            name: name.into(),
            version: Some(ver.into()),
            packages: vec![Package {
                registry_type: "npm".into(),
                identifier: "p".into(),
                ..Default::default()
            }],
            ..Default::default()
        };
        // numeric-aware: 1.0.10 must beat 1.0.2 (not string-compared)
        let r = dedupe_latest(vec![
            mk("a/x", "1.0.1"),
            mk("a/x", "1.0.10"),
            mk("a/x", "1.0.2"),
            mk("b/y", "0.1.0"),
        ]);
        assert_eq!(r.len(), 2);
        assert_eq!(
            r.iter()
                .find(|s| s.name == "a/x")
                .unwrap()
                .version
                .as_deref(),
            Some("1.0.10")
        );
    }

    #[test]
    fn host_of_extracts_authority() {
        assert_eq!(
            host_of("https://spotify.api.trendsmcp.ai/mcp"),
            "spotify.api.trendsmcp.ai"
        );
        assert_eq!(host_of("http://localhost:8080/x"), "localhost:8080");
    }

    #[test]
    fn build_remote_entry_resolves_secret_header_and_bearer() {
        let s = RegistryServer {
            name: "io.github.github/github-mcp-server".into(),
            remotes: vec![Remote {
                kind: "streamable-http".into(),
                url: "https://api.githubcopilot.com/mcp/".into(),
                headers: vec![kv("Authorization", true, true, None)],
            }],
            ..Default::default()
        };
        let mut answers = vec!["ghp_abc123".to_string()].into_iter();
        let mut ask = |_: &PromptSpec| answers.next().unwrap();
        let entry = build_entry(&s, TransportChoice::Remote(0), &mut ask).unwrap();
        assert_eq!(entry["url"], json!("https://api.githubcopilot.com/mcp/"));
        assert_eq!(
            entry["headers"]["Authorization"],
            json!("Bearer ghp_abc123"),
            "bare token gets Bearer"
        );
    }

    #[test]
    fn build_remote_entry_substitutes_placeholder_template() {
        let s = RegistryServer {
            remotes: vec![Remote {
                kind: "streamable-http".into(),
                url: "https://x/mcp".into(),
                headers: vec![kv("Authorization", true, true, Some("Bearer {tok}"))],
            }],
            ..Default::default()
        };
        let mut answers = vec!["XYZ".to_string()].into_iter();
        let mut ask = |_: &PromptSpec| answers.next().unwrap();
        let entry = build_entry(&s, TransportChoice::Remote(0), &mut ask).unwrap();
        assert_eq!(entry["headers"]["Authorization"], json!("Bearer XYZ"));
    }

    #[test]
    fn build_npm_package_entry_pins_version_and_collects_env() {
        let s = RegistryServer {
            packages: vec![Package {
                registry_type: "npm".into(),
                identifier: "airtable-mcp-server".into(),
                version: Some("1.7.2".into()),
                runtime_hint: Some("npx".into()),
                environment_variables: vec![kv("AIRTABLE_API_KEY", true, true, None)],
                ..Default::default()
            }],
            ..Default::default()
        };
        let mut answers = vec!["pat.secret".to_string()].into_iter();
        let mut ask = |_: &PromptSpec| answers.next().unwrap();
        let entry = build_entry(&s, TransportChoice::Package(0), &mut ask).unwrap();
        assert_eq!(entry["command"], json!("npx"));
        assert_eq!(entry["args"], json!(["-y", "airtable-mcp-server@1.7.2"]));
        assert_eq!(entry["env"]["AIRTABLE_API_KEY"], json!("pat.secret"));
    }

    #[test]
    fn build_docker_entry_forwards_env_with_dash_e() {
        let s = RegistryServer {
            packages: vec![Package {
                registry_type: "oci".into(),
                identifier: "docker.io/x/srv:1.0".into(),
                runtime_hint: Some("docker".into()),
                environment_variables: vec![kv("TOKEN", true, true, None)],
                ..Default::default()
            }],
            ..Default::default()
        };
        let mut answers = vec!["t".to_string()].into_iter();
        let mut ask = |_: &PromptSpec| answers.next().unwrap();
        let entry = build_entry(&s, TransportChoice::Package(0), &mut ask).unwrap();
        assert_eq!(entry["command"], json!("docker"));
        let args: Vec<String> = entry["args"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap().to_string())
            .collect();
        assert_eq!(
            args,
            vec!["run", "-i", "--rm", "-e", "TOKEN", "docker.io/x/srv:1.0"]
        );
        assert_eq!(entry["env"]["TOKEN"], json!("t"));
    }

    #[test]
    fn pick_best_prefers_vendor_hint_among_viable() {
        let make = |name: &str, viable: bool| RegistryServer {
            name: name.into(),
            packages: if viable {
                vec![Package {
                    registry_type: "npm".into(),
                    identifier: "p".into(),
                    ..Default::default()
                }]
            } else {
                vec![]
            },
            remotes: if viable {
                vec![]
            } else {
                vec![Remote {
                    kind: "sse".into(),
                    url: "u".into(),
                    headers: vec![],
                }]
            },
            ..Default::default()
        };
        let results = vec![
            make("com.random/notion-thing", true),
            make("com.notion/official", false),
            make("io.github.x/notion", true),
        ];
        // official is not viable (OAuth) → pick the viable one whose name contains "notion"
        let best = pick_best(&results, "notion").unwrap();
        assert_eq!(best.name, "com.random/notion-thing");
    }

    #[test]
    fn placeholders_extracts_braced_tokens() {
        assert_eq!(placeholders("Bearer {tok}"), vec!["tok"]);
        assert_eq!(placeholders("{a}/{b}"), vec!["a", "b"]);
        assert!(placeholders("no braces").is_empty());
    }

    #[test]
    fn slug_from_name_takes_tail_and_sanitizes() {
        assert_eq!(
            slug_from_name("io.github.github/github-mcp-server"),
            "github_mcp_server"
        );
        assert_eq!(slug_from_name("com.notion/mcp"), "mcp");
    }

    #[test]
    fn optional_input_without_value_is_skipped() {
        let inp = kv("OPTIONAL", false, false, None);
        let mut ask =
            |_: &PromptSpec| -> String { panic!("must not ask for an optional empty input") };
        assert!(resolve_input(&inp, &mut ask).unwrap().is_none());
    }

    #[test]
    fn required_input_left_empty_is_rejected() {
        // A required secret answered with "" must ERROR (not silently write an empty value).
        let inp = kv("API_KEY", true, true, None);
        let mut ask = |_: &PromptSpec| -> String { String::new() };
        assert!(resolve_input(&inp, &mut ask).is_err());
    }

    #[test]
    fn optional_secret_left_empty_is_dropped_not_written() {
        let inp = kv("OPTIONAL_TOKEN", false, true, None);
        let mut ask = |_: &PromptSpec| -> String { "   ".to_string() };
        assert!(resolve_input(&inp, &mut ask).unwrap().is_none());
    }

    #[test]
    fn required_positional_arg_is_prompted_not_dropped() {
        // The official filesystem server pattern: a required positional arg (the root dir) with no
        // fixed value must be PROMPTED and emitted — previously it was silently dropped.
        let p = Package {
            registry_type: "npm".into(),
            identifier: "@modelcontextprotocol/server-filesystem".into(),
            runtime_hint: Some("npx".into()),
            package_arguments: vec![Argument {
                kind: "positional".into(),
                is_required: true,
                value_hint: Some("<allowed-dir>".into()),
                ..Default::default()
            }],
            ..Default::default()
        };
        let s = RegistryServer {
            packages: vec![p],
            ..Default::default()
        };
        let mut answers = vec!["/home/me/project".to_string()].into_iter();
        let mut ask = |_: &PromptSpec| answers.next().unwrap();
        let entry = build_entry(&s, TransportChoice::Package(0), &mut ask).unwrap();
        let args: Vec<String> = entry["args"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap().to_string())
            .collect();
        assert!(
            args.contains(&"/home/me/project".to_string()),
            "required positional arg must be emitted: {args:?}"
        );
    }

    #[test]
    fn arg_value_template_substitutes_from_variables() {
        // A package arg whose value is `--token={tok}` with a `variables` map → prompted + filled.
        let mut vars = std::collections::HashMap::new();
        vars.insert("tok".to_string(), kv("tok", true, true, None));
        let args = arg_tokens(
            &[Argument {
                kind: "positional".into(),
                value: Some("--token={tok}".into()),
                variables: vars,
                ..Default::default()
            }],
            &mut |_: &PromptSpec| "SEKRET".to_string(),
        )
        .unwrap();
        assert_eq!(args, vec!["--token=SEKRET".to_string()]);
    }

    #[test]
    fn version_key_orders_ga_above_prerelease_and_numeric() {
        assert!(
            version_key(Some("1.10.0")) > version_key(Some("1.9.0")),
            "numeric, not lexical"
        );
        assert!(
            version_key(Some("1.2.0")) > version_key(Some("1.2.0-rc1")),
            "GA beats its own pre-release"
        );
        assert!(
            version_key(Some("2.0.0-rc1")) > version_key(Some("1.5.0")),
            "higher core wins even as pre-release"
        );
        assert!(
            version_key(None) < version_key(Some("0.0.1")),
            "absent sorts lowest"
        );
        // dedupe of the same core keeps the GA, not a same-core pre-release listed alongside it.
        let mk = |v: &str| RegistryServer {
            name: "a/x".into(),
            version: Some(v.into()),
            packages: vec![Package {
                registry_type: "npm".into(),
                identifier: "p".into(),
                ..Default::default()
            }],
            ..Default::default()
        };
        let r = dedupe_latest(vec![mk("1.2.0-rc1"), mk("1.2.0")]);
        assert_eq!(
            r[0].version.as_deref(),
            Some("1.2.0"),
            "GA of the same core wins"
        );
    }
}
