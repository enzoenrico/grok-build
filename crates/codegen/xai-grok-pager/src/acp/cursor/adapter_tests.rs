use super::*;

fn parse(routed: &Routed) -> Value {
    match routed {
        Routed::ToAgent(s) | Routed::ToClient(s) => serde_json::from_str(s).unwrap(),
        Routed::Cloud { .. } => panic!("expected a JSON line, got a cloud handoff"),
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
        adapter.outbound(r#"{"jsonrpc":"2.0","id":7,"method":"_x.ai/session/list","params":{}}"#);
    assert_eq!(out.len(), 1);
    let Routed::ToClient(line) = &out[0] else {
        panic!("extension request must not reach cursor: {out:?}");
    };
    let resp: Value = serde_json::from_str(line).unwrap();
    assert_eq!(resp["id"], 7);
    assert_eq!(resp["error"]["code"], METHOD_NOT_FOUND);
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
fn ask_and_debug_modes_pass_through() {
    let mut adapter = CursorAdapter::new(None, false);
    adapter.outbound(r#"{"jsonrpc":"2.0","id":1,"method":"session/new","params":{"cwd":"/tmp","mcpServers":[]}}"#);
    let created = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "result": {
            "sessionId": "s1",
            "modes": {
                "currentModeId": "ask",
                "availableModes": [
                    {"id": "agent", "name": "Agent"},
                    {"id": "plan", "name": "Plan"},
                    {"id": "ask", "name": "Ask"},
                    {"id": "debug", "name": "Debug"}
                ]
            }
        }
    })
    .to_string();
    let out = adapter.inbound(&created);
    let resp = parse(&out[0]);
    assert_eq!(resp["result"]["modes"]["currentModeId"], "ask");
    assert_eq!(
        resp["result"]["modes"]["availableModes"][0]["id"],
        "default"
    );
    assert_eq!(resp["result"]["modes"]["availableModes"][3]["id"], "debug");

    let out = adapter.outbound(
        r#"{"jsonrpc":"2.0","id":2,"method":"session/set_mode","params":{"sessionId":"s1","modeId":"debug"}}"#,
    );
    assert_eq!(parse(&out[0])["params"]["modeId"], "debug");
}

#[test]
fn cursor_skills_hoist_from_the_middle_of_a_prompt() {
    let mut adapter = CursorAdapter::new(None, false);
    adapter.outbound(r#"{"jsonrpc":"2.0","id":1,"method":"session/new","params":{"cwd":"/tmp","mcpServers":[]}}"#);
    let created = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "result": {
            "sessionId": "s1",
            "skills": [{ "name": "poteto-mode", "description": "Poteto", "path": "/skills/poteto-mode/SKILL.md" }]
        }
    })
    .to_string();
    let out = adapter.inbound(&created);
    let cmd = &parse(&out[0])["result"]["availableCommands"][0];
    assert_eq!(cmd["name"], "poteto-mode");
    assert_eq!(cmd["_meta"]["scope"], "user");

    let out = adapter.outbound(
        r#"{"jsonrpc":"2.0","id":2,"method":"session/prompt","params":{"sessionId":"s1","prompt":[{"type":"text","text":"please /poteto-mode review this"}]}}"#,
    );
    assert_eq!(
        parse(&out[0])["params"]["prompt"][0]["text"],
        "/poteto-mode please review this"
    );
}

#[test]
fn cloud_handoff_leaves_the_local_agent_and_a_later_ampersand_follows_up() {
    let mut adapter = CursorAdapter::new(None, false);
    let sent = adapter.outbound(
        r#"{"jsonrpc":"2.0","id":4,"method":"session/prompt","params":{"sessionId":"s1","prompt":[{"type":"text","text":"& refactor the auth module"}]}}"#,
    );
    let Routed::Cloud {
        session_id,
        task,
        agent_id,
    } = &sent[0]
    else {
        panic!("expected a cloud handoff: {sent:?}");
    };
    assert_eq!(session_id, "s1");
    assert_eq!(task, "refactor the auth module");
    assert_eq!(agent_id, &None);

    adapter.note_cloud_agent("s1", "bc-123");
    let local = adapter.outbound(
        r#"{"jsonrpc":"2.0","id":5,"method":"session/prompt","params":{"sessionId":"s1","prompt":[{"type":"text","text":"keep going locally"}]}}"#,
    );
    assert_eq!(
        parse(&local[0])["params"]["prompt"][0]["text"],
        "keep going locally"
    );
    assert!(parse(&local[0])["params"].get("_meta").is_none());

    let follow = adapter.outbound(
        r#"{"jsonrpc":"2.0","id":6,"method":"session/prompt","params":{"sessionId":"s1","prompt":[{"type":"text","text":"& add tests"}]}}"#,
    );
    let Routed::Cloud {
        task, agent_id, ..
    } = &follow[0]
    else {
        panic!("expected a cloud follow-up: {follow:?}");
    };
    assert_eq!(task, "add tests");
    assert_eq!(agent_id.as_deref(), Some("bc-123"));
}

#[test]
fn two_sessions_keep_their_own_skills_and_cloud_agents() {
    let mut adapter = CursorAdapter::new(None, false);
    for (id, skill, cloud) in [("s1", "poteto-mode", "bc-one"), ("s2", "review", "bc-two")] {
        adapter.outbound(&format!(
            r#"{{"jsonrpc":"2.0","id":1,"method":"session/new","params":{{"cwd":"/tmp","mcpServers":[]}}}}"#
        ));
        let created = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "result": {
                "sessionId": id,
                "skills": [{ "name": skill, "description": skill }]
            }
        });
        adapter.inbound(&created.to_string());
        let handoff = adapter.outbound(&format!(
            r#"{{"jsonrpc":"2.0","id":4,"method":"session/prompt","params":{{"sessionId":"{id}","prompt":[{{"type":"text","text":"& ship it"}}]}}}}"#
        ));
        let Routed::Cloud { task, agent_id, .. } = &handoff[0] else {
            panic!("expected a cloud handoff: {handoff:?}");
        };
        assert_eq!(task, "ship it");
        assert_eq!(agent_id, &None);
        adapter.note_cloud_agent(id, cloud);
    }

    let first = adapter.outbound(
        r#"{"jsonrpc":"2.0","id":8,"method":"session/prompt","params":{"sessionId":"s1","prompt":[{"type":"text","text":"please /poteto-mode the tests"},{"type":"text","text":"and /review later"}]}}"#,
    );
    let sent = parse(&first[0]);
    assert_eq!(sent["params"]["prompt"][0]["text"], "/poteto-mode please the tests");
    assert_eq!(sent["params"]["prompt"][1]["text"], "and /review later");
    assert!(sent["params"].get("_meta").is_none());

    let second = adapter.outbound(
        r#"{"jsonrpc":"2.0","id":9,"method":"session/prompt","params":{"sessionId":"s2","prompt":[{"type":"text","text":"please /review the diff"}]}}"#,
    );
    let sent = parse(&second[0]);
    assert_eq!(sent["params"]["prompt"][0]["text"], "/review please the diff");
    assert!(sent["params"].get("_meta").is_none());
    let again = adapter.outbound(
        r#"{"jsonrpc":"2.0","id":10,"method":"session/prompt","params":{"sessionId":"s2","prompt":[{"type":"text","text":"& keep going"}]}}"#,
    );
    let Routed::Cloud { agent_id, .. } = &again[0] else {
        panic!("expected s2 follow-up: {again:?}");
    };
    assert_eq!(agent_id.as_deref(), Some("bc-two"));

    let asked = adapter.inbound(
        r#"{"jsonrpc":"2.0","id":11,"method":"cursor/ask_question","params":{"sessionId":"s2","toolCallId":"q","questions":[{"id":"q1","prompt":"Ship?","options":[{"id":"yes","label":"Yes"}]}]}}"#,
    );
    assert_eq!(parse(&asked[0])["params"]["params"]["sessionId"], "s2");
}

#[test]
fn cursor_subagent_updates_become_child_session_notices() {
    let mut adapter = CursorAdapter::new(None, false);
    let spawned = adapter.inbound(
        r#"{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"s1","update":{"sessionUpdate":"subagent_spawned","subagentSessionId":"child-1","name":"explore","task":"Find auth"}}}"#,
    );
    let notice = parse(&spawned[0]);
    assert_eq!(notice["method"], "_x.ai/session_notification");
    assert_eq!(notice["params"]["update"]["sessionUpdate"], "subagent_spawned");
    assert_eq!(notice["params"]["update"]["child_session_id"], "child-1");
    assert_eq!(notice["params"]["update"]["parent_session_id"], "s1");

    let init = adapter.outbound(
        r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":1,"clientCapabilities":{}}}"#,
    );
    assert_eq!(parse(&init[0])["params"]["clientCapabilities"]["subagents"], true);
}

#[test]
fn a_cursor_command_without_skill_meta_still_hoists() {
    let mut adapter = CursorAdapter::new(None, false);
    adapter.inbound(
        r#"{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"s1","update":{"sessionUpdate":"available_commands_update","availableCommands":[{"name":"poteto-mode","description":"Poteto"}]}}}"#,
    );
    let sent = adapter.outbound(
        r#"{"jsonrpc":"2.0","id":2,"method":"session/prompt","params":{"sessionId":"s1","prompt":[{"type":"text","text":"please /poteto-mode review"}]}}"#,
    );
    assert_eq!(
        parse(&sent[0])["params"]["prompt"][0]["text"],
        "/poteto-mode please review"
    );
}

#[test]
fn cursor_question_is_answered_through_the_pager_ext_method() {
    let mut adapter = CursorAdapter::new(None, false);
    adapter.outbound(r#"{"jsonrpc":"2.0","id":1,"method":"session/new","params":{"cwd":"/tmp","mcpServers":[]}}"#);
    adapter.inbound(
        r#"{"jsonrpc":"2.0","id":1,"result":{"sessionId":"s1","modes":{"currentModeId":"agent","availableModes":[]}}}"#,
    );
    let asked = adapter.inbound(
        r#"{"jsonrpc":"2.0","id":9,"method":"cursor/ask_question","params":{"toolCallId":"call_123","questions":[{"id":"q1","prompt":"Which mode?","options":[{"id":"plan","label":"Plan"}]}]}}"#,
    );
    assert_eq!(parse(&asked[0])["method"], "ext_method");
    assert_eq!(parse(&asked[0])["params"]["params"]["sessionId"], "s1");

    let answered = adapter.outbound(
        r#"{"jsonrpc":"2.0","id":9,"result":{"outcome":"accepted","answers":{"Which mode?":["Plan"]}}}"#,
    );
    assert_eq!(
        parse(&answered[0])["result"]["outcome"]["outcome"],
        "answered"
    );
    assert_eq!(
        parse(&answered[0])["result"]["outcome"]["answers"][0]["selectedOptionIds"][0],
        "plan"
    );
}

#[test]
fn unknown_models_pass_through_unchanged() {
    let mut adapter = CursorAdapter::new(None, false);
    let out = adapter.outbound(
        r#"{"jsonrpc":"2.0","id":2,"method":"session/set_model","params":{"sessionId":"s1","modelId":"mystery"}}"#,
    );
    assert_eq!(parse(&out[0])["params"]["modelId"], "mystery");
}
