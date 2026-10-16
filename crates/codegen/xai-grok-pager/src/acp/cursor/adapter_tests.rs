use std::io::Write;

use super::*;

fn parse(routed: &Routed) -> Value {
    match routed {
        Routed::ToAgent(s) | Routed::ToClient(s) => serde_json::from_str(s).unwrap(),
        Routed::Cloud { .. } => panic!("expected a JSON line, got a cloud handoff"),
        Routed::Catalog(request) => {
            panic!("expected a JSON line, got a catalog request: {request:?}")
        }
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
    let released =
        dropped.inbound(r#"{"jsonrpc":"2.0","id":"grok-cursor-bridge-auth","result":{}}"#);
    assert_eq!(parse(&released[0])["params"]["mcpServers"], json!([]));

    let mut forwarded = CursorAdapter::new(None, true);
    forwarded.outbound(req);
    let released =
        forwarded.inbound(r#"{"jsonrpc":"2.0","id":"grok-cursor-bridge-auth","result":{}}"#);
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
    assert!(
        parse(&failed[0])["error"]["message"]
            .as_str()
            .unwrap()
            .contains("agent login")
    );

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
    assert!(
        notice["error"]["message"]
            .as_str()
            .unwrap()
            .contains("agent login")
    );
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
        repo,
        model,
    } = &sent[0]
    else {
        panic!("expected a cloud handoff: {sent:?}");
    };
    assert_eq!(session_id, "s1");
    assert_eq!(
        request,
        &CloudRequest::Send("refactor the auth module".to_owned())
    );
    assert_eq!(agent_id, &None);
    assert_eq!(prompt_rpc_id.as_ref().and_then(Value::as_u64), Some(4));
    assert_eq!(prompt_id, &None);
    assert!(repo.is_none());
    assert!(model.is_none());

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
    assert_eq!(
        closed.stop_reason,
        agent_client_protocol::StopReason::EndTurn
    );

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
        let Routed::Cloud {
            request, agent_id, ..
        } = &handoff[0]
        else {
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
    assert_eq!(
        sent["params"]["prompt"][0]["text"],
        "/poteto-mode please the tests\nand /review later"
    );
    assert_eq!(sent["params"]["prompt"][1]["text"], "");
    assert!(sent["params"].get("_meta").is_none());

    let second = adapter.outbound(
        r#"{"jsonrpc":"2.0","id":9,"method":"session/prompt","params":{"sessionId":"s2","prompt":[{"type":"text","text":"please /review the diff"}]}}"#,
    );
    let sent = parse(&second[0]);
    assert_eq!(
        sent["params"]["prompt"][0]["text"],
        "/review please the diff"
    );
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
    assert_eq!(
        notice["params"]["update"]["sessionUpdate"],
        "subagent_spawned"
    );
    assert_eq!(notice["params"]["update"]["child_session_id"], "child-1");
    assert_eq!(notice["params"]["update"]["parent_session_id"], "s1");

    let init = adapter.outbound(
        r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":1,"clientCapabilities":{}}}"#,
    );
    assert_eq!(
        parse(&init[0])["params"]["clientCapabilities"]["subagents"],
        true
    );
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

#[test]
fn installed_agent_accepts_the_bridge_initialize() {
    let mut adapter = CursorAdapter::new(None, false);
    let out = adapter.outbound(
        r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":1,"clientCapabilities":{}}}"#,
    );
    let Routed::ToAgent(line) = &out[0] else {
        panic!("expected initialize to be forwarded: {out:?}");
    };
    let sent: Value = serde_json::from_str(line).unwrap();
    assert_eq!(
        sent["params"]["clientCapabilities"]["_meta"]["subagents"],
        true
    );

    let bin = std::env::var("GROK_CURSOR_AGENT_BIN").unwrap_or_else(|_| {
        format!(
            "{}/.local/bin/agent",
            std::env::var("HOME").unwrap_or_default()
        )
    });
    if !std::path::Path::new(&bin).exists() {
        return;
    }
    let mut child = std::process::Command::new(&bin)
        .arg("acp")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("spawn agent acp");
    let mut stdin = child.stdin.take().expect("agent stdin");
    let stdout = child.stdout.take().expect("agent stdout");
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut reader = std::io::BufReader::new(stdout);
        let mut response = String::new();
        let _ = std::io::BufRead::read_line(&mut reader, &mut response);
        let _ = tx.send(response);
    });
    stdin
        .write_all(format!("{line}\n").as_bytes())
        .expect("write initialize");
    let _ = stdin.flush();
    let response = match rx.recv_timeout(std::time::Duration::from_secs(5)) {
        Ok(line) => line,
        Err(err) => {
            let _ = child.kill();
            let _ = child.wait();
            panic!("agent initialize timed out: {err}");
        }
    };
    let _ = child.kill();
    let _ = child.wait();
    let value: Value = serde_json::from_str(response.trim()).expect("agent json");
    assert_eq!(
        value["result"]["agentCapabilities"]["sessionCapabilities"]["subagents"],
        json!({})
    );
    assert_eq!(value["result"]["authMethods"][0]["id"], "cursor_login");
}

#[test]
fn cloud_create_uses_the_session_git_remote() {
    let dir = std::env::temp_dir().join(format!("grok-cloud-cwd-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    assert!(
        std::process::Command::new("git")
            .args(["init", "-b", "feature"])
            .current_dir(&dir)
            .status()
            .unwrap()
            .success()
    );
    assert!(
        std::process::Command::new("git")
            .args([
                "remote",
                "add",
                "origin",
                "https://github.com/acme/payments.git",
            ])
            .current_dir(&dir)
            .status()
            .unwrap()
            .success()
    );
    let mut adapter = CursorAdapter::new(None, false);
    let opened = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "session/new",
        "params": { "cwd": dir, "mcpServers": [] }
    });
    let _ = adapter.outbound(&opened.to_string());
    let created = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "result": { "sessionId": "s-repo" }
    });
    let _ = adapter.inbound(&created.to_string());
    let prompt = json!({
        "jsonrpc": "2.0",
        "id": 2,
        "method": "session/prompt",
        "params": {
            "sessionId": "s-repo",
            "prompt": [{ "type": "text", "text": "& fix the tests" }]
        }
    });
    let sent = adapter.outbound(&prompt.to_string());
    let Routed::Cloud {
        repo,
        agent_id,
        request,
        ..
    } = &sent[0]
    else {
        panic!("expected a cloud create: {sent:?}");
    };
    assert_eq!(agent_id, &None);
    assert_eq!(request, &CloudRequest::Send("fix the tests".to_owned()));
    let repo = repo.as_ref().expect("git remote");
    assert_eq!(repo.url, "https://github.com/acme/payments");
    assert_eq!(repo.starting_ref.as_deref(), Some("feature"));

    adapter.note_cloud_agent("s-repo", "bc-1");
    let follow = json!({
        "jsonrpc": "2.0",
        "id": 3,
        "method": "session/prompt",
        "params": {
            "sessionId": "s-repo",
            "prompt": [{ "type": "text", "text": "& add a note" }]
        }
    });
    let sent = adapter.outbound(&follow.to_string());
    let Routed::Cloud { repo, agent_id, .. } = &sent[0] else {
        panic!("expected a follow-up: {sent:?}");
    };
    assert_eq!(agent_id.as_deref(), Some("bc-1"));
    assert!(repo.is_none());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn cloud_create_sends_the_session_model_and_follows_switches() {
    let mut adapter = CursorAdapter::new(None, false);
    let _ = adapter.outbound(
        r#"{"jsonrpc":"2.0","id":1,"method":"session/new","params":{"cwd":"/tmp","mcpServers":[]}}"#,
    );
    let _ = adapter.inbound(&session_new_response(1));
    let sent = adapter.outbound(
        r#"{"jsonrpc":"2.0","id":2,"method":"session/prompt","params":{"sessionId":"s1","prompt":[{"type":"text","text":"& fix the build"}]}}"#,
    );
    let Routed::Cloud { model, .. } = &sent[0] else {
        panic!("expected a cloud create: {sent:?}");
    };
    assert_eq!(model.as_deref(), Some("claude-opus-5-5[context=300k]"));

    let _ = adapter.outbound(
        r#"{"jsonrpc":"2.0","id":3,"method":"session/set_model","params":{"sessionId":"s1","modelId":"grok-4.7"}}"#,
    );
    let sent = adapter.outbound(
        r#"{"jsonrpc":"2.0","id":4,"method":"session/prompt","params":{"sessionId":"s1","prompt":[{"type":"text","text":"& try again"}]}}"#,
    );
    let Routed::Cloud { model, .. } = &sent[0] else {
        panic!("expected a cloud create: {sent:?}");
    };
    assert_eq!(model.as_deref(), Some("grok-4.7[context=256k,fast=true]"));
}

#[test]
fn cursor_models_enrich_meta_and_apply_parameter_overrides() {
    let mut adapter = CursorAdapter::new(None, false);
    let _ = adapter.outbound(
        r#"{"jsonrpc":"2.0","id":1,"method":"session/new","params":{"cwd":"/tmp","mcpServers":[]}}"#,
    );
    let out = adapter.inbound(&session_new_response(1));
    let resp = parse(&out[0]);
    let available = resp["result"]["models"]["availableModels"]
        .as_array()
        .expect("availableModels array");
    assert_eq!(available[0]["_meta"]["totalContextTokens"], 300_000);
    assert_eq!(available[0]["_meta"]["contextWindow"], 300_000);
    assert_eq!(
        available[0]["_meta"]["contextWindows"],
        json!([300_000, 1_000_000])
    );
    assert_eq!(available[0]["_meta"]["supportsReasoningEffort"], true);
    assert_eq!(available[1]["_meta"]["totalContextTokens"], 256_000);
    assert_eq!(available[1]["_meta"]["fastMode"], true);
    assert_eq!(available[1]["_meta"]["supportsFastMode"], true);
    assert_eq!(
        available[1]["_meta"]["modelParameters"],
        json!({"context": "256k", "fast": "true"})
    );

    let set_fast_off = adapter.outbound(
        r#"{"jsonrpc":"2.0","id":2,"method":"session/set_model","params":{"sessionId":"s1","modelId":"grok-4.7[context=256k,fast=true]","_meta":{"fastMode":false,"reasoningEffort":"high","contextWindow":1000000}}}"#,
    );
    assert_eq!(
        parse(&set_fast_off[0])["params"]["modelId"],
        "grok-4.7[context=1m,fast=false,effort=high]"
    );

    let set_bracketed = adapter.outbound(
        r#"{"jsonrpc":"2.0","id":3,"method":"session/set_model","params":{"sessionId":"s1","modelId":"claude-opus-5-5[context=1m,effort=xhigh,maxMode=true]"}}"#,
    );
    assert_eq!(
        parse(&set_bracketed[0])["params"]["modelId"],
        "claude-opus-5-5[context=1m,effort=xhigh,maxMode=true]"
    );
}

/// Every catalog method the pager sends for the skills, hooks, plugins, marketplace, workflows, and commands
/// surfaces, with the params it sends them with.
fn catalog_requests() -> Vec<(&'static str, Value)> {
    let session = json!({ "sessionId": "s1" });
    vec![
        (
            "x.ai/skills/list",
            json!({ "cwd": ".", "sessionId": "s1", "refresh": true }),
        ),
        ("x.ai/skills/config", json!({ "cwd": "." })),
        ("x.ai/skills/refresh-baseline", json!({})),
        ("x.ai/workflows/list", session.clone()),
        ("x.ai/commands/list", session.clone()),
        ("x.ai/hooks/list", session.clone()),
        (
            "x.ai/hooks/action",
            json!({ "sessionId": "s1", "action": { "type": "reload" } }),
        ),
        ("x.ai/plugins/list", session.clone()),
        (
            "x.ai/plugins/action",
            json!({ "sessionId": "s1", "action": { "type": "reload" } }),
        ),
        ("x.ai/plugins/reload", json!({})),
        (
            "x.ai/plugins/notify-updates",
            json!({ "sessionId": "s1", "updates": [] }),
        ),
        ("x.ai/marketplace/list", session.clone()),
        (
            "x.ai/marketplace/action",
            json!({ "sessionId": "s1", "action": { "type": "refresh" } }),
        ),
    ]
}

fn no_disabled_skills() -> HashSet<String> {
    HashSet::new()
}

fn lint_disabled() -> HashSet<String> {
    HashSet::from(["lint".to_owned()])
}

/// An adapter with session `s1` open in `cwd` and Cursor's command catalog announced.
fn adapter_with_session(cwd: &std::path::Path, disabled: fn() -> HashSet<String>) -> CursorAdapter {
    let mut adapter = CursorAdapter::new(None, false).with_disabled_skills(disabled);
    adapter.outbound(
        &json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "session/new",
            "params": { "cwd": cwd, "mcpServers": [] },
        })
        .to_string(),
    );
    adapter.inbound(&session_new_response(1));
    adapter.inbound(
        &json!({
            "jsonrpc": "2.0",
            "method": "session/update",
            "params": {
                "sessionId": "s1",
                "update": {
                    "sessionUpdate": "available_commands_update",
                    "availableCommands": [
                        { "name": "cursor-team-review", "description": "Team review" },
                        { "name": "lint", "description": "Lint" },
                        { "name": "copy-request-id", "description": "Copy the last request ID" },
                    ],
                },
            },
        })
        .to_string(),
    );
    adapter
}

fn catalog_request(
    adapter: &mut CursorAdapter,
    id: u64,
    method: &str,
    params: &Value,
) -> CatalogRequest {
    let out = adapter.outbound(
        &json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }).to_string(),
    );
    assert_eq!(out.len(), 1, "{method}: {out:?}");
    let Routed::Catalog(request) = &out[0] else {
        panic!("{method} must be answered locally, got {out:?}");
    };
    (**request).clone()
}

#[test]
fn catalog_methods_are_answered_locally_for_the_session_cwd() {
    let dir = tempfile::tempdir().unwrap();
    let mut adapter = adapter_with_session(dir.path(), no_disabled_skills);

    for (index, (method, params)) in catalog_requests().into_iter().enumerate() {
        for wire in [method.to_owned(), format!("_{method}")] {
            let id = 100 + index as u64;
            let request = catalog_request(&mut adapter, id, &wire, &params);
            assert_eq!(request.method, method);
            assert_eq!(request.id, json!(id));
            assert_eq!(request.cwd, dir.path());
            assert_eq!(request.session_id.as_deref(), Some("s1"));
            assert_eq!(request.params, params);
            let catalog = &request.sessions[0];
            assert_eq!(catalog.0, "s1");
            assert_eq!(
                catalog
                    .1
                    .iter()
                    .map(|c| c["name"].as_str().unwrap())
                    .collect::<Vec<_>>(),
                ["cursor-team-review", "lint", "copy-request-id"]
            );
        }
    }
}

#[test]
fn catalog_notifications_are_dropped() {
    let mut adapter = CursorAdapter::new(None, false);
    assert!(
        adapter
            .outbound(r#"{"jsonrpc":"2.0","method":"_x.ai/skills/list","params":{"cwd":"."}}"#)
            .is_empty()
    );
}

#[test]
fn every_catalog_method_succeeds_on_the_cursor_backend() {
    let dir = tempfile::tempdir().unwrap();
    let mut adapter = adapter_with_session(dir.path(), no_disabled_skills);

    for (index, (method, params)) in catalog_requests().into_iter().enumerate() {
        let id = 200 + index as u64;
        let request = catalog_request(&mut adapter, id, &format!("_{method}"), &params);
        let lines = catalog::answer_blocking(request);
        let reply: Value = serde_json::from_str(&lines[0]).unwrap();
        assert_eq!(reply["id"], json!(id), "{method}: {reply}");
        assert!(reply.get("error").is_none(), "{method} failed: {reply}");
        let result = &reply["result"];
        assert!(
            result.get("error").is_none_or(Value::is_null),
            "{method} answered an error: {reply}"
        );
        match method {
            "x.ai/skills/list" => {
                let names: Vec<&str> = result["result"]["skills"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .filter_map(|s| s["name"].as_str())
                    .collect();
                assert!(names.contains(&"cursor-team-review"), "{names:?}");
                assert!(!names.contains(&"copy-request-id"), "{names:?}");
            }
            "x.ai/commands/list" => {
                let names: Vec<&str> = result["commands"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .filter_map(|c| c["name"].as_str())
                    .collect();
                assert_eq!(names, ["cursor-team-review", "lint", "copy-request-id"]);
            }
            "x.ai/skills/config" => assert!(result["result"]["skills"].is_array()),
            "x.ai/workflows/list" => assert!(result["result"]["workflows"].is_array()),
            "x.ai/hooks/list" => assert!(result["result"]["hooks"].is_array()),
            "x.ai/plugins/list" => assert!(result["result"]["plugins"].is_array()),
            "x.ai/marketplace/list" => assert!(result["result"]["sources"].is_array()),
            "x.ai/hooks/action" | "x.ai/plugins/action" | "x.ai/marketplace/action" => {
                assert_eq!(result["result"]["status"], "success", "{method}: {reply}");
            }
            _ => assert_eq!(result["result"]["ok"], true, "{method}: {reply}"),
        }
    }
}

#[test]
fn refresh_baseline_republishes_each_sessions_cursor_catalog() {
    let dir = tempfile::tempdir().unwrap();
    let mut adapter = adapter_with_session(dir.path(), no_disabled_skills);

    let request = catalog_request(&mut adapter, 9, "_x.ai/skills/refresh-baseline", &json!({}));
    let lines = catalog::answer_blocking(request);

    assert_eq!(lines.len(), 2, "{lines:?}");
    let update: Value = serde_json::from_str(&lines[1]).unwrap();
    assert_eq!(update["method"], "session/update");
    assert_eq!(update["params"]["sessionId"], "s1");
    assert_eq!(
        update["params"]["update"]["sessionUpdate"],
        "available_commands_update"
    );
    assert!(update["params"]["update"]["availableCommands"].is_array());
}

#[test]
fn disabled_skills_leave_the_slash_menu_but_stay_in_the_catalog() {
    let dir = tempfile::tempdir().unwrap();
    let mut adapter = CursorAdapter::new(None, false).with_disabled_skills(lint_disabled);
    adapter.outbound(
        &json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "session/new",
            "params": { "cwd": dir.path(), "mcpServers": [] },
        })
        .to_string(),
    );
    adapter.inbound(&session_new_response(1));
    let forwarded = adapter.inbound(
        r#"{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"s1","update":{"sessionUpdate":"available_commands_update","availableCommands":[{"name":"lint","description":"Lint"},{"name":"review","description":"Review"}]}}}"#,
    );

    let shown: Vec<String> = parse(&forwarded[0])["params"]["update"]["availableCommands"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["name"].as_str().unwrap().to_owned())
        .collect();
    assert_eq!(shown, ["review"]);

    let request = catalog_request(&mut adapter, 2, "x.ai/skills/list", &json!({ "cwd": "." }));
    let names: Vec<&str> = request.sessions[0]
        .1
        .iter()
        .filter_map(|c| c["name"].as_str())
        .collect();
    assert_eq!(names, ["lint", "review"]);
}

#[test]
fn cursor_skills_become_skill_entries_once() {
    let commands = vec![
        json!({ "name": "lint", "description": "Lint", "_meta": { "path": "cursor://skills/lint", "scope": "user" } }),
        json!({ "name": "lint", "description": "Lint again", "_meta": { "path": "cursor://skills/lint", "scope": "user" } }),
        json!({ "name": "copy-request-id", "description": "Copy" }),
        json!({ "name": "deploy", "description": "", "_meta": { "path": "/repo/.cursor/skills/deploy/SKILL.md", "scope": "project" } }),
    ];

    let skills = catalog::cursor_skills(&commands);

    assert_eq!(
        skills
            .iter()
            .map(|s| (s.name.as_str(), s.path.as_str(), s.description.as_str()))
            .collect::<Vec<_>>(),
        [
            ("lint", "cursor://skills/lint", "Lint"),
            ("deploy", "/repo/.cursor/skills/deploy/SKILL.md", "deploy"),
        ]
    );
}

#[test]
fn other_grok_extensions_stay_unsupported() {
    let mut adapter = CursorAdapter::new(None, false);
    for method in [
        "_x.ai/queue/clear",
        "_x.ai/rewind/points",
        "_x.ai/memory/flush",
    ] {
        let out = adapter.outbound(
            &json!({ "jsonrpc": "2.0", "id": 5, "method": method, "params": {} }).to_string(),
        );
        assert_eq!(
            parse(&out[0])["error"]["code"],
            METHOD_NOT_FOUND,
            "{method}"
        );
    }
}
