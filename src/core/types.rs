//! OpenAI-compatible chat-completions wire types (the hermes-style provider-agnostic format).
//! We deliberately mirror the OpenAI `/chat/completions` schema so the CLI can point at any
//! OpenAI-compatible endpoint (OpenAI, OpenRouter, a local model, or the Aizen gateway later).

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize)]
pub struct ChatRequest {
    pub model: String,
    pub messages: Vec<Message>,
    pub stream: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f32>,
    /// Cap on the model's OUTPUT tokens (an output-safety knob, not an input-token saving). Omitted
    /// from the wire when None → provider default; set from `CliConfig.max_tokens`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u32>,
    /// Tools the model may call. Omitted from the wire when empty (plain chat).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub tools: Vec<ToolDef>,
    /// `auto` | `none` | `required` | a named-function object. Omitted when no tools.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_choice: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parallel_tool_calls: Option<bool>,
    /// Ask a STREAMING endpoint to emit a final usage chunk (`{include_usage:true}`). Omitted on the
    /// wire when `None` (non-streaming responses carry `usage` natively). Endpoints that don't honor
    /// it simply never send the chunk → the cost meter falls back to the chars/4 estimate.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stream_options: Option<StreamOptions>,
    /// Reasoning-effort dial for reasoning models exposed via chat-completions (o-series/GPT-5
    /// style: "low"/"medium"/"high"; free-form — the provider validates). Omitted from the wire
    /// when `None`, so the request is byte-identical for current users; providers that don't know
    /// the field ignore it (graceful degrade). Set from `CliConfig.reasoning_effort`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning_effort: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct StreamOptions {
    pub include_usage: bool,
}

/// An Anthropic prompt-cache breakpoint: attached to the LAST tool def and to a stable message so
/// the provider caches everything up to and including it (replayed at ~0.1× input cost). Serialized
/// only when present → the wire is byte-identical for providers that don't support caching.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct CacheControl {
    #[serde(rename = "type")]
    pub kind: String, // always "ephemeral"
}
impl CacheControl {
    pub fn ephemeral() -> Self {
        Self {
            kind: "ephemeral".into(),
        }
    }
}

/// Real token accounting from the provider (when it reports it). All fields optional — providers
/// vary in which they send. The `cache_*` fields let `/cost` show prompt-cache hits (Anthropic
/// surfaces `cache_read_input_tokens`; OpenAI-style nests `prompt_tokens_details.cached_tokens`).
#[derive(Debug, Clone, Default, Deserialize)]
pub struct Usage {
    #[serde(default)]
    pub prompt_tokens: Option<u64>,
    #[serde(default)]
    pub completion_tokens: Option<u64>,
    #[serde(default)]
    pub total_tokens: Option<u64>,
    #[serde(default)]
    pub cache_read_input_tokens: Option<u64>,
    /// Anthropic-compatible gateways: cache WRITE tokens this call (the breakpoint-creation cost).
    /// Deserialize-only for now — lets `/cost` grow a write-side line without a wire change.
    #[serde(default)]
    #[allow(dead_code)]
    pub cache_creation_input_tokens: Option<u64>,
    #[serde(default)]
    pub prompt_tokens_details: Option<PromptTokensDetails>,
}

impl Usage {
    /// Cached input tokens read this call, from whichever shape the provider used (0 if none).
    pub fn cache_read(&self) -> u64 {
        self.cache_read_input_tokens
            .or_else(|| {
                self.prompt_tokens_details
                    .as_ref()
                    .and_then(|d| d.cached_tokens)
            })
            .unwrap_or(0)
    }
}

/// OpenAI-style nested cache accounting (`usage.prompt_tokens_details.cached_tokens`).
#[derive(Debug, Clone, Default, Deserialize)]
pub struct PromptTokensDetails {
    #[serde(default)]
    pub cached_tokens: Option<u64>,
}

/// A chat message. Carries optional `tool_calls` (assistant turn) and `tool_call_id`
/// (tool-result turn) so one Vec<Message> models the whole tool-calling history.
///
/// `content` is serialized even when `None` (→ JSON `null`): an assistant turn that only
/// emits tool calls is the canonical `{role:"assistant", content:null, tool_calls:[…]}`.
///
/// `images` (data URLs) is NOT a plain wire field: it's a vision attachment. BOTH directions are
/// hand-written so the two agree. A user turn with images serializes `content` as a parts array
/// — `[{type:text,…},{type:image_url,image_url:{url}}]` — and [`Deserialize`] splits that array back
/// apart. Images live OUT of `content` in memory on purpose: the `chars/4` token HUD + auto-compact
/// count only `content`, so a multi-MB base64 image never inflates the gauge.
#[derive(Debug, Clone)]
pub struct Message {
    pub role: String,
    pub content: Option<String>,
    pub tool_calls: Vec<ToolCall>,
    pub tool_call_id: Option<String>,
    /// Vision attachments as `data:<mime>;base64,…` URLs (user turns only). Emitted via the custom
    /// `Serialize` below as parts inside `content`, and recovered back OUT of those parts by the
    /// custom `Deserialize` — see [`ContentField`].
    pub images: Vec<String>,
    /// Anthropic prompt-cache breakpoint, set on a stable message just before the volatile turn so
    /// the provider caches the prefix up to here. Write-only: emitted by the custom `Serialize`,
    /// never read from the wire (a stale breakpoint from a saved transcript must not be replayed).
    pub cache_control: Option<CacheControl>,
}

/// The three shapes `content` legitimately takes on the wire and on disk: absent/`null`, a plain
/// string, or an OpenAI multimodal parts array. The array arm is not merely provider compatibility —
/// it is what OUR OWN [`Serialize`](Message::serialize) writes for a user turn carrying `images`.
///
/// Deserializing only `Option<String>` therefore made every transcript containing a pasted image
/// unreadable BY THE PROGRAM THAT WROTE IT: `serde` failed on the array, `parse_session_bytes`
/// returned `None`, and `/sessions` listed the conversation as "(unreadable)" forever. Round-tripping
/// through this enum closes that asymmetry, and makes already-saved files load again with no
/// migration — the data was never lost, only unparseable.
#[derive(Deserialize)]
#[serde(untagged)]
enum ContentField {
    Text(String),
    Parts(Vec<ContentPart>),
}

/// One element of a multimodal `content` array. Unknown part types deserialize to `Other` and are
/// dropped rather than failing the whole message: a provider adding a part kind (audio, video, file)
/// must not make a saved conversation unreadable — the same failure this enum exists to fix.
#[derive(Deserialize)]
#[serde(tag = "type")]
enum ContentPart {
    #[serde(rename = "text")]
    Text { text: String },
    #[serde(rename = "image_url")]
    ImageUrl { image_url: ImageUrlPart },
    #[serde(other)]
    Other,
}

#[derive(Deserialize)]
struct ImageUrlPart {
    url: String,
}

/// Mirror of the deserialized field set. `images`/`cache_control` are deliberately absent: images
/// arrive inside `content` parts (see [`ContentField`]), and a cache breakpoint is per-request state
/// that must be recomputed, never replayed from disk.
#[derive(Deserialize)]
struct MessageWire {
    role: String,
    #[serde(default)]
    content: Option<ContentField>,
    #[serde(default)]
    tool_calls: Vec<ToolCall>,
    #[serde(default)]
    tool_call_id: Option<String>,
}

impl<'de> Deserialize<'de> for Message {
    fn deserialize<D: serde::Deserializer<'de>>(de: D) -> Result<Self, D::Error> {
        let wire = MessageWire::deserialize(de)?;
        // Split the parts array back into the in-memory split the rest of the codebase relies on:
        // text in `content`, image data URLs in `images`. Keeping images OUT of `content` is load
        // bearing — the `chars/4` token HUD and auto-compact measure `content` only, so folding a
        // multi-MB base64 URL into it would blow up both (see the struct doc above).
        let (content, images) = match wire.content {
            None => (None, Vec::new()),
            Some(ContentField::Text(s)) => (Some(s), Vec::new()),
            Some(ContentField::Parts(parts)) => {
                let mut text = String::new();
                let mut images = Vec::new();
                for part in parts {
                    match part {
                        ContentPart::Text { text: t } => {
                            if !text.is_empty() {
                                text.push('\n');
                            }
                            text.push_str(&t);
                        }
                        ContentPart::ImageUrl { image_url } => images.push(image_url.url),
                        ContentPart::Other => {}
                    }
                }
                // An image-only turn serializes with no text part at all. Round-tripping it back to
                // `Some("")` rather than `None` keeps the message shape stable across save/load.
                (Some(text), images)
            }
        };
        Ok(Message {
            role: wire.role,
            content,
            tool_calls: wire.tool_calls,
            tool_call_id: wire.tool_call_id,
            images,
            cache_control: None,
        })
    }
}

impl Serialize for Message {
    /// Mirror the derived shape exactly, with one addition: a user turn carrying `images` serializes
    /// `content` as an OpenAI parts array (text part first, then one `image_url` part per image).
    fn serialize<S: serde::Serializer>(&self, ser: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeMap;
        let mut map = ser.serialize_map(None)?;
        map.serialize_entry("role", &self.role)?;
        if self.role == "user" && !self.images.is_empty() {
            let mut parts: Vec<serde_json::Value> = Vec::with_capacity(self.images.len() + 1);
            if let Some(text) = &self.content {
                if !text.is_empty() {
                    parts.push(serde_json::json!({"type": "text", "text": text}));
                }
            }
            for url in &self.images {
                parts.push(serde_json::json!({"type": "image_url", "image_url": {"url": url}}));
            }
            map.serialize_entry("content", &parts)?;
        } else {
            // content is always present (null when None) — preserves the assistant tool-call shape.
            map.serialize_entry("content", &self.content)?;
        }
        if !self.tool_calls.is_empty() {
            map.serialize_entry("tool_calls", &self.tool_calls)?;
        }
        if let Some(id) = &self.tool_call_id {
            map.serialize_entry("tool_call_id", id)?;
        }
        // Prompt-cache breakpoint (omitted entirely when None → byte-identical for non-cache providers).
        if let Some(cc) = &self.cache_control {
            map.serialize_entry("cache_control", cc)?;
        }
        map.end()
    }
}

impl Message {
    pub fn system(content: impl Into<String>) -> Self {
        Self {
            role: "system".into(),
            content: Some(content.into()),
            tool_calls: Vec::new(),
            tool_call_id: None,
            images: Vec::new(),
            cache_control: None,
        }
    }
    pub fn user(content: impl Into<String>) -> Self {
        Self {
            role: "user".into(),
            content: Some(content.into()),
            tool_calls: Vec::new(),
            tool_call_id: None,
            images: Vec::new(),
            cache_control: None,
        }
    }
    /// A user turn with vision attachments (data URLs). `content` may be empty (image-only message).
    pub fn user_with_images(content: impl Into<String>, images: Vec<String>) -> Self {
        Self {
            role: "user".into(),
            content: Some(content.into()),
            tool_calls: Vec::new(),
            tool_call_id: None,
            images,
            cache_control: None,
        }
    }
    /// An assistant turn carrying natural-language content (used to thread multi-turn chat history).
    pub fn assistant(content: impl Into<String>) -> Self {
        Self {
            role: "assistant".into(),
            content: Some(content.into()),
            tool_calls: Vec::new(),
            tool_call_id: None,
            images: Vec::new(),
            cache_control: None,
        }
    }
    /// An assistant turn that requests tool calls (no natural-language content).
    /// The loop builds its own assistant message inline (to preserve any content); this is
    /// the canonical constructor used by tests + future callers.
    #[allow(dead_code)]
    pub fn assistant_tool_calls(tool_calls: Vec<ToolCall>) -> Self {
        Self {
            role: "assistant".into(),
            content: None,
            tool_calls,
            tool_call_id: None,
            images: Vec::new(),
            cache_control: None,
        }
    }
    /// A tool-result turn, linked back to the call by `tool_call_id`.
    pub fn tool_result(tool_call_id: impl Into<String>, content: impl Into<String>) -> Self {
        Self {
            role: "tool".into(),
            content: Some(content.into()),
            tool_calls: Vec::new(),
            tool_call_id: Some(tool_call_id.into()),
            images: Vec::new(),
            cache_control: None,
        }
    }
}

// ── tool-calling: request-side definition ───────────────────────────────────

/// A tool advertised to the model in the request.
#[derive(Debug, Clone, Serialize)]
pub struct ToolDef {
    #[serde(rename = "type")]
    pub kind: String, // always "function"
    pub function: FunctionDef,
    /// Prompt-cache breakpoint on the LAST tool def caches the whole tool-schema block. Omitted when
    /// None → byte-identical wire for providers without caching.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_control: Option<CacheControl>,
}

impl ToolDef {
    /// Build a `function` tool. `parameters` is a JSON-Schema object.
    pub fn function(
        name: impl Into<String>,
        description: impl Into<String>,
        parameters: serde_json::Value,
    ) -> Self {
        Self {
            kind: "function".into(),
            function: FunctionDef {
                name: name.into(),
                description: description.into(),
                parameters,
            },
            cache_control: None,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct FunctionDef {
    pub name: String,
    pub description: String,
    /// JSON-Schema for the arguments object.
    pub parameters: serde_json::Value,
}

// ── tool-calling: model-emitted call (round-trips: deserialize from the response,
//    serialize back into the assistant message we append to history) ───────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCall {
    #[serde(default)]
    pub id: String,
    #[serde(rename = "type", default = "default_function_type")]
    pub kind: String,
    pub function: FunctionCall,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FunctionCall {
    #[serde(default)]
    pub name: String,
    /// FOOTGUN: arguments arrive as a STRINGIFIED JSON object, not a nested object.
    /// Parse with `serde_json::from_str` before use; empty string → treat as `{}`.
    #[serde(default)]
    pub arguments: String,
}

fn default_function_type() -> String {
    "function".to_string()
}

// ── non-streaming response (used by `chat_with_tools` + the protocol parse tests) ────

#[allow(dead_code)]
#[derive(Debug, Deserialize)]
pub struct ChatResponse {
    #[serde(default)]
    pub choices: Vec<RespChoice>,
    #[serde(default)]
    pub usage: Option<Usage>,
}

#[allow(dead_code)]
#[derive(Debug, Deserialize)]
pub struct RespChoice {
    pub message: RespMessage,
    #[serde(default)]
    pub finish_reason: Option<String>,
}

#[allow(dead_code)]
#[derive(Debug, Default, Deserialize)]
pub struct RespMessage {
    #[serde(default)]
    #[allow(dead_code)]
    pub role: Option<String>,
    #[serde(default)]
    pub content: Option<String>,
    /// The signal that the model wants tools — trust THIS, not `finish_reason` (some
    /// gateways emit `stop`/`end_turn` alongside tool calls).
    #[serde(default)]
    pub tool_calls: Vec<ToolCall>,
    /// Dedicated reasoning channel some providers return INSTEAD of `content` (DeepSeek
    /// `reasoning_content`, OpenRouter `reasoning`) — the same field the streaming [`Delta`] has
    /// always read.
    ///
    /// This struct did NOT read it, so a reasoning-only reply deserialized to
    /// `content: None, tool_calls: []` — which every caller classifies as an empty-200 provider
    /// failure even though the model spoke. The asymmetry only bit SUB-AGENTS, because they are the
    /// one caller that runs exclusively on the non-streaming path (`chat_with_tools`): the same
    /// model answering the same way over the streaming path was fine. Read here so
    /// [`crate::llm::client::chat_with_tools`] can fall back to it rather than report silence.
    #[serde(default, alias = "reasoning")]
    pub reasoning_content: Option<String>,
}

// ── streaming response (plain chat; tool-call delta reassembly is H5/v2) ──────

/// One streamed chunk: `data: {json}` lines from the SSE response.
#[derive(Debug, Deserialize)]
pub struct ChatChunk {
    #[serde(default)]
    pub choices: Vec<ChunkChoice>,
    /// Present on the FINAL chunk when `stream_options.include_usage` was requested (choices empty).
    #[serde(default)]
    pub usage: Option<Usage>,
}

#[derive(Debug, Deserialize)]
pub struct ChunkChoice {
    #[serde(default)]
    pub delta: Delta,
    // Deserialized from the wire; consumed once we add stop-reason handling to the loop.
    #[serde(default)]
    #[allow(dead_code)]
    pub finish_reason: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
pub struct Delta {
    // Present on the first chunk ("assistant"); used when we track full message structure.
    #[serde(default)]
    #[allow(dead_code)]
    pub role: Option<String>,
    #[serde(default)]
    pub content: Option<String>,
    /// Dedicated chain-of-thought channel some providers stream alongside `content` (DeepSeek uses
    /// `reasoning_content`; OpenRouter uses `reasoning`). Captured so the CLI can SUPPRESS it from
    /// the display — the user sees only the answer, uniform across models. (Tag-based `<think>…`
    /// reasoning that arrives inside `content` is stripped separately by the `ThinkFilter`.)
    #[serde(default, alias = "reasoning")]
    pub reasoning_content: Option<String>,
    /// Tool-call fragments (streaming). Reassembled by `(index)` across chunks.
    #[serde(default)]
    pub tool_calls: Vec<ToolCallDelta>,
}

/// One streamed tool-call fragment. `id`+`function.name` arrive on the first fragment for an
/// index; `function.arguments` is streamed in pieces that CONCATENATE.
#[derive(Debug, Deserialize)]
pub struct ToolCallDelta {
    #[serde(default)]
    pub index: usize,
    #[serde(default)]
    pub id: Option<String>,
    #[serde(default)]
    pub function: Option<FunctionDelta>,
}

#[derive(Debug, Deserialize)]
pub struct FunctionDelta {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub arguments: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_tool_call_response() {
        let json = r#"{"choices":[{"message":{"role":"assistant","content":null,"tool_calls":[{"id":"call_1","type":"function","function":{"name":"memory_search","arguments":"{\"query\":\"pnpm\"}"}}]},"finish_reason":"tool_calls"}]}"#;
        let resp: ChatResponse = serde_json::from_str(json).unwrap();
        let choice = &resp.choices[0];
        assert_eq!(choice.finish_reason.as_deref(), Some("tool_calls"));
        assert_eq!(choice.message.tool_calls.len(), 1);
        assert_eq!(choice.message.tool_calls[0].function.name, "memory_search");
        assert_eq!(
            choice.message.tool_calls[0].function.arguments,
            "{\"query\":\"pnpm\"}"
        );
        assert!(choice.message.content.is_none());
    }

    #[test]
    fn parses_final_answer_no_tools() {
        let json = r#"{"choices":[{"message":{"role":"assistant","content":"all done"},"finish_reason":"stop"}]}"#;
        let resp: ChatResponse = serde_json::from_str(json).unwrap();
        assert_eq!(resp.choices[0].message.content.as_deref(), Some("all done"));
        assert!(resp.choices[0].message.tool_calls.is_empty());
    }

    #[test]
    fn tool_calls_present_even_when_finish_reason_is_stop() {
        // The hard-won extension lesson: a gateway can emit finish_reason="stop"/"end_turn"
        // WITH tool calls. The structured array is the source of truth.
        let json = r#"{"choices":[{"message":{"role":"assistant","content":null,"tool_calls":[{"id":"c","type":"function","function":{"name":"f","arguments":""}}]},"finish_reason":"stop"}]}"#;
        let resp: ChatResponse = serde_json::from_str(json).unwrap();
        assert_eq!(resp.choices[0].finish_reason.as_deref(), Some("stop"));
        assert!(
            !resp.choices[0].message.tool_calls.is_empty(),
            "must detect tools by the array, not finish_reason"
        );
    }

    #[test]
    fn tolerates_missing_finish_reason_and_empty_args_and_missing_type() {
        let json = r#"{"choices":[{"message":{"role":"assistant","tool_calls":[{"id":"c","function":{"name":"f"}}]}}]}"#;
        let resp: ChatResponse = serde_json::from_str(json).unwrap();
        assert!(resp.choices[0].finish_reason.is_none());
        let tc = &resp.choices[0].message.tool_calls[0];
        assert_eq!(tc.kind, "function", "type defaults to function");
        assert_eq!(
            tc.function.arguments, "",
            "missing arguments → empty string"
        );
    }

    #[test]
    fn assistant_tool_call_message_serializes_with_null_content() {
        let m = Message::assistant_tool_calls(vec![ToolCall {
            id: "c".into(),
            kind: "function".into(),
            function: FunctionCall {
                name: "f".into(),
                arguments: "{}".into(),
            },
        }]);
        let v = serde_json::to_value(&m).unwrap();
        assert_eq!(v["role"], "assistant");
        assert!(
            v["content"].is_null(),
            "assistant tool-call turn must send content:null"
        );
        assert_eq!(v["tool_calls"][0]["function"]["name"], "f");
        assert_eq!(v["tool_calls"][0]["function"]["arguments"], "{}");
    }

    #[test]
    fn plain_user_message_serializes_content_as_string() {
        // No images → content stays a plain string (not a parts array), images field absent.
        let v = serde_json::to_value(Message::user("hi there")).unwrap();
        assert_eq!(v["content"], "hi there");
        assert!(v.get("images").is_none(), "images is never a wire field");
    }

    #[test]
    fn user_message_with_images_serializes_content_as_parts_array() {
        let m = Message::user_with_images(
            "what is this?",
            vec![
                "data:image/png;base64,AAAA".into(),
                "data:image/jpeg;base64,BBBB".into(),
            ],
        );
        let v = serde_json::to_value(&m).unwrap();
        assert_eq!(v["role"], "user");
        let parts = v["content"]
            .as_array()
            .expect("content is a parts array when images present");
        assert_eq!(parts.len(), 3, "1 text part + 2 image parts");
        assert_eq!(parts[0]["type"], "text");
        assert_eq!(parts[0]["text"], "what is this?");
        assert_eq!(parts[1]["type"], "image_url");
        assert_eq!(parts[1]["image_url"]["url"], "data:image/png;base64,AAAA");
        assert_eq!(parts[2]["image_url"]["url"], "data:image/jpeg;base64,BBBB");
        assert!(
            v.get("images").is_none(),
            "the images field is not emitted directly"
        );
    }

    #[test]
    fn image_only_message_omits_the_empty_text_part() {
        // Image with no caption → only the image part, no empty text part.
        let m = Message::user_with_images("", vec!["data:image/png;base64,AAAA".into()]);
        let v = serde_json::to_value(&m).unwrap();
        let parts = v["content"].as_array().unwrap();
        assert_eq!(parts.len(), 1, "no empty text part");
        assert_eq!(parts[0]["type"], "image_url");
    }

    #[test]
    fn tool_result_message_shape() {
        let m = Message::tool_result("call_1", "the result");
        let v = serde_json::to_value(&m).unwrap();
        assert_eq!(v["role"], "tool");
        assert_eq!(v["tool_call_id"], "call_1");
        assert_eq!(v["content"], "the result");
        assert!(v.get("tool_calls").is_none(), "empty tool_calls omitted");
    }

    #[test]
    fn request_omits_tool_fields_when_no_tools() {
        let req = ChatRequest {
            model: "m".into(),
            messages: vec![Message::user("hi")],
            stream: false,
            temperature: None,
            max_tokens: None,
            tools: Vec::new(),
            tool_choice: None,
            parallel_tool_calls: None,
            stream_options: None,
            reasoning_effort: None,
        };
        let v = serde_json::to_value(&req).unwrap();
        assert!(v.get("tools").is_none(), "empty tools must be omitted");
        assert!(v.get("tool_choice").is_none());
        assert!(v.get("stream_options").is_none(), "omitted when None");
        assert!(v.get("max_tokens").is_none(), "omitted when None");
        assert_eq!(v["messages"][0]["content"], "hi");
    }

    #[test]
    fn request_serializes_tools() {
        let tool = ToolDef::function(
            "memory_search",
            "find a stored fact",
            serde_json::json!({"type":"object","properties":{"query":{"type":"string"}},"required":["query"],"additionalProperties":false}),
        );
        let req = ChatRequest {
            model: "m".into(),
            messages: vec![Message::user("hi")],
            stream: false,
            temperature: None,
            max_tokens: None,
            tools: vec![tool],
            tool_choice: Some("auto".into()),
            parallel_tool_calls: Some(true),
            stream_options: None,
            reasoning_effort: None,
        };
        let v = serde_json::to_value(&req).unwrap();
        assert_eq!(v["tools"][0]["type"], "function");
        assert_eq!(v["tools"][0]["function"]["name"], "memory_search");
        assert_eq!(v["tool_choice"], "auto");
        assert_eq!(v["parallel_tool_calls"], true);
        assert!(
            v["tools"][0].get("cache_control").is_none(),
            "no breakpoint unless stamped"
        );
    }

    #[test]
    fn message_omits_cache_control_when_none() {
        // Default messages must be byte-identical to before (no cache_control key) — the no-op
        // invariant for providers that don't support caching.
        let v = serde_json::to_value(Message::user("hi")).unwrap();
        assert!(v.get("cache_control").is_none());
        let v2 = serde_json::to_value(Message::system("sys")).unwrap();
        assert!(v2.get("cache_control").is_none());
    }

    #[test]
    fn message_emits_cache_control_when_set() {
        let mut m = Message::system("sys");
        m.cache_control = Some(CacheControl::ephemeral());
        let v = serde_json::to_value(&m).unwrap();
        assert_eq!(v["cache_control"]["type"], "ephemeral");
        assert_eq!(
            v["content"], "sys",
            "content still emitted alongside the breakpoint"
        );
    }

    #[test]
    fn tool_def_emits_cache_control_when_set() {
        let mut t = ToolDef::function("f", "d", serde_json::json!({"type":"object"}));
        assert!(serde_json::to_value(&t)
            .unwrap()
            .get("cache_control")
            .is_none());
        t.cache_control = Some(CacheControl::ephemeral());
        assert_eq!(
            serde_json::to_value(&t).unwrap()["cache_control"]["type"],
            "ephemeral"
        );
    }

    #[test]
    fn usage_cache_read_reads_either_shape() {
        let anthropic: Usage =
            serde_json::from_str(r#"{"prompt_tokens":10,"cache_read_input_tokens":7}"#).unwrap();
        assert_eq!(anthropic.cache_read(), 7);
        let openai: Usage = serde_json::from_str(
            r#"{"prompt_tokens":10,"prompt_tokens_details":{"cached_tokens":5}}"#,
        )
        .unwrap();
        assert_eq!(openai.cache_read(), 5);
        let none: Usage = serde_json::from_str(r#"{"prompt_tokens":10}"#).unwrap();
        assert_eq!(none.cache_read(), 0);
    }

    /// A message with vision attachments must survive its OWN serialization.
    ///
    /// It did not: `Serialize` wrote `content` as a multimodal parts array while `content` only
    /// deserialized as `Option<String>`, so every saved conversation containing a pasted image was
    /// rejected by its own reader and listed as "(unreadable)" in `/sessions` — permanently, with the
    /// serde error swallowed. Two real transcripts on the maintainer's machine were dead this way.
    #[test]
    fn message_with_images_round_trips_through_its_own_serializer() {
        let original = Message::user_with_images(
            "what is in this screenshot?",
            vec![
                "data:image/png;base64,iVBORw0KGgo=".to_string(),
                "data:image/jpeg;base64,/9j/4AAQ".to_string(),
            ],
        );
        let json = serde_json::to_string(&original).unwrap();
        // Guard the wire shape too: the provider contract is a parts array, and a "fix" that made
        // this test pass by writing a plain string would silently break vision requests.
        assert!(json.contains("image_url"), "wire shape is parts: {json}");

        let back: Message = serde_json::from_str(&json).expect("must read what it wrote");
        assert_eq!(back.role, "user");
        assert_eq!(back.content.as_deref(), Some("what is in this screenshot?"));
        assert_eq!(back.images, original.images);
    }

    /// The image-only case: no text part is emitted at all, so the array is images end to end.
    #[test]
    fn image_only_message_round_trips() {
        let original =
            Message::user_with_images("", vec!["data:image/png;base64,AAAA".to_string()]);
        let json = serde_json::to_string(&original).unwrap();
        let back: Message = serde_json::from_str(&json).unwrap();
        assert_eq!(back.content.as_deref(), Some(""));
        assert_eq!(back.images, vec!["data:image/png;base64,AAAA".to_string()]);
    }

    /// Plain and null `content` keep working, and an unknown part type is dropped rather than
    /// failing the whole message — the same "one new field makes a transcript unreadable" trap.
    #[test]
    fn plain_null_and_unknown_part_shapes_still_parse() {
        let plain: Message = serde_json::from_str(r#"{"role":"user","content":"hi"}"#).unwrap();
        assert_eq!(plain.content.as_deref(), Some("hi"));
        assert!(plain.images.is_empty());

        let null: Message = serde_json::from_str(r#"{"role":"assistant","content":null}"#).unwrap();
        assert_eq!(null.content, None);

        let exotic: Message = serde_json::from_str(
            r#"{"role":"user","content":[{"type":"text","text":"a"},{"type":"input_audio","input_audio":{"data":"x"}}]}"#,
        )
        .expect("an unknown part type must not sink the message");
        assert_eq!(exotic.content.as_deref(), Some("a"));
        assert!(exotic.images.is_empty());
    }
}
