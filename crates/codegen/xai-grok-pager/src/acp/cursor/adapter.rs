//! JSON-RPC line rewriter between the pager's ACP client and `cursor-agent acp`.
//!
//! The pager speaks grok-shell's ACP dialect: `x.ai/*` extension methods, a client-generated `promptId` echoed on
//! every notification, grok model ids (`grok-4.7`) and the `default` session mode. Cursor speaks plain ACP with its own
//! parameterized model ids (`grok-4.7[context=256k,...]`) and an `agent` mode. This adapter translates between the two
//! one line at a time so neither side needs to know about the other.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

use serde_json::{Map, Value, json};
use xai_grok_shell::sampling::types::{
    CONTEXT_WINDOW_META_KEY, CONTEXT_WINDOWS_META_KEY, FAST_MODE_META_KEY, MAX_MODE_META_KEY,
    MODEL_PARAMETERS_META_KEY, REASONING_EFFORT_META_KEY, REASONING_EFFORTS_META_KEY,
    SUPPORTS_FAST_MODE_META_KEY, SUPPORTS_MAX_MODE_META_KEY, SUPPORTS_REASONING_EFFORT_META_KEY,
    format_context_window_param, model_parameters_meta_value, parse_canonical_effort_token,
    parse_context_window_meta, parse_context_window_token, parse_fast_mode_meta,
    parse_max_mode_meta, parse_model_parameters_meta, parse_parameterized_model_spec,
    parse_reasoning_effort_meta,
};

use super::catalog::{self, CatalogRequest};
use super::protocol;

const INTERNAL_ID_PREFIX: &str = "grok-cursor-bridge-";
const METHOD_NOT_FOUND: i64 = -32601;

/// Where a rewritten line must be delivered.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Routed {
    /// Write to the `cursor-agent` subprocess stdin.
    ToAgent(String),
    /// Hand back to the pager's ACP client.
    ToClient(String),
    /// Create, follow up, refresh, or cancel a Cursor cloud agent. ACP does not do this.
    Cloud {
        session_id: String,
        request: CloudRequest,
        agent_id: Option<String>,
        /// Set on the first `&` task when the session cwd has a git remote.
        repo: Option<super::cloud::CloudRepo>,
        model: Option<String>,
        /// `session/prompt` id to answer when the cloud handoff is done.
        prompt_rpc_id: Option<Value>,
        prompt_id: Option<String>,
    },
    /// Answer a skills/hooks/plugins/marketplace/workflows/commands request locally; Cursor has none of them.
    Catalog(Box<CatalogRequest>),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum CloudRequest {
    Send(String),
    Refresh,
    Cancel,
}

#[derive(Debug)]
enum Pending {
    /// `session/new` or `session/load`; `requested_model` is the pager's `_meta.modelId`.
    Session {
        requested_model: Option<String>,
        session_id: Option<String>,
        cwd: Option<String>,
    },
    Prompt {
        session_id: String,
        cloud_handoff: bool,
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
    sessions: HashMap<String, CursorSession>,
    bridge_wait: HashMap<String, protocol::PendingBridge>,
    /// `cursor_login` has succeeded for this agent process.
    cursor_authenticated: bool,
    /// In-flight `authenticate` id, either injected or rewritten from the pager.
    auth_request_id: Option<String>,
    /// `session/new` and `session/load` lines waiting for that authenticate call.
    held_for_auth: Vec<String>,
    /// Reads `[skills].disabled`; tests swap in a fixed set.
    disabled_skills: Option<fn() -> HashSet<String>>,
    /// `x.ai/session/list` requests forwarded to Cursor as ACP `session/list`, keyed by the internal id.
    session_lists: HashMap<String, SessionListRequest>,
}

#[derive(Debug)]
struct SessionListRequest {
    client_id: Value,
    query: Option<String>,
    limit: Option<usize>,
}

#[derive(Debug, Default)]
struct CursorSession {
    skills: HashSet<String>,
    /// Cursor's latest `availableCommands`, normalized and before disabled skills are hidden.
    commands: Vec<Value>,
    cloud_agent_id: Option<String>,
    cwd: Option<String>,
    model: Option<String>,
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
            if let Some(id) = &id
                && let Some(pending) = self.bridge_wait.remove(&id_key(id))
                && let Some(result) = msg.get_mut("result")
            {
                *result = protocol::cursor_result_from_ext(&pending, result);
            }
            return vec![Routed::ToAgent(msg.to_string())];
        };

        if let Some(catalog_method) = catalog::catalog_method(&method) {
            let Some(id) = id else {
                return Vec::new();
            };
            let request = self.catalog_request(id, catalog_method, msg.get("params"));
            return vec![Routed::Catalog(Box::new(request))];
        }

        if method.strip_prefix('_').unwrap_or(&method) == "x.ai/session/list" {
            let Some(id) = id else {
                return Vec::new();
            };
            return self.list_cursor_sessions(id, msg.get("params"));
        }

        if is_grok_extension(&method) {
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
            "initialize" => protocol::advertise_cursor_client(&mut msg),
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
                            cwd: params
                                .and_then(|p| p.get("cwd"))
                                .and_then(Value::as_str)
                                .map(str::to_owned),
                        },
                    );
                }
            }
            "session/set_model" => {
                let session_id = msg
                    .pointer("/params/sessionId")
                    .and_then(Value::as_str)
                    .map(str::to_owned);
                let meta = msg
                    .pointer("/params/_meta")
                    .and_then(Value::as_object)
                    .cloned();
                let requested = msg
                    .pointer("/params/modelId")
                    .and_then(Value::as_str)
                    .map(str::to_owned);
                if let Some(requested) = requested {
                    if let Some(resolved) = self.resolve_model_with_meta(&requested, meta.as_ref())
                    {
                        self.preferred_model = Some(resolved.clone());
                        if let Some(session_id) = &session_id {
                            self.remember_model(session_id, &resolved);
                        }
                        if let Some(model) = msg.pointer_mut("/params/modelId") {
                            *model = Value::String(resolved);
                        }
                    } else if let Some(session_id) = &session_id {
                        self.remember_model(session_id, &requested);
                    }
                }
            }
            "session/set_mode" => {
                if let Some(mode) = msg.pointer_mut("/params/modeId") {
                    protocol::rewrite_mode_value(mode, false);
                }
            }
            "authenticate" => {
                if let Some(params) = msg.get_mut("params").and_then(Value::as_object_mut) {
                    params.insert(
                        "methodId".to_owned(),
                        Value::String("cursor_login".to_owned()),
                    );
                }
                if let Some(id) = &id {
                    self.auth_request_id = Some(id_key(id));
                }
            }
            "session/prompt" => {
                let session_id = msg
                    .pointer("/params/sessionId")
                    .and_then(Value::as_str)
                    .map(str::to_owned);
                let skills = session_id
                    .as_deref()
                    .and_then(|id| self.sessions.get(id))
                    .map(|session| session.skills.clone())
                    .unwrap_or_default();
                let cloud_agent_id = session_id
                    .as_deref()
                    .and_then(|id| self.sessions.get(id))
                    .and_then(|session| session.cloud_agent_id.clone());
                let mut cloud_handoff = false;
                let mut cloud_task = None;
                if let Some(params) = msg.get_mut("params").and_then(Value::as_object_mut) {
                    cloud_handoff = protocol::prompt_is_cloud_handoff(params);
                    if cloud_handoff {
                        cloud_task = params
                            .get("prompt")
                            .and_then(Value::as_array)
                            .and_then(|blocks| {
                                blocks.iter().find_map(|block| {
                                    block.get("text").and_then(Value::as_str).map(str::to_owned)
                                })
                            })
                            .as_deref()
                            .and_then(super::cloud::handoff_text);
                    } else {
                        protocol::hoist_skill_prompt(params, &skills);
                    }
                }
                if cloud_handoff {
                    let prompt_id = msg
                        .pointer("/params/_meta/promptId")
                        .and_then(Value::as_str)
                        .map(str::to_owned);
                    let end = id
                        .as_ref()
                        .map(|id| protocol::prompt_end(id, prompt_id.as_deref()).to_string());
                    let Some(session_id) = session_id.clone() else {
                        return end.into_iter().map(Routed::ToClient).collect();
                    };
                    let request = cloud_request(cloud_task);
                    if matches!(request, CloudRequest::Refresh | CloudRequest::Cancel)
                        && cloud_agent_id.is_none()
                    {
                        let mut out = vec![Routed::ToClient(
                            protocol::agent_notice(
                                &session_id,
                                "No cloud agent on this session yet. Send & and a task to start one.\n",
                            )
                            .to_string(),
                        )];
                        out.extend(end.map(Routed::ToClient));
                        return out;
                    }
                    let model = self
                        .sessions
                        .get(&session_id)
                        .and_then(|session| session.model.clone())
                        .or_else(|| self.preferred_model.clone());
                    return vec![Routed::Cloud {
                        session_id: session_id.clone(),
                        request,
                        repo: self.cloud_repo(cloud_agent_id.as_deref(), &session_id),
                        model,
                        agent_id: cloud_agent_id,
                        prompt_rpc_id: id,
                        prompt_id,
                    }];
                }
                let prompt_id = msg
                    .pointer("/params/_meta/promptId")
                    .and_then(Value::as_str)
                    .map(str::to_owned);
                if let Some(session_id) = session_id {
                    self.sessions.entry(session_id.clone()).or_default();
                    if let Some(prompt_id) = prompt_id {
                        self.active_prompts.insert(session_id.clone(), prompt_id);
                    }
                    if let Some(id) = &id {
                        self.pending.insert(
                            id_key(id),
                            Pending::Prompt {
                                session_id,
                                cloud_handoff,
                            },
                        );
                    }
                }
            }
            _ => {}
        }
        if matches!(method.as_str(), "session/new" | "session/load") && !self.cursor_authenticated {
            self.held_for_auth.push(msg.to_string());
            return self.start_cursor_auth();
        }
        vec![Routed::ToAgent(msg.to_string())]
    }

    /// Rewrite one line Cursor sent toward the pager.
    pub(crate) fn inbound(&mut self, line: &str) -> Vec<Routed> {
        let Ok(mut msg) = serde_json::from_str::<Value>(line) else {
            return vec![Routed::ToClient(line.to_owned())];
        };

        if let Some(method) = msg.get("method").and_then(Value::as_str).map(str::to_owned) {
            if method == "session/update" {
                if let Some(ext) = protocol::cursor_subagent_ext(&msg) {
                    return vec![Routed::ToClient(ext.to_string())];
                }
                self.note_session_update(&mut msg);
                if !self.stamp_session_update(&mut msg) {
                    return Vec::new();
                }
                return vec![Routed::ToClient(msg.to_string())];
            }
            if let Some(translated) =
                protocol::translate_cursor_inbound(&msg, self.inbound_session_id(&msg).as_deref())
            {
                if let (Some(id), Some(pending)) = (msg.get("id"), translated.pending) {
                    self.bridge_wait.insert(id_key(id), pending);
                }
                let mut routed = Vec::new();
                routed.extend(
                    translated
                        .to_agent
                        .into_iter()
                        .map(|value| Routed::ToAgent(value.to_string())),
                );
                routed.extend(
                    translated
                        .to_client
                        .into_iter()
                        .map(|value| Routed::ToClient(value.to_string())),
                );
                return routed;
            }
            return vec![Routed::ToClient(msg.to_string())];
        }

        let Some(id) = msg.get("id").cloned() else {
            return vec![Routed::ToClient(line.to_owned())];
        };
        if self.auth_request_id.as_ref() == Some(&id_key(&id)) {
            return self.finish_cursor_auth(&msg);
        }
        if let Some(reply) = self.session_list_reply(&id, Some(&msg)) {
            return vec![Routed::ToClient(reply)];
        }
        if id
            .as_str()
            .is_some_and(|s| s.starts_with(INTERNAL_ID_PREFIX))
        {
            if id.as_str() == Some(&format!("{INTERNAL_ID_PREFIX}auth"))
                && msg.get("error").is_none()
            {
                self.cursor_authenticated = true;
            }
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
                cwd,
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
                    if let Some(session_id) = &session_id {
                        let session = self.sessions.entry(session_id.clone()).or_default();
                        if session.cwd.is_none() {
                            session.cwd = cwd;
                        }
                    }
                    rewrite_modes(result);
                    protocol::absorb_cursor_skills(result);
                    if let Some(session_id) = &session_id {
                        self.remember_commands(session_id, result.get_mut("availableCommands"));
                    }
                    self.record_models(result);
                    if let Some(session_id) = session_id {
                        let switched = requested_model.as_deref().and_then(|requested| {
                            self.apply_requested_model(result, &session_id, requested)
                        });
                        if let Some(set_model) = switched {
                            out.push(Routed::ToAgent(set_model));
                        } else if let Some(current) = result
                            .get("models")
                            .and_then(|m| m.get("currentModelId"))
                            .and_then(Value::as_str)
                        {
                            self.remember_model(&session_id, current);
                        }
                    }
                }
            }
            Some(Pending::Prompt {
                session_id,
                cloud_handoff,
            }) => {
                if let Some(result) = msg.get_mut("result").and_then(Value::as_object_mut) {
                    if let Some(prompt_id) = self.active_prompts.remove(&session_id) {
                        meta_mut(result)
                            .entry("promptId")
                            .or_insert(Value::String(prompt_id));
                    }
                    if cloud_handoff && let Some(agent_id) = protocol::cloud_agent_id(result) {
                        self.sessions
                            .entry(session_id.clone())
                            .or_default()
                            .cloud_agent_id = Some(agent_id.clone());
                        out.push(Routed::ToClient(
                            protocol::cloud_status_update(&session_id, &agent_id).to_string(),
                        ));
                    }
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
        {
            protocol::rewrite_mode_value(mode, true);
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

    fn note_session_update(&mut self, msg: &mut Value) {
        let Some(params) = msg.get_mut("params").and_then(Value::as_object_mut) else {
            return;
        };
        let session_id = params
            .get("sessionId")
            .and_then(Value::as_str)
            .map(str::to_owned);
        let Some(update) = params.get_mut("update").and_then(Value::as_object_mut) else {
            return;
        };
        if update.get("sessionUpdate").and_then(Value::as_str) == Some("available_commands_update")
        {
            protocol::normalize_commands(update.get_mut("availableCommands"));
            if let Some(session_id) = session_id {
                self.remember_commands(&session_id, update.get_mut("availableCommands"));
            }
        }
    }

    fn start_cursor_auth(&mut self) -> Vec<Routed> {
        if self.cursor_authenticated || self.auth_request_id.is_some() {
            return Vec::new();
        }
        let id = format!("{INTERNAL_ID_PREFIX}auth");
        self.auth_request_id = Some(id_key(&Value::String(id.clone())));
        vec![Routed::ToAgent(
            json!({
                "jsonrpc": "2.0",
                "id": id,
                "method": "authenticate",
                "params": { "methodId": "cursor_login" }
            })
            .to_string(),
        )]
    }

    fn finish_cursor_auth(&mut self, msg: &Value) -> Vec<Routed> {
        let injected = msg
            .get("id")
            .and_then(Value::as_str)
            .is_some_and(|id| id.starts_with(INTERNAL_ID_PREFIX));
        self.auth_request_id = None;
        if msg.get("error").is_some() {
            let held = std::mem::take(&mut self.held_for_auth);
            let mut out = Vec::new();
            for line in held {
                let Ok(held_msg) = serde_json::from_str::<Value>(&line) else {
                    continue;
                };
                let Some(held_id) = held_msg.get("id") else {
                    continue;
                };
                if let Some(reply) = self.session_list_reply(held_id, None) {
                    out.push(Routed::ToClient(reply));
                    continue;
                }
                self.pending.remove(&id_key(held_id));
                out.push(Routed::ToClient(error_response(
                    held_id,
                    -32000,
                    &cursor_auth_failure(msg),
                )));
            }
            if !injected {
                out.push(Routed::ToClient(msg.to_string()));
            }
            return out;
        }
        self.cursor_authenticated = true;
        let mut out: Vec<_> = std::mem::take(&mut self.held_for_auth)
            .into_iter()
            .map(Routed::ToAgent)
            .collect();
        if !injected {
            out.push(Routed::ToClient(msg.to_string()));
        }
        out
    }

    /// `cursor_login` opens a browser and does not answer until that finishes.
    /// The caller gives up so `session/new` is not held forever.
    pub(crate) fn auth_timed_out(&mut self) -> Vec<String> {
        let injected = self
            .auth_request_id
            .as_deref()
            .is_some_and(|id| id.contains("grok-cursor-bridge-auth"));
        if self.cursor_authenticated || !injected {
            return Vec::new();
        }
        self.auth_request_id = None;
        let held = std::mem::take(&mut self.held_for_auth);
        let mut out = Vec::new();
        for line in held {
            let Ok(held_msg) = serde_json::from_str::<Value>(&line) else {
                continue;
            };
            let Some(held_id) = held_msg.get("id") else {
                continue;
            };
            if let Some(reply) = self.session_list_reply(held_id, None) {
                out.push(reply);
                continue;
            }
            self.pending.remove(&id_key(held_id));
            out.push(error_response(
                held_id,
                -32000,
                "Cursor login did not finish. Run `agent login`, then start the session again.",
            ));
        }
        out
    }

    fn inbound_session_id(&self, msg: &Value) -> Option<String> {
        if let Some(session_id) = msg.pointer("/params/sessionId").and_then(Value::as_str) {
            return Some(session_id.to_owned());
        }
        if self.sessions.len() == 1 {
            return self.sessions.keys().next().cloned();
        }
        None
    }

    pub(crate) fn note_cloud_agent(&mut self, session_id: &str, agent_id: &str) {
        self.sessions
            .entry(session_id.to_owned())
            .or_default()
            .cloud_agent_id = Some(agent_id.to_owned());
    }

    fn cloud_repo(
        &self,
        agent_id: Option<&str>,
        session_id: &str,
    ) -> Option<super::cloud::CloudRepo> {
        if agent_id.is_some() {
            return None;
        }
        let cwd = self.sessions.get(session_id)?.cwd.as_deref()?;
        super::cloud::repo_from_cwd(std::path::Path::new(cwd))
    }

    /// Record Cursor's catalog for the session, then hide disabled skills from the copy the pager sees.
    fn remember_commands(&mut self, session_id: &str, commands: Option<&mut Value>) {
        let Some(commands) = commands else {
            return;
        };
        let session = self.sessions.entry(session_id.to_owned()).or_default();
        session.skills.extend(protocol::skill_names(Some(commands)));
        let Some(list) = commands.as_array() else {
            return;
        };
        session.commands = list.clone();
        let disabled =
            self.disabled_skills
                .unwrap_or(xai_grok_shell::extensions::catalog::disabled_skills)();
        if !disabled.is_empty() {
            *commands = Value::Array(catalog::visible_commands(list, &disabled));
        }
    }

    /// Forward the picker's `x.ai/session/list` as Cursor's ACP `session/list`, which needs an absolute `cwd`.
    /// Held behind `cursor_login`, since Cursor rejects the list until `authenticate` succeeds.
    fn list_cursor_sessions(&mut self, client_id: Value, params: Option<&Value>) -> Vec<Routed> {
        self.next_internal_id += 1;
        let internal_id = format!("{INTERNAL_ID_PREFIX}session-list-{}", self.next_internal_id);
        let cwd = params
            .and_then(|p| p.get("cwd"))
            .and_then(Value::as_str)
            .filter(|cwd| std::path::Path::new(cwd).is_absolute());
        let mut list_params = Map::new();
        if let Some(cwd) = cwd {
            list_params.insert("cwd".to_owned(), Value::String(cwd.to_owned()));
        }
        self.session_lists.insert(
            id_key(&Value::String(internal_id.clone())),
            SessionListRequest {
                client_id,
                query: params
                    .and_then(|p| p.get("query"))
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|q| !q.is_empty())
                    .map(str::to_lowercase),
                limit: params
                    .and_then(|p| p.get("limit"))
                    .and_then(Value::as_u64)
                    .and_then(|limit| usize::try_from(limit).ok()),
            },
        );
        let line = json!({
            "jsonrpc": "2.0",
            "id": internal_id,
            "method": "session/list",
            "params": list_params,
        })
        .to_string();
        if !self.cursor_authenticated {
            self.held_for_auth.push(line);
            return self.start_cursor_auth();
        }
        vec![Routed::ToAgent(line)]
    }

    /// The pager's answer to a forwarded session list: Cursor's sessions as picker rows, or none when Cursor
    /// failed (logged out, for example) so the picker shows an empty list instead of an error.
    fn session_list_reply(
        &mut self,
        internal_id: &Value,
        response: Option<&Value>,
    ) -> Option<String> {
        let request = self.session_lists.remove(&id_key(internal_id))?;
        if let Some(error) = response.and_then(|r| r.get("error")) {
            tracing::warn!(%error, "cursor backend: session/list failed");
        }
        let sessions: Vec<Value> = response
            .and_then(|r| r.pointer("/result/sessions"))
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|session| {
                let session_id = session.get("sessionId").and_then(Value::as_str)?;
                let title = session
                    .get("title")
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|t| !t.is_empty())
                    .unwrap_or("Cursor chat");
                if let Some(query) = &request.query
                    && !title.to_lowercase().contains(query)
                {
                    return None;
                }
                let mut row = Map::new();
                row.insert("sessionId".to_owned(), Value::String(session_id.to_owned()));
                row.insert("summary".to_owned(), Value::String(title.to_owned()));
                row.insert("source".to_owned(), Value::String("local".to_owned()));
                for key in ["cwd", "updatedAt"] {
                    if let Some(value) = session.get(key).filter(|v| v.is_string()) {
                        row.insert(key.to_owned(), value.clone());
                    }
                }
                Some(Value::Object(row))
            })
            .take(request.limit.unwrap_or(usize::MAX))
            .collect();
        Some(
            json!({
                "jsonrpc": "2.0",
                "id": request.client_id,
                "result": { "sessions": sessions },
            })
            .to_string(),
        )
    }

    /// The catalog request for `method`, resolved against the session it names (or the only session).
    fn catalog_request(
        &self,
        id: Value,
        method: &'static str,
        params: Option<&Value>,
    ) -> CatalogRequest {
        let params = params.cloned().unwrap_or_else(|| json!({}));
        let session_id = params
            .get("sessionId")
            .and_then(Value::as_str)
            .filter(|id| self.sessions.contains_key(*id))
            .map(str::to_owned)
            .or_else(|| {
                (self.sessions.len() == 1)
                    .then(|| self.sessions.keys().next().cloned())
                    .flatten()
            });
        let cwd = session_id
            .as_deref()
            .and_then(|id| self.sessions.get(id))
            .and_then(|session| session.cwd.clone())
            .map(PathBuf::from)
            .or_else(|| std::env::current_dir().ok())
            .unwrap_or_default();
        let mut sessions: Vec<(String, Vec<Value>)> = self
            .sessions
            .iter()
            .map(|(id, session)| (id.clone(), session.commands.clone()))
            .collect();
        sessions.sort_by(|a, b| a.0.cmp(&b.0));
        CatalogRequest {
            id,
            method,
            params,
            cwd,
            session_id,
            sessions,
        }
    }

    #[cfg(test)]
    pub(crate) fn with_disabled_skills(mut self, disabled: fn() -> HashSet<String>) -> Self {
        self.disabled_skills = Some(disabled);
        self
    }

    fn remember_model(&mut self, session_id: &str, model_id: &str) {
        let trimmed = model_id.trim();
        if trimmed.is_empty() {
            return;
        }
        self.sessions
            .entry(session_id.to_owned())
            .or_default()
            .model = Some(trimmed.to_owned());
    }

    fn record_models(&mut self, result: &mut Map<String, Value>) {
        let config_options = result
            .get("configOptions")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let (config_windows, config_efforts) = extract_config_option_hints(&config_options);

        let Some(available) = result
            .get_mut("models")
            .and_then(|m| m.get_mut("availableModels"))
            .and_then(Value::as_array_mut)
        else {
            return;
        };

        #[derive(Default)]
        struct BaseSummary {
            windows: Vec<u64>,
            efforts: Vec<String>,
            has_fast: bool,
            has_max: bool,
        }

        let mut models = Vec::new();
        let mut by_base: HashMap<String, BaseSummary> = HashMap::new();

        for m in available.iter() {
            let Some(id) = m.get("modelId").and_then(Value::as_str) else {
                continue;
            };
            let name = m
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned();
            models.push(CursorModel {
                id: id.to_owned(),
                name,
            });
            if let Some(spec) = parse_parameterized_model_spec(id) {
                let summary = by_base
                    .entry(spec.base_model_id.to_ascii_lowercase())
                    .or_default();
                if let Some(w) = spec.context_window
                    && !summary.windows.contains(&w.get())
                {
                    summary.windows.push(w.get());
                }
                if let Some(eff) = spec.effort {
                    let s = eff.to_string();
                    if !summary.efforts.contains(&s) {
                        summary.efforts.push(s);
                    }
                }
                if spec.fast_mode.is_some() {
                    summary.has_fast = true;
                }
                if spec.max_mode.is_some() {
                    summary.has_max = true;
                }
            }
        }

        for m in available.iter_mut() {
            let Some(obj) = m.as_object_mut() else {
                continue;
            };
            let Some(id) = obj
                .get("modelId")
                .and_then(Value::as_str)
                .map(str::to_owned)
            else {
                continue;
            };
            let Some(spec) = parse_parameterized_model_spec(&id) else {
                continue;
            };
            let summary = by_base.get(&spec.base_model_id.to_ascii_lowercase());
            let meta = meta_mut(obj);

            if let Some(cw) = spec.context_window {
                meta.entry("totalContextTokens")
                    .or_insert_with(|| json!(cw.get()));
                meta.entry(CONTEXT_WINDOW_META_KEY)
                    .or_insert_with(|| json!(cw.get()));
            }

            let mut windows = summary.map(|s| s.windows.clone()).unwrap_or_default();
            for &w in &config_windows {
                if !windows.contains(&w) {
                    windows.push(w);
                }
            }
            if windows.len() == 1 && spec.context_window.is_some() {
                let alt = if windows[0] == 1_000_000 {
                    200_000
                } else {
                    1_000_000
                };
                windows.push(alt);
            }
            if windows.len() > 1 {
                meta.entry(CONTEXT_WINDOWS_META_KEY)
                    .or_insert_with(|| json!(windows));
            }

            let has_efforts = summary.is_some_and(|s| !s.efforts.is_empty())
                || !config_efforts.is_empty()
                || spec.effort.is_some()
                || spec.has_brackets;
            if has_efforts {
                meta.entry(SUPPORTS_REASONING_EFFORT_META_KEY)
                    .or_insert(Value::Bool(true));
            }
            if let Some(effort) = spec.effort {
                meta.entry(REASONING_EFFORT_META_KEY)
                    .or_insert_with(|| json!(effort.to_string()));
            }
            let mut efforts = summary.map(|s| s.efforts.clone()).unwrap_or_default();
            for eff in &config_efforts {
                if !efforts.contains(eff) {
                    efforts.push(eff.clone());
                }
            }
            if efforts.len() > 1 {
                let effort_objs: Vec<Value> = efforts
                    .iter()
                    .map(|e| json!({ "id": e, "value": e, "label": e }))
                    .collect();
                meta.entry(REASONING_EFFORTS_META_KEY)
                    .or_insert_with(|| Value::Array(effort_objs));
            }

            if spec.fast_mode.is_some() || summary.is_some_and(|s| s.has_fast) || spec.has_brackets
            {
                meta.entry(SUPPORTS_FAST_MODE_META_KEY)
                    .or_insert(Value::Bool(true));
            }
            if let Some(fast) = spec.fast_mode {
                meta.entry(FAST_MODE_META_KEY).or_insert(Value::Bool(fast));
            }

            if spec.max_mode.is_some()
                || summary.is_some_and(|s| s.has_max)
                || spec.context_window.is_some()
                || spec.has_brackets
            {
                meta.entry(SUPPORTS_MAX_MODE_META_KEY)
                    .or_insert(Value::Bool(true));
            }
            if let Some(max_mode) = spec.max_mode {
                meta.entry(MAX_MODE_META_KEY)
                    .or_insert(Value::Bool(max_mode));
            }

            if !spec.raw_params.is_empty() {
                meta.entry(MODEL_PARAMETERS_META_KEY)
                    .or_insert_with(|| model_parameters_meta_value(&spec.raw_params));
            }
        }

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
        let resolved = self.resolve_model_with_meta(requested, None)?;
        self.remember_model(session_id, &resolved);
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

    /// Resolve a model id and apply any `_meta` or bracket parameter overrides (`context`, `effort`, `fast`, `maxMode`, `modelParameters`).
    fn resolve_model_with_meta(
        &self,
        requested: &str,
        meta: Option<&Map<String, Value>>,
    ) -> Option<String> {
        let requested = requested.trim();
        if requested.is_empty() {
            return None;
        }
        let has_meta_overrides = meta.is_some_and(|m| {
            parse_context_window_meta(Some(m)).is_some()
                || parse_reasoning_effort_meta(Some(m)).is_some()
                || parse_fast_mode_meta(Some(m)).is_some()
                || parse_max_mode_meta(Some(m)).is_some()
                || parse_model_parameters_meta(Some(m)).is_some_and(|p| !p.is_empty())
        });
        let spec = parse_parameterized_model_spec(requested)?;
        if !spec.has_brackets && !has_meta_overrides {
            return self.resolve_model(requested);
        }

        let catalog_match = self
            .models
            .iter()
            .find(|m| m.id.eq_ignore_ascii_case(requested))
            .or_else(|| {
                self.models.iter().find(|m| {
                    model_base(&m.id).eq_ignore_ascii_case(&spec.base_model_id)
                        || m.name.eq_ignore_ascii_case(&spec.base_model_id)
                })
            });

        let canonical_base = catalog_match
            .map(|m| model_base(&m.id).to_owned())
            .unwrap_or_else(|| spec.base_model_id.clone());

        let mut params: Vec<(String, String)> = catalog_match
            .and_then(|m| parse_parameterized_model_spec(&m.id))
            .map(|s| s.raw_params)
            .unwrap_or_default();

        for (k, v) in &spec.raw_params {
            upsert_model_param(&mut params, &[k.as_str()], k, v.clone());
        }

        if let Some(meta) = meta {
            if let Some(cw) = parse_context_window_meta(Some(meta)) {
                upsert_model_param(
                    &mut params,
                    &["context", "contextWindow", "context_window"],
                    "context",
                    format_context_window_param(cw.get()),
                );
            }
            if let Some(effort) = parse_reasoning_effort_meta(Some(meta)) {
                upsert_model_param(
                    &mut params,
                    &[
                        "effort",
                        "reasoning",
                        "reasoningEffort",
                        "reasoning_effort",
                        "thinking",
                    ],
                    "effort",
                    effort.to_string(),
                );
            }
            if let Some(fast) = parse_fast_mode_meta(Some(meta)) {
                upsert_model_param(
                    &mut params,
                    &["fast", "fastMode", "fast_mode", "speed"],
                    "fast",
                    if fast { "true" } else { "false" }.to_owned(),
                );
            }
            if let Some(max_mode) = parse_max_mode_meta(Some(meta)) {
                upsert_model_param(
                    &mut params,
                    &["maxMode", "max_mode", "max"],
                    "maxMode",
                    if max_mode { "true" } else { "false" }.to_owned(),
                );
            }
            if let Some(meta_params) = parse_model_parameters_meta(Some(meta)) {
                for (k, v) in meta_params {
                    upsert_model_param(&mut params, &[k.as_str()], &k, v);
                }
            }
        }

        if params.is_empty() {
            return catalog_match
                .map(|m| m.id.clone())
                .or_else(|| Some(canonical_base));
        }

        // Prefer an exact catalog variant when its parameter set matches.
        if let Some(existing) = self.models.iter().find(|m| {
            model_base(&m.id).eq_ignore_ascii_case(&canonical_base)
                && parse_parameterized_model_spec(&m.id)
                    .is_some_and(|s| params_match(&s.raw_params, &params))
        }) {
            return Some(existing.id.clone());
        }

        let formatted_params = params
            .iter()
            .map(|(k, v)| format!("{k}={v}"))
            .collect::<Vec<_>>()
            .join(",");
        Some(format!("{canonical_base}[{formatted_params}]"))
    }
}

fn upsert_model_param(
    params: &mut Vec<(String, String)>,
    aliases: &[&str],
    default_key: &str,
    value: String,
) {
    if let Some((_, existing_val)) = params
        .iter_mut()
        .find(|(k, _)| aliases.iter().any(|a| k.eq_ignore_ascii_case(a)))
    {
        *existing_val = value;
    } else {
        params.push((default_key.to_owned(), value));
    }
}

fn params_match(a: &[(String, String)], b: &[(String, String)]) -> bool {
    a.len() == b.len()
        && a.iter().all(|(ka, va)| {
            b.iter()
                .any(|(kb, vb)| ka.eq_ignore_ascii_case(kb) && va.eq_ignore_ascii_case(vb))
        })
}

fn extract_config_option_hints(config_options: &[Value]) -> (Vec<u64>, Vec<String>) {
    let mut windows = Vec::new();
    let mut efforts = Vec::new();
    for opt in config_options {
        let category = opt
            .get("category")
            .or_else(|| opt.get("id"))
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_ascii_lowercase();
        let values = opt
            .get("options")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        for val in values {
            let raw = val
                .get("value")
                .or_else(|| val.get("id"))
                .and_then(Value::as_str)
                .unwrap_or_default();
            if category.contains("context") {
                if let Some(w) = parse_context_window_token(raw)
                    && !windows.contains(&w.get())
                {
                    windows.push(w.get());
                }
            } else if category.contains("thought")
                || category.contains("effort")
                || category.contains("reasoning")
            {
                if let Some(eff) = parse_canonical_effort_token(raw) {
                    let s = eff.to_string();
                    if !efforts.contains(&s) {
                        efforts.push(s);
                    }
                }
            }
        }
    }
    (windows, efforts)
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

fn cloud_request(task: Option<String>) -> CloudRequest {
    let Some(task) = task else {
        return CloudRequest::Refresh;
    };
    if task.is_empty() || task.eq_ignore_ascii_case("status") {
        CloudRequest::Refresh
    } else if task.eq_ignore_ascii_case("cancel") {
        CloudRequest::Cancel
    } else {
        CloudRequest::Send(task)
    }
}

fn cursor_auth_failure(msg: &Value) -> String {
    let detail = msg
        .pointer("/error/message")
        .and_then(Value::as_str)
        .unwrap_or("Cursor authentication failed.");
    format!("{detail} Run `agent login` if this did not open a browser.")
}

fn rewrite_modes(result: &mut Map<String, Value>) {
    let Some(modes) = result.get_mut("modes").and_then(Value::as_object_mut) else {
        return;
    };
    if let Some(current) = modes.get_mut("currentModeId") {
        protocol::rewrite_mode_value(current, true);
    }
    if let Some(available) = modes
        .get_mut("availableModes")
        .and_then(Value::as_array_mut)
    {
        for mode in available {
            if let Some(id) = mode.get_mut("id") {
                protocol::rewrite_mode_value(id, true);
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
