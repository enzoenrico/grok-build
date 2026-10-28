//! Real hub client. One [`xai_computer_hub_sdk::ToolHarness::call`] per tool.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use futures::StreamExt;
use serde_json::{Value, json};
use url::Url;
use xai_computer_hub_sdk::{AuthCredential, HubConnectionPool, ToolHarness, ToolHarnessBuilder};
use xai_grok_login::AuthManager;
use xai_tool_protocol::{SessionId, ToolId};
use xai_tool_runtime::{ContentBlock, ToolCallContext, ToolError, ToolErrorKind, ToolStreamItem};

use super::hub::{AwaitedTurn, BotHub, HubBot, HubError, OnBusy, SendAccepted};
use super::session::LOGIN_HINT;
use super::{BotInput, BotSession, HubRun, SubmitOutcome};

/// Talk to the computer hub with the grok.com credential the shell already uses.
pub struct SdkBotHub {
    harness: ToolHarness,
}

impl SdkBotHub {
    pub async fn connect() -> Result<Self, HubError> {
        let token = grok_com_token().await?;
        let url = hub_url()?;
        let allow_insecure = url.scheme() == "ws";
        let session = SessionId::new(format!("pager-bot-{}", uuid::Uuid::new_v4()))
            .map_err(|error| HubError::Message(error.to_string()))?;
        let harness = ToolHarnessBuilder::default()
            .pool(HubConnectionPool::new())
            .url(url)
            .auth(AuthCredential::bearer(token))
            .session(session)
            .allow_insecure_ws(allow_insecure)
            .build()
            .await
            .map_err(classify_client_error)?;
        // Async sends finish on `bot_agent_turn_completed`. Auto-subscribe
        // omits that kind, so `bot_await_turn` never returns and the pager
        // paints nothing. Opt in, and keep the inbox alive for the call.
        enable_bot_turn_notifications(&harness).await;
        Ok(Self { harness })
    }

    async fn call_tool(&self, name: &str, args: Value) -> Result<Value, HubError> {
        let tool_id = ToolId::new(name).map_err(|error| HubError::Message(error.to_string()))?;
        let mut stream = self
            .harness
            .call(tool_id, args, ToolCallContext::default())
            .await;
        while let Some(item) = stream.next().await {
            match item {
                ToolStreamItem::Progress(_) => {}
                ToolStreamItem::Terminal(Ok(output)) => {
                    return Ok(value_or_model_text(output.value, &output.model_output));
                }
                ToolStreamItem::Terminal(Err(error)) => return Err(classify_tool_error(&error)),
            }
        }
        Err(HubError::Message(format!("{name} ended without a result")))
    }
}

#[async_trait]
impl BotHub for SdkBotHub {
    async fn list_agents(&self) -> Result<Vec<HubBot>, HubError> {
        let value = self.call_tool("bot_list_agents", json!({})).await?;
        bots_from_tool_result(&value)
    }

    async fn search_agents(&self, query: &str) -> Result<Vec<HubBot>, HubError> {
        let value = self
            .call_tool("bot_search_agents", json!({ "query": query }))
            .await?;
        bots_from_tool_result(&value)
    }

    async fn create_agent(&self, name: &str) -> Result<HubBot, HubError> {
        let value = self
            .call_tool("bot_create_agent", json!({ "name": name }))
            .await?;
        let payload = tool_payload(&value);
        let agent = payload.get("agent").cloned().unwrap_or(payload);
        bot_from_value(&agent)
    }

    async fn send_prompt(
        &self,
        agent_id: &str,
        prompt: &str,
        on_busy: OnBusy,
    ) -> Result<SendAccepted, HubError> {
        let value = self
            .call_tool(
                "bot_send_prompt",
                json!({
                    "agent_id": agent_id,
                    "prompt": prompt,
                    "mode": "async",
                    "on_busy": on_busy.as_wire(),
                }),
            )
            .await?;
        let value = tool_payload(&value);
        let handle = value.get("handle").cloned().unwrap_or(value);
        Ok(SendAccepted { handle })
    }

    async fn await_turn(
        &self,
        agent_id: &str,
        handle: Option<&Value>,
    ) -> Result<AwaitedTurn, HubError> {
        let mut args = json!({ "agent_id": agent_id });
        if let Some(handle) = handle
            && let Some(obj) = args.as_object_mut()
        {
            obj.insert("handle".to_string(), handle.clone());
        }
        let value = self.call_tool("bot_await_turn", args).await?;
        Ok(awaited_turn_from_tool_result(&value))
    }
}

/// Roster rows from one `bot_list_agents` / `bot_search_agents` tool result.
pub(crate) fn bots_from_tool_result(value: &Value) -> Result<Vec<HubBot>, HubError> {
    bots_from_value(&tool_payload(value))
}

/// One `bot_await_turn` tool result.
pub(crate) fn awaited_turn_from_tool_result(value: &Value) -> AwaitedTurn {
    parse_await(&tool_payload(value))
}

/// Ask the hub to deliver turn-finished notifications on this connection.
async fn enable_bot_turn_notifications(harness: &ToolHarness) {
    let Ok(mut notifications) = harness.subscribe_notifications().await else {
        return;
    };
    let opt_in =
        harness.opt_in_notification_kinds(&[xai_tool_protocol::HUB_KIND_BOT_AGENT_TURN_COMPLETED]);
    match tokio::time::timeout(Duration::from_secs(5), opt_in).await {
        Ok(Ok(())) => {}
        Ok(Err(error)) => {
            tracing::debug!(%error, "bot turn notifications were not enabled");
        }
        Err(_elapsed) => {
            tracing::debug!("bot turn notification opt-in timed out");
        }
    }
    tokio::spawn(async move { while notifications.recv().await.is_some() {} });
}

/// Open the hub once and run one `/bot` line or one plain prompt.
pub async fn run_with_hub(mut session: BotSession, input: BotInput) -> HubRun {
    match SdkBotHub::connect().await {
        Ok(hub) => {
            let outcome = session.submit(input, &hub).await;
            HubRun { session, outcome }
        }
        Err(HubError::NeedsLogin) => HubRun {
            session,
            outcome: SubmitOutcome::Shown(LOGIN_HINT.to_string()),
        },
        Err(error) => HubRun {
            session,
            outcome: SubmitOutcome::Shown(error.to_string()),
        },
    }
}

async fn grok_com_token() -> Result<String, HubError> {
    let manager = auth_manager()?;
    if manager.current().is_none() {
        let _ = manager.auth().await;
    }
    manager
        .current()
        .map(|auth| auth.key)
        .filter(|key| !key.is_empty())
        .ok_or(HubError::NeedsLogin)
}

fn auth_manager() -> Result<Arc<AuthManager>, HubError> {
    let raw = xai_grok_shell::config::load_effective_config()
        .map_err(|error| HubError::Message(format!("could not load config: {error}")))?;
    let config = xai_grok_shell::agent::config::Config::new_from_toml_cfg(&raw)
        .map_err(HubError::Message)?;
    let manager = Arc::new(AuthManager::new_with_proxy_base_url(
        &xai_grok_shell::util::grok_home::grok_home(),
        config.grok_com_config.clone(),
        config.endpoints.proxy_url(),
    ));
    manager.configure_refresher(config.grok_com_config.auth_provider_command.clone(), None);
    Ok(manager)
}

fn hub_url() -> Result<Url, HubError> {
    let raw = std::env::var("GROK_COMPUTER_HUB_URL")
        .unwrap_or_else(|_| xai_grok_shell::env::PROD_COMPUTER_HUB_WS_URL.to_string());
    let with_role = if raw.contains('?') {
        raw
    } else {
        format!("{raw}?role=harness")
    };
    Url::parse(with_role.as_str()).map_err(|error| HubError::Message(format!("hub url: {error}")))
}

fn classify_client_error(error: xai_computer_hub_sdk::ClientError) -> HubError {
    let text = error.to_string();
    if looks_like_auth(text.as_str()) {
        HubError::NeedsLogin
    } else {
        HubError::Message(text)
    }
}

fn classify_tool_error(error: &ToolError) -> HubError {
    if error.kind == ToolErrorKind::Unauthorized || looks_like_auth(error.detail.as_str()) {
        return HubError::NeedsLogin;
    }
    if error.kind == ToolErrorKind::Timeout {
        return HubError::TimedOut;
    }
    let lower = error.detail.to_ascii_lowercase();
    if lower.contains("busy") || lower.contains("in flight") {
        return HubError::Busy;
    }
    HubError::Message(error.detail.clone())
}

fn looks_like_auth(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    lower.contains("unauthor")
        || lower.contains("not logged")
        || lower.contains("login required")
        || lower.contains("401")
}

fn bots_from_value(value: &Value) -> Result<Vec<HubBot>, HubError> {
    let list = value
        .get("agents")
        .or_else(|| value.get("bots"))
        .or_else(|| value.as_array().map(|_| value))
        .and_then(Value::as_array)
        .ok_or_else(|| HubError::Message("bot list was not a list".to_string()))?;
    list.iter().map(bot_from_value).collect()
}

fn bot_from_value(value: &Value) -> Result<HubBot, HubError> {
    let id = non_empty_str(value, &["id", "agentId", "agent_id"])
        .ok_or_else(|| HubError::Message("bot is missing an id".to_string()))?;
    let name = value
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or(id)
        .to_string();
    Ok(HubBot {
        id: id.to_string(),
        name,
        description: value
            .get("description")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
        status: value
            .get("status")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
    })
}

fn parse_await(value: &Value) -> AwaitedTurn {
    let finished = value
        .get("finished")
        .and_then(Value::as_bool)
        .or_else(|| {
            value
                .get("event")
                .and_then(|event| event.get("finished"))
                .and_then(Value::as_bool)
        })
        .unwrap_or(true);
    if !finished {
        return AwaitedTurn::Timeout {
            handle: value.get("handle").cloned().unwrap_or(Value::Null),
        };
    }
    AwaitedTurn::Finished {
        reply: reply_text(value),
    }
}

fn reply_text(value: &Value) -> String {
    const KEYS: &[&str] = &[
        "reply",
        "text",
        "message",
        "content",
        "last_message",
        "preview",
    ];
    if let Some(text) = KEYS.iter().find_map(|key| non_empty_str(value, &[key])) {
        return text.to_string();
    }
    if let Some(text) = block_text(value).filter(|text| !text.is_empty()) {
        return text;
    }
    for key in ["event", "result"] {
        if let Some(nested) = value.get(key) {
            let text = reply_text(nested);
            if !text.is_empty() {
                return text;
            }
        }
    }
    String::new()
}

fn non_empty_str<'a>(value: &'a Value, keys: &[&str]) -> Option<&'a str> {
    keys.iter()
        .find_map(|key| value.get(*key).and_then(Value::as_str))
        .filter(|text| !text.is_empty())
}

/// The harness returns MCP text blocks, or a JSON string, not the roster object.
fn tool_payload(value: &Value) -> Value {
    unwrap_payload(value, 0)
}

fn unwrap_payload(value: &Value, depth: usize) -> Value {
    if depth > 4 {
        return value.clone();
    }
    if let Some(text) = value.as_str() {
        return unwrap_text(text, depth);
    }
    if let Some(text) = block_text(value) {
        return unwrap_text(text.as_str(), depth);
    }
    value.clone()
}

fn unwrap_text(text: &str, depth: usize) -> Value {
    let trimmed = text.trim();
    if let Ok(parsed) = serde_json::from_str::<Value>(trimmed)
        && (parsed.is_object() || parsed.is_array())
    {
        return unwrap_payload(&parsed, depth + 1);
    }
    if trimmed.is_empty() {
        return Value::String(String::new());
    }
    json!({ "reply": trimmed, "finished": true })
}

fn block_text(value: &Value) -> Option<String> {
    let blocks = value.get("blocks").or_else(|| value.get("content"))?;
    let items = blocks.as_array()?;
    let mut parts = Vec::new();
    for item in items {
        if let Some(text) = item.as_str() {
            if !text.is_empty() {
                parts.push(text.to_string());
            }
            continue;
        }
        if let Some(text) = item.get("text").and_then(Value::as_str)
            && !text.is_empty()
        {
            parts.push(text.to_string());
        }
    }
    if parts.is_empty() {
        None
    } else {
        Some(parts.join("\n"))
    }
}

fn value_or_model_text(value: Value, blocks: &[ContentBlock]) -> Value {
    if !value.is_null() {
        return value;
    }
    let text = blocks
        .iter()
        .filter_map(content_block_text)
        .collect::<Vec<_>>()
        .join("\n");
    if text.trim().is_empty() {
        value
    } else {
        Value::String(text)
    }
}

fn content_block_text(block: &ContentBlock) -> Option<&str> {
    match block {
        ContentBlock::Text { text } => Some(text.as_str()),
        ContentBlock::Resource { text, .. } => text.as_deref(),
        ContentBlock::Image { .. } => None,
    }
}
