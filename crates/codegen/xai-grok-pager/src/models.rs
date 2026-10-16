//! `grok models` subcommand.

use agent_client_protocol as acp;
use anyhow::Result;
use tokio_util::sync::CancellationToken;
use xai_grok_shell::agent::config::Config as AgentConfig;
use xai_grok_shell::cli_models::{AuthStatus, list_models};
use xai_grok_shell::sampling::types::{
    parse_context_windows_meta, parse_reasoning_efforts_meta, supports_fast_mode_meta,
    supports_max_mode_meta, supports_reasoning_effort_meta,
};

use crate::client_identity::{PAGER_CLIENT_TYPE, PAGER_CLIENT_VERSION};
use crate::slash::commands::context_window::format_window;

pub async fn list_available_models(agent_config: &AgentConfig) -> Result<()> {
    match AuthStatus::resolve(agent_config) {
        AuthStatus::ApiKey => println!("You are using XAI_API_KEY."),
        AuthStatus::LoggedIn(host) => println!("You are logged in with {}.", host),
        AuthStatus::ModelCredentials(model) => {
            println!("Model '{model}' is using its own API key.");
        }
        AuthStatus::DeploymentKey => println!("You are authenticated via deployment key."),
        AuthStatus::NotAuthenticated => println!("You are not authenticated."),
    }
    println!();

    let cancel = CancellationToken::new();
    xai_grok_telemetry::startup::mark_utility_process();
    let spawned = crate::acp::spawn::spawn_grok_shell(agent_config.clone(), &cancel, None).await?;
    // Cancel and join on every return path, including the `?` below
    let _agent_guard =
        crate::acp::spawn::AgentShutdownGuard::new(cancel.clone(), Some(spawned.thread_handle));

    let state = list_models(&spawned.channel.tx, PAGER_CLIENT_TYPE, PAGER_CLIENT_VERSION).await?;

    println!("Default model: {}", state.current_model_id.0);
    println!();
    println!("Available models:");
    for m in state.available_models {
        println!("{}", format_model_entry(&m, &state.current_model_id));
    }

    Ok(())
}

pub(crate) fn format_model_entry(m: &acp::ModelInfo, default_id: &acp::ModelId) -> String {
    let prefix = if &m.model_id == default_id {
        "  * "
    } else {
        "  - "
    };
    let default_tag = if &m.model_id == default_id {
        " (default)"
    } else {
        ""
    };
    let capabilities = format_model_capabilities(m.meta.as_ref());
    if capabilities.is_empty() {
        format!("{prefix}{}{default_tag}", m.model_id.0)
    } else {
        format!("{prefix}{}{default_tag} [{capabilities}]", m.model_id.0)
    }
}

fn format_model_capabilities(meta: Option<&acp::Meta>) -> String {
    let Some(meta) = meta else {
        return String::new();
    };
    let mut parts = Vec::new();
    if let Some(windows) = parse_context_windows_meta(Some(meta))
        && !windows.is_empty()
    {
        let formatted: Vec<String> = windows
            .into_iter()
            .map(|w| format_window(w.get()))
            .collect();
        parts.push(format!("context: {}", formatted.join("/")));
    } else if let Some(total) = meta.get("totalContextTokens").and_then(|v| v.as_u64())
        && total > 0
    {
        parts.push(format!("context: {}", format_window(total)));
    }
    if supports_reasoning_effort_meta(Some(meta)) {
        if let Some(efforts) = parse_reasoning_efforts_meta(Some(meta))
            && !efforts.is_empty()
        {
            let ids: Vec<&str> = efforts.iter().map(|e| e.id.as_str()).collect();
            parts.push(format!("effort: {}", ids.join("/")));
        } else {
            parts.push("effort".to_string());
        }
    }
    if supports_fast_mode_meta(Some(meta)) {
        parts.push("fast".to_string());
    }
    if supports_max_mode_meta(Some(meta)) {
        parts.push("max-mode".to_string());
    }
    parts.join(", ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[test]
    fn format_model_entry_shows_capabilities_when_present() {
        let default_id = acp::ModelId::new(Arc::from("claude-opus-4-6"));
        let plain = acp::ModelInfo::new(default_id.clone(), "Claude 4.6 Opus");
        assert_eq!(
            format_model_entry(&plain, &default_id),
            "  * claude-opus-4-6 (default)"
        );

        let mut meta = serde_json::Map::new();
        meta.insert(
            "contextWindows".into(),
            serde_json::json!([200_000, 1_000_000]),
        );
        meta.insert(
            "supportsReasoningEffort".into(),
            serde_json::Value::Bool(true),
        );
        meta.insert(
            "reasoningEfforts".into(),
            serde_json::json!([
                {"id": "low", "value": "low", "label": "Low", "description": "Fast"},
                {"id": "high", "value": "high", "label": "High", "description": "Deep"}
            ]),
        );
        meta.insert("supportsFastMode".into(), serde_json::Value::Bool(true));
        let other_id = acp::ModelId::new(Arc::from("grok-4.7"));
        let rich = acp::ModelInfo::new(other_id, "Grok 4.7").meta(Some(meta));
        assert_eq!(
            format_model_entry(&rich, &default_id),
            "  - grok-4.7 [context: 200k/1m, effort: low/high, fast]"
        );
    }
}
