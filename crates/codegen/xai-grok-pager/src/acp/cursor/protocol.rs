use std::collections::HashSet;

use serde_json::{Map, Value, json};

pub(crate) const GROK_DEFAULT_MODE: &str = "default";
pub(crate) const CURSOR_AGENT_MODE: &str = "agent";

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CursorOption {
    pub label: String,
    pub id: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CursorQuestion {
    pub prompt: String,
    pub id: String,
    pub allow_multiple: bool,
    pub options: Vec<CursorOption>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct QuestionWait {
    pub questions: Vec<CursorQuestion>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum PendingBridge {
    Question(QuestionWait),
    Plan(QuestionWait),
}

#[derive(Debug)]
pub(crate) struct CursorInbound {
    pub to_client: Vec<Value>,
    pub to_agent: Vec<Value>,
    pub pending: Option<PendingBridge>,
}

pub(crate) fn mode_to_cursor(id: &str) -> &str {
    if id == GROK_DEFAULT_MODE {
        CURSOR_AGENT_MODE
    } else {
        id
    }
}

pub(crate) fn mode_from_cursor(id: &str) -> &str {
    if id == CURSOR_AGENT_MODE {
        GROK_DEFAULT_MODE
    } else {
        id
    }
}

pub(crate) fn rewrite_mode_value(mode: &mut Value, from_cursor: bool) {
    let Some(id) = mode.as_str() else {
        return;
    };
    let translated = if from_cursor {
        mode_from_cursor(id)
    } else {
        mode_to_cursor(id)
    };
    if translated != id {
        *mode = Value::String(translated.to_owned());
    }
}

pub(crate) fn rewrite_mode_config_options(result: &mut Map<String, Value>, from_cursor: bool) {
    let Some(options) = result.get_mut("configOptions").and_then(Value::as_array_mut) else {
        return;
    };
    for option in options {
        if option.get("id").and_then(Value::as_str) != Some("mode") {
            continue;
        }
        if let Some(current) = option.get_mut("currentValue") {
            rewrite_mode_value(current, from_cursor);
        }
        let Some(values) = option.get_mut("options").and_then(Value::as_array_mut) else {
            continue;
        };
        for value in values {
            if let Some(id) = value.get_mut("value") {
                rewrite_mode_value(id, from_cursor);
            }
        }
    }
}

pub(crate) fn advertise_cursor_client(msg: &mut Value) {
    let Some(params) = msg.get_mut("params").and_then(Value::as_object_mut) else {
        return;
    };
    let caps = params
        .entry("clientCapabilities")
        .or_insert_with(|| Value::Object(Map::new()));
    if !caps.is_object() {
        *caps = Value::Object(Map::new());
    }
    let caps = caps
        .as_object_mut()
        .expect("clientCapabilities is an object");
    caps.insert("subagents".to_owned(), Value::Bool(true));
    let meta = caps
        .entry("_meta")
        .or_insert_with(|| Value::Object(Map::new()));
    if !meta.is_object() {
        *meta = Value::Object(Map::new());
    }
    let meta = meta.as_object_mut().expect("_meta is an object");
    meta.insert("cursorExtensions".to_owned(), Value::Bool(true));
    meta.insert("subagents".to_owned(), Value::Bool(true));
}

pub(crate) fn absorb_cursor_skills(result: &mut Map<String, Value>) {
    let Some(skills) = result.get("skills").cloned() else {
        normalize_commands(result.get_mut("availableCommands"));
        return;
    };
    let Some(skills) = skills.as_array() else {
        normalize_commands(result.get_mut("availableCommands"));
        return;
    };
    let commands = result
        .entry("availableCommands")
        .or_insert_with(|| Value::Array(Vec::new()));
    if !commands.is_array() {
        *commands = Value::Array(Vec::new());
    }
    let commands = commands
        .as_array_mut()
        .expect("availableCommands is an array");
    for skill in skills {
        let Some(name) = skill.get("name").and_then(Value::as_str) else {
            continue;
        };
        if commands
            .iter()
            .any(|cmd| cmd.get("name").and_then(Value::as_str) == Some(name))
        {
            continue;
        }
        let description = skill
            .get("description")
            .and_then(Value::as_str)
            .unwrap_or(name);
        commands.push(json!({
            "name": name,
            "description": description,
            "_meta": skill_meta(name, skill.get("path").and_then(Value::as_str)),
        }));
    }
    normalize_commands(result.get_mut("availableCommands"));
}

pub(crate) fn normalize_commands(commands: Option<&mut Value>) {
    let Some(commands) = commands.and_then(Value::as_array_mut) else {
        return;
    };
    for cmd in commands {
        let Some(obj) = cmd.as_object_mut() else {
            continue;
        };
        if !command_is_skill(obj) {
            stamp_bare_cursor_skill(obj);
            continue;
        }
        let name = obj
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or("skill")
            .to_owned();
        let path = obj
            .get("_meta")
            .and_then(Value::as_object)
            .and_then(|m| {
                m.get("path")
                    .and_then(Value::as_str)
                    .or_else(|| m.get("skillPath").and_then(Value::as_str))
            })
            .map(str::to_owned);
        let meta = obj
            .entry("_meta")
            .or_insert_with(|| Value::Object(Map::new()));
        if !meta.is_object() {
            *meta = Value::Object(Map::new());
        }
        let meta = meta.as_object_mut().expect("_meta is an object");
        if !meta.get("path").and_then(Value::as_str).is_some() {
            let path = path.unwrap_or_else(|| format!("cursor://skills/{name}"));
            meta.insert("path".to_owned(), Value::String(path));
        }
        if !meta.get("scope").and_then(Value::as_str).is_some() {
            meta.insert("scope".to_owned(), Value::String("user".to_owned()));
        }
    }
}

fn stamp_bare_cursor_skill(cmd: &mut Map<String, Value>) {
    if cmd.get("_meta").is_some() {
        return;
    }
    let Some(name) = cmd.get("name").and_then(Value::as_str) else {
        return;
    };
    if name == "copy-request-id" {
        return;
    }
    let name = name.to_owned();
    cmd.insert("_meta".to_owned(), skill_meta(&name, None));
}

pub(crate) fn command_names(commands: Option<&Value>) -> HashSet<String> {
    let mut names = HashSet::new();
    let Some(commands) = commands.and_then(Value::as_array) else {
        return names;
    };
    for cmd in commands {
        if let Some(name) = cmd.get("name").and_then(Value::as_str) {
            names.insert(name.to_owned());
        }
    }
    names
}

pub(crate) fn skill_names(commands: Option<&Value>) -> HashSet<String> {
    let mut names = HashSet::new();
    let Some(commands) = commands.and_then(Value::as_array) else {
        return names;
    };
    for cmd in commands {
        let Some(obj) = cmd.as_object() else {
            continue;
        };
        if command_is_skill(obj)
            && let Some(name) = obj.get("name").and_then(Value::as_str)
        {
            names.insert(name.to_owned());
        }
    }
    names
}

fn command_is_skill(cmd: &Map<String, Value>) -> bool {
    let Some(meta) = cmd.get("_meta").and_then(Value::as_object) else {
        return false;
    };
    if meta.get("path").and_then(Value::as_str).is_some()
        && meta.get("scope").and_then(Value::as_str).is_some()
    {
        return true;
    }
    if meta.get("skillPath").and_then(Value::as_str).is_some() {
        return true;
    }
    if meta.get("skill").and_then(Value::as_bool) == Some(true) {
        return true;
    }
    matches!(
        meta.get("kind")
            .or_else(|| meta.get("type"))
            .and_then(Value::as_str),
        Some("skill")
    )
}

fn skill_meta(name: &str, path: Option<&str>) -> Value {
    let path = path
        .map(str::to_owned)
        .unwrap_or_else(|| format!("cursor://skills/{name}"));
    json!({ "path": path, "scope": "user" })
}

pub(crate) fn cursor_subagent_ext(msg: &Value) -> Option<Value> {
    let params = msg.get("params")?;
    let update = params.get("update")?;
    let session_id = params.get("sessionId").and_then(Value::as_str)?;
    let child = update.get("subagentSessionId").and_then(Value::as_str)?;
    let body = match update.get("sessionUpdate").and_then(Value::as_str)? {
        "subagent_spawned" => {
            let mut body = json!({
                "sessionUpdate": "subagent_spawned",
                "subagent_id": child,
                "parent_session_id": session_id,
                "child_session_id": child,
                "subagent_type": update.get("name").and_then(Value::as_str).unwrap_or("subagent"),
                "description": update.get("task").and_then(Value::as_str).unwrap_or(""),
            });
            if let Some(model) = update
                .pointer("/_meta/cursor/model")
                .and_then(Value::as_str)
            {
                body["model"] = json!(model);
            }
            body
        }
        "subagent_state_update" => {
            let state = update.get("state").and_then(Value::as_str).unwrap_or("");
            if subagent_state_is_terminal(state) {
                json!({
                    "sessionUpdate": "subagent_finished",
                    "subagent_id": child,
                    "child_session_id": child,
                    "status": subagent_finish_status(state),
                    "tool_calls": 0,
                    "turns": 0,
                    "duration_ms": 0,
                })
            } else {
                json!({
                    "sessionUpdate": "subagent_progress",
                    "subagent_id": child,
                    "parent_session_id": session_id,
                    "child_session_id": child,
                    "duration_ms": 0,
                    "turn_count": 0,
                    "tool_call_count": 0,
                    "tokens_used": 0,
                    "context_window_tokens": 0,
                    "context_usage_pct": 0,
                    "tools_used": [],
                    "error_count": 0,
                })
            }
        }
        _ => return None,
    };
    Some(json!({
        "jsonrpc": "2.0",
        "method": "_x.ai/session_notification",
        "params": { "sessionId": session_id, "update": body }
    }))
}

fn subagent_state_is_terminal(state: &str) -> bool {
    matches!(
        state,
        "completed"
            | "done"
            | "finished"
            | "success"
            | "failed"
            | "error"
            | "disconnected"
            | "cancelled"
            | "canceled"
    )
}

fn subagent_finish_status(state: &str) -> &'static str {
    match state {
        "failed" | "error" | "disconnected" => "failed",
        "cancelled" | "canceled" => "cancelled",
        _ => "completed",
    }
}

pub(crate) fn hoist_skill_prompt(params: &mut Map<String, Value>, skills: &HashSet<String>) {
    let Some(prompt) = params.get("prompt").and_then(Value::as_array) else {
        return;
    };
    let mut indexes = Vec::new();
    let mut parts = Vec::new();
    for (index, block) in prompt.iter().enumerate() {
        if block.get("type").and_then(Value::as_str) != Some("text") {
            continue;
        }
        let Some(text) = block.get("text").and_then(Value::as_str) else {
            continue;
        };
        indexes.push(index);
        parts.push(text.to_owned());
    }
    if indexes.is_empty() {
        return;
    }
    // Cursor joins text blocks with newlines, then runs a slash command only when that string starts with `/`.
    let Some(hoisted) = hoist_skill_text(&parts.join("\n"), skills) else {
        return;
    };
    let Some(prompt) = params.get_mut("prompt").and_then(Value::as_array_mut) else {
        return;
    };
    let first = indexes[0];
    prompt[first]["text"] = Value::String(hoisted);
    for index in indexes.into_iter().skip(1) {
        if let Some(block) = prompt.get_mut(index) {
            block["text"] = Value::String(String::new());
        }
    }
}

pub(crate) fn hoist_skill_text(text: &str, skills: &HashSet<String>) -> Option<String> {
    let trimmed = text.trim();
    if trimmed.is_empty() || trimmed.starts_with('&') {
        return None;
    }
    if let Some(word) = leading_slash_word(trimmed)
        && skills.contains(word)
    {
        return None;
    }
    let bytes = trimmed.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] != b'/' {
            i += 1;
            continue;
        }
        if i > 0 && !bytes[i - 1].is_ascii_whitespace() {
            i += 1;
            continue;
        }
        let start = i + 1;
        let end = trimmed[start..]
            .find(char::is_whitespace)
            .map(|rel| start + rel)
            .unwrap_or(trimmed.len());
        let word = &trimmed[start..end];
        if skills.contains(word) {
            let before = trimmed[..i].trim();
            let after = trimmed[end..].trim();
            let rest = match (before.is_empty(), after.is_empty()) {
                (true, true) => String::new(),
                (false, true) => before.to_owned(),
                (true, false) => after.to_owned(),
                (false, false) => format!("{before} {after}"),
            };
            return Some(if rest.is_empty() {
                format!("/{word}")
            } else {
                format!("/{word} {rest}")
            });
        }
        i = end.max(start);
    }
    None
}

fn leading_slash_word(text: &str) -> Option<&str> {
    let rest = text.strip_prefix('/')?;
    let end = rest.find(char::is_whitespace).unwrap_or(rest.len());
    let word = &rest[..end];
    (!word.is_empty()).then_some(word)
}

pub(crate) fn prompt_is_cloud_handoff(params: &Map<String, Value>) -> bool {
    first_prompt_text(params).is_some_and(|text| text.trim_start().starts_with('&'))
}

pub(crate) fn cloud_task_text(params: &Map<String, Value>) -> Option<String> {
    let prompt = params.get("prompt")?.as_array()?;
    let mut parts = Vec::new();
    for block in prompt {
        if block.get("type").and_then(Value::as_str) != Some("text") {
            continue;
        }
        if let Some(text) = block.get("text").and_then(Value::as_str) {
            parts.push(text);
        }
    }
    if parts.is_empty() {
        return None;
    }
    super::cloud::handoff_text(&parts.join("\n"))
}

pub(crate) fn stamp_cloud_handoff(params: &mut Map<String, Value>, agent_id: Option<&str>) {
    let handoff = prompt_is_cloud_handoff(params);
    let meta = meta_mut(params);
    if handoff {
        meta.insert("cloudHandoff".to_owned(), Value::Bool(true));
    } else if let Some(agent_id) = agent_id {
        meta.insert(
            "cloudAgentId".to_owned(),
            Value::String(agent_id.to_owned()),
        );
    }
}

pub(crate) fn cloud_agent_id(result: &Map<String, Value>) -> Option<String> {
    for key in ["cloudAgentId", "agentId", "bcId"] {
        if let Some(id) = result.get(key).and_then(Value::as_str) {
            return Some(id.to_owned());
        }
        if let Some(id) = result
            .get("_meta")
            .and_then(|m| m.get(key))
            .and_then(Value::as_str)
        {
            return Some(id.to_owned());
        }
    }
    None
}

pub(crate) fn agent_notice(session_id: &str, text: &str) -> Value {
    json!({
        "jsonrpc": "2.0",
        "method": "session/update",
        "params": {
            "sessionId": session_id,
            "update": {
                "sessionUpdate": "agent_message_chunk",
                "content": { "type": "text", "text": text }
            }
        }
    })
}

pub(crate) fn prompt_end(id: &Value, prompt_id: Option<&str>) -> Value {
    let mut result = Map::new();
    result.insert("stopReason".to_owned(), json!("end_turn"));
    if let Some(prompt_id) = prompt_id {
        result.insert("_meta".to_owned(), json!({ "promptId": prompt_id }));
    }
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "result": result,
    })
}

pub(crate) fn cloud_status_update(session_id: &str, agent_id: &str) -> Value {
    agent_notice(
        session_id,
        &format!("Cloud agent {agent_id} is running. Send another message to follow up.\n"),
    )
}

pub(crate) fn translate_cursor_inbound(
    msg: &Value,
    session_id: Option<&str>,
) -> Option<CursorInbound> {
    let method = msg.get("method").and_then(Value::as_str)?;
    if !method.starts_with("cursor/") {
        return None;
    }
    let params = msg.get("params").cloned().unwrap_or_else(|| json!({}));
    let id = msg.get("id").cloned();
    match method {
        "cursor/ask_question" => {
            let wait = question_wait(&params);
            let ext = ext_method(
                id.clone()?,
                "x.ai/ask_user_question",
                ask_user_params(session_id, &params, &wait),
            );
            Some(CursorInbound {
                to_client: vec![ext],
                to_agent: Vec::new(),
                pending: Some(PendingBridge::Question(wait)),
            })
        }
        "cursor/create_plan" => {
            let wait = plan_wait(&params);
            let ext = ext_method(
                id.clone()?,
                "x.ai/ask_user_question",
                ask_user_params(session_id, &plan_as_question(&params), &wait),
            );
            Some(CursorInbound {
                to_client: vec![ext],
                to_agent: Vec::new(),
                pending: Some(PendingBridge::Plan(wait)),
            })
        }
        "cursor/update_todos" => {
            let mut to_client = Vec::new();
            if let Some(session_id) = session_id {
                to_client.push(plan_update(session_id, &params));
            }
            let to_agent = id
                .map(|id| {
                    vec![json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "result": { "outcome": { "outcome": "accepted", "todos": params.get("todos").cloned().unwrap_or_else(|| json!([])) } }
                    })]
                })
                .unwrap_or_default();
            Some(CursorInbound {
                to_client,
                to_agent,
                pending: None,
            })
        }
        "cursor/task" => {
            let mut to_client = Vec::new();
            if let Some(session_id) = session_id {
                to_client.push(task_update(session_id, &params));
            }
            let to_agent = id
                .map(|id| {
                    vec![json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "result": { "outcome": { "outcome": "completed", "agentId": params.get("agentId").cloned().unwrap_or(Value::Null) } }
                    })]
                })
                .unwrap_or_default();
            Some(CursorInbound {
                to_client,
                to_agent,
                pending: None,
            })
        }
        "cursor/generate_image" => {
            let mut to_client = Vec::new();
            if let Some(session_id) = session_id {
                to_client.push(image_update(session_id, &params));
            }
            let file_path = params.get("filePath").and_then(Value::as_str).unwrap_or("");
            let outcome = if file_path.is_empty() {
                json!({ "outcome": "rejected", "reason": "no file path" })
            } else {
                json!({ "outcome": "generated", "filePath": file_path })
            };
            let to_agent = id
                .map(|id| {
                    vec![json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "result": { "outcome": outcome }
                    })]
                })
                .unwrap_or_default();
            Some(CursorInbound {
                to_client,
                to_agent,
                pending: None,
            })
        }
        _ => None,
    }
}

pub(crate) fn cursor_result_from_ext(pending: &PendingBridge, result: &Value) -> Value {
    match pending {
        PendingBridge::Question(wait) => question_result(wait, result),
        PendingBridge::Plan(wait) => plan_result(wait, result),
    }
}

fn question_result(wait: &QuestionWait, result: &Value) -> Value {
    let outcome = result.get("outcome").and_then(Value::as_str).unwrap_or("");
    if outcome != "accepted" {
        let cursor_outcome = if outcome == "cancelled" {
            "cancelled"
        } else {
            "skipped"
        };
        return json!({ "outcome": { "outcome": cursor_outcome } });
    }
    let answers = result.get("answers").and_then(Value::as_object);
    let mut cursor_answers = Vec::new();
    for (prompt, labels) in answers.into_iter().flat_map(|m| m.iter()) {
        let Some(question) = wait.questions.iter().find(|q| q.prompt == *prompt) else {
            continue;
        };
        let selected: Vec<String> = labels
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .map(|label| {
                question
                    .options
                    .iter()
                    .find(|option| option.label == label)
                    .map(|option| option.id.clone())
                    .unwrap_or_else(|| label.to_owned())
            })
            .collect();
        cursor_answers.push(json!({
            "questionId": question.id,
            "selectedOptionIds": selected,
        }));
    }
    json!({ "outcome": { "outcome": "answered", "answers": cursor_answers } })
}

fn plan_result(wait: &QuestionWait, result: &Value) -> Value {
    let answered = question_result(wait, result);
    let selected = answered
        .pointer("/outcome/answers/0/selectedOptionIds/0")
        .and_then(Value::as_str);
    match selected {
        Some("accepted") => json!({ "outcome": { "outcome": "accepted" } }),
        Some("rejected") => json!({ "outcome": { "outcome": "rejected", "reason": "rejected" } }),
        _ => json!({ "outcome": { "outcome": "cancelled" } }),
    }
}

fn question_wait(params: &Value) -> QuestionWait {
    let questions = params
        .get("questions")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .map(|question| {
            let prompt = question
                .get("prompt")
                .and_then(Value::as_str)
                .unwrap_or("Question")
                .to_owned();
            let id = question
                .get("id")
                .and_then(Value::as_str)
                .unwrap_or(&prompt)
                .to_owned();
            let options = question
                .get("options")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .map(|option| {
                    let label = option
                        .get("label")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_owned();
                    let id = option
                        .get("id")
                        .and_then(Value::as_str)
                        .unwrap_or(&label)
                        .to_owned();
                    CursorOption { label, id }
                })
                .collect();
            CursorQuestion {
                prompt,
                id,
                allow_multiple: question
                    .get("allowMultiple")
                    .and_then(Value::as_bool)
                    .unwrap_or(false),
                options,
            }
        })
        .collect();
    QuestionWait { questions }
}

fn plan_wait(params: &Value) -> QuestionWait {
    question_wait(&plan_as_question(params))
}

fn plan_as_question(params: &Value) -> Value {
    let name = params.get("name").and_then(Value::as_str).unwrap_or("Plan");
    let mut prompt = format!("Accept this plan ({name})?");
    for field in ["overview", "plan"] {
        if let Some(text) = params.get(field).and_then(Value::as_str)
            && !text.is_empty()
        {
            prompt.push_str("\n\n");
            prompt.push_str(text);
        }
    }
    append_plan_tasks(&mut prompt, params);
    json!({
        "toolCallId": params.get("toolCallId").cloned().unwrap_or_else(|| json!("plan")),
        "questions": [{
            "id": "plan",
            "prompt": prompt,
            "allowMultiple": false,
            "options": [
                {"id": "accepted", "label": "Accept"},
                {"id": "rejected", "label": "Reject"}
            ]
        }]
    })
}

fn append_plan_tasks(prompt: &mut String, params: &Value) {
    let mut lines = Vec::new();
    push_todo_lines(&mut lines, params.get("todos"));
    if let Some(phases) = params.get("phases").and_then(Value::as_array) {
        for phase in phases {
            let name = phase.get("name").and_then(Value::as_str).unwrap_or("Phase");
            let mut phase_lines = Vec::new();
            push_todo_lines(&mut phase_lines, phase.get("todos"));
            if phase_lines.is_empty() {
                continue;
            }
            lines.push(name.to_owned());
            lines.extend(phase_lines);
        }
    }
    if lines.is_empty() {
        return;
    }
    prompt.push_str("\n\n");
    prompt.push_str(&lines.join("\n"));
    prompt.push('\n');
}

fn push_todo_lines(lines: &mut Vec<String>, todos: Option<&Value>) {
    let Some(todos) = todos.and_then(Value::as_array) else {
        return;
    };
    for todo in todos {
        if let Some(text) = todo.get("content").and_then(Value::as_str)
            && !text.is_empty()
        {
            lines.push(format!("- {text}"));
        }
    }
}

pub(crate) fn normalize_plan_session_update(msg: &mut Value) {
    let Some(update) = msg.pointer_mut("/params/update") else {
        return;
    };
    if update.get("sessionUpdate").and_then(Value::as_str) != Some("plan") {
        return;
    }
    let Some(entries) = update.get_mut("entries").and_then(Value::as_array_mut) else {
        return;
    };
    for entry in entries {
        let status = entry
            .get("status")
            .and_then(Value::as_str)
            .unwrap_or("pending");
        entry["status"] = json!(plan_status(status));
        if entry.get("priority").and_then(Value::as_str).is_none() {
            entry["priority"] = json!("medium");
        }
        if entry.get("content").and_then(Value::as_str).is_none() {
            let content = entry
                .get("id")
                .and_then(Value::as_str)
                .unwrap_or("Plan task");
            entry["content"] = json!(content);
        }
    }
}

fn ask_user_params(session_id: Option<&str>, params: &Value, wait: &QuestionWait) -> Value {
    let mut questions = Vec::new();
    for question in &wait.questions {
        let options: Vec<Value> = question
            .options
            .iter()
            .map(|option| {
                json!({
                    "label": option.label,
                    "description": option.label,
                })
            })
            .collect();
        questions.push(json!({
            "question": question.prompt,
            "options": options,
            "multiSelect": question.allow_multiple,
        }));
    }
    json!({
        "sessionId": session_id.unwrap_or(""),
        "toolCallId": params.get("toolCallId").cloned().unwrap_or_else(|| json!("cursor")),
        "questions": questions,
        "mode": "default",
    })
}

fn ext_method(id: Value, method: &str, params: Value) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": "ext_method",
        "params": { "method": method, "params": params }
    })
}

fn plan_update(session_id: &str, params: &Value) -> Value {
    let entries: Vec<Value> = params
        .get("todos")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .map(|todo| {
            json!({
                "content": todo.get("content").and_then(Value::as_str).unwrap_or(""),
                "priority": "medium",
                "status": plan_status(todo.get("status").and_then(Value::as_str).unwrap_or("pending")),
            })
        })
        .collect();
    json!({
        "jsonrpc": "2.0",
        "method": "session/update",
        "params": {
            "sessionId": session_id,
            "update": { "sessionUpdate": "plan", "entries": entries }
        }
    })
}

fn plan_status(status: &str) -> &'static str {
    match status {
        "in_progress" => "in_progress",
        "completed" | "cancelled" => "completed",
        _ => "pending",
    }
}

fn task_update(session_id: &str, params: &Value) -> Value {
    let description = params
        .get("description")
        .and_then(Value::as_str)
        .unwrap_or("subagent");
    let prompt = params.get("prompt").and_then(Value::as_str).unwrap_or("");
    let kind = params
        .get("subagentType")
        .and_then(Value::as_str)
        .unwrap_or("subagent");
    json!({
        "jsonrpc": "2.0",
        "method": "session/update",
        "params": {
            "sessionId": session_id,
            "update": {
                "sessionUpdate": "tool_call",
                "toolCallId": params.get("toolCallId").and_then(Value::as_str).unwrap_or("cursor-task"),
                "title": format!("{kind}: {description}"),
                "kind": "other",
                "status": "completed",
                "content": [{ "type": "content", "content": { "type": "text", "text": prompt } }]
            }
        }
    })
}

fn image_update(session_id: &str, params: &Value) -> Value {
    let description = params
        .get("description")
        .and_then(Value::as_str)
        .unwrap_or("image");
    let path = params.get("filePath").and_then(Value::as_str).unwrap_or("");
    json!({
        "jsonrpc": "2.0",
        "method": "session/update",
        "params": {
            "sessionId": session_id,
            "update": {
                "sessionUpdate": "agent_message_chunk",
                "content": { "type": "text", "text": format!("{description}\n{path}") }
            }
        }
    })
}

fn first_prompt_text(params: &Map<String, Value>) -> Option<&str> {
    params
        .get("prompt")
        .and_then(Value::as_array)?
        .iter()
        .find(|block| block.get("type").and_then(Value::as_str) == Some("text"))
        .and_then(|block| block.get("text"))
        .and_then(Value::as_str)
}

fn meta_mut(obj: &mut Map<String, Value>) -> &mut Map<String, Value> {
    let meta = obj
        .entry("_meta")
        .or_insert_with(|| Value::Object(Map::new()));
    if !meta.is_object() {
        *meta = Value::Object(Map::new());
    }
    meta.as_object_mut().expect("_meta is an object")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn modes_round_trip_agent_and_keep_the_rest() {
        assert_eq!(mode_to_cursor("default"), "agent");
        assert_eq!(mode_from_cursor("agent"), "default");
        assert_eq!(mode_to_cursor("plan"), "plan");
        assert_eq!(mode_from_cursor("ask"), "ask");
        assert_eq!(mode_from_cursor("debug"), "debug");
    }

    #[test]
    fn skill_list_becomes_a_pager_skill_command() {
        let mut result = json!({
            "skills": [{ "name": "poteto-mode", "description": "Poteto", "path": "/skills/poteto-mode/SKILL.md" }]
        });
        absorb_cursor_skills(result.as_object_mut().unwrap());
        let cmd = &result["availableCommands"][0];
        assert_eq!(cmd["name"], "poteto-mode");
        assert_eq!(cmd["_meta"]["path"], "/skills/poteto-mode/SKILL.md");
        assert_eq!(cmd["_meta"]["scope"], "user");
        let names = skill_names(Some(&result["availableCommands"]));
        assert!(names.contains("poteto-mode"));
    }

    #[test]
    fn mid_message_skill_moves_to_the_front_for_cursor() {
        let mut skills = HashSet::new();
        skills.insert("poteto-mode".to_owned());
        assert_eq!(
            hoist_skill_text("please /poteto-mode review this", &skills).as_deref(),
            Some("/poteto-mode please review this")
        );
        assert_eq!(hoist_skill_text("/poteto-mode review", &skills), None);
        assert_eq!(hoist_skill_text("see /tmp/file", &skills), None);
        assert_eq!(
            hoist_skill_text("/compact please /poteto-mode review", &skills).as_deref(),
            Some("/poteto-mode /compact please review")
        );
    }

    #[test]
    fn a_skill_in_a_later_text_block_is_hoisted() {
        let mut skills = HashSet::new();
        skills.insert("poteto-mode".to_owned());
        let mut params = json!({
            "prompt": [
                {"type": "text", "text": "look at the diff"},
                {"type": "image", "data": "abc"},
                {"type": "text", "text": "then /poteto-mode please"}
            ]
        });
        hoist_skill_prompt(params.as_object_mut().unwrap(), &skills);
        assert_eq!(
            params["prompt"][0]["text"],
            "/poteto-mode look at the diff\nthen please"
        );
        assert_eq!(params["prompt"][1]["data"], "abc");
        assert_eq!(params["prompt"][2]["text"], "");
        let joined = params["prompt"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|block| block.get("type").and_then(Value::as_str) == Some("text"))
            .map(|block| block["text"].as_str().unwrap_or(""))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(joined.trim().starts_with("/poteto-mode"));
    }

    #[test]
    fn a_bare_cursor_command_is_still_a_hoist_name() {
        let commands = json!([
            {"name": "poteto-mode", "description": "Poteto"}
        ]);
        let names = command_names(Some(&commands));
        assert!(names.contains("poteto-mode"));
        assert_eq!(
            hoist_skill_text("please /poteto-mode review", &names).as_deref(),
            Some("/poteto-mode please review")
        );
    }

    #[test]
    fn subagent_spawn_becomes_the_pager_child_session_notice() {
        use xai_grok_shell::extensions::notification::SessionUpdate;
        let msg = json!({
            "jsonrpc": "2.0",
            "method": "session/update",
            "params": {
                "sessionId": "parent",
                "update": {
                    "sessionUpdate": "subagent_spawned",
                    "subagentSessionId": "child-1",
                    "name": "explore",
                    "task": "Find auth",
                    "_meta": {"cursor": {"model": "composer-2", "toolCallId": "call-1"}}
                }
            }
        });
        let ext = cursor_subagent_ext(&msg).unwrap();
        assert_eq!(ext["method"], "_x.ai/session_notification");
        let parsed: SessionUpdate = serde_json::from_value(ext["params"]["update"].clone()).unwrap();
        let SessionUpdate::SubagentSpawned {
            subagent_id,
            parent_session_id,
            child_session_id,
            subagent_type,
            description,
            model,
            ..
        } = parsed
        else {
            panic!("expected SubagentSpawned");
        };
        assert_eq!(subagent_id, "child-1");
        assert_eq!(parent_session_id, "parent");
        assert_eq!(child_session_id, "child-1");
        assert_eq!(subagent_type, "explore");
        assert_eq!(description, "Find auth");
        assert_eq!(model.as_deref(), Some("composer-2"));

        let done = json!({
            "method": "session/update",
            "params": {
                "sessionId": "parent",
                "update": {
                    "sessionUpdate": "subagent_state_update",
                    "subagentSessionId": "child-1",
                    "state": "completed"
                }
            }
        });
        let ext = cursor_subagent_ext(&done).unwrap();
        let parsed: SessionUpdate = serde_json::from_value(ext["params"]["update"].clone()).unwrap();
        let SessionUpdate::SubagentFinished { status, child_session_id, .. } = parsed else {
            panic!("expected SubagentFinished");
        };
        assert_eq!(child_session_id, "child-1");
        assert_eq!(status, "completed");

        for (state, expected) in [
            ("failed", "failed"),
            ("cancelled", "cancelled"),
            ("disconnected", "failed"),
        ] {
            let done = json!({
                "method": "session/update",
                "params": {
                    "sessionId": "parent",
                    "update": {
                        "sessionUpdate": "subagent_state_update",
                        "subagentSessionId": "child-1",
                        "state": state
                    }
                }
            });
            let ext = cursor_subagent_ext(&done).unwrap();
            let parsed: SessionUpdate =
                serde_json::from_value(ext["params"]["update"].clone()).unwrap();
            let SessionUpdate::SubagentFinished { status, .. } = parsed else {
                panic!("expected SubagentFinished for {state}");
            };
            assert_eq!(status, expected, "{state}");
        }
    }

    #[test]
    fn initialize_advertises_subagents() {
        let mut msg = json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":1,"clientCapabilities":{}}});
        advertise_cursor_client(&mut msg);
        assert_eq!(msg["params"]["clientCapabilities"]["subagents"], true);
        assert_eq!(msg["params"]["clientCapabilities"]["_meta"]["subagents"], true);
    }

    #[test]
    fn cloud_handoff_is_a_leading_ampersand() {
        let params = json!({
            "prompt": [{ "type": "text", "text": "& refactor the auth module" }]
        });
        assert!(prompt_is_cloud_handoff(params.as_object().unwrap()));
        let follow = json!({
            "prompt": [{ "type": "text", "text": "add tests" }]
        });
        assert!(!prompt_is_cloud_handoff(follow.as_object().unwrap()));
    }

    #[test]
    fn ask_question_round_trips_through_the_pager_question() {
        let inbound = json!({
            "jsonrpc": "2.0",
            "id": 5,
            "method": "cursor/ask_question",
            "params": {
                "toolCallId": "call_123",
                "questions": [{
                    "id": "q1",
                    "prompt": "Which mode?",
                    "allowMultiple": false,
                    "options": [
                        {"id": "agent", "label": "Agent"},
                        {"id": "plan", "label": "Plan"}
                    ]
                }]
            }
        });
        let translated = translate_cursor_inbound(&inbound, Some("s1")).unwrap();
        let ext = &translated.to_client[0];
        assert_eq!(ext["method"], "ext_method");
        assert_eq!(ext["params"]["method"], "x.ai/ask_user_question");
        assert_eq!(ext["params"]["params"]["sessionId"], "s1");
        assert_eq!(
            ext["params"]["params"]["questions"][0]["question"],
            "Which mode?"
        );
        let PendingBridge::Question(wait) = translated.pending.unwrap() else {
            panic!("question wait");
        };
        let result = cursor_result_from_ext(
            &PendingBridge::Question(wait),
            &json!({"outcome": "accepted", "answers": {"Which mode?": ["Plan"]}}),
        );
        assert_eq!(result["outcome"]["outcome"], "answered");
        assert_eq!(result["outcome"]["answers"][0]["questionId"], "q1");
        assert_eq!(
            result["outcome"]["answers"][0]["selectedOptionIds"][0],
            "plan"
        );
    }

    #[test]
    fn plan_approval_uses_accept_and_reject() {
        let inbound = json!({
            "jsonrpc": "2.0",
            "id": 6,
            "method": "cursor/create_plan",
            "params": {
                "toolCallId": "call_124",
                "name": "Refactor",
                "plan": "1. Inspect\n2. Edit"
            }
        });
        let translated = translate_cursor_inbound(&inbound, Some("s1")).unwrap();
        let question = translated.to_client[0]["params"]["params"]["questions"][0]["question"]
            .as_str()
            .unwrap();
        assert!(question.contains("1. Inspect"));
        let overview = json!({
            "jsonrpc": "2.0",
            "id": 7,
            "method": "cursor/create_plan",
            "params": {
                "name": "Auth",
                "overview": "Replace the token check.",
                "plan": "",
                "todos": [{"id": "t1", "content": "Read the login file", "status": "pending"}],
                "phases": [{"name": "Ship", "todos": [{"id": "t2", "content": "Open the pull request", "status": "pending"}]}]
            }
        });
        let translated = translate_cursor_inbound(&overview, Some("s1")).unwrap();
        let question = translated.to_client[0]["params"]["params"]["questions"][0]["question"]
            .as_str()
            .unwrap();
        assert!(question.contains("Replace the token check."));
        assert!(question.contains("Read the login file"));
        assert!(question.contains("Ship"));
        assert!(question.contains("Open the pull request"));
        let PendingBridge::Plan(wait) = translated.pending.unwrap() else {
            panic!("plan wait");
        };
        let prompt = wait.questions[0].prompt.clone();
        let result = cursor_result_from_ext(
            &PendingBridge::Plan(wait),
            &json!({"outcome": "accepted", "answers": {prompt: ["Accept"]}}),
        );
        assert_eq!(result["outcome"]["outcome"], "accepted");
    }

    #[test]
    fn task_notification_becomes_a_tool_call() {
        let inbound = json!({
            "jsonrpc": "2.0",
            "method": "cursor/task",
            "params": {
                "toolCallId": "call_126",
                "description": "Explore codebase",
                "prompt": "Find auth",
                "subagentType": "explore"
            }
        });
        let translated = translate_cursor_inbound(&inbound, Some("s1")).unwrap();
        assert!(translated.pending.is_none());
        let update = &translated.to_client[0]["params"]["update"];
        assert_eq!(update["sessionUpdate"], "tool_call");
        assert_eq!(update["title"], "explore: Explore codebase");
        assert_eq!(update["toolCallId"], "call_126");
    }

    #[test]
    fn a_cursor_plan_update_decodes_for_the_pager() {
        let mut msg = json!({
            "jsonrpc": "2.0",
            "method": "session/update",
            "params": {
                "sessionId": "s1",
                "update": {
                    "sessionUpdate": "plan",
                    "entries": [{"content": "Read the login file", "status": "cancelled"}]
                }
            }
        });
        normalize_plan_session_update(&mut msg);
        let note: agent_client_protocol::SessionNotification =
            serde_json::from_value(msg["params"].clone()).expect("pager plan update");
        let agent_client_protocol::SessionUpdate::Plan(plan) = note.update else {
            panic!("expected a plan");
        };
        assert_eq!(plan.entries[0].content, "Read the login file");
        assert_eq!(
            plan.entries[0].status,
            agent_client_protocol::PlanEntryStatus::Completed
        );
        assert_eq!(
            plan.entries[0].priority,
            agent_client_protocol::PlanEntryPriority::Medium
        );
    }
}
