use agent_client_protocol as acp;
use serde_json::{Value, json};

use super::*;

fn reducer(partial: bool) -> (CursorReducer, Value) {
    let mut r = CursorReducer::new();
    let init = r
        .begin(SessionContext {
            session_id: "sess-1".into(),
            model: Some("grok-4".into()),
            cwd: "/repo".into(),
            permission_mode: None,
            mcp_servers: Vec::new(),
            include_partial_messages: partial,
            api_key_auth: false,
            context_window: None,
        })
        .remove(0);
    (r, init)
}

fn call(id: &str, kind: &str, name: &str, raw_input: Value, locations: Value) -> StreamEvent {
    StreamEvent::ToolCall(ToolCallEvent {
        tool_call_id: id.into(),
        title: name.into(),
        tool_kind: Some(kind.into()),
        status: Some(acp::ToolCallStatus::InProgress),
        tool_name: name.into(),
        raw_input,
        content: json!([]),
        locations,
        backend_web_search: false,
    })
}

fn done(id: &str, status: acp::ToolCallStatus, text: &str, raw_output: Value) -> StreamEvent {
    StreamEvent::ToolCallUpdate(ToolCallUpdateEvent {
        tool_call_id: id.into(),
        status: Some(status),
        content: json!([{"type": "content", "content": {"type": "text", "text": text}}]),
        raw_output,
        locations: json!([]),
    })
}

fn end(result_text: &str) -> TurnEnd<'_> {
    TurnEnd {
        stop_reason: "end_turn",
        session_id: "sess-1",
        request_id: "req-1",
        usage: None,
        structured_output: None,
        result_text,
        duration_ms: 42,
    }
}

fn kinds(lines: &[Value]) -> Vec<String> {
    lines
        .iter()
        .map(|l| match l.get("subtype").and_then(Value::as_str) {
            Some(sub) => format!("{}/{sub}", l["type"].as_str().unwrap()),
            None => l["type"].as_str().unwrap().to_string(),
        })
        .collect()
}

#[test]
fn a_turn_with_a_read_matches_cursor_event_order_and_shapes() {
    let (mut r, init) = reducer(false);
    assert_eq!(init["type"], "system");
    assert_eq!(init["subtype"], "init");
    assert_eq!(init["apiKeySource"], "login");
    assert_eq!(init["permissionMode"], "default");
    assert_eq!(init["session_id"], "sess-1");

    let mut lines = r.prompt_sent(&[acp::ContentBlock::from("fix the test".to_string())]);
    lines.extend(r.reduce(StreamEvent::AgentMessage("Reading ".into())));
    lines.extend(r.reduce(StreamEvent::AgentMessage("the file.".into())));
    lines.extend(r.reduce(call(
        "c1",
        "read",
        "read_file",
        json!({"target_file": "calc.py"}),
        json!([{"path": "/repo/calc.py"}]),
    )));
    lines.extend(r.reduce(done(
        "c1",
        acp::ToolCallStatus::Completed,
        "def add(a, b):",
        json!({"total_lines": 2}),
    )));
    lines.extend(r.reduce(StreamEvent::AgentThought("hidden".into())));
    lines.extend(r.reduce(StreamEvent::AgentMessage("Fixed.".into())));
    lines.extend(r.finish(&end("Reading the file.Fixed.")));

    assert_eq!(
        kinds(&lines),
        [
            "user",
            "assistant",
            "tool_call/started",
            "tool_call/completed",
            "assistant",
            "result/success"
        ]
    );
    assert_eq!(lines[0]["message"]["content"][0]["text"], "fix the test");
    assert_eq!(
        lines[1]["message"]["content"][0]["text"],
        "Reading the file."
    );
    assert!(lines[1].get("timestamp_ms").is_none());
    assert_eq!(lines[2]["call_id"], "c1");
    assert_eq!(
        lines[2]["tool_call"],
        json!({"readToolCall": {"args": {"path": "/repo/calc.py"}}})
    );
    let success = &lines[3]["tool_call"]["readToolCall"]["result"]["success"];
    assert_eq!(lines[3]["call_id"], "c1");
    assert_eq!(success["content"], "def add(a, b):");
    assert_eq!(success["isEmpty"], false);
    assert_eq!(success["totalChars"], 14);
    assert_eq!(success["totalLines"], 2);
    assert_eq!(lines[4]["message"]["content"][0]["text"], "Fixed.");
    let result = &lines[5];
    assert_eq!(result["is_error"], false);
    assert_eq!(result["duration_ms"], 42);
    assert_eq!(result["result"], "Reading the file.Fixed.");
    assert_eq!(result["session_id"], "sess-1");
    assert_eq!(result["request_id"], "req-1");
}

#[test]
fn other_tools_are_function_calls_and_failures_carry_an_error() {
    let (mut r, _) = reducer(false);
    let mut lines = r.reduce(call(
        "c2",
        "execute",
        "bash",
        json!({"command": "ls"}),
        json!([]),
    ));
    lines.extend(r.reduce(done("c2", acp::ToolCallStatus::Failed, "boom", Value::Null)));
    assert_eq!(lines[0]["tool_call"]["function"]["name"], "bash");
    assert_eq!(
        lines[0]["tool_call"]["function"]["arguments"],
        r#"{"command":"ls"}"#
    );
    assert_eq!(lines[1]["tool_call"]["result"]["error"]["message"], "boom");
}

#[test]
fn a_repeated_tool_call_notification_starts_once() {
    let (mut r, _) = reducer(false);
    let first = r.reduce(call("c3", "other", "bash", json!({}), json!([])));
    let again = r.reduce(call(
        "c3",
        "execute",
        "bash",
        json!({"command": "ls"}),
        json!([]),
    ));
    let completed = r.reduce(done(
        "c3",
        acp::ToolCallStatus::Completed,
        "ok",
        Value::Null,
    ));
    assert_eq!(kinds(&first), ["tool_call/started"]);
    assert!(again.is_empty());
    assert_eq!(
        completed[0]["tool_call"]["function"]["arguments"], r#"{"command":"ls"}"#,
        "completed repeats the latest args"
    );
}

#[test]
fn partial_output_marks_deltas_and_the_pre_tool_flush_only() {
    let (mut r, _) = reducer(true);
    let delta = r.reduce(StreamEvent::AgentMessage("Hi".into()));
    assert!(delta[0].get("timestamp_ms").is_some());
    assert!(delta[0].get("model_call_id").is_none());

    let before_tool = r.reduce(call("c4", "execute", "bash", json!({}), json!([])));
    assert_eq!(before_tool[0]["type"], "assistant");
    assert!(before_tool[0].get("timestamp_ms").is_some());
    assert_eq!(before_tool[0]["model_call_id"], "c4");

    r.reduce(StreamEvent::AgentMessage("Bye".into()));
    let end_lines = r.finish(&end("HiBye"));
    assert_eq!(end_lines[0]["type"], "assistant");
    assert!(end_lines[0].get("timestamp_ms").is_none());
    assert!(end_lines[0].get("model_call_id").is_none());
}
