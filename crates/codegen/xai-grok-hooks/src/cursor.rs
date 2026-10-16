//! Cursor `hooks.json` dialect.
//!
//! Cursor's files (`~/.cursor/hooks.json`, `<project>/.cursor/hooks.json`, the enterprise file, and
//! plugin `hooks/hooks.json`) use flat entries per camelCase event (`{"command", "matcher",
//! "timeout", "failClosed", "loop_limit"}`) instead of Claude's matcher groups, and their scripts read
//! and write Cursor's snake_case JSON. Each entry becomes a [`HookSpec`] on the closest grok event,
//! tagged with a [`CursorHook`] so the dispatcher applies Cursor's per-event tool filter and matcher
//! target and the runner speaks Cursor's stdin/stdout protocol.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{LazyLock, Mutex};

use serde::{Deserialize, Serialize};

use crate::config::{HandlerType, HookProvenance, HookSpec};
use crate::error::HookError;
use crate::event::{
    GateKind, HookEventEnvelope, HookEventName, HookPayload, MAX_HOOK_FEEDBACK_CHARS, clip_reason,
    clip_text,
};
use crate::result::{OutputReplacement, PostToolUseHookOutcome, ReplacementKind, StopHookOutcome};
use crate::runner::HookRunnerResult;

/// Reported as `cursor_version` / `CURSOR_VERSION` so scripts can tell grok apart from Cursor.
pub const CURSOR_VERSION_LABEL: &str = "grok-build";

/// Cursor's default per-script cap on `stop` / `subagentStop` follow-ups.
pub const DEFAULT_LOOP_LIMIT: u32 = 5;

/// `beforeReadFile` sends the file body on stdin; larger files are clipped so one read can't stall the hook.
const MAX_READ_CONTENT_BYTES: u64 = 2 * 1024 * 1024;

/// Cursor events with no grok lifecycle point; entries under them are skipped with a warning.
const UNSUPPORTED_EVENTS: &[(&str, &str)] = &[
    (
        "afterAgentThought",
        "grok does not surface finished thinking blocks to hooks",
    ),
    (
        "beforeTabFileRead",
        "Tab completions are a Cursor editor feature",
    ),
    (
        "afterTabFileEdit",
        "Tab completions are a Cursor editor feature",
    ),
    (
        "workspaceOpen",
        "grok has no workspace-open lifecycle outside a session",
    ),
];

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum CursorEvent {
    SessionStart,
    SessionEnd,
    PreToolUse,
    PostToolUse,
    PostToolUseFailure,
    SubagentStart,
    SubagentStop,
    BeforeShellExecution,
    AfterShellExecution,
    BeforeMcpExecution,
    AfterMcpExecution,
    BeforeReadFile,
    AfterFileEdit,
    BeforeSubmitPrompt,
    PreCompact,
    Stop,
    AfterAgentResponse,
}

impl CursorEvent {
    pub const ALL: &'static [CursorEvent] = &[
        Self::SessionStart,
        Self::SessionEnd,
        Self::PreToolUse,
        Self::PostToolUse,
        Self::PostToolUseFailure,
        Self::SubagentStart,
        Self::SubagentStop,
        Self::BeforeShellExecution,
        Self::AfterShellExecution,
        Self::BeforeMcpExecution,
        Self::AfterMcpExecution,
        Self::BeforeReadFile,
        Self::AfterFileEdit,
        Self::BeforeSubmitPrompt,
        Self::PreCompact,
        Self::Stop,
        Self::AfterAgentResponse,
    ];

    /// The key as written in `hooks.json` and sent as `hook_event_name`.
    pub fn key(self) -> &'static str {
        match self {
            Self::SessionStart => "sessionStart",
            Self::SessionEnd => "sessionEnd",
            Self::PreToolUse => "preToolUse",
            Self::PostToolUse => "postToolUse",
            Self::PostToolUseFailure => "postToolUseFailure",
            Self::SubagentStart => "subagentStart",
            Self::SubagentStop => "subagentStop",
            Self::BeforeShellExecution => "beforeShellExecution",
            Self::AfterShellExecution => "afterShellExecution",
            Self::BeforeMcpExecution => "beforeMCPExecution",
            Self::AfterMcpExecution => "afterMCPExecution",
            Self::BeforeReadFile => "beforeReadFile",
            Self::AfterFileEdit => "afterFileEdit",
            Self::BeforeSubmitPrompt => "beforeSubmitPrompt",
            Self::PreCompact => "preCompact",
            Self::Stop => "stop",
            Self::AfterAgentResponse => "afterAgentResponse",
        }
    }

    /// Snake-case label for spec names, matching the `{event}[i].hooks[j]` shape the display code strips.
    fn label(self) -> &'static str {
        match self {
            Self::SessionStart => "session_start",
            Self::SessionEnd => "session_end",
            Self::PreToolUse => "pre_tool_use",
            Self::PostToolUse => "post_tool_use",
            Self::PostToolUseFailure => "post_tool_use_failure",
            Self::SubagentStart => "subagent_start",
            Self::SubagentStop => "subagent_stop",
            Self::BeforeShellExecution => "before_shell_execution",
            Self::AfterShellExecution => "after_shell_execution",
            Self::BeforeMcpExecution => "before_mcp_execution",
            Self::AfterMcpExecution => "after_mcp_execution",
            Self::BeforeReadFile => "before_read_file",
            Self::AfterFileEdit => "after_file_edit",
            Self::BeforeSubmitPrompt => "before_submit_prompt",
            Self::PreCompact => "pre_compact",
            Self::Stop => "stop",
            Self::AfterAgentResponse => "after_agent_response",
        }
    }

    pub fn from_key(key: &str) -> Option<Self> {
        Self::ALL.iter().copied().find(|event| event.key() == key)
    }

    /// The grok lifecycle event this Cursor event runs on.
    pub fn grok_event(self) -> HookEventName {
        match self {
            Self::SessionStart => HookEventName::SessionStart,
            Self::SessionEnd => HookEventName::SessionEnd,
            Self::PreToolUse
            | Self::BeforeShellExecution
            | Self::BeforeMcpExecution
            | Self::BeforeReadFile => HookEventName::PreToolUse,
            Self::PostToolUse
            | Self::AfterShellExecution
            | Self::AfterMcpExecution
            | Self::AfterFileEdit => HookEventName::PostToolUse,
            Self::PostToolUseFailure => HookEventName::PostToolUseFailure,
            Self::SubagentStart => HookEventName::SubagentStart,
            Self::SubagentStop => HookEventName::SubagentStop,
            Self::BeforeSubmitPrompt => HookEventName::UserPromptSubmit,
            Self::PreCompact => HookEventName::PreCompact,
            Self::Stop | Self::AfterAgentResponse => HookEventName::Stop,
        }
    }

    /// Events whose response is a permission decision; Cursor blocks these on invalid output.
    fn is_permission(self) -> bool {
        matches!(
            self,
            Self::PreToolUse
                | Self::BeforeShellExecution
                | Self::BeforeMcpExecution
                | Self::BeforeReadFile
        )
    }
}

/// The Cursor-specific parts of a [`HookSpec`] loaded from a Cursor-format file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CursorHook {
    pub event: CursorEvent,
    /// Cursor regex, tested against the event's own target (tool name, command, subagent type, or a fixed value).
    pub matcher: Option<String>,
    pub fail_closed: bool,
    /// `None` means no cap (`"loop_limit": null`).
    pub loop_limit: Option<u32>,
    /// Project hooks run from the project root, user hooks from `~/.cursor`.
    pub working_dir: PathBuf,
}

/// Cursor's tool vocabulary for `tool_name` and `preToolUse` matchers.
#[derive(Debug, Clone, PartialEq, Eq)]
enum CursorTool<'a> {
    Shell,
    Read,
    Write,
    Grep,
    Glob,
    Delete,
    Task,
    WebFetch,
    WebSearch,
    Mcp { server: &'a str, tool: &'a str },
    Other(&'a str),
}

impl<'a> CursorTool<'a> {
    fn classify(grok_name: &'a str) -> Self {
        if let Some((server, tool)) = grok_name.split_once("__")
            && !server.is_empty()
            && !tool.is_empty()
        {
            return Self::Mcp { server, tool };
        }
        match grok_name {
            "bash" | "shell" | "exec_command" | "run_terminal_cmd" | "run_terminal_command" => {
                return Self::Shell;
            }
            "read" | "read_file" | "hashline_read" => return Self::Read,
            "write" | "edit" | "edit_file" | "search_replace" | "hashline_edit" | "apply_patch"
            | "multi_edit" => return Self::Write,
            "grep" | "grep_files" | "hashline_grep" => return Self::Grep,
            "glob" | "list_dir" => return Self::Glob,
            "delete" | "delete_file" => return Self::Delete,
            "task" | "spawn_subagent" => return Self::Task,
            "web_fetch" => return Self::WebFetch,
            "web_search" => return Self::WebSearch,
            _ => {}
        }
        for claude in xai_grok_tools::types::claude_names_for(grok_name) {
            match claude {
                "Bash" | "PowerShell" => return Self::Shell,
                "Read" => return Self::Read,
                "Write" | "Edit" | "MultiEdit" | "NotebookEdit" => return Self::Write,
                "Grep" => return Self::Grep,
                "Glob" | "LS" => return Self::Glob,
                "Agent" | "Task" => return Self::Task,
                _ => {}
            }
        }
        Self::Other(grok_name)
    }

    fn cursor_name(&self) -> String {
        match self {
            Self::Shell => "Shell".into(),
            Self::Read => "Read".into(),
            Self::Write => "Write".into(),
            Self::Grep => "Grep".into(),
            Self::Glob => "Glob".into(),
            Self::Delete => "Delete".into(),
            Self::Task => "Task".into(),
            Self::WebFetch => "WebFetch".into(),
            Self::WebSearch => "WebSearch".into(),
            Self::Mcp { tool, .. } => format!("MCP:{tool}"),
            Self::Other(name) => (*name).to_string(),
        }
    }
}

/// A Cursor document has a top-level `version`, or entries that are handlers rather than matcher groups.
pub fn is_cursor_document(top_level: &serde_json::Value) -> bool {
    if top_level
        .get("version")
        .is_some_and(serde_json::Value::is_number)
    {
        return true;
    }
    let Some(events) = top_level
        .get("hooks")
        .and_then(serde_json::Value::as_object)
    else {
        return false;
    };
    events
        .values()
        .filter_map(serde_json::Value::as_array)
        .flatten()
        .filter_map(serde_json::Value::as_object)
        .any(|entry| !entry.contains_key("hooks") && entry.contains_key("command"))
}

/// Where Cursor runs a hook file's commands: the project root for `<project>/.cursor/hooks.json`,
/// otherwise the file's own directory (`~/.cursor`, the enterprise dir, a plugin's `hooks/`).
pub fn working_dir_for(file_path: &Path, home: Option<&Path>) -> PathBuf {
    let parent = file_path.parent().unwrap_or(Path::new("."));
    let is_dot_cursor = parent.file_name().is_some_and(|name| name == ".cursor");
    let is_user_dir = home.is_some_and(|home| parent == home.join(".cursor"));
    if is_dot_cursor
        && !is_user_dir
        && let Some(project_root) = parent.parent()
    {
        return project_root.to_path_buf();
    }
    parent.to_path_buf()
}

/// Parse a Cursor-format document into specs. `name_prefix` labels specs like the Claude parser does.
pub fn parse_document(
    top_level: &serde_json::Value,
    file_path: &Path,
    working_dir: &Path,
    name_prefix: &str,
    provenance: HookProvenance,
) -> (Vec<HookSpec>, Vec<HookError>) {
    let mut specs = Vec::new();
    let mut errors = Vec::new();
    let Some(events) = top_level
        .get("hooks")
        .and_then(serde_json::Value::as_object)
    else {
        return (specs, errors);
    };

    let mut keys: Vec<&String> = events.keys().collect();
    keys.sort();
    for key in keys {
        let Some(entries) = events.get(key.as_str()) else {
            continue;
        };
        let Some(event) = CursorEvent::from_key(key) else {
            match UNSUPPORTED_EVENTS.iter().find(|(name, _)| name == key) {
                Some((_, why)) => tracing::warn!(
                    file = %file_path.display(),
                    event = %key,
                    "cursor hooks: skipping event grok cannot run ({why})"
                ),
                None => tracing::warn!(
                    file = %file_path.display(),
                    event = %key,
                    "cursor hooks: skipping unrecognized event name (check for typos)"
                ),
            }
            continue;
        };
        let Some(entries) = entries.as_array() else {
            errors.push(HookError::ParseFile {
                path: file_path.to_path_buf(),
                detail: format!("cursor hooks: '{key}' must be an array of hook definitions"),
            });
            continue;
        };
        for (idx, entry) in entries.iter().enumerate() {
            let name = format!("{name_prefix}:{}[{idx}].hooks[0]", event.label());
            match build_spec(entry, event, name, file_path, working_dir, provenance) {
                Ok(spec) => specs.push(spec),
                Err(e) => errors.push(e),
            }
        }
    }
    (specs, errors)
}

fn build_spec(
    entry: &serde_json::Value,
    event: CursorEvent,
    name: String,
    file_path: &Path,
    working_dir: &Path,
    provenance: HookProvenance,
) -> Result<HookSpec, HookError> {
    let invalid = |detail: String| HookError::InvalidConfig {
        name: name.clone(),
        path: file_path.to_path_buf(),
        detail,
    };
    let Some(entry) = entry.as_object() else {
        return Err(invalid("cursor hook definition must be an object".into()));
    };
    let handler_type = entry
        .get("type")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("command");
    if handler_type != "command" {
        return Err(HookError::UnsupportedHandlerType {
            name,
            path: file_path.to_path_buf(),
            handler_type: handler_type.to_string(),
        });
    }
    let Some(command) = entry
        .get("command")
        .and_then(serde_json::Value::as_str)
        .filter(|c| !c.trim().is_empty())
    else {
        return Err(invalid("cursor hook requires a 'command' string".into()));
    };

    let matcher = entry
        .get("matcher")
        .and_then(serde_json::Value::as_str)
        .filter(|m| !m.is_empty() && *m != "*")
        .map(str::to_string);
    if let Some(pattern) = &matcher {
        regex::Regex::new(pattern).map_err(|source| HookError::InvalidMatcher {
            name: name.clone(),
            path: file_path.to_path_buf(),
            source,
        })?;
    }

    let timeout_ms = match entry.get("timeout").and_then(serde_json::Value::as_f64) {
        Some(secs) if secs > 0.0 => (secs * 1000.0).round() as u64,
        _ => crate::config::default_timeout_ms(event.grok_event()),
    };
    let fail_closed = entry
        .get("failClosed")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false);
    let loop_limit = match entry.get("loop_limit") {
        None => Some(DEFAULT_LOOP_LIMIT),
        Some(serde_json::Value::Null) => None,
        Some(value) => match value.as_u64() {
            Some(n) => Some(u32::try_from(n).unwrap_or(u32::MAX)),
            None => return Err(invalid("'loop_limit' must be a number or null".into())),
        },
    };

    let expanded = crate::config::expand_env_skipping_runner_vars(command);
    Ok(HookSpec {
        name,
        event: event.grok_event(),
        handler_type: HandlerType::Command,
        configured_matcher: matcher.clone(),
        matcher: None,
        enabled: true,
        command: Some(PathBuf::from(expanded)),
        command_raw: Some(command.to_string()),
        url: None,
        url_raw: None,
        timeout_ms,
        source_dir: working_dir.to_path_buf(),
        extra_env: HashMap::new(),
        layer: provenance,
        cursor: Some(CursorHook {
            event,
            matcher,
            fail_closed,
            loop_limit,
            working_dir: working_dir.to_path_buf(),
        }),
    })
}

fn str_field<'a>(input: &'a serde_json::Value, keys: &[&str]) -> Option<&'a str> {
    keys.iter()
        .find_map(|key| input.get(*key).and_then(serde_json::Value::as_str))
}

fn shell_command(input: &serde_json::Value) -> Option<&str> {
    str_field(input, &["command", "cmd"])
}

fn tool_file_path(input: &serde_json::Value, cwd: &str) -> Option<String> {
    let raw = str_field(
        input,
        &["file_path", "path", "target_file", "filePath", "file"],
    )?;
    let path = Path::new(raw);
    Some(if path.is_absolute() {
        raw.to_string()
    } else {
        Path::new(cwd).join(path).to_string_lossy().into_owned()
    })
}

fn json_text(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::String(text) => text.clone(),
        other => other.to_string(),
    }
}

fn shell_output(result: &serde_json::Value) -> String {
    if let Some(text) = result.as_str() {
        return text.to_string();
    }
    match str_field(result, &["output", "stdout", "content"]) {
        Some(text) => text.to_string(),
        None => result.to_string(),
    }
}

fn file_edits(input: &serde_json::Value) -> Vec<serde_json::Value> {
    let edit = |item: &serde_json::Value| {
        let old = str_field(item, &["old_string", "oldString", "old_str"]).unwrap_or_default();
        let new = str_field(
            item,
            &["new_string", "newString", "new_str", "content", "contents"],
        )
        .unwrap_or_default();
        serde_json::json!({ "old_string": old, "new_string": new })
    };
    match input.get("edits").and_then(serde_json::Value::as_array) {
        Some(items) => items.iter().map(edit).collect(),
        None => vec![edit(input)],
    }
}

fn read_capped(path: &str) -> String {
    use std::io::Read as _;
    let Ok(file) = std::fs::File::open(path) else {
        return String::new();
    };
    let mut bytes = Vec::new();
    if file
        .take(MAX_READ_CONTENT_BYTES)
        .read_to_end(&mut bytes)
        .is_err()
    {
        return String::new();
    }
    String::from_utf8_lossy(&bytes).into_owned()
}

fn failure_type(error: &str, is_interrupt: bool) -> &'static str {
    let lower = error.to_ascii_lowercase();
    if lower.contains("timed out") || lower.contains("timeout") {
        "timeout"
    } else if !is_interrupt && (lower.contains("denied") || lower.contains("rejected")) {
        "permission_denied"
    } else {
        "error"
    }
}

/// Per-(session, hook) count of follow-ups a `stop`/`subagentStop` hook has triggered in a row.
static LOOP_COUNTS: LazyLock<Mutex<HashMap<(String, String), u32>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// Env returned by `sessionStart` hooks, passed to every later Cursor hook in the session.
static SESSION_ENV: LazyLock<Mutex<HashMap<String, HashMap<String, String>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

pub fn session_env(session_id: &str) -> HashMap<String, String> {
    SESSION_ENV
        .lock()
        .ok()
        .and_then(|map| map.get(session_id).cloned())
        .unwrap_or_default()
}

fn record_session_env(session_id: &str, env: HashMap<String, String>) {
    if env.is_empty() {
        return;
    }
    if let Ok(mut map) = SESSION_ENV.lock() {
        map.entry(session_id.to_string()).or_default().extend(env);
    }
}

fn loop_key(envelope: &HookEventEnvelope, hook_name: &str) -> (String, String) {
    (envelope.session_id.clone(), hook_name.to_string())
}

/// The follow-up count for this stop; a stop that no hook forced starts the count over.
fn loop_count(envelope: &HookEventEnvelope, hook_name: &str) -> u32 {
    let forced = match &envelope.payload {
        HookPayload::Stop {
            stop_hook_active, ..
        } => *stop_hook_active,
        HookPayload::SubagentStop {
            stop_hook_active, ..
        } => stop_hook_active.unwrap_or(false),
        _ => return 0,
    };
    let Ok(mut counts) = LOOP_COUNTS.lock() else {
        return 0;
    };
    let key = loop_key(envelope, hook_name);
    if !forced {
        counts.remove(&key);
        return 0;
    }
    counts.get(&key).copied().unwrap_or(0)
}

fn bump_loop_count(envelope: &HookEventEnvelope, hook_name: &str) {
    if let Ok(mut counts) = LOOP_COUNTS.lock() {
        *counts.entry(loop_key(envelope, hook_name)).or_default() += 1;
    }
}

impl CursorHook {
    /// Whether this hook runs for `envelope`: the event's tool filter, then the matcher against Cursor's target.
    pub fn applies(&self, envelope: &HookEventEnvelope) -> bool {
        match self.match_target(envelope) {
            None => false,
            Some(None) => true,
            Some(Some(value)) => self.matcher_allows(&value),
        }
    }

    fn matcher_allows(&self, value: &str) -> bool {
        let Some(pattern) = &self.matcher else {
            return true;
        };
        match regex::Regex::new(pattern) {
            Ok(re) => re.is_match(value),
            Err(_) => false,
        }
    }

    /// `None`: the event does not apply. `Some(None)`: applies with nothing to match. `Some(Some(v))`: test `v`.
    fn match_target(&self, envelope: &HookEventEnvelope) -> Option<Option<String>> {
        let payload = &envelope.payload;
        let tool = payload.tool_name().map(CursorTool::classify);
        let tool_input = match payload {
            HookPayload::PreToolUse { tool_input, .. }
            | HookPayload::PostToolUse { tool_input, .. }
            | HookPayload::PostToolUseFailure { tool_input, .. } => Some(tool_input),
            _ => None,
        };
        match self.event {
            CursorEvent::PreToolUse
            | CursorEvent::PostToolUse
            | CursorEvent::PostToolUseFailure => Some(Some(tool?.cursor_name())),
            CursorEvent::BeforeShellExecution | CursorEvent::AfterShellExecution => {
                if tool? != CursorTool::Shell {
                    return None;
                }
                Some(Some(
                    shell_command(tool_input?).unwrap_or_default().to_string(),
                ))
            }
            CursorEvent::BeforeMcpExecution | CursorEvent::AfterMcpExecution => match tool? {
                CursorTool::Mcp { tool, .. } => Some(Some(tool.to_string())),
                _ => None,
            },
            CursorEvent::BeforeReadFile => {
                (tool? == CursorTool::Read).then(|| Some("Read".to_string()))
            }
            CursorEvent::AfterFileEdit => {
                (tool? == CursorTool::Write).then(|| Some("Write".to_string()))
            }
            CursorEvent::BeforeSubmitPrompt => Some(Some("UserPromptSubmit".into())),
            CursorEvent::Stop => Some(Some("Stop".into())),
            CursorEvent::AfterAgentResponse => Some(Some("AgentResponse".into())),
            CursorEvent::SubagentStart | CursorEvent::SubagentStop => {
                Some(payload.match_value().map(str::to_string))
            }
            CursorEvent::SessionStart | CursorEvent::SessionEnd | CursorEvent::PreCompact => {
                Some(None)
            }
        }
    }

    /// Cursor's stdin JSON for this hook: the common fields plus the event's own.
    pub fn stdin_json(&self, envelope: &HookEventEnvelope, hook_name: &str) -> serde_json::Value {
        use serde_json::json;
        let mut map = serde_json::Map::new();
        let session_id = envelope.session_id.as_str();
        let model = match &envelope.payload {
            HookPayload::SessionStart { model_id, .. } => model_id.clone().unwrap_or_default(),
            _ => String::new(),
        };
        map.insert("conversation_id".into(), json!(session_id));
        map.insert(
            "generation_id".into(),
            json!(envelope.prompt_id.as_deref().unwrap_or(session_id)),
        );
        map.insert("model".into(), json!(model));
        map.insert("hook_event_name".into(), json!(self.event.key()));
        map.insert("cursor_version".into(), json!(CURSOR_VERSION_LABEL));
        map.insert("workspace_roots".into(), json!([envelope.workspace_root]));
        map.insert("user_email".into(), serde_json::Value::Null);
        map.insert("transcript_path".into(), json!(envelope.transcript_path));
        map.insert("session_id".into(), json!(session_id));
        map.insert("cwd".into(), json!(envelope.cwd));

        let mut put = |key: &str, value: serde_json::Value| {
            map.insert(key.to_string(), value);
        };
        match (&self.event, &envelope.payload) {
            (
                CursorEvent::PreToolUse,
                HookPayload::PreToolUse {
                    tool_name,
                    tool_use_id,
                    tool_input,
                    ..
                },
            ) => {
                put(
                    "tool_name",
                    json!(CursorTool::classify(tool_name).cursor_name()),
                );
                put("tool_input", tool_input.clone());
                put("tool_use_id", json!(tool_use_id));
            }
            (
                CursorEvent::PostToolUse,
                HookPayload::PostToolUse {
                    tool_name,
                    tool_use_id,
                    tool_input,
                    tool_result,
                    duration_ms,
                    ..
                },
            ) => {
                put(
                    "tool_name",
                    json!(CursorTool::classify(tool_name).cursor_name()),
                );
                put("tool_input", tool_input.clone());
                put("tool_output", json!(json_text(tool_result)));
                put("tool_use_id", json!(tool_use_id));
                put("duration", json!(duration_ms.unwrap_or(0)));
            }
            (
                CursorEvent::PostToolUseFailure,
                HookPayload::PostToolUseFailure {
                    tool_name,
                    tool_use_id,
                    tool_input,
                    error,
                    duration_ms,
                    is_interrupt,
                    ..
                },
            ) => {
                put(
                    "tool_name",
                    json!(CursorTool::classify(tool_name).cursor_name()),
                );
                put("tool_input", tool_input.clone());
                put("tool_use_id", json!(tool_use_id));
                put("error_message", json!(error));
                put("failure_type", json!(failure_type(error, *is_interrupt)));
                put("duration", json!(duration_ms.unwrap_or(0)));
                put("is_interrupt", json!(is_interrupt));
            }
            (CursorEvent::BeforeShellExecution, HookPayload::PreToolUse { tool_input, .. }) => {
                put(
                    "command",
                    json!(shell_command(tool_input).unwrap_or_default()),
                );
                let cwd = str_field(tool_input, &["working_directory", "cwd", "workdir"])
                    .unwrap_or(&envelope.cwd);
                put("cwd", json!(cwd));
                put("sandbox", json!(false));
            }
            (
                CursorEvent::AfterShellExecution,
                HookPayload::PostToolUse {
                    tool_input,
                    tool_result,
                    duration_ms,
                    ..
                },
            ) => {
                put(
                    "command",
                    json!(shell_command(tool_input).unwrap_or_default()),
                );
                put("output", json!(shell_output(tool_result)));
                put("duration", json!(duration_ms.unwrap_or(0)));
                put("sandbox", json!(false));
            }
            (
                CursorEvent::BeforeMcpExecution,
                HookPayload::PreToolUse {
                    tool_name,
                    tool_input,
                    ..
                },
            ) => {
                if let CursorTool::Mcp { server, tool } = CursorTool::classify(tool_name) {
                    put("tool_name", json!(tool));
                    put("mcp_server_name", json!(server));
                }
                put("tool_input", json!(tool_input.to_string()));
            }
            (
                CursorEvent::AfterMcpExecution,
                HookPayload::PostToolUse {
                    tool_name,
                    tool_input,
                    tool_result,
                    duration_ms,
                    ..
                },
            ) => {
                if let CursorTool::Mcp { server, tool } = CursorTool::classify(tool_name) {
                    put("tool_name", json!(tool));
                    put("mcp_server_name", json!(server));
                }
                put("tool_input", json!(tool_input.to_string()));
                put("result_json", json!(json_text(tool_result)));
                put("duration", json!(duration_ms.unwrap_or(0)));
            }
            (CursorEvent::BeforeReadFile, HookPayload::PreToolUse { tool_input, .. }) => {
                let path = tool_file_path(tool_input, &envelope.cwd).unwrap_or_default();
                put("content", json!(read_capped(&path)));
                put("file_path", json!(path));
                put("attachments", json!([]));
            }
            (CursorEvent::AfterFileEdit, HookPayload::PostToolUse { tool_input, .. }) => {
                put(
                    "file_path",
                    json!(tool_file_path(tool_input, &envelope.cwd).unwrap_or_default()),
                );
                put("edits", json!(file_edits(tool_input)));
            }
            (CursorEvent::BeforeSubmitPrompt, HookPayload::UserPromptSubmit { prompt, .. }) => {
                put("prompt", json!(prompt.as_deref().unwrap_or_default()));
                put("attachments", json!([]));
            }
            (CursorEvent::Stop, HookPayload::Stop { .. }) => {
                put("status", json!("completed"));
                put("loop_count", json!(loop_count(envelope, hook_name)));
            }
            (
                CursorEvent::AfterAgentResponse,
                HookPayload::Stop {
                    last_assistant_message,
                    ..
                },
            ) => {
                put(
                    "text",
                    json!(last_assistant_message.as_deref().unwrap_or_default()),
                );
            }
            (CursorEvent::SessionStart, HookPayload::SessionStart { .. }) => {
                put("is_background_agent", json!(false));
                put("composer_mode", json!("agent"));
            }
            (CursorEvent::SessionEnd, HookPayload::SessionEnd { reason, .. }) => {
                put("reason", json!(reason));
                put("final_status", json!(reason));
                put("is_background_agent", json!(false));
            }
            (
                CursorEvent::SubagentStart,
                HookPayload::SubagentStart {
                    subagent_id,
                    subagent_type,
                    description,
                },
            ) => {
                put("subagent_id", json!(subagent_id));
                put("subagent_type", json!(subagent_type));
                put("task", json!(description.as_deref().unwrap_or_default()));
                put("parent_conversation_id", json!(session_id));
                put("is_parallel_worker", json!(false));
            }
            (
                CursorEvent::SubagentStop,
                HookPayload::SubagentStop {
                    subagent_type,
                    last_assistant_message,
                    ..
                },
            ) => {
                put("subagent_type", json!(subagent_type));
                put("status", json!("completed"));
                put(
                    "summary",
                    json!(last_assistant_message.as_deref().unwrap_or_default()),
                );
                put("loop_count", json!(loop_count(envelope, hook_name)));
                put("agent_transcript_path", json!(envelope.transcript_path));
            }
            (CursorEvent::PreCompact, HookPayload::PreCompact { source }) => {
                put("trigger", json!(source));
            }
            _ => {}
        }
        serde_json::Value::Object(map)
    }

    /// Translate a finished Cursor hook process into the runner result `mode` expects.
    pub fn parse_output(
        &self,
        mode: GateKind,
        stdout: &str,
        stderr: &str,
        exit_code: i32,
        hook_name: &str,
        envelope: &HookEventEnvelope,
    ) -> HookRunnerResult {
        let trimmed = stdout.trim();
        let document = (!trimmed.is_empty()).then(|| {
            serde_json::from_str::<serde_json::Value>(trimmed)
                .ok()
                .and_then(|value| match value {
                    serde_json::Value::Object(map) => Some(map),
                    _ => None,
                })
        });
        let text = |key: &str| {
            document
                .as_ref()
                .and_then(|doc| doc.as_ref())
                .and_then(|doc| doc.get(key))
                .and_then(serde_json::Value::as_str)
                .map(str::trim)
                .filter(|text| !text.is_empty())
                .map(str::to_string)
        };
        let stderr_line = stderr
            .lines()
            .map(str::trim)
            .find(|line| !line.is_empty())
            .map(str::to_string);
        let reason = || {
            clip_reason(
                &text("agent_message")
                    .or_else(|| text("user_message"))
                    .or_else(|| stderr_line.clone())
                    .unwrap_or_else(|| format!("Blocked by Cursor hook '{hook_name}'")),
            )
        };

        if exit_code == 2 {
            return match self.event {
                event if event.is_permission() => HookRunnerResult::Deny {
                    reason: reason(),
                    hook_name: hook_name.to_string(),
                },
                CursorEvent::BeforeSubmitPrompt => HookRunnerResult::Block {
                    reason: reason(),
                    hook_name: hook_name.to_string(),
                },
                _ => neutral(mode),
            };
        }
        if exit_code != 0 {
            let detail = match &stderr_line {
                Some(line) => format!("exit code {exit_code}: {}", clip_reason(line)),
                None => format!("exit code {exit_code}"),
            };
            return HookRunnerResult::Failed(detail);
        }

        let doc = match &document {
            None if self.fail_closed => {
                return HookRunnerResult::Failed("hook produced no output".into());
            }
            None => return neutral(mode),
            Some(None) if self.event.is_permission() => {
                return HookRunnerResult::Deny {
                    reason: format!(
                        "Cursor hook '{hook_name}' returned invalid JSON, so the action was blocked"
                    ),
                    hook_name: hook_name.to_string(),
                };
            }
            Some(None) => {
                return HookRunnerResult::Failed("hook output is not a JSON object".into());
            }
            Some(Some(doc)) => doc,
        };

        match self.event {
            event if event.is_permission() => {
                let permission = doc.get("permission").map(|value| value.as_str());
                let deny = || HookRunnerResult::Deny {
                    reason: reason(),
                    hook_name: hook_name.to_string(),
                };
                let allow = |updated_input| HookRunnerResult::Allow {
                    updated_input,
                    additional_context: None,
                };
                let updated_input = (self.event == CursorEvent::PreToolUse)
                    .then(|| {
                        doc.get("updated_input")
                            .and_then(|v| v.as_object())
                            .cloned()
                    })
                    .flatten();
                match permission {
                    None | Some(Some("allow")) => allow(updated_input),
                    Some(Some("deny")) => deny(),
                    Some(Some("ask")) => match self.event {
                        // Cursor's schema accepts `ask` on preToolUse but does not enforce it.
                        CursorEvent::PreToolUse => allow(updated_input),
                        CursorEvent::BeforeShellExecution | CursorEvent::BeforeMcpExecution => {
                            HookRunnerResult::Ask {
                                reason: text("user_message").map(|r| clip_reason(&r)),
                                updated_input: None,
                                additional_context: None,
                            }
                        }
                        _ => deny(),
                    },
                    Some(_) => HookRunnerResult::Deny {
                        reason: format!(
                            "Cursor hook '{hook_name}' returned an invalid 'permission', so the action was blocked"
                        ),
                        hook_name: hook_name.to_string(),
                    },
                }
            }
            CursorEvent::BeforeSubmitPrompt => {
                if doc.get("continue").and_then(serde_json::Value::as_bool) == Some(false) {
                    HookRunnerResult::Block {
                        reason: clip_reason(&text("user_message").unwrap_or_else(|| {
                            format!("Prompt blocked by Cursor hook '{hook_name}'")
                        })),
                        hook_name: hook_name.to_string(),
                    }
                } else {
                    HookRunnerResult::Success
                }
            }
            CursorEvent::Stop | CursorEvent::SubagentStop => {
                let Some(followup) = text("followup_message") else {
                    return neutral(mode);
                };
                let count = loop_count(envelope, hook_name);
                if self.loop_limit.is_some_and(|limit| count >= limit) {
                    tracing::info!(
                        hook_name,
                        loop_count = count,
                        "cursor stop hook reached its loop_limit; ignoring followup_message"
                    );
                    return neutral(mode);
                }
                bump_loop_count(envelope, hook_name);
                HookRunnerResult::Stop(StopHookOutcome {
                    block_reason: None,
                    additional_context: Some(clip_text(&followup, MAX_HOOK_FEEDBACK_CHARS)),
                    force_stop: None,
                })
            }
            CursorEvent::PostToolUse | CursorEvent::PostToolUseFailure => {
                let is_mcp = matches!(
                    envelope.payload.tool_name().map(CursorTool::classify),
                    Some(CursorTool::Mcp { .. })
                );
                let output_replacement = (self.event == CursorEvent::PostToolUse && is_mcp)
                    .then(|| doc.get("updated_mcp_tool_output").cloned())
                    .flatten()
                    .map(|value| OutputReplacement {
                        kind: ReplacementKind::Mcp,
                        hook_name: hook_name.to_string(),
                        value,
                    });
                HookRunnerResult::PostToolUse {
                    outcome: PostToolUseHookOutcome {
                        block_reason: None,
                        additional_context: text("additional_context")
                            .map(|context| clip_text(&context, MAX_HOOK_FEEDBACK_CHARS)),
                        output_replacement,
                    },
                    failure: None,
                }
            }
            CursorEvent::SessionStart => {
                let env: HashMap<String, String> = doc
                    .get("env")
                    .and_then(serde_json::Value::as_object)
                    .map(|env| {
                        env.iter()
                            .filter_map(|(k, v)| v.as_str().map(|v| (k.clone(), v.to_string())))
                            .collect()
                    })
                    .unwrap_or_default();
                record_session_env(&envelope.session_id, env);
                if text("additional_context").is_some() {
                    tracing::warn!(
                        hook_name,
                        "cursor sessionStart additional_context is not injected: grok session-start hooks cannot add model context"
                    );
                }
                neutral(mode)
            }
            CursorEvent::SubagentStart => {
                if doc.get("permission").and_then(serde_json::Value::as_str) == Some("deny") {
                    tracing::warn!(
                        hook_name,
                        "cursor subagentStart deny is not enforced: grok subagent-start hooks only observe"
                    );
                }
                neutral(mode)
            }
            _ => neutral(mode),
        }
    }

    /// `failClosed`: a crash, timeout, or bad exit blocks a gating event instead of failing open.
    pub fn apply_fail_closed(&self, result: HookRunnerResult, hook_name: &str) -> HookRunnerResult {
        let HookRunnerResult::Failed(error) = result else {
            return result;
        };
        if !self.fail_closed {
            return HookRunnerResult::Failed(error);
        }
        let reason = clip_reason(&format!("Cursor hook '{hook_name}' failed closed: {error}"));
        match self.event {
            event if event.is_permission() => HookRunnerResult::Deny {
                reason,
                hook_name: hook_name.to_string(),
            },
            CursorEvent::BeforeSubmitPrompt => HookRunnerResult::Block {
                reason,
                hook_name: hook_name.to_string(),
            },
            _ => HookRunnerResult::Failed(error),
        }
    }

    /// Env Cursor sets for every hook process, plus anything a `sessionStart` hook exported.
    pub fn process_env(&self, envelope: &HookEventEnvelope) -> HashMap<String, String> {
        let mut env = session_env(&envelope.session_id);
        if let Some(path) = &envelope.transcript_path {
            env.insert("CURSOR_TRANSCRIPT_PATH".into(), path.clone());
        }
        env
    }
}

/// The no-op result for `mode`, so a Cursor event that has nothing to say never changes the gate.
fn neutral(mode: GateKind) -> HookRunnerResult {
    match mode {
        GateKind::Observe | GateKind::Prompt => HookRunnerResult::Success,
        GateKind::Tool => HookRunnerResult::Allow {
            updated_input: None,
            additional_context: None,
        },
        GateKind::Stop => HookRunnerResult::Stop(StopHookOutcome::default()),
        GateKind::PostTool => HookRunnerResult::PostToolUse {
            outcome: PostToolUseHookOutcome::default(),
            failure: None,
        },
    }
}

#[cfg(test)]
#[path = "cursor_tests.rs"]
mod tests;
