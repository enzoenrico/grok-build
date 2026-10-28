//! Grok Bot over its HTTP gateway, the transport the MIT grok-bot-cli uses:
//! `POST {gateway}/api/<method>` with a bearer token.
//!
//! Credentials come only from `GROK_BOT_GATEWAY_URL` and `GROK_BOT_GATEWAY_TOKEN`,
//! the explicit override grok-bot-cli documents. Its other two sources, the desktop
//! app's stored session and `CURSOR_ACCESS_TOKEN` through `EnsureSandBox`, reuse
//! credentials minted for another app and are not read here.
//! See `docs/grokbot-integration.md`.

use std::time::Duration;

use async_trait::async_trait;
use serde_json::{Value, json};
use url::Url;

use super::hub::{AwaitedTurn, BotHub, HubBot, HubError, OnBusy, SendAccepted};

pub const GATEWAY_URL_ENV: &str = "GROK_BOT_GATEWAY_URL";
pub const GATEWAY_TOKEN_ENV: &str = "GROK_BOT_GATEWAY_TOKEN";
pub const ALLOW_LOCAL_GATEWAY_ENV: &str = "GROK_BOT_ALLOW_LOCAL_GATEWAY";

pub const MISSING_CREDENTIALS: &str =
    "Set GROK_BOT_GATEWAY_URL and GROK_BOT_GATEWAY_TOKEN to talk to Grok Bot over its gateway.";

const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
const TRANSCRIPT_LIMIT: u64 = 200;
const DETAIL_CHARS: usize = 300;

/// A gateway base URL that passed the host policy, and the token that may be sent to it.
#[derive(Clone)]
pub struct GatewayCredentials {
    base: String,
    token: String,
}

impl std::fmt::Debug for GatewayCredentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GatewayCredentials")
            .field("base", &self.base)
            .finish_non_exhaustive()
    }
}

impl GatewayCredentials {
    pub fn from_env() -> Result<Self, HubError> {
        let url = std::env::var(GATEWAY_URL_ENV).ok();
        let token = std::env::var(GATEWAY_TOKEN_ENV).ok();
        let allow_local = std::env::var(ALLOW_LOCAL_GATEWAY_ENV)
            .is_ok_and(|value| matches!(value.trim(), "1" | "true" | "yes"));
        Self::parse(url.as_deref(), token.as_deref(), allow_local)
    }

    /// Same host policy as grok-bot-cli: https on a Cursor gateway host, or loopback when allowed.
    pub fn parse(
        url: Option<&str>,
        token: Option<&str>,
        allow_local: bool,
    ) -> Result<Self, HubError> {
        let (Some(url), Some(token)) = (nonempty(url), nonempty(token)) else {
            return Err(HubError::Message(MISSING_CREDENTIALS.to_string()));
        };
        let parsed =
            Url::parse(url).map_err(|_| HubError::Message("Invalid gateway URL.".to_string()))?;
        check_host(&parsed, allow_local).map_err(HubError::Message)?;
        Ok(Self {
            base: url.trim_end_matches('/').to_string(),
            token: token.to_string(),
        })
    }
}

fn check_host(url: &Url, allow_local: bool) -> Result<(), String> {
    if !url.username().is_empty() || url.password().is_some() {
        return Err("Rejected gateway URL: userinfo is not allowed.".to_string());
    }
    let host = url.host_str().unwrap_or("").to_ascii_lowercase();
    let host = host.trim_start_matches('[').trim_end_matches(']');
    if matches!(host, "localhost" | "127.0.0.1" | "::1") {
        if !allow_local {
            return Err(format!(
                "Rejected gateway URL host \"{host}\". Set {ALLOW_LOCAL_GATEWAY_ENV}=1 to permit localhost/127.0.0.1 gateways."
            ));
        }
        if !matches!(url.scheme(), "http" | "https") {
            return Err("Rejected gateway URL: local gateways must use http or https.".to_string());
        }
        return Ok(());
    }
    if url.scheme() != "https" {
        return Err(format!(
            "Rejected gateway URL: only https is allowed (got {}:).",
            url.scheme()
        ));
    }
    let cursor_host = ["cursor.sh", "cursor.com"].contains(&host)
        || [".cursor.sh", ".cursor.com", ".cursorvm.com"]
            .iter()
            .any(|suffix| host.ends_with(suffix));
    if !cursor_host {
        return Err(format!(
            "Rejected gateway URL host \"{host}\". Expected *.cursor.sh / *.cursor.com / *.cursorvm.com."
        ));
    }
    Ok(())
}

/// A failed gateway call. `status` is `None` when no HTTP response arrived.
struct CallFailure {
    status: Option<u16>,
    message: String,
}

impl From<CallFailure> for HubError {
    fn from(failure: CallFailure) -> Self {
        HubError::Message(failure.message)
    }
}

/// [`BotHub`] over the Grok Bot gateway. A turn is read back from the transcript tail.
pub struct GatewayBotHub {
    client: reqwest::Client,
    credentials: GatewayCredentials,
    timing: WaitTiming,
}

/// How [`GatewayBotHub::await_turn`] polls the transcript tail.
#[derive(Clone, Copy, Debug)]
pub(crate) struct WaitTiming {
    pub poll_every: Duration,
    /// Replies unchanged this long end the turn. The gateway has no turn-finished signal.
    pub settle: Duration,
    pub budget: Duration,
}

impl GatewayBotHub {
    pub fn new(credentials: GatewayCredentials) -> Result<Self, HubError> {
        Self::with_timing(
            credentials,
            WaitTiming {
                poll_every: Duration::from_secs(1),
                settle: Duration::from_secs(3),
                budget: Duration::from_secs(60),
            },
        )
    }

    pub(crate) fn with_timing(
        credentials: GatewayCredentials,
        timing: WaitTiming,
    ) -> Result<Self, HubError> {
        // No redirects, so the bearer token never follows one to another host.
        let client = xai_grok_extra_ca::build_reqwest_client(|builder| {
            builder
                .redirect(reqwest::redirect::Policy::none())
                .timeout(REQUEST_TIMEOUT)
        })
        .map_err(|error| HubError::Message(format!("gateway client: {error}")))?;
        Ok(Self {
            client,
            credentials,
            timing,
        })
    }

    async fn call(&self, method: &str, body: Value) -> Result<Value, CallFailure> {
        let url = format!("{}/api/{method}", self.credentials.base);
        let response = self
            .client
            .post(url)
            .bearer_auth(&self.credentials.token)
            .json(&body)
            .send()
            .await
            .map_err(|error| CallFailure {
                status: None,
                message: format!("{method} failed: {}", self.redact(&error.to_string())),
            })?;
        let status = response.status();
        let text = response.text().await.map_err(|error| CallFailure {
            status: None,
            message: format!("{method} failed: {}", self.redact(&error.to_string())),
        })?;
        if !status.is_success() {
            let detail = error_detail(&text)
                .unwrap_or_else(|| status.canonical_reason().unwrap_or("").to_string());
            let detail: String = self.redact(&detail).chars().take(DETAIL_CHARS).collect();
            return Err(CallFailure {
                status: Some(status.as_u16()),
                message: format!("{method} failed: {} {detail}", status.as_u16()),
            });
        }
        if text.trim().is_empty() {
            return Ok(json!({}));
        }
        Ok(serde_json::from_str(&text).unwrap_or_else(|_| json!({ "raw": text })))
    }

    fn redact(&self, text: &str) -> String {
        text.replace(self.credentials.token.as_str(), "[redacted]")
    }
}

#[async_trait]
impl BotHub for GatewayBotHub {
    async fn list_agents(&self) -> Result<Vec<HubBot>, HubError> {
        let value = self.call("listAgents", json!({})).await?;
        Ok(agents_from_value(&value))
    }

    /// The gateway has no search. Like grok-bot-cli, match a name or id ignoring case.
    async fn search_agents(&self, query: &str) -> Result<Vec<HubBot>, HubError> {
        let needle = query.trim().to_lowercase();
        let bots = self.list_agents().await?;
        Ok(bots
            .into_iter()
            .filter(|bot| bot.name.to_lowercase() == needle || bot.id.to_lowercase() == needle)
            .collect())
    }

    async fn create_agent(&self, name: &str) -> Result<HubBot, HubError> {
        let value = self
            .call(
                "createAgent",
                json!({
                    "name": name,
                    "description": "",
                    "title": "",
                    "avatarShape": "",
                    "avatarColor": "",
                    "origin": "user",
                }),
            )
            .await?;
        let agent = value.get("agent").unwrap_or(&value);
        agent_from_value(agent)
            .ok_or_else(|| HubError::Message("createAgent returned no agent id".to_string()))
    }

    /// The gateway has no `on_busy`. A prompt to a busy bot is queued by Grok Bot itself.
    async fn send_prompt(
        &self,
        agent_id: &str,
        prompt: &str,
        _on_busy: OnBusy,
    ) -> Result<SendAccepted, HubError> {
        let client_nonce = uuid::Uuid::new_v4().to_string();
        let body = json!({
            "agentId": agent_id,
            "prompt": prompt,
            "clientNonce": client_nonce,
        });
        match self.call("sendPrompt", body).await {
            Ok(_) => Ok(SendAccepted {
                handle: json!({ "clientNonce": client_nonce }),
            }),
            // A 4xx is a refusal. Anything else may still have landed.
            Err(failure)
                if failure
                    .status
                    .is_some_and(|status| (400..500).contains(&status)) =>
            {
                Err(failure.into())
            }
            Err(failure) => Err(HubError::Message(format!(
                "{} (delivery unknown; check the thread before resending)",
                failure.message
            ))),
        }
    }

    /// Polls the transcript tail until the replies to this prompt stop changing.
    ///
    /// A 5xx or transport failure mid-wait keeps the handle, so the next line resumes the wait.
    async fn await_turn(
        &self,
        agent_id: &str,
        handle: Option<&Value>,
    ) -> Result<AwaitedTurn, HubError> {
        let Some(client_nonce) = handle
            .and_then(|handle| handle.get("clientNonce"))
            .and_then(Value::as_str)
        else {
            return Err(HubError::Message(
                "No Grok Bot prompt is waiting for a reply.".to_string(),
            ));
        };
        let pending = AwaitedTurn::Timeout {
            handle: json!({ "clientNonce": client_nonce }),
        };
        let started = tokio::time::Instant::now();
        let mut replies: Vec<String> = Vec::new();
        let mut changed_at = started;
        loop {
            let tail = match self
                .call(
                    "getAgentTranscriptTail",
                    json!({ "id": agent_id, "limit": TRANSCRIPT_LIMIT }),
                )
                .await
            {
                Ok(tail) => tail,
                Err(failure) if failure.status.is_some_and(|status| status < 500) => {
                    return Err(failure.into());
                }
                Err(_) => return Ok(pending),
            };
            let now = tokio::time::Instant::now();
            let latest = turn_replies(&tail, client_nonce);
            if latest != replies {
                replies = latest;
                changed_at = now;
            }
            if !replies.is_empty() && now.duration_since(changed_at) >= self.timing.settle {
                return Ok(AwaitedTurn::Finished {
                    reply: replies.join("\n\n"),
                });
            }
            if now.duration_since(started) + self.timing.poll_every > self.timing.budget {
                return Ok(pending);
            }
            tokio::time::sleep(self.timing.poll_every).await;
        }
    }
}

/// Bot replies to the prompt sent with `client_nonce`.
///
/// The prompt's entry carries the nonce and a `requestId`. The bot's
/// `send-message` entries for that prompt carry the same `requestId`.
fn turn_replies(tail: &Value, client_nonce: &str) -> Vec<String> {
    let Some(entries) = tail.get("entries").and_then(Value::as_array) else {
        return Vec::new();
    };
    let Some(request_id) = entries
        .iter()
        .find(|entry| entry.get("clientNonce").and_then(Value::as_str) == Some(client_nonce))
        .and_then(|entry| entry.get("requestId"))
        .and_then(Value::as_str)
    else {
        return Vec::new();
    };
    entries
        .iter()
        .filter(|entry| entry.get("kind").and_then(Value::as_str) == Some("send-message"))
        .filter(|entry| entry.get("requestId").and_then(Value::as_str) == Some(request_id))
        .filter_map(|entry| reply_text(entry.get("message")?))
        .collect()
}

/// A text reply, or a note for an approval card so a turn that ends on one still finishes.
fn reply_text(message: &Value) -> Option<String> {
    if let Some(text) = message.get("content").and_then(Value::as_str)
        && !text.is_empty()
    {
        return Some(text.to_string());
    }
    match message.get("type").and_then(Value::as_str)? {
        "auto-review-approval" | "local-tool-permission" => {
            Some("Waiting for your approval. Answer it in Grok Bot.".to_string())
        }
        _ => None,
    }
}

fn agents_from_value(value: &Value) -> Vec<HubBot> {
    let list = value
        .as_array()
        .or_else(|| value.get("agents").and_then(Value::as_array));
    match (list, value.get("agent")) {
        (Some(list), _) => list.iter().filter_map(agent_from_value).collect(),
        (None, Some(agent)) => agent_from_value(agent).into_iter().collect(),
        (None, None) => Vec::new(),
    }
}

fn agent_from_value(value: &Value) -> Option<HubBot> {
    let text = |key: &str| value.get(key).and_then(Value::as_str).unwrap_or("");
    let id = [text("id"), text("agentId")]
        .into_iter()
        .find(|id| !id.is_empty())?;
    let is_group = value.get("isGroup").and_then(Value::as_bool) == Some(true);
    Some(HubBot {
        id: id.to_string(),
        name: if text("name").is_empty() {
            id.to_string()
        } else {
            text("name").to_string()
        },
        description: text("description").to_string(),
        status: if is_group { "group" } else { "" }.to_string(),
    })
}

/// `message`, then `error`, then the raw body, as grok-bot-cli reports it.
fn error_detail(body: &str) -> Option<String> {
    let trimmed = body.trim();
    if trimmed.is_empty() {
        return None;
    }
    let Ok(value) = serde_json::from_str::<Value>(trimmed) else {
        return Some(trimmed.to_string());
    };
    ["message", "error"]
        .iter()
        .find_map(|key| match value.get(*key)? {
            Value::String(text) if !text.is_empty() => Some(text.clone()),
            Value::String(_) | Value::Null => None,
            other => Some(other.to_string()),
        })
}

fn nonempty(value: Option<&str>) -> Option<&str> {
    value.map(str::trim).filter(|text| !text.is_empty())
}
