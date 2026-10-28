//! The `cursor-stream-json` reducer: the Cursor CLI `--output-format stream-json` event shapes.
//! `system`/`init`, `user`, one `assistant` per message between tool calls, `tool_call` `started`/`completed`
//! correlated by `call_id`, and a terminal `result`. Thinking is suppressed, as in Cursor's print mode.
//! An error writes nothing here; the emitter reports it on stderr and the stream ends without a `result`.

use std::collections::HashMap;
use std::time::{SystemTime, UNIX_EPOCH};

use agent_client_protocol as proto;
use serde_json::{Map, Value, json};

use super::{Reducer, SessionContext, StreamEvent, ToolCallEvent, ToolCallUpdateEvent, TurnEnd};

pub(crate) struct CursorReducer {
    session_id: String,
    partial: bool,
    /// Assistant text since the last flush; flushed as one `assistant` event before a tool call and at turn end.
    pending_text: String,
    /// `started` payloads by `call_id`, repeated with the result on `completed`.
    open_calls: HashMap<String, Value>,
}

impl CursorReducer {
    pub(crate) fn new() -> Self {
        Self {
            session_id: String::new(),
            partial: false,
            pending_text: String::new(),
            open_calls: HashMap::new(),
        }
    }

    fn assistant(&self, text: &str) -> Value {
        json!({
            "type": "assistant",
            "message": {"role": "assistant", "content": [{"type": "text", "text": text}]},
            "session_id": self.session_id,
        })
    }

    /// One `assistant` event for the text since the last flush.
    /// With partial output, the pre-tool-call flush carries `timestamp_ms` and `model_call_id` so consumers that keep
    /// only deltas skip it, and the end-of-turn flush carries neither.
    fn flush(&mut self, before_tool_call: Option<&str>) -> Vec<Value> {
        if self.pending_text.is_empty() {
            return Vec::new();
        }
        let text = std::mem::take(&mut self.pending_text);
        let mut line = self.assistant(&text);
        if self.partial
            && let Some(call_id) = before_tool_call
            && let Some(obj) = line.as_object_mut()
        {
            obj.insert("timestamp_ms".into(), now_ms().into());
            obj.insert("model_call_id".into(), call_id.into());
        }
        vec![line]
    }

    fn started(&mut self, tc: ToolCallEvent) -> Vec<Value> {
        let call_id = tc.tool_call_id.clone();
        let payload = tool_call_payload(&tc);
        let first = !self.open_calls.contains_key(&call_id);
        self.open_calls.insert(call_id.clone(), payload.clone());
        // The early notification and a later richer one share an id; Cursor reports a call once
        if !first {
            return Vec::new();
        }
        let mut lines = self.flush(Some(&call_id));
        lines.push(json!({
            "type": "tool_call",
            "subtype": "started",
            "call_id": call_id,
            "tool_call": payload,
            "session_id": self.session_id,
        }));
        lines
    }

    fn completed(&mut self, update: ToolCallUpdateEvent) -> Vec<Value> {
        let succeeded = match update.status {
            Some(proto::ToolCallStatus::Completed) => true,
            Some(proto::ToolCallStatus::Failed) => false,
            _ => return Vec::new(),
        };
        let Some(mut payload) = self.open_calls.remove(&update.tool_call_id) else {
            return Vec::new();
        };
        let text = result_text(&update.content, &update.raw_output);
        let result = if succeeded {
            let mut success = Map::new();
            success.insert("content".into(), text.clone().into());
            success.insert("isEmpty".into(), text.is_empty().into());
            success.insert("totalChars".into(), text.chars().count().into());
            if let Some(lines) = update.raw_output.get("total_lines").and_then(Value::as_u64) {
                success.insert("totalLines".into(), lines.into());
            }
            json!({"success": success})
        } else {
            json!({"error": {"message": text}})
        };
        attach_result(&mut payload, result);
        vec![json!({
            "type": "tool_call",
            "subtype": "completed",
            "call_id": update.tool_call_id,
            "tool_call": payload,
            "session_id": self.session_id,
        })]
    }
}

impl Reducer for CursorReducer {
    fn begin(&mut self, ctx: SessionContext) -> Vec<Value> {
        self.session_id = ctx.session_id;
        self.partial = ctx.include_partial_messages;
        vec![json!({
            "type": "system",
            "subtype": "init",
            "apiKeySource": if ctx.api_key_auth { "env" } else { "login" },
            "cwd": ctx.cwd,
            "session_id": self.session_id,
            "model": ctx.model,
            "permissionMode": ctx.permission_mode.unwrap_or_else(|| "default".to_string()),
        })]
    }

    fn prompt_sent(&mut self, blocks: &[proto::ContentBlock]) -> Vec<Value> {
        let content: Vec<Value> = blocks
            .iter()
            .filter_map(|block| match block {
                proto::ContentBlock::Text(t) => Some(json!({"type": "text", "text": t.text})),
                _ => None,
            })
            .collect();
        vec![json!({
            "type": "user",
            "message": {"role": "user", "content": content},
            "session_id": self.session_id,
        })]
    }

    fn reduce(&mut self, event: StreamEvent) -> Vec<Value> {
        match event {
            StreamEvent::AgentMessage(text) => {
                self.pending_text.push_str(&text);
                if !self.partial {
                    return Vec::new();
                }
                let mut delta = self.assistant(&text);
                if let Some(obj) = delta.as_object_mut() {
                    obj.insert("timestamp_ms".into(), now_ms().into());
                }
                vec![delta]
            }
            StreamEvent::ToolCall(tc) => self.started(tc),
            StreamEvent::ToolCallUpdate(update) => self.completed(update),
            _ => Vec::new(),
        }
    }

    fn finish(&mut self, end: &TurnEnd<'_>) -> Vec<Value> {
        let mut lines = self.flush(None);
        lines.push(json!({
            "type": "result",
            "subtype": "success",
            "is_error": false,
            "duration_ms": end.duration_ms,
            "duration_api_ms": end.duration_ms,
            "result": end.result_text,
            "session_id": end.session_id,
            "request_id": end.request_id,
        }));
        lines
    }

    fn error(
        &mut self,
        _message: &str,
        _usage: Option<&Value>,
        _duration_ms: u64,
        _stop_reason: Option<&str>,
    ) -> Vec<Value> {
        Vec::new()
    }
}

/// Cursor keys a read or write by its tool type with a `path` argument; every other tool is a `function` call.
fn tool_call_payload(tc: &ToolCallEvent) -> Value {
    let path = tc
        .locations
        .get(0)
        .and_then(|l| l.get("path"))
        .or_else(|| {
            ["path", "file_path", "target_file"]
                .iter()
                .find_map(|k| tc.raw_input.get(*k))
        })
        .and_then(Value::as_str);
    match (tc.tool_kind.as_deref(), path) {
        (Some("read"), Some(path)) => json!({"readToolCall": {"args": {"path": path}}}),
        (Some("edit"), Some(path)) => json!({"writeToolCall": {"args": {"path": path}}}),
        _ => json!({"function": {"name": tc.tool_name, "arguments": tc.raw_input.to_string()}}),
    }
}

/// A read or write result sits beside its `args`; a `function` result sits beside the `function` object.
fn attach_result(payload: &mut Value, result: Value) {
    let Some(obj) = payload.as_object_mut() else {
        return;
    };
    for key in ["readToolCall", "writeToolCall"] {
        if let Some(inner) = obj.get_mut(key).and_then(Value::as_object_mut) {
            inner.insert("result".into(), result);
            return;
        }
    }
    obj.insert("result".into(), result);
}

/// The text blocks of an ACP tool result, else the raw output.
fn result_text(content: &Value, raw_output: &Value) -> String {
    let texts: Vec<&str> = content
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|block| {
            block
                .get("content")
                .and_then(|c| c.get("text"))
                .and_then(Value::as_str)
        })
        .collect();
    if !texts.is_empty() {
        return texts.join("\n");
    }
    match raw_output {
        Value::Null => String::new(),
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
}

#[cfg(test)]
#[path = "cursor_tests.rs"]
mod tests;
