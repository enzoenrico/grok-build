//! JSON-RPC line rewriter between the pager's ACP client and `cursor-agent acp`.
//!
//! The pager speaks grok-shell's ACP dialect: `x.ai/*` extension methods, a client-generated `promptId` echoed on
//! every notification, grok model ids (`grok-4.7`) and the `default` session mode. Cursor speaks plain ACP with its own
//! parameterized model ids (`grok-4.7[context=256k,...]`) and an `agent` mode. This adapter translates between the two
//! one line at a time so neither side needs to know about the other.

use std::collections::{HashMap, HashSet};

use serde_json::{Map, Value, json};

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
        /// `agent` or `plan` when this handoff sends a task. Ask and other pager modes are omitted.
        mode: Option<String>,
        /// `session/prompt` id to answer when the cloud handoff is done.
        prompt_rpc_id: Option<Value>,
        prompt_id: Option<String>,
    },
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
}

#[derive(Debug, Default)]
struct CursorSession {
    skills: HashSet<String>,
    cloud_agent_id: Option<String>,
    cwd: Option<String>,
    /// Last Cursor conversation mode the cloud API accepts (`agent` or `plan`).
    mode: Option<String>,
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
                if let Some(model) = msg.pointer_mut("/params/modelId")
                    && let Some(requested) = model.as_str()
                    && let Some(resolved) = self.resolve_model(requested)
                {
                    self.preferred_model = Some(resolved.clone());
                    *model = Value::String(resolved);
                }
            }
            "session/set_mode" => {
                if let Some(mode) = msg.pointer_mut("/params/modeId") {
                    protocol::rewrite_mode_value(mode, false);
                }
                self.note_outbound_mode(&msg);
            }
            "session/set_config_option" => {
                if msg.pointer("/params/configId").and_then(Value::as_str) == Some("mode")
                    && let Some(value) = msg.pointer_mut("/params/value")
                {
                    protocol::rewrite_mode_value(value, false);
                }
                self.note_outbound_mode(&msg);
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
                        cloud_task = protocol::cloud_task_text(params);
                        if let Some(task) = cloud_task.as_mut() {
                            let control = task.is_empty()
                                || task.eq_ignore_ascii_case("status")
                                || task.eq_ignore_ascii_case("cancel");
                            if !control
                                && let Some(hoisted) = protocol::hoist_skill_text(task, &skills)
                            {
                                *task = hoisted;
                            }
                        }
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
                    return vec![Routed::Cloud {
                        session_id: session_id.clone(),
                        repo: self.cloud_repo(cloud_agent_id.as_deref(), &session_id),
                        mode: self.cloud_mode(&session_id, &request),
                        agent_id: cloud_agent_id,
                        request,
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
        if matches!(method.as_str(), "session/new" | "session/load") && !self.cursor_authenticated
        {
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
                protocol::normalize_plan_session_update(&mut msg);
                if !self.stamp_session_update(&mut msg) {
                    return Vec::new();
                }
                return vec![Routed::ToClient(msg.to_string())];
            }
            if let Some(translated) = protocol::translate_cursor_inbound(
                &msg,
                self.inbound_session_id(&msg).as_deref(),
            ) {
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
                        if let Some(mode) = result
                            .get("modes")
                            .and_then(|modes| modes.get("currentModeId"))
                            .and_then(Value::as_str)
                        {
                            self.remember_cloud_mode(session_id, mode);
                        }
                        let session = self.sessions.entry(session_id.clone()).or_default();
                        if session.cwd.is_none() {
                            session.cwd = cwd;
                        }
                    }
                    rewrite_modes(result);
                    protocol::rewrite_mode_config_options(result, true);
                    protocol::absorb_cursor_skills(result);
                    if let Some(session_id) = &session_id {
                        self.remember_skills(session_id, result.get("availableCommands"));
                    }
                    self.record_models(result);
                    if let (Some(session_id), Some(requested)) = (session_id, requested_model)
                        && let Some(set_model) =
                            self.apply_requested_model(result, &session_id, &requested)
                    {
                        out.push(Routed::ToAgent(set_model));
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
        if let Some(result) = msg.get_mut("result").and_then(Value::as_object_mut) {
            protocol::rewrite_mode_config_options(result, true);
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
        if update.get("sessionUpdate").and_then(Value::as_str) == Some("current_mode_update")
            && let Some(mode) = update.get("currentModeId").and_then(Value::as_str)
            && let Some(session_id) = session_id.as_deref()
        {
            self.remember_cloud_mode(session_id, mode);
        }
        if update.get("sessionUpdate").and_then(Value::as_str) == Some("available_commands_update")
        {
            protocol::normalize_commands(update.get_mut("availableCommands"));
            if let Some(session_id) = session_id {
                self.remember_skills(&session_id, update.get("availableCommands"));
            }
        }
    }

    fn note_outbound_mode(&mut self, msg: &Value) {
        let Some(session_id) = msg.pointer("/params/sessionId").and_then(Value::as_str) else {
            return;
        };
        let mode = if msg.get("method").and_then(Value::as_str) == Some("session/set_mode") {
            msg.pointer("/params/modeId").and_then(Value::as_str)
        } else if msg.pointer("/params/configId").and_then(Value::as_str) == Some("mode") {
            msg.pointer("/params/value").and_then(Value::as_str)
        } else {
            None
        };
        if let Some(mode) = mode {
            self.remember_cloud_mode(session_id, mode);
        }
    }

    fn remember_cloud_mode(&mut self, session_id: &str, mode_id: &str) {
        let mode = protocol::mode_to_cursor(mode_id);
        if mode != "agent" && mode != "plan" {
            return;
        }
        self.sessions
            .entry(session_id.to_owned())
            .or_default()
            .mode = Some(mode.to_owned());
    }

    fn cloud_mode(&self, session_id: &str, request: &CloudRequest) -> Option<String> {
        match request {
            CloudRequest::Send(_) => self
                .sessions
                .get(session_id)
                .and_then(|session| session.mode.clone()),
            CloudRequest::Refresh | CloudRequest::Cancel => None,
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
        if let Some(session_id) = msg
            .pointer("/params/sessionId")
            .and_then(Value::as_str)
        {
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

    fn remember_skills(&mut self, session_id: &str, commands: Option<&Value>) {
        self.sessions
            .entry(session_id.to_owned())
            .or_default()
            .skills
            .extend(protocol::skill_names(commands));
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
