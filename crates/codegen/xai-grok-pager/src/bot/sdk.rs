//! Real hub client. One [`xai_computer_hub_sdk::ToolHarness::call`] per tool.

use std::sync::Arc;

use async_trait::async_trait;
use futures::StreamExt;
use serde_json::{Value, json};
use url::Url;
use xai_computer_hub_sdk::{AuthCredential, HubConnectionPool, ToolHarness, ToolHarnessBuilder};
use xai_grok_login::AuthManager;
use xai_tool_protocol::{SessionId, ToolId};
use xai_tool_runtime::{ToolCallContext, ToolError, ToolErrorKind, ToolStreamItem};

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
                ToolStreamItem::Terminal(Ok(output)) => return Ok(output.value),
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
        bots_from_value(&value)
    }

    async fn search_agents(&self, query: &str) -> Result<Vec<HubBot>, HubError> {
        let value = self
            .call_tool("bot_search_agents", json!({ "query": query }))
            .await?;
        bots_from_value(&value)
    }

    async fn create_agent(&self, name: &str) -> Result<HubBot, HubError> {
        let value = self
            .call_tool("bot_create_agent", json!({ "name": name }))
            .await?;
        bot_from_value(value.get("agent").unwrap_or(&value))
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
        Ok(parse_await(&value))
    }
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
    list.iter()
        .cloned()
        .map(|item| bot_from_value(&item))
        .collect()
}

fn bot_from_value(value: &Value) -> Result<HubBot, HubError> {
    let id = value
        .get("id")
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty())
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
        .unwrap_or(true);
    if !finished {
        return AwaitedTurn::Timeout {
            handle: value.get("handle").cloned().unwrap_or(Value::Null),
        };
    }
    let reply = ["reply", "text", "message", "content", "last_message"]
        .iter()
        .find_map(|key| value.get(*key).and_then(Value::as_str))
        .unwrap_or("")
        .to_string();
    AwaitedTurn::Finished { reply }
}
