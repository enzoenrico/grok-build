use super::*;

fn parse(routed: &Routed) -> Value {
    match routed {
        Routed::ToAgent(s) | Routed::ToClient(s) => serde_json::from_str(s).unwrap(),
    }
}

fn session_new_response(id: u64) -> String {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "result": {
            "sessionId": "s1",
            "modes": {
                "currentModeId": "agent",
                "availableModes": [
                    {"id": "agent", "name": "Agent"},
                    {"id": "plan", "name": "Plan"},
                ],
            },
            "models": {
                "currentModelId": "claude-opus-5-5[context=300k]",
                "availableModels": [
                    {"modelId": "claude-opus-5-5[context=300k]", "name": "claude-opus-5-5"},
                    {"modelId": "grok-4.7[context=256k,fast=true]", "name": "grok-4.7"},
                ],
            },
        },
    })
    .to_string()
}

#[test]
fn grok_extension_requests_are_answered_locally() {
    let mut adapter = CursorAdapter::new(None, false);
    let out =
        adapter.outbound(r#"{"jsonrpc":"2.0","id":7,"method":"_x.ai/queue/snapshot","params":{}}"#);
    assert_eq!(out.len(), 1);
    let Routed::ToClient(line) = &out[0] else {
        panic!("extension request must not reach cursor: {out:?}");
    };
    let resp: Value = serde_json::from_str(line).unwrap();
    assert_eq!(resp["id"], 7);
    assert_eq!(resp["error"]["code"], METHOD_NOT_FOUND);
}

#[test]
fn session_list_and_mcp_list_are_answered_from_disk() {
    let mut adapter = CursorAdapter::new(None, false);
    let sessions =
        adapter.outbound(r#"{"jsonrpc":"2.0","id":7,"method":"_x.ai/session/list","params":{}}"#);
    let Routed::ToClient(line) = &sessions[0] else {
        panic!("session list must stay local: {sessions:?}");
    };
    let resp: Value = serde_json::from_str(line).unwrap();
    assert_eq!(resp["result"]["sessions"], json!([]));

    let mcps = adapter.outbound(r#"{"jsonrpc":"2.0","id":8,"method":"x.ai/mcp/list","params":{}}"#);
    let Routed::ToClient(line) = &mcps[0] else {
        panic!("mcp list must stay local: {mcps:?}");
    };
    let resp: Value = serde_json::from_str(line).unwrap();
    assert!(resp["result"]["servers"].is_array());
}

#[test]
fn grok_extension_notifications_are_dropped() {
    let mut adapter = CursorAdapter::new(None, false);
    assert!(
        adapter
            .outbound(r#"{"jsonrpc":"2.0","method":"_x.ai/queue/interject","params":{}}"#)
            .is_empty()
    );
}

#[test]
fn session_updates_carry_the_active_prompt_id() {
    let mut adapter = CursorAdapter::new(None, false);
    adapter.outbound(
        r#"{"jsonrpc":"2.0","id":3,"method":"session/prompt","params":{"sessionId":"s1","prompt":[],"_meta":{"promptId":"p-1"}}}"#,
    );
    let update = adapter.inbound(
        r#"{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"s1","update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"hi"}}}}"#,
    );
    assert_eq!(parse(&update[0])["params"]["_meta"]["promptId"], "p-1");

    let done = adapter.inbound(r#"{"jsonrpc":"2.0","id":3,"result":{"stopReason":"end_turn"}}"#);
    assert_eq!(parse(&done[0])["result"]["_meta"]["promptId"], "p-1");

    let late = adapter.inbound(
        r#"{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"s1","update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"x"}}}}"#,
    );
    assert!(parse(&late[0])["params"]["_meta"].get("promptId").is_none());
}

#[test]
fn new_session_maps_modes_and_applies_requested_model() {
    let mut adapter = CursorAdapter::new(None, false);
    adapter.outbound(
        r#"{"jsonrpc":"2.0","id":1,"method":"session/new","params":{"cwd":"/tmp","mcpServers":[],"_meta":{"modelId":"grok-4.7"}}}"#,
    );
    let out = adapter.inbound(&session_new_response(1));
    assert_eq!(out.len(), 2, "{out:?}");

    let Routed::ToAgent(set_model) = &out[0] else {
        panic!("expected a set_model request first: {out:?}");
    };
    let set_model: Value = serde_json::from_str(set_model).unwrap();
    assert_eq!(set_model["method"], "session/set_model");
    assert_eq!(
        set_model["params"]["modelId"],
        "grok-4.7[context=256k,fast=true]"
    );

    let resp = parse(&out[1]);
    assert_eq!(
        resp["result"]["models"]["currentModelId"],
        "grok-4.7[context=256k,fast=true]"
    );
    assert_eq!(resp["result"]["modes"]["currentModeId"], "default");
    assert_eq!(
        resp["result"]["modes"]["availableModes"][0]["id"],
        "default"
    );

    let internal_ack = json!({"jsonrpc":"2.0","id": set_model["id"], "result": {}}).to_string();
    assert!(adapter.inbound(&internal_ack).is_empty());
}

#[test]
fn already_selected_model_sends_no_switch() {
    let mut adapter = CursorAdapter::new(None, false);
    adapter.outbound(
        r#"{"jsonrpc":"2.0","id":1,"method":"session/new","params":{"cwd":"/tmp","mcpServers":[],"_meta":{"modelId":"claude-opus-5-5"}}}"#,
    );
    let out = adapter.inbound(&session_new_response(1));
    assert_eq!(out.len(), 1);
    assert!(matches!(out[0], Routed::ToClient(_)));
}

#[test]
fn set_model_and_set_mode_are_translated() {
    let mut adapter = CursorAdapter::new(None, false);
    adapter.outbound(r#"{"jsonrpc":"2.0","id":1,"method":"session/new","params":{"cwd":"/tmp","mcpServers":[]}}"#);
    adapter.inbound(&session_new_response(1));

    let out = adapter.outbound(
        r#"{"jsonrpc":"2.0","id":2,"method":"session/set_model","params":{"sessionId":"s1","modelId":"GROK-4.7"}}"#,
    );
    assert_eq!(
        parse(&out[0])["params"]["modelId"],
        "grok-4.7[context=256k,fast=true]"
    );

    let out = adapter.outbound(
        r#"{"jsonrpc":"2.0","id":3,"method":"session/set_mode","params":{"sessionId":"s1","modeId":"default"}}"#,
    );
    assert_eq!(parse(&out[0])["params"]["modeId"], "agent");

    let out = adapter.inbound(
        r#"{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"s1","update":{"sessionUpdate":"current_mode_update","currentModeId":"agent"}}}"#,
    );
    assert_eq!(
        parse(&out[0])["params"]["update"]["currentModeId"],
        "default"
    );
}

#[test]
fn load_replay_is_stamped_or_dropped() {
    let chunk = r#"{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"s1","update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"old"}}}}"#;

    let mut adapter = CursorAdapter::new(None, false);
    adapter.outbound(r#"{"jsonrpc":"2.0","id":1,"method":"session/load","params":{"sessionId":"s1","cwd":"/tmp","mcpServers":[]}}"#);
    let out = adapter.inbound(chunk);
    assert_eq!(parse(&out[0])["params"]["_meta"]["isReplay"], true);
    adapter.inbound(r#"{"jsonrpc":"2.0","id":1,"result":{}}"#);
    let live = adapter.inbound(chunk);
    assert!(parse(&live[0])["params"]["_meta"].get("isReplay").is_none());

    let mut adapter = CursorAdapter::new(None, false);
    adapter.outbound(
        r#"{"jsonrpc":"2.0","id":1,"method":"session/load","params":{"sessionId":"s1","cwd":"/tmp","mcpServers":[],"_meta":{"noReplay":true}}}"#,
    );
    assert!(adapter.inbound(chunk).is_empty());
    adapter.inbound(r#"{"jsonrpc":"2.0","id":1,"result":{}}"#);
    assert_eq!(adapter.inbound(chunk).len(), 1);
}

#[test]
fn mcp_servers_are_dropped_unless_forwarded() {
    let req = r#"{"jsonrpc":"2.0","id":1,"method":"session/new","params":{"cwd":"/tmp","mcpServers":[{"type":"http","name":"x","url":"https://x","headers":[]}]}}"#;
    let out = CursorAdapter::new(None, false).outbound(req);
    assert_eq!(parse(&out[0])["params"]["mcpServers"], json!([]));
    let out = CursorAdapter::new(None, true).outbound(req);
    assert_eq!(parse(&out[0])["params"]["mcpServers"][0]["name"], "x");
}

#[test]
fn preferred_model_beats_grok_default_and_follows_switches() {
    let mut adapter = CursorAdapter::new(Some("claude-opus-5-5".into()), false);
    adapter.outbound(
        r#"{"jsonrpc":"2.0","id":1,"method":"session/new","params":{"cwd":"/tmp","mcpServers":[],"_meta":{"modelId":"grok-4.7"}}}"#,
    );
    let out = adapter.inbound(&session_new_response(1));
    assert_eq!(out.len(), 1, "claude-opus-5-5 is already current: {out:?}");

    adapter.outbound(
        r#"{"jsonrpc":"2.0","id":2,"method":"session/set_model","params":{"sessionId":"s1","modelId":"grok-4.7"}}"#,
    );
    adapter.outbound(r#"{"jsonrpc":"2.0","id":3,"method":"session/new","params":{"cwd":"/tmp","mcpServers":[]}}"#);
    let out = adapter.inbound(&session_new_response(3));
    let Routed::ToAgent(set_model) = &out[0] else {
        panic!("a later session must follow the switched model: {out:?}");
    };
    assert!(set_model.contains("grok-4.7[context=256k,fast=true]"));
}

#[test]
fn unknown_models_pass_through_unchanged() {
    let mut adapter = CursorAdapter::new(None, false);
    let out = adapter.outbound(
        r#"{"jsonrpc":"2.0","id":2,"method":"session/set_model","params":{"sessionId":"s1","modelId":"mystery"}}"#,
    );
    assert_eq!(parse(&out[0])["params"]["modelId"], "mystery");
}
