//! HTTP client for a GrokBot host gateway.
//!
//! `GET /health` is open. `POST /api/listAgents` and `POST /api/sendPrompt`
//! send `Authorization: Bearer`. This module does not read `gateway.json`.

use std::time::Duration;

use serde::{Deserialize, Serialize};

pub const MISSING_URL_LINE: &str = "GrokBot HTTP is missing GROKBOT_GATEWAY_URL.";
pub const MISSING_URL_NEED: &str = "Set GROKBOT_GATEWAY_URL to the gateway base URL, for example http://host:1340. SAND_GATEWAY_URL is accepted when GROKBOT_GATEWAY_URL is unset.";
pub const MISSING_TOKEN_LINE: &str = "GrokBot HTTP is missing SAND_GATEWAY_TOKEN.";
pub const MISSING_TOKEN_NEED: &str =
    "Set SAND_GATEWAY_TOKEN to the bearer token the gateway owner issued for this CLI.";

const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GatewayConfig {
    pub base_url: Option<String>,
    pub token: Option<String>,
}

impl GatewayConfig {
    pub fn from_env() -> Self {
        Self::from_parts(
            std::env::var("GROKBOT_GATEWAY_URL").ok(),
            std::env::var("SAND_GATEWAY_URL").ok(),
            std::env::var("SAND_GATEWAY_TOKEN").ok(),
        )
    }

    pub fn from_parts(url: Option<String>, alias: Option<String>, token: Option<String>) -> Self {
        let base_url = nonempty(url)
            .or_else(|| nonempty(alias))
            .map(|value| value.trim_end_matches('/').to_string());
        Self {
            base_url,
            token: nonempty(token),
        }
    }
}

fn nonempty(value: Option<String>) -> Option<String> {
    value.and_then(|raw| {
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            None
        } else {
            Some(trimmed.to_string())
        }
    })
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GrokBotAgent {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub is_running: bool,
    #[serde(default)]
    pub is_active: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GatewayHealth {
    pub ok: bool,
    #[serde(default)]
    pub pid: Option<u32>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GrokBotError {
    MissingConfig {
        need_url: bool,
        need_token: bool,
    },
    BadUrl {
        url: String,
    },
    Http {
        endpoint: String,
        status: u16,
        message: String,
    },
    Transport {
        endpoint: String,
        message: String,
    },
    Rejected {
        message: &'static str,
    },
}

impl std::fmt::Display for GrokBotError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::MissingConfig { need_url, need_token } => {
                if *need_url {
                    writeln!(f, "{MISSING_URL_LINE}")?;
                    writeln!(f, "{MISSING_URL_NEED}")?;
                }
                if *need_token {
                    writeln!(f, "{MISSING_TOKEN_LINE}")?;
                    write!(f, "{MISSING_TOKEN_NEED}")?;
                }
                Ok(())
            }
            Self::BadUrl { url } => write!(
                f,
                "GrokBot HTTP cannot use {url}. GROKBOT_GATEWAY_URL must start with http:// or https://."
            ),
            Self::Http {
                endpoint,
                status,
                message,
            } => write!(
                f,
                "GrokBot {endpoint}\nstatus: {status}\nmessage: {message}"
            ),
            Self::Transport { endpoint, message } => {
                write!(f, "GrokBot {endpoint}\nstatus: 0\nmessage: {message}")
            }
            Self::Rejected { message } => f.write_str(message),
        }
    }
}

impl std::error::Error for GrokBotError {}

pub struct GatewayClient {
    config: GatewayConfig,
    timeout: Duration,
}

impl GatewayClient {
    pub fn new(config: GatewayConfig) -> Self {
        Self {
            config,
            timeout: DEFAULT_TIMEOUT,
        }
    }

    pub async fn health(&self) -> Result<GatewayHealth, GrokBotError> {
        let (endpoint, text, status) = self.exchange("GET", "/health", None, false).await?;
        parse_json(&endpoint, status, &text)
    }

    pub async fn list_agents(&self) -> Result<Vec<GrokBotAgent>, GrokBotError> {
        let (endpoint, text, status) = self
            .exchange("POST", "/api/listAgents", Some("{}"), true)
            .await?;
        parse_json(&endpoint, status, &text)
    }

    pub async fn send_prompt(&self, agent_id: &str, prompt: &str) -> Result<(), GrokBotError> {
        if agent_id.trim().is_empty() {
            return Err(GrokBotError::Rejected {
                message: "agent id is empty",
            });
        }
        if prompt.is_empty() {
            return Err(GrokBotError::Rejected {
                message: "prompt is empty",
            });
        }
        let body = serde_json::json!({
            "agentId": agent_id,
            "prompt": prompt,
        })
        .to_string();
        let (endpoint, text, status) = self
            .exchange("POST", "/api/sendPrompt", Some(&body), true)
            .await?;
        let accepted: Accepted = parse_json(&endpoint, status, &text)?;
        if accepted.accepted {
            Ok(())
        } else {
            Err(GrokBotError::Http {
                endpoint,
                status,
                message: "gateway did not accept the prompt".to_string(),
            })
        }
    }

    async fn exchange(
        &self,
        method: &str,
        path: &str,
        body: Option<&str>,
        auth: bool,
    ) -> Result<(String, String, u16), GrokBotError> {
        let base = self.base_url(auth)?;
        let url = format!("{base}{path}");
        let endpoint = format!("{method} {url}");
        if !(base.starts_with("http://") || base.starts_with("https://")) {
            return Err(GrokBotError::BadUrl { url: base });
        }
        let client = reqwest::Client::builder()
            .timeout(self.timeout)
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|err| GrokBotError::Transport {
                endpoint: endpoint.clone(),
                message: redact(&err.to_string(), self.config.token.as_deref()),
            })?;
        let mut request = match method {
            "GET" => client.get(&url),
            "POST" => client.post(&url),
            _ => {
                return Err(GrokBotError::Transport {
                    endpoint,
                    message: format!("unsupported method {method}"),
                })
            }
        };
        request = request.header("x-sand-slim-avatars", "1");
        if let Some(token) = self.config.token.as_deref().filter(|_| auth) {
            request = request.bearer_auth(token);
        }
        if let Some(body) = body {
            request = request
                .header("content-type", "application/json")
                .body(body.to_string());
        }
        let response = request
            .send()
            .await
            .map_err(|err| GrokBotError::Transport {
                endpoint: endpoint.clone(),
                message: redact(&err.to_string(), self.config.token.as_deref()),
            })?;
        let status = response.status().as_u16();
        let text = response
            .text()
            .await
            .map_err(|err| GrokBotError::Transport {
                endpoint: endpoint.clone(),
                message: redact(&err.to_string(), self.config.token.as_deref()),
            })?;
        if (200..300).contains(&status) {
            Ok((endpoint, text, status))
        } else {
            Err(GrokBotError::Http {
                endpoint,
                status,
                message: redact(
                    &message_from_error_body(status, &text),
                    self.config.token.as_deref(),
                ),
            })
        }
    }

    fn base_url(&self, auth: bool) -> Result<String, GrokBotError> {
        let need_url = self.config.base_url.is_none();
        let need_token = auth && self.config.token.is_none();
        if need_url || need_token {
            return Err(GrokBotError::MissingConfig {
                need_url,
                need_token,
            });
        }
        self.config
            .base_url
            .clone()
            .ok_or(GrokBotError::MissingConfig {
                need_url: true,
                need_token,
            })
    }
}

#[derive(Deserialize)]
struct Accepted {
    accepted: bool,
}

fn parse_json<T: for<'de> Deserialize<'de>>(
    endpoint: &str,
    status: u16,
    text: &str,
) -> Result<T, GrokBotError> {
    serde_json::from_str(text).map_err(|_| GrokBotError::Http {
        endpoint: endpoint.to_string(),
        status,
        message: "response was not the JSON this command expects".to_string(),
    })
}

fn message_from_error_body(status: u16, text: &str) -> String {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return format!("HTTP {status}");
    }
    let Ok(value) = serde_json::from_str::<serde_json::Value>(trimmed) else {
        return trimmed.to_string();
    };
    if let Some(error) = value.get("error").and_then(serde_json::Value::as_str) {
        return error.to_string();
    }
    if let Some(message) = value.get("message").and_then(serde_json::Value::as_str) {
        return message.to_string();
    }
    if let Some(message) = value
        .get("error")
        .and_then(|error| error.get("message"))
        .and_then(serde_json::Value::as_str)
    {
        return message.to_string();
    }
    trimmed.to_string()
}

fn redact(message: &str, token: Option<&str>) -> String {
    match token {
        Some(token) if !token.is_empty() => message.replace(token, "[redacted]"),
        _ => message.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::sync::{Arc, Mutex};
    use std::thread;
    use std::time::Duration;

    #[test]
    fn url_prefers_grokbot_gateway_url_and_strips_a_trailing_slash() {
        let config = GatewayConfig::from_parts(
            Some(" http://host:1340/ ".to_string()),
            Some("http://alias:1".to_string()),
            Some("  secret  ".to_string()),
        );
        assert_eq!(config.base_url.as_deref(), Some("http://host:1340"));
        assert_eq!(config.token.as_deref(), Some("secret"));
    }

    #[test]
    fn alias_url_is_used_when_the_primary_is_blank() {
        let config = GatewayConfig::from_parts(
            Some("  ".to_string()),
            Some("http://alias:1340".to_string()),
            None,
        );
        assert_eq!(config.base_url.as_deref(), Some("http://alias:1340"));
        assert!(config.token.is_none());
    }

    #[tokio::test]
    async fn list_without_a_token_does_not_call_the_network() {
        let client = GatewayClient::new(GatewayConfig::from_parts(
            Some("http://127.0.0.1:1".to_string()),
            None,
            None,
        ));
        let err = client.list_agents().await.expect_err("token required");
        assert_eq!(
            err,
            GrokBotError::MissingConfig {
                need_url: false,
                need_token: true,
            }
        );
        let text = err.to_string();
        assert!(text.contains(MISSING_TOKEN_LINE));
        assert!(text.contains(MISSING_TOKEN_NEED));
        assert!(!text.contains(MISSING_URL_LINE));
    }

    #[tokio::test]
    async fn health_without_a_url_names_the_missing_setting() {
        let client = GatewayClient::new(GatewayConfig::from_parts(None, None, None));
        let err = client.health().await.expect_err("url required");
        let text = err.to_string();
        assert!(text.contains(MISSING_URL_LINE));
        assert!(text.contains(MISSING_URL_NEED));
        assert!(!text.contains(MISSING_TOKEN_LINE));
    }

    #[tokio::test]
    async fn bad_scheme_is_refused_before_a_request() {
        let client = GatewayClient::new(GatewayConfig::from_parts(
            Some("ftp://host".to_string()),
            None,
            Some("t".to_string()),
        ));
        let err = client.list_agents().await.expect_err("scheme");
        assert!(matches!(err, GrokBotError::BadUrl { .. }));
        assert!(err.to_string().contains("http:// or https://"));
    }

    #[tokio::test]
    async fn http_error_names_endpoint_status_and_message_and_hides_the_token() {
        let token = "token-should-not-leak";
        let body = format!(r#"{{"error":"refused {token}"}}"#);
        let url = spawn_once(401, body, Arc::new(Mutex::new(String::new())));
        let client = GatewayClient::new(GatewayConfig::from_parts(
            Some(url),
            None,
            Some(token.to_string()),
        ));
        let err = client.list_agents().await.expect_err("401");
        let GrokBotError::Http {
            endpoint,
            status,
            message,
        } = err
        else {
            panic!("expected http error");
        };
        assert!(endpoint.contains("POST "));
        assert!(endpoint.contains("/api/listAgents"));
        assert_eq!(status, 401);
        assert_eq!(message, "refused [redacted]");
        assert!(!message.contains(token));
        let rendered = GrokBotError::Http {
            endpoint: endpoint.clone(),
            status,
            message: message.clone(),
        }
        .to_string();
        assert!(rendered.contains(&endpoint));
        assert!(rendered.contains("status: 401"));
        assert!(rendered.contains("message: refused [redacted]"));
    }

    #[tokio::test]
    async fn list_and_send_use_the_gateway_contract() {
        let seen = Arc::new(Mutex::new(String::new()));
        let roster = r#"[{"id":"a1","name":"Ada","description":"plans","title":"Keeper","isRunning":true,"isActive":false}]"#;
        let url = spawn_script(
            vec![
                (200, roster.to_string()),
                (200, r#"{"accepted":true}"#.to_string()),
            ],
            Arc::clone(&seen),
        );
        let client = GatewayClient::new(GatewayConfig::from_parts(
            Some(url),
            None,
            Some("issued-token".to_string()),
        ));
        let agents = client.list_agents().await.expect("list");
        let agent = agents.first().expect("one agent");
        assert_eq!(agent.id, "a1");
        assert_eq!(agent.name, "Ada");
        assert_eq!(agent.description, "plans");
        assert!(agent.is_running);
        assert!(!agent.is_active);
        client.send_prompt("a1", "status only").await.expect("send");
        let recorded = seen.lock().expect("lock").clone();
        let lowered = recorded.to_ascii_lowercase();
        assert!(recorded.contains("POST /api/listAgents"));
        assert!(recorded.contains("POST /api/sendPrompt"));
        assert!(lowered.contains("authorization: bearer issued-token"));
        assert!(lowered.contains("x-sand-slim-avatars: 1"));
        assert!(recorded.contains(r#""agentId":"a1""#));
        assert!(recorded.contains(r#""prompt":"status only""#));
    }

    #[tokio::test]
    async fn health_skips_the_bearer_token() {
        let seen = Arc::new(Mutex::new(String::new()));
        let url = spawn_once(200, r#"{"ok":true,"pid":7}"#.to_string(), Arc::clone(&seen));
        let client = GatewayClient::new(GatewayConfig::from_parts(
            Some(url),
            None,
            Some("issued-token".to_string()),
        ));
        let health = client.health().await.expect("health");
        assert!(health.ok);
        assert_eq!(health.pid, Some(7));
        let recorded = seen.lock().expect("lock").clone();
        assert!(recorded.contains("GET /health"));
        assert!(!recorded.to_ascii_lowercase().contains("authorization"));
    }

    fn spawn_once(status: u16, body: String, seen: Arc<Mutex<String>>) -> String {
        spawn_script(vec![(status, body)], seen)
    }

    fn spawn_script(script: Vec<(u16, String)>, seen: Arc<Mutex<String>>) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let address = listener.local_addr().expect("addr");
        thread::spawn(move || {
            for (status, body) in script {
                let mut socket = match listener.accept() {
                    Ok((socket, _)) => socket,
                    Err(_) => return,
                };
                let _ = socket.set_read_timeout(Some(Duration::from_secs(2)));
                let request = read_request(&mut socket);
                if let Ok(mut slot) = seen.lock() {
                    slot.push_str(&request);
                    slot.push('\n');
                }
                let header = format!(
                    "HTTP/1.1 {status} STATUS\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                    body.len()
                );
                let _ = socket.write_all(header.as_bytes());
                let _ = socket.write_all(body.as_bytes());
                let _ = socket.flush();
            }
        });
        format!("http://{address}")
    }

    fn read_request(socket: &mut std::net::TcpStream) -> String {
        let mut bytes = Vec::new();
        let mut chunk = [0_u8; 4096];
        loop {
            match socket.read(&mut chunk) {
                Ok(0) => break,
                Ok(count) => {
                    bytes.extend(chunk.iter().take(count).copied());
                    if request_complete(&bytes) {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
        String::from_utf8_lossy(&bytes).into_owned()
    }

    fn request_complete(bytes: &[u8]) -> bool {
        let text = String::from_utf8_lossy(bytes);
        let Some((head, body)) = text.split_once("\r\n\r\n") else {
            return false;
        };
        let Some(length) = content_length(head) else {
            return true;
        };
        body.len() >= length
    }

    fn content_length(head: &str) -> Option<usize> {
        for line in head.lines() {
            let Some((name, value)) = line.split_once(':') else {
                continue;
            };
            if name.eq_ignore_ascii_case("content-length") {
                return value.trim().parse().ok();
            }
        }
        None
    }
}
