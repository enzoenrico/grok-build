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
    let note: agent_client_protocol::SessionNotification =
        serde_json::from_value(parse(&out[0])["params"].clone()).expect("pager mode update");
    let agent_client_protocol::SessionUpdate::CurrentModeUpdate(mode) = note.update else {
        panic!("expected a mode update");
    };
    assert_eq!(mode.current_mode_id.0.as_ref(), "default");
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
    let mut dropped = CursorAdapter::new(None, false);
    let auth = dropped.outbound(req);
    assert_eq!(parse(&auth[0])["params"]["methodId"], "cursor_login");
    let released = dropped.inbound(
        r#"{"jsonrpc":"2.0","id":"grok-cursor-bridge-auth","result":{}}"#,
    );
    assert_eq!(parse(&released[0])["params"]["mcpServers"], json!([]));

    let mut forwarded = CursorAdapter::new(None, true);
    forwarded.outbound(req);
    let released = forwarded.inbound(
        r#"{"jsonrpc":"2.0","id":"grok-cursor-bridge-auth","result":{}}"#,
    );
    assert_eq!(parse(&released[0])["params"]["mcpServers"][0]["name"], "x");
}

#[test]
fn session_new_waits_for_cursor_login() {
    let mut adapter = CursorAdapter::new(None, false);
    let first = adapter.outbound(
        r#"{"jsonrpc":"2.0","id":4,"method":"session/new","params":{"cwd":"/tmp","mcpServers":[]}}"#,
    );
    assert_eq!(parse(&first[0])["method"], "authenticate");
    assert_eq!(parse(&first[0])["params"]["methodId"], "cursor_login");
    let second = adapter.outbound(
        r#"{"jsonrpc":"2.0","id":5,"method":"session/new","params":{"cwd":"/work","mcpServers":[]}}"#,
    );
    assert!(second.is_empty());

    let failed = adapter.inbound(
        r#"{"jsonrpc":"2.0","id":"grok-cursor-bridge-auth","error":{"code":-32000,"message":"Authentication required"}}"#,
    );
    assert_eq!(failed.len(), 2);
    assert!(parse(&failed[0])["error"]["message"]
        .as_str()
        .unwrap()
        .contains("agent login"));

    let again = adapter.outbound(
        r#"{"jsonrpc":"2.0","id":6,"method":"authenticate","params":{"methodId":"xai.api_key"}}"#,
    );
    assert_eq!(parse(&again[0])["params"]["methodId"], "cursor_login");
    let created = adapter.outbound(
        r#"{"jsonrpc":"2.0","id":7,"method":"session/new","params":{"cwd":"/tmp","mcpServers":[]}}"#,
    );
    assert!(created.is_empty());
    let released = adapter.inbound(r#"{"jsonrpc":"2.0","id":6,"result":{}}"#);
    assert_eq!(parse(&released[0])["method"], "session/new");
    assert_eq!(parse(&released[1])["id"], 6);
    let later = adapter.outbound(
        r#"{"jsonrpc":"2.0","id":8,"method":"session/new","params":{"cwd":"/tmp","mcpServers":[]}}"#,
    );
    assert_eq!(parse(&later[0])["method"], "session/new");

    let mut stalled = CursorAdapter::new(None, false);
    stalled.outbound(
        r#"{"jsonrpc":"2.0","id":9,"method":"session/new","params":{"cwd":"/tmp","mcpServers":[]}}"#,
    );
    let notices = stalled.auth_timed_out();
    assert_eq!(notices.len(), 1);
    let notice: Value = serde_json::from_str(&notices[0]).unwrap();
    assert!(notice["error"]["message"]
        .as_str()
        .unwrap()
        .contains("agent login"));
    let after = stalled.outbound(
        r#"{"jsonrpc":"2.0","id":10,"method":"session/new","params":{"cwd":"/tmp","mcpServers":[]}}"#,
    );
    assert_eq!(parse(&after[0])["method"], "authenticate");
    assert_eq!(parse(&after[0])["params"]["methodId"], "cursor_login");
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
    let created: agent_client_protocol::NewSessionResponse =
        serde_json::from_value(resp["result"].clone()).expect("pager session result");
    let modes = created.modes.expect("modes");
    assert_eq!(modes.current_mode_id.0.as_ref(), "ask");
    let ids: Vec<&str> = modes
        .available_modes
        .iter()
        .map(|mode| mode.id.0.as_ref())
        .collect();
    assert_eq!(ids, ["default", "plan", "ask", "debug"]);

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
        request,
        agent_id,
        prompt_rpc_id,
        prompt_id,
    } = &sent[0]
    else {
        panic!("expected a cloud handoff: {sent:?}");
    };
    assert_eq!(session_id, "s1");
    assert_eq!(request, &CloudRequest::Send("refactor the auth module".to_owned()));
    assert_eq!(agent_id, &None);
    assert_eq!(prompt_rpc_id.as_ref().and_then(Value::as_u64), Some(4));
    assert_eq!(prompt_id, &None);

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
        request, agent_id, ..
    } = &follow[0]
    else {
        panic!("expected a cloud follow-up: {follow:?}");
    };
    assert_eq!(request, &CloudRequest::Send("add tests".to_owned()));
    assert_eq!(agent_id.as_deref(), Some("bc-123"));
}

#[test]
fn bare_ampersand_refreshes_the_recorded_agent() {
    let mut adapter = CursorAdapter::new(None, false);
    let missing = adapter.outbound(
        r#"{"jsonrpc":"2.0","id":4,"method":"session/prompt","params":{"sessionId":"s1","_meta":{"promptId":"p-9"},"prompt":[{"type":"text","text":"&"}]}}"#,
    );
    let Routed::ToClient(line) = &missing[0] else {
        panic!("expected a client notice: {missing:?}");
    };
    let notice: Value = serde_json::from_str(line).unwrap();
    assert_eq!(
        notice["params"]["update"]["content"]["text"],
        "No cloud agent on this session yet. Send & and a task to start one.\n"
    );
    let end: Value = match &missing[1] {
        Routed::ToClient(line) => serde_json::from_str(line).unwrap(),
        other => panic!("expected the prompt to end: {other:?}"),
    };
    assert_eq!(end["id"], 4);
    assert_eq!(end["result"]["stopReason"], "end_turn");
    assert_eq!(end["result"]["_meta"]["promptId"], "p-9");
    let closed: agent_client_protocol::PromptResponse =
        serde_json::from_value(end["result"].clone()).expect("pager prompt result");
    assert_eq!(closed.stop_reason, agent_client_protocol::StopReason::EndTurn);

    adapter.note_cloud_agent("s1", "bc-123");
    for text in ["&", "& ", "& status", "& Status"] {
        let refresh = adapter.outbound(&format!(
            r#"{{"jsonrpc":"2.0","id":5,"method":"session/prompt","params":{{"sessionId":"s1","prompt":[{{"type":"text","text":"{text}"}}]}}}}"#
        ));
        let Routed::Cloud {
            request, agent_id, ..
        } = &refresh[0]
        else {
            panic!("expected a refresh for {text}: {refresh:?}");
        };
        assert_eq!(request, &CloudRequest::Refresh);
        assert_eq!(agent_id.as_deref(), Some("bc-123"));
    }
    for text in ["& cancel", "& Cancel"] {
        let cancel = adapter.outbound(&format!(
            r#"{{"jsonrpc":"2.0","id":6,"method":"session/prompt","params":{{"sessionId":"s1","prompt":[{{"type":"text","text":"{text}"}}]}}}}"#
        ));
        let Routed::Cloud {
            request, agent_id, ..
        } = &cancel[0]
        else {
            panic!("expected a cancel for {text}: {cancel:?}");
        };
        assert_eq!(request, &CloudRequest::Cancel);
        assert_eq!(agent_id.as_deref(), Some("bc-123"));
    }
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
        let Routed::Cloud { request, agent_id, .. } = &handoff[0] else {
            panic!("expected a cloud handoff: {handoff:?}");
        };
        assert_eq!(request, &CloudRequest::Send("ship it".to_owned()));
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
    let listed = adapter.inbound(
        r#"{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"s1","update":{"sessionUpdate":"available_commands_update","availableCommands":[{"name":"poteto-mode","description":"Poteto"},{"name":"copy-request-id","description":"Copy the last request ID"}]}}}"#,
    );
    let update = &parse(&listed[0])["params"]["update"]["availableCommands"];
    assert_eq!(update[0]["_meta"]["path"], "cursor://skills/poteto-mode");
    assert_eq!(update[0]["_meta"]["scope"], "user");
    assert!(update[1].get("_meta").is_none());
    let sent = adapter.outbound(
        r#"{"jsonrpc":"2.0","id":2,"method":"session/prompt","params":{"sessionId":"s1","prompt":[{"type":"text","text":"please /poteto-mode review"}]}}"#,
    );
    assert_eq!(
        parse(&sent[0])["params"]["prompt"][0]["text"],
        "/poteto-mode please review"
    );
    let clipboard = adapter.outbound(
        r#"{"jsonrpc":"2.0","id":3,"method":"session/prompt","params":{"sessionId":"s1","prompt":[{"type":"text","text":"please /copy-request-id now"}]}}"#,
    );
    assert_eq!(
        parse(&clipboard[0])["params"]["prompt"][0]["text"],
        "please /copy-request-id now"
    );
    let params = parse(&listed[0])["params"].clone();
    let note: agent_client_protocol::SessionNotification =
        serde_json::from_value(params).expect("pager can read the command list");
    let agent_client_protocol::SessionUpdate::AvailableCommandsUpdate(update) = note.update else {
        panic!("expected available commands");
    };
    let skill = crate::slash::acp_command::AcpSlashCommand::from(&update.available_commands[0]);
    let plain = crate::slash::acp_command::AcpSlashCommand::from(&update.available_commands[1]);
    assert!(crate::slash::command::SlashCommand::is_skill(&skill));
    assert!(!crate::slash::command::SlashCommand::is_skill(&plain));
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
