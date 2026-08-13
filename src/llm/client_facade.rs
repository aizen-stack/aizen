//! Compatibility facade for model transports.
//!
//! Everything remains OpenAI-chat-completions by default. The exact ChatGPT Codex backend is the
//! one exception: it requires OAuth + Responses API, so those calls are routed through `chatgpt`.

pub use super::legacy_client::*;

use anyhow::Result;
use crate::core::types::{Message, ToolDef};
use super::{chatgpt, legacy_client};

pub async fn chat_with_tools(
    client: &reqwest::Client,
    base_url: &str,
    api_key: &str,
    model: &str,
    messages: &[Message],
    tools: &[ToolDef],
) -> Result<ChatTurn> {
    let effort = crate::core::cli_config::resolved_reasoning_effort(
        crate::core::cli_config::load().reasoning_effort,
    );
    chat_with_tools_effort(client, base_url, api_key, model, messages, tools, effort).await
}

#[allow(clippy::too_many_arguments)]
pub async fn chat_with_tools_effort(
    client: &reqwest::Client,
    base_url: &str,
    api_key: &str,
    model: &str,
    messages: &[Message],
    tools: &[ToolDef],
    effort: Option<String>,
) -> Result<ChatTurn> {
    if chatgpt::is_chatgpt_base(base_url) {
        chatgpt::chat_turn(client, base_url, model, messages, tools, effort, false).await
    } else {
        legacy_client::chat_with_tools_effort(client, base_url, api_key, model, messages, tools, effort).await
    }
}

pub async fn stream_chat_with_tools(
    client: &reqwest::Client,
    base_url: &str,
    api_key: &str,
    model: &str,
    messages: &[Message],
    tools: &[ToolDef],
) -> Result<ChatTurn> {
    stream_chat_with_tools_eager(client, base_url, api_key, model, messages, tools, None).await
}

pub async fn stream_chat_with_tools_eager(
    client: &reqwest::Client,
    base_url: &str,
    api_key: &str,
    model: &str,
    messages: &[Message],
    tools: &[ToolDef],
    eager_hook: Option<EagerStartFn<'_>>,
) -> Result<ChatTurn> {
    if chatgpt::is_chatgpt_base(base_url) {
        let effort = crate::core::cli_config::resolved_reasoning_effort(
            crate::core::cli_config::load().reasoning_effort,
        );
        // The Responses adapter currently finalizes function calls from output-item events. It keeps
        // semantics correct but deliberately skips eager execution until argument-delta parity lands.
        let _ = eager_hook;
        chatgpt::chat_turn(client, base_url, model, messages, tools, effort, true).await
    } else {
        legacy_client::stream_chat_with_tools_eager(client, base_url, api_key, model, messages, tools, eager_hook).await
    }
}

pub async fn stream_chat_with_visual_contract(
    client: &reqwest::Client,
    base_url: &str,
    api_key: &str,
    model: &str,
    mut messages: Vec<Message>,
    visual_contract: bool,
) -> Result<String> {
    if !chatgpt::is_chatgpt_base(base_url) {
        return legacy_client::stream_chat_with_visual_contract(client, base_url, api_key, model, messages, visual_contract).await;
    }
    if visual_contract {
        if let Some(block) = crate::agent::response_visuals_prompt_block(
            crate::core::cli_config::load().response_visuals(),
        ) {
            messages.insert(0, Message::system(block));
        }
    }
    let effort = crate::core::cli_config::resolved_reasoning_effort(
        crate::core::cli_config::load().reasoning_effort,
    );
    let turn = chatgpt::chat_turn(client, base_url, model, &messages, &[], effort, true).await?;
    Ok(turn.content.unwrap_or_default())
}

pub async fn fetch_models_info(
    client: &reqwest::Client,
    base_url: &str,
    api_key: &str,
) -> Result<Vec<ModelInfo>> {
    if chatgpt::is_chatgpt_base(base_url) {
        chatgpt::models(client, base_url).await
    } else {
        legacy_client::fetch_models_info(client, base_url, api_key).await
    }
}

pub async fn probe_models(client: &reqwest::Client, base_url: &str, api_key: &str) -> Result<()> {
    if chatgpt::is_chatgpt_base(base_url) {
        chatgpt::models(client, base_url).await.map(|_| ())
    } else {
        legacy_client::probe_models(client, base_url, api_key).await
    }
}

pub async fn check_endpoint(
    client: &reqwest::Client,
    base_url: &str,
    api_key: Option<&str>,
) -> EndpointCheck {
    if chatgpt::is_chatgpt_base(base_url) {
        return match chatgpt::models(client, base_url).await {
            Ok(models) => EndpointCheck::Ok(models),
            Err(e) => EndpointCheck::Auth(e.to_string()),
        };
    }
    legacy_client::check_endpoint(client, base_url, api_key).await
}
