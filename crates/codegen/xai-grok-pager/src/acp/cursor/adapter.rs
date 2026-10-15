//! JSON-RPC line rewriter between the pager's ACP client and `cursor-agent acp`.
//!
//! The pager speaks grok-shell's ACP dialect: `x.ai/*` extension methods, a client-generated `promptId` echoed on
//! every notification, grok model ids (`grok-4.7`) and the `default` session mode. Cursor speaks plain ACP with its own
//! parameterized model ids (`grok-4.7[context=256k,...]`) and an `agent` mode. This adapter translates between the two
//! one line at a time so neither side needs to know about the other.

use std::collections::HashMap;

use serde_json::{Map, Value, json};

const GROK_DEFAULT_MODE: &str = "default";
const CURSOR_AGENT_MODE: &str = "agent";
const INTERNAL_ID_PREFIX: &str = "grok-cursor-bridge-";
const METHOD_NOT_FOUND: i64 = -32601;

/// Where a rewritten line must be delivered.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Routed {
    /// Write to the `cursor-agent` subprocess stdin.
    ToAgent(String),
    /// Hand back to the pager's ACP client.
    ToClient(String),
}

#[derive(Debug)]
enum Pending {
    /// `session/new` or `session/load`; `requested_model` is the pager's `_meta.modelId`.
    Session {
        requested_model: Option<String>,
        session_id: Option<String>,
    },
    Prompt {
        session_id: String,
    },
}

#[derive(Debug, Clone)]
struct CursorModel {
    id: String,
    name: String,
}

#[derive(Debug, Default)]
pub(crate) struct CursorAdapter {
    pending: HashMap<String, Pending>,
    active_prompts: HashMap<String, String>,
    /// Sessions mid-`session/load`, mapped to whether the pager asked for no history replay.
    loading: HashMap<String, bool>,
    models: Vec<CursorModel>,
    next_internal_id: u64,
    /// `[cursor] model`, then the last model the user switched to; wins over grok's `_meta.modelId` for new sessions.
    preferred_model: Option<String>,
    /// Cursor loads its own MCP config; the pager's list (which already includes `~/.cursor/mcp.json`) is dropped unless opted in.
    forward_mcp_servers: bool,
}

impl CursorAdapter {
    pub(crate) fn new(preferred_model: Option<String>, forward_mcp_servers: bool) -> Self {
        Self {
            preferred_model,
            forward_mcp_servers,
            ..Self::default()
        }
    }

    /// Rewrite one line the pager sent toward Cursor.
    pub(crate) fn outbound(&mut self, line: &str) -> Vec<Routed> {
        let Ok(mut msg) = serde_json::from_str::<Value>(line) else {
            return vec![Routed::ToAgent(line.to_owned())];
        };
        let method = msg.get("method").and_then(Value::as_str).map(str::to_owned);
        let id = msg.get("id").cloned();
        let Some(method) = method else {
            return vec![Routed::ToAgent(line.to_owned())];
        };

        if is_grok_extension(&method) {
            if let Some(id) = id.as_ref()
                && let Some(result) = cursor_local_extension_result(&method)
            {
                return vec![Routed::ToClient(success_response(id, result))];
            }
            return match id {
                Some(id) => vec![Routed::ToClient(error_response(
                    &id,
                    METHOD_NOT_FOUND,
                    &format!("{method} is not supported by the Cursor backend"),
                ))],
                None => Vec::new(),
            };
        }

        match method.as_str() {
            "session/new" | "session/load" => {
                if !self.forward_mcp_servers
                    && let Some(servers) = msg.pointer_mut("/params/mcpServers")
                {
                    *servers = Value::Array(Vec::new());
                }
                if let Some(id) = &id {
                    let params = msg.get("params");
                    if method == "session/load"
                        && let Some(session_id) = params
                            .and_then(|p| p.get("sessionId"))
                            .and_then(Value::as_str)
                    {
                        let no_replay = params
                            .and_then(|p| p.pointer("/_meta/noReplay"))
                            .and_then(Value::as_bool)
                            .unwrap_or(false);
                        self.loading.insert(session_id.to_owned(), no_replay);
                    }
                    self.pending.insert(
                        id_key(id),
                        Pending::Session {
                            requested_model: self.preferred_model.clone().or_else(|| {
                                params
                                    .and_then(|p| p.pointer("/_meta/modelId"))
                                    .and_then(Value::as_str)
                                    .map(str::to_owned)
                            }),
                            session_id: params
                                .and_then(|p| p.get("sessionId"))
                                .and_then(Value::as_str)
                                .map(str::to_owned),
                        },
                    );
                }
            }
            "session/set_model" => {
                if let Some(model) = msg.pointer_mut("/params/modelId")
                    && let Some(requested) = model.as_str()
                    && let Some(resolved) = self.resolve_model(requested)
                {
                    self.preferred_model = Some(resolved.clone());
                    *model = Value::String(resolved);
                }
            }
            "session/set_mode" => {
                if let Some(mode) = msg.pointer_mut("/params/modeId")
                    && mode.as_str() == Some(GROK_DEFAULT_MODE)
                {
                    *mode = Value::String(CURSOR_AGENT_MODE.to_owned());
                }
            }
            "session/prompt" => {
                let session_id = msg
                    .pointer("/params/sessionId")
                    .and_then(Value::as_str)
                    .map(str::to_owned);
                let prompt_id = msg
                    .pointer("/params/_meta/promptId")
                    .and_then(Value::as_str)
                    .map(str::to_owned);
                if let Some(session_id) = session_id {
                    if let Some(prompt_id) = prompt_id {
                        self.active_prompts.insert(session_id.clone(), prompt_id);
                    }
                    if let Some(id) = &id {
                        self.pending
                            .insert(id_key(id), Pending::Prompt { session_id });
                    }
                }
            }
            _ => {}
        }
        vec![Routed::ToAgent(msg.to_string())]
    }

    /// Rewrite one line Cursor sent toward the pager.
    pub(crate) fn inbound(&mut self, line: &str) -> Vec<Routed> {
        let Ok(mut msg) = serde_json::from_str::<Value>(line) else {
            return vec![Routed::ToClient(line.to_owned())];
        };

        if let Some(method) = msg.get("method").and_then(Value::as_str) {
            if method == "session/update" && !self.stamp_session_update(&mut msg) {
                return Vec::new();
            }
            return vec![Routed::ToClient(msg.to_string())];
        }

        let Some(id) = msg.get("id").cloned() else {
            return vec![Routed::ToClient(line.to_owned())];
        };
        if id
            .as_str()
            .is_some_and(|s| s.starts_with(INTERNAL_ID_PREFIX))
        {
            if let Some(error) = msg.get("error") {
                tracing::warn!(%error, "cursor backend: internal request failed");
            }
            return Vec::new();
        }

        let mut out = Vec::new();
        match self.pending.remove(&id_key(&id)) {
            Some(Pending::Session {
                requested_model,
                session_id,
            }) => {
                if let Some(sid) = &session_id {
                    self.loading.remove(sid);
                }
                if let Some(result) = msg.get_mut("result").and_then(Value::as_object_mut) {
                    let session_id = result
                        .get("sessionId")
                        .and_then(Value::as_str)
                        .map(str::to_owned)
                        .or(session_id);
                    rewrite_modes(result);
                    self.record_models(result);
                    if let (Some(session_id), Some(requested)) = (session_id, requested_model)
                        && let Some(set_model) =
                            self.apply_requested_model(result, &session_id, &requested)
                    {
                        out.push(Routed::ToAgent(set_model));
                    }
                }
            }
            Some(Pending::Prompt { session_id }) => {
                if let Some(prompt_id) = self.active_prompts.remove(&session_id)
                    && let Some(result) = msg.get_mut("result").and_then(Value::as_object_mut)
                {
                    meta_mut(result)
                        .entry("promptId")
                        .or_insert(Value::String(prompt_id));
                }
            }
            None => {}
        }
        out.push(Routed::ToClient(msg.to_string()));
        out
    }

    /// Returns `false` when the update must be dropped (history replay the pager opted out of).
    fn stamp_session_update(&self, msg: &mut Value) -> bool {
        let Some(params) = msg.get_mut("params").and_then(Value::as_object_mut) else {
            return true;
        };
        let session_id = params
            .get("sessionId")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let replay = self.loading.get(session_id).copied();
        if replay == Some(true) {
            return false;
        }
        let prompt_id = self.active_prompts.get(session_id).cloned();
        if let Some(update) = params.get_mut("update").and_then(Value::as_object_mut)
            && update.get("sessionUpdate").and_then(Value::as_str) == Some("current_mode_update")
            && let Some(mode) = update.get_mut("currentModeId")
            && mode.as_str() == Some(CURSOR_AGENT_MODE)
        {
            *mode = Value::String(GROK_DEFAULT_MODE.to_owned());
        }
        let meta = meta_mut(params);
        if replay.is_some() {
            meta.insert("isReplay".to_owned(), Value::Bool(true));
        } else if let Some(prompt_id) = prompt_id {
            meta.entry("promptId").or_insert(Value::String(prompt_id));
        }
        meta.entry("agentTimestampMs")
            .or_insert_with(|| json!(chrono_now_ms()));
        true
    }

    fn record_models(&mut self, result: &Map<String, Value>) {
        let Some(available) = result
            .get("models")
            .and_then(|m| m.get("availableModels"))
            .and_then(Value::as_array)
        else {
            return;
        };
        let models: Vec<CursorModel> = available
            .iter()
            .filter_map(|m| {
                Some(CursorModel {
                    id: m.get("modelId")?.as_str()?.to_owned(),
                    name: m
                        .get("name")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_owned(),
                })
            })
            .collect();
        if !models.is_empty() {
            self.models = models;
        }
    }

    /// Switch a fresh session to the pager's requested model, returning the `session/set_model` line to send.
    fn apply_requested_model(
        &mut self,
        result: &mut Map<String, Value>,
        session_id: &str,
        requested: &str,
    ) -> Option<String> {
        let resolved = self.resolve_model(requested)?;
        let current = result.get_mut("models")?.get_mut("currentModelId")?;
        if current.as_str() == Some(resolved.as_str()) {
            return None;
        }
        *current = Value::String(resolved.clone());
        self.next_internal_id += 1;
        Some(
            json!({
                "jsonrpc": "2.0",
                "id": format!("{INTERNAL_ID_PREFIX}{}", self.next_internal_id),
                "method": "session/set_model",
                "params": { "sessionId": session_id, "modelId": resolved },
            })
            .to_string(),
        )
    }

    /// Map a grok-style model id (`grok-4.7`, `claude-opus-5-5`, or a full Cursor id) onto a Cursor model id.
    fn resolve_model(&self, requested: &str) -> Option<String> {
        let requested = requested.trim();
        if requested.is_empty() {
            return None;
        }
        if let Some(m) = self.models.iter().find(|m| m.id == requested) {
            return Some(m.id.clone());
        }
        let base = model_base(requested);
        self.models
            .iter()
            .find(|m| {
                model_base(&m.id).eq_ignore_ascii_case(base) || m.name.eq_ignore_ascii_case(base)
            })
            .map(|m| m.id.clone())
    }
}

fn model_base(id: &str) -> &str {
    id.split('[').next().unwrap_or(id).trim()
}

fn is_grok_extension(method: &str) -> bool {
    method
        .strip_prefix('_')
        .unwrap_or(method)
        .starts_with("x.ai/")
}

/// Session list and MCP list are answered from disk so the picker and `/mcps` modal work
/// while `cursor-agent` is the ACP backend. Other `x.ai/*` methods stay unsupported.
fn cursor_local_extension_result(method: &str) -> Option<Value> {
    match method.strip_prefix('_').unwrap_or(method) {
        "x.ai/session/list" => Some(json!({ "sessions": [] })),
        "x.ai/mcp/list" => Some(cursor_mcp_list_result()),
        _ => None,
    }
}

fn cursor_mcp_list_result() -> Value {
    let cwd = std::env::current_dir().unwrap_or_default();
    let servers: Vec<Value> = xai_grok_config::mcp_servers::list_cursor_mcp_for_display(&cwd)
        .into_iter()
        .map(|server| {
            json!({
                "name": server.name,
                "source": "local",
                "sourceLabel": server.source_label(),
                "session": {
                    "enabled": server.enabled,
                    "status": if server.enabled { "ready" } else { "unavailable" },
                },
            })
        })
        .collect();
    json!({ "servers": servers })
}

fn rewrite_modes(result: &mut Map<String, Value>) {
    let Some(modes) = result.get_mut("modes").and_then(Value::as_object_mut) else {
        return;
    };
    if let Some(current) = modes.get_mut("currentModeId")
        && current.as_str() == Some(CURSOR_AGENT_MODE)
    {
        *current = Value::String(GROK_DEFAULT_MODE.to_owned());
    }
    if let Some(available) = modes
        .get_mut("availableModes")
        .and_then(Value::as_array_mut)
    {
        for mode in available {
            if let Some(id) = mode.get_mut("id")
                && id.as_str() == Some(CURSOR_AGENT_MODE)
            {
                *id = Value::String(GROK_DEFAULT_MODE.to_owned());
            }
        }
    }
}

fn meta_mut(obj: &mut Map<String, Value>) -> &mut Map<String, Value> {
    let meta = obj
        .entry("_meta")
        .or_insert_with(|| Value::Object(Map::new()));
    if !meta.is_object() {
        *meta = Value::Object(Map::new());
    }
    meta.as_object_mut()
        .expect("just ensured _meta is an object")
}

fn id_key(id: &Value) -> String {
    id.to_string()
}

fn success_response(id: &Value, result: Value) -> String {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "result": result,
    })
    .to_string()
}

fn error_response(id: &Value, code: i64, message: &str) -> String {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": { "code": code, "message": message },
    })
    .to_string()
}

fn chrono_now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or_default()
}

#[cfg(test)]
#[path = "adapter_tests.rs"]
mod tests;
