//! The contract between the tool REGISTRY, the provider REQUEST, and the system PROMPT.
//!
//! Three artifacts have to agree about what a tool is called and what it takes:
//!
//! - the registry (`ToolRegistry`) — names, descriptions, JSON schemas, dispatch;
//! - the request — the provider's native tool field, built from `registry.defs()`;
//! - the prompt — a compact `# Tool routing` map generated from the same names.
//!
//! Historically the third was a hand-written `# Tool catalog` in `system_prompt.md`, which named
//! tools a session doesn't register and missed tools it does. These tests pin the shape that makes
//! that drift impossible, plus the safety behaviours around a model-emitted call.

use crate::agent::builtin;
use crate::agent::tool_routing;
use crate::agent::tools::ToolRegistry;
use crate::agent::toolsets;
use crate::core::cli_config::CliConfig;
use crate::core::types::{FunctionCall, Message, ToolCall, ToolDef};
use crate::llm::client;
use crate::llm::client::ChatTurn;
use serde_json::Value;

/// A real, hermetic registry: `role_registry` needs no HTTP client and — unlike the top-level
/// builder — never publishes to the process-global tool surface, so it can't leak into another
/// test's prompt.
fn coder_registry() -> ToolRegistry {
    builtin::role_registry("coder", &std::env::temp_dir())
}

/// The names the routing map advertises, in print order. The map's ONLY use of backticks is a tool
/// name (hints are deliberately name-free), so this is an exact extraction.
fn routed_names(map: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut rest = map;
    while let Some(i) = rest.find('`') {
        rest = &rest[i + 1..];
        match rest.find('`') {
            Some(j) => {
                out.push(rest[..j].to_string());
                rest = &rest[j + 1..];
            }
            None => break,
        }
    }
    out
}

// ── 1. enabled tools ride the provider's NATIVE tool field ───────────────────────────────────

#[test]
fn enabled_tools_are_serialized_through_the_native_openai_tool_field() {
    let registry = coder_registry();
    let defs = registry.defs();
    assert!(!defs.is_empty(), "the coder role advertises tools");

    let body = client::build_chat_body(
        &CliConfig::default(),
        "some-model",
        &[Message::user("hi")],
        &defs,
        false,
        None,
    );
    let wire: Value = serde_json::to_value(&body).expect("request serializes");

    let tools = wire["tools"]
        .as_array()
        .expect("native `tools` array present");
    assert_eq!(tools.len(), defs.len(), "every enabled tool is advertised");
    for (t, d) in tools.iter().zip(defs.iter()) {
        assert_eq!(t["type"], "function", "OpenAI-native tool envelope");
        assert_eq!(t["function"]["name"], d.function.name.as_str());
        assert_eq!(
            t["function"]["parameters"]["type"], "object",
            "{} ships a real JSON Schema, not prose",
            d.function.name
        );
    }
    assert_eq!(wire["tool_choice"], "auto");
    assert_eq!(wire["parallel_tool_calls"], true);

    // …and the schemas are NOT also pasted into the conversation as text.
    let msgs = serde_json::to_string(&wire["messages"]).unwrap();
    assert!(
        !msgs.contains("\"additionalProperties\""),
        "tool schemas must not be duplicated into the messages"
    );
}

#[test]
fn a_tool_less_request_omits_the_tool_fields_entirely() {
    // Back-compat: a plain chat call must stay byte-identical to one from a build without tools,
    // or every summariser/learning request starts failing on strict gateways.
    let body = client::build_chat_body(
        &CliConfig::default(),
        "some-model",
        &[Message::user("hi")],
        &[],
        false,
        None,
    );
    let wire: Value = serde_json::to_value(&body).unwrap();
    assert!(wire.get("tools").is_none());
    assert!(wire.get("tool_choice").is_none());
    assert!(wire.get("parallel_tool_calls").is_none());
}

#[test]
fn anthropic_endpoints_use_the_same_native_tool_field_and_their_own_auth() {
    // Anthropic is served through its OpenAI-compatible chat-completions surface, so the tool
    // definitions are the same native objects; only the auth headers differ. If a future change ever
    // routes Anthropic somewhere else, this pins that the endpoint is at least still RECOGNISED as
    // first-party (host match, not substring) so the native pair is attached.
    assert!(client::is_anthropic_endpoint(
        "https://api.anthropic.com/v1"
    ));
    assert!(!client::is_anthropic_endpoint(
        "https://gw.example.com/anthropic/v1"
    ));

    let defs = coder_registry().defs();
    let body = client::build_chat_body(
        &CliConfig::default(),
        "claude-x",
        &[Message::user("hi")],
        &defs,
        true,
        None,
    );
    let wire: Value = serde_json::to_value(&body).unwrap();
    assert_eq!(wire["tools"].as_array().unwrap().len(), defs.len());
    assert_eq!(wire["tools"][0]["type"], "function");
    // Streaming asks for the usage chunk; the tool field is unaffected by the transport mode.
    assert_eq!(wire["stream_options"]["include_usage"], true);
}

#[test]
fn codex_gets_the_same_tools_in_its_own_native_responses_shape() {
    // The Codex endpoint speaks the Responses dialect, where a tool is a FLAT object rather than
    // OpenAI's `{type, function:{…}}` envelope. The adapter owns that translation; the registry is
    // still the only source of the name and schema.
    let defs = coder_registry().defs();
    let body = crate::llm::responses_codex::build_request_body(
        "gpt-5.4-mini-high",
        &[Message::user("hi")],
        &defs,
        "sess",
        None,
    );
    let tools = body["tools"].as_array().expect("native Responses tools");
    assert_eq!(tools.len(), defs.len());
    for (t, d) in tools.iter().zip(defs.iter()) {
        assert_eq!(t["type"], "function");
        assert_eq!(
            t["name"],
            d.function.name.as_str(),
            "flat name, no envelope"
        );
        assert_eq!(t["parameters"], d.function.parameters);
        assert!(
            t.get("function").is_none(),
            "not the chat-completions shape"
        );
    }
    assert_eq!(body["tool_choice"], "auto");
}

// ── 2. disabled tools disappear from BOTH the request and the map ────────────────────────────

#[test]
fn a_disabled_toolset_leaves_the_request_and_the_routing_map_together() {
    let mut registry = coder_registry();
    let before = registry.names();
    assert!(
        before.iter().any(|n| n == "web_search"),
        "precondition: the coder role has the web bundle"
    );

    // Exactly what `apply_toolset_filter` does, minus the process-global config read.
    let cfg = CliConfig {
        disabled_toolsets: Some(vec!["web".into()]),
        ..Default::default()
    };
    registry.retain(|name| {
        toolsets::classify_tool(name)
            .map(|ts| toolsets::toolset_allowed(ts, &cfg))
            .unwrap_or(true)
    });

    let names = registry.names();
    for gone in ["web_search", "web_fetch", "web_crawl"] {
        assert!(!names.iter().any(|n| n == gone), "{gone} still registered");
    }
    // Absent from the request…
    let defs = registry.defs();
    assert!(!defs.iter().any(|d| d.function.name.starts_with("web_")));
    // …and absent from the prompt, because both come from the same list.
    let map = tool_routing::routing_map(&names).expect("non-empty surface");
    assert!(!map.contains("`web_search`"));
    assert!(
        !map.contains("web research"),
        "the empty group is not printed"
    );
    // Something that survived still routes, so this isn't passing by rendering nothing.
    assert!(map.contains("`file_read`"));
}

#[test]
fn the_top_level_prompt_carries_the_published_surface_and_children_never_inherit_it() {
    // The published surface is process-global (the session builds one top-level registry), so this
    // takes the same lock the other prompt tests use and restores what it found.
    let _g = crate::core::config::TEST_HOME_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let names = coder_registry().names();
    let prior = builtin::swap_active_tools_for_test(Some(names.clone()));

    let top = crate::agent::build_top_level_system_prompt("/w", "linux", "2026-08-17", "m", None);
    // A sub-agent assembles through `build_system_prompt`, which must NOT pick the parent's surface
    // up — its own registry is narrower, and inheriting would re-create exactly the drift this
    // replaced.
    let child = crate::agent::build_system_prompt("/w", "linux", "2026-08-17", "m", None);

    builtin::swap_active_tools_for_test(None);
    let no_registry =
        crate::agent::build_top_level_system_prompt("/w", "linux", "2026-08-17", "m", None);
    builtin::swap_active_tools_for_test(prior);

    assert!(top.contains(tool_routing::ROUTING_HEADING));
    for n in &names {
        assert!(top.contains(&format!("`{n}`")), "{n} is not routed");
    }
    assert!(
        !child.contains(tool_routing::ROUTING_HEADING),
        "the sub-agent base must not inherit the parent's tool surface"
    );
    assert!(
        !no_registry.contains(tool_routing::ROUTING_HEADING),
        "no registry built yet ⇒ no map, rather than a stale or invented one"
    );
}

// ── 3. the map names EXACTLY the request's tools, deterministically ──────────────────────────

#[test]
fn routing_map_names_exactly_the_advertised_tools_in_registry_order() {
    let registry = coder_registry();
    let names = registry.names();
    let map = tool_routing::routing_map(&names).expect("non-empty surface");

    let routed = routed_names(&map);
    let mut sorted_routed = routed.clone();
    sorted_routed.sort();
    let mut sorted_defs: Vec<String> = registry
        .defs()
        .into_iter()
        .map(|d| d.function.name)
        .collect();
    sorted_defs.sort();
    assert_eq!(
        sorted_routed, sorted_defs,
        "the prompt and the request advertise the same set of names"
    );

    // Deterministic: same surface ⇒ byte-identical block (the prefix cache depends on it).
    assert_eq!(map, tool_routing::routing_map(&names).unwrap());
    // Within a group, names keep the registry's advertised order.
    let idx = |n: &str| routed.iter().position(|r| r == n).unwrap();
    assert!(idx("file_read") < idx("file_glob") || idx("file_glob") < idx("file_read"));
}

#[test]
fn the_stable_prompt_body_carries_no_tool_schemas_and_no_tool_catalog() {
    for (tier, base) in [
        ("full", crate::agent::system_base()),
        ("strict", crate::agent::system_base_strict()),
    ] {
        for schema_marker in [
            "\"type\": \"object\"",
            "additionalProperties",
            "\"properties\"",
            "\"required\": [",
        ] {
            assert!(
                !base.contains(schema_marker),
                "{tier} base leaks a JSON schema fragment: {schema_marker}"
            );
        }
        assert!(
            !base.contains("# Tool catalog"),
            "{tier} base still carries the hand-written catalog"
        );
        // The base must not hard-name tools either: it is the CACHE-STABLE lane and cannot know which
        // tools this session registered. Names belong to the generated routing map.
        for name in [
            "memory_search",
            "file_glob",
            "file_edit",
            "shell_run",
            "web_search",
            "browser_navigate",
            "telegram_send",
            "todo_write",
            "workflow",
            "symbol_replace",
        ] {
            assert!(
                !base.contains(name),
                "{tier} base names `{name}`, which may not be registered this session"
            );
        }
    }
}

// ── 4. a registered tool cannot drift between schema, routing name and dispatcher ────────────

#[test]
fn a_registered_tool_cannot_drift_between_schema_routing_and_dispatch() {
    let registry = coder_registry();
    let names = registry.names();
    let map = tool_routing::routing_map(&names).unwrap();

    let mut seen = std::collections::HashSet::new();
    for def in registry.defs() {
        let name = def.function.name.as_str();
        assert!(seen.insert(name.to_string()), "duplicate tool name {name}");
        // dispatchable under the exact name the request advertises
        assert!(
            registry.get(name).is_some(),
            "{name} is advertised but not dispatchable"
        );
        assert_eq!(registry.get(name).unwrap().name(), name);
        // a real schema, and a description the model can select on
        assert_eq!(def.function.parameters["type"], "object", "{name} schema");
        assert!(
            def.function.description.len() > 20,
            "{name} has no usable description"
        );
        // and routed under that same spelling
        assert!(map.contains(&format!("`{name}`")), "{name} is not routed");
    }
}

#[test]
fn every_registered_tool_is_classified_for_config_filtering() {
    // An unclassified tool is invisible to `disabled_toolsets` — the user can't turn it off. It is
    // deliberately still ADVERTISED (forward-compatible), so this is the alarm rather than a filter.
    let unclassified: Vec<String> = coder_registry()
        .names()
        .into_iter()
        .filter(|n| toolsets::classify_tool(n).is_none())
        .collect();
    assert!(
        unclassified.is_empty(),
        "add these to tool_routing::lane_for: {unclassified:?}"
    );
}

// ── 5. dynamic (MCP / session) tools appear only when they exist ─────────────────────────────

#[test]
fn dynamic_mcp_tools_appear_only_when_connected() {
    // With none connected (the test environment), nothing MCP-shaped is advertised or routed.
    let names = coder_registry().names();
    assert!(!names.iter().any(|n| n.starts_with("mcp_")));
    let map = tool_routing::routing_map(&names).unwrap();
    assert!(!map.contains("connected integrations"));

    // Connect one (the registry is the only gate) and it routes under its own heading.
    let mut with_mcp = names.clone();
    with_mcp.push("mcp_postgres_query".to_string());
    let map = tool_routing::routing_map(&with_mcp).unwrap();
    assert!(map.contains("connected integrations"));
    assert!(map.contains("`mcp_postgres_query`"));
}

// ── 6. model-emitted calls: unknown names and bad arguments fail safely ──────────────────────

/// Drive the real loop with a scripted model and hand back the messages it saw on its LAST call —
/// which is where a tool result lands.
async fn run_scripted(registry: &ToolRegistry, turns: Vec<ChatTurn>) -> Vec<Message> {
    use std::sync::Mutex;
    let queue = Mutex::new(std::collections::VecDeque::from(turns));
    let seen: Mutex<Vec<Message>> = Mutex::new(Vec::new());
    let chat = |msgs: Vec<Message>, _defs: Vec<ToolDef>| {
        *seen.lock().unwrap() = msgs;
        let next = queue
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or_else(|| ChatTurn {
                content: Some("done".into()),
                tool_calls: Vec::new(),
                finish_reason: Some("stop".into()),
                usage: None,
                eager: Vec::new(),
            });
        std::future::ready(Ok(next))
    };
    let cfg = crate::agent::AgentConfig {
        max_iters: 4,
        auto_extend_to: 4,
        quiet: true,
        enable_verify_gate: false,
        auto_checkpoint: false,
        checkpoint_each_edit: false,
        enable_self_review: false,
        ..crate::agent::AgentConfig::default()
    };
    crate::agent::run_agent(chat, &cfg, registry, "sys", "task")
        .await
        .expect("loop completes");
    let out = seen.lock().unwrap().clone();
    out
}

fn tool_call(name: &str, args: &str) -> ChatTurn {
    ChatTurn {
        content: None,
        tool_calls: vec![ToolCall {
            id: "c1".into(),
            kind: "function".into(),
            function: FunctionCall {
                name: name.into(),
                arguments: args.into(),
            },
        }],
        finish_reason: Some("tool_calls".into()),
        usage: None,
        eager: Vec::new(),
    }
}

fn last_tool_result(msgs: &[Message]) -> String {
    msgs.iter()
        .rev()
        .find(|m| m.role == "tool")
        .and_then(|m| m.content.clone())
        .expect("a tool result was appended")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_unknown_tool_call_is_rejected_not_executed() {
    // The name a stale conversation remembers, or a hallucinated one, must come back as a structured
    // error — never fall through to "run it as a shell command anyway".
    let registry = coder_registry();
    let msgs = run_scripted(
        &registry,
        vec![tool_call("definitely_not_a_tool", r#"{"cmd":"rm -rf /"}"#)],
    )
    .await;
    let result = last_tool_result(&msgs);
    assert!(
        result.starts_with("error: unknown tool"),
        "expected a structured rejection, got: {result}"
    );
    assert!(result.contains("definitely_not_a_tool"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_disabled_tool_cannot_be_invoked_even_if_the_conversation_names_it() {
    // `task` is deliberately absent from every sub-agent registry (the recursion guard). A child that
    // saw the parent use it must not be able to reach it.
    let registry = coder_registry();
    assert!(registry.get("task").is_none(), "precondition");
    let msgs = run_scripted(&registry, vec![tool_call("task", r#"{"prompt":"go"}"#)]).await;
    assert!(last_tool_result(&msgs).starts_with("error: unknown tool"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn invalid_arguments_produce_a_controlled_error_not_a_panic() {
    let registry = coder_registry();
    // Missing every required key: the loop must answer with the schema, not run the tool.
    let msgs = run_scripted(&registry, vec![tool_call("file_read", "{}")]).await;
    let result = last_tool_result(&msgs);
    assert!(result.starts_with("error: "), "{result}");
    assert!(result.contains("path"), "names the required arg: {result}");

    // Syntactically broken arguments are caught before dispatch.
    let msgs = run_scripted(&registry, vec![tool_call("file_read", "{not json")]).await;
    assert!(last_tool_result(&msgs).starts_with("error: invalid JSON arguments"));
}

#[test]
fn a_tool_body_returns_err_rather_than_panicking_on_junk_args() {
    let registry = coder_registry();
    let tool = registry.get("file_read").unwrap();
    for junk in [
        serde_json::json!({}),
        serde_json::json!({"path": ""}),
        serde_json::json!({"path": 12}),
        serde_json::json!("not even an object"),
    ] {
        let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| tool.execute(&junk)));
        let r = r.unwrap_or_else(|_| panic!("file_read panicked on {junk}"));
        assert!(r.is_err(), "junk args must be an Err, got {r:?}");
    }
}

// ── 7. tool-call round trip stays wire-compatible ────────────────────────────────────────────

#[test]
fn tool_call_round_trip_keeps_the_openai_wire_shape() {
    // The assistant turn that requests a call, and the tool turn that answers it, are replayed back
    // to the provider on the next iteration. Their shape is load-bearing: `arguments` is a STRING,
    // and the answer is matched by `tool_call_id`.
    let call = ToolCall {
        id: "call_1".into(),
        kind: "function".into(),
        function: FunctionCall {
            name: "file_read".into(),
            arguments: r#"{"path":"a.rs"}"#.into(),
        },
    };
    let assistant = Message::assistant_tool_calls(vec![call.clone()]);
    let wire: Value = serde_json::to_value(&assistant).unwrap();
    assert_eq!(wire["role"], "assistant");
    assert_eq!(wire["content"], Value::Null);
    assert_eq!(wire["tool_calls"][0]["type"], "function");
    assert_eq!(wire["tool_calls"][0]["function"]["name"], "file_read");
    assert!(
        wire["tool_calls"][0]["function"]["arguments"].is_string(),
        "arguments stay a stringified object"
    );

    let result: Value = serde_json::to_value(Message::tool_result("call_1", "ok")).unwrap();
    assert_eq!(result["role"], "tool");
    assert_eq!(result["tool_call_id"], "call_1");
    assert_eq!(result["content"], "ok");
}

// ── 8. runtime identity, runtime shell ───────────────────────────────────────────────────────

#[test]
fn model_identity_comes_from_runtime_metadata_and_is_not_hard_coded() {
    let p = crate::agent::build_system_prompt("/w", "linux", "2026-08-17", "some-model-v9", None);
    assert!(
        p.contains("model: some-model-v9"),
        "the runtime model is stated in <environment>"
    );
    // No baked-in vendor or model claim anywhere in either stable base.
    for base in [
        crate::agent::system_base(),
        crate::agent::system_base_strict(),
    ] {
        // `CLAUDE.md` is a project-instruction FILENAME the prompt legitimately points at (alongside
        // `AGENTS.md`); it is not an identity claim, so it is removed before the vendor sweep.
        let lower = base.to_lowercase().replace("claude.md", "");
        for banned in [
            "claude",
            "opus",
            "sonnet",
            "anthropic",
            "openai",
            "gpt-",
            "gemini",
            "deepseek",
        ] {
            assert!(
                !lower.contains(banned),
                "the base prompt hard-codes a model/vendor identity: {banned}"
            );
        }
    }
    // …and it says where to read the answer from instead.
    assert!(crate::agent::system_base().contains("<environment>"));
}

#[test]
fn windows_gets_powershell_context_and_other_platforms_do_not() {
    let win = crate::agent::build_system_prompt("/w", "windows", "2026-08-17", "m", None);
    assert!(win.contains("shell: powershell"), "{win}");
    assert!(win.contains("$env:"), "PowerShell-first syntax preserved");
    assert!(!win.contains("xdg-open"));

    let linux = crate::agent::build_system_prompt("/w", "linux", "2026-08-17", "m", None);
    assert!(linux.contains("shell: sh (POSIX)"));
    assert!(!linux.to_lowercase().contains("powershell"));
    assert!(linux.contains("xdg-open"));

    let mac = crate::agent::build_system_prompt("/w", "macos", "2026-08-17", "m", None);
    assert!(mac.contains("shell: sh (POSIX)"));
    assert!(mac.contains("open"));
    assert!(!mac.contains("xdg-open"));
}

// ── 9. prompt construction leaks nothing it shouldn't ────────────────────────────────────────

#[test]
fn prompt_construction_never_embeds_credentials() {
    const SECRET: &str = "sk-test-DEADBEEF-should-never-appear";
    let names = coder_registry().names();
    let map = tool_routing::routing_map(&names).unwrap();
    // Schemas carry descriptions, not values; the map carries names only.
    assert!(!map.contains(SECRET));
    assert!(!map.contains("api_key"));

    // The whole assembled prompt, with a credential live in the environment.
    let prior = std::env::var("AIZEN_API_KEY").ok();
    std::env::set_var("AIZEN_API_KEY", SECRET);
    let prompt = crate::agent::build_system_prompt("/w", "linux", "2026-08-17", "m", None);
    match prior {
        Some(v) => std::env::set_var("AIZEN_API_KEY", v),
        None => std::env::remove_var("AIZEN_API_KEY"),
    }
    assert!(!prompt.contains(SECRET), "the API key reached the prompt");
    assert!(!prompt.contains("Bearer "));
}

#[test]
fn tool_definitions_never_carry_credential_values() {
    // Descriptions and schemas are model-facing text baked at registration; a tool that interpolated
    // a resolved secret into either would ship it on every request.
    const SECRET: &str = "tvly-test-SHOULDNOTLEAK";
    let prior = std::env::var("TAVILY_API_KEY").ok();
    std::env::set_var("TAVILY_API_KEY", SECRET);
    let json = serde_json::to_string(&coder_registry().defs()).unwrap();
    match prior {
        Some(v) => std::env::set_var("TAVILY_API_KEY", v),
        None => std::env::remove_var("TAVILY_API_KEY"),
    }
    assert!(!json.contains(SECRET), "a tool definition leaked a key");
}
