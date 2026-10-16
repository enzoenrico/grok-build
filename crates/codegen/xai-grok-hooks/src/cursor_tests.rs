use std::path::Path;

use super::*;
use crate::discovery::{HookRegistry, registry_from_specs_deduped};
use crate::event::{HookEventEnvelope, HookEventName, HookPayload};
use crate::result::{HookDecision, PromptDecision};
use crate::runner::RunContext;

fn envelope(event: HookEventName, payload: HookPayload) -> HookEventEnvelope {
    HookEventEnvelope {
        hook_event_name: event,
        session_id: "cursor-test-session".into(),
        cwd: "/work".into(),
        workspace_root: "/work".into(),
        timestamp: "2026-01-01T00:00:00Z".into(),
        transcript_path: Some("/tmp/transcript.jsonl".into()),
        client_identifier: None,
        prompt_id: Some("gen-1".into()),
        permission_mode: None,
        payload,
    }
}

fn pre_tool(tool_name: &str, tool_input: serde_json::Value) -> HookEventEnvelope {
    envelope(
        HookEventName::PreToolUse,
        HookPayload::PreToolUse {
            tool_name: tool_name.into(),
            tool_use_id: "tu-1".into(),
            tool_input,
            tool_input_truncated: false,
            subagent_type: None,
        },
    )
}

fn post_tool(
    tool_name: &str,
    tool_input: serde_json::Value,
    result: serde_json::Value,
) -> HookEventEnvelope {
    envelope(
        HookEventName::PostToolUse,
        HookPayload::PostToolUse {
            tool_name: tool_name.into(),
            tool_use_id: "tu-1".into(),
            tool_input,
            tool_result: result,
            tool_input_truncated: false,
            tool_result_truncated: false,
            duration_ms: Some(42),
            is_backgrounded: false,
            subagent_type: None,
        },
    )
}

fn stop(session: &str, stop_hook_active: bool) -> HookEventEnvelope {
    let mut env = envelope(
        HookEventName::Stop,
        HookPayload::Stop {
            reason: "end_turn".into(),
            stop_hook_active,
            last_assistant_message: Some("done".into()),
            background_tasks: None,
            session_crons: None,
        },
    );
    env.session_id = session.into();
    env
}

fn hook(event: CursorEvent, matcher: Option<&str>) -> CursorHook {
    CursorHook {
        event,
        matcher: matcher.map(str::to_string),
        fail_closed: false,
        loop_limit: Some(DEFAULT_LOOP_LIMIT),
        working_dir: "/work".into(),
    }
}

fn parse(json: serde_json::Value) -> (Vec<HookSpec>, Vec<HookError>) {
    parse_document(
        &json,
        Path::new("/work/.cursor/hooks.json"),
        Path::new("/work"),
        "hooks",
        HookProvenance::File,
    )
}

#[test]
fn detects_cursor_documents_but_not_claude_ones() {
    assert!(is_cursor_document(
        &serde_json::json!({"version": 1, "hooks": {}})
    ));
    assert!(is_cursor_document(
        &serde_json::json!({"hooks": {"stop": [{"command": "./a.sh"}]}})
    ));
    assert!(!is_cursor_document(&serde_json::json!({
        "hooks": {"PreToolUse": [{"matcher": "Bash", "hooks": [{"type": "command", "command": "a"}]}]}
    })));
    assert!(!is_cursor_document(&serde_json::json!({"permissions": {}})));
}

#[test]
fn parses_every_supported_event_onto_its_grok_event() {
    let mut hooks = serde_json::Map::new();
    for event in CursorEvent::ALL {
        hooks.insert(
            event.key().into(),
            serde_json::json!([{"command": format!("./{}.sh", event.key())}]),
        );
    }
    let (specs, errors) = parse(serde_json::json!({"version": 1, "hooks": hooks}));
    assert!(errors.is_empty(), "{errors:?}");
    assert_eq!(specs.len(), CursorEvent::ALL.len());
    for spec in &specs {
        let cursor = spec.cursor.as_ref().expect("cursor dialect");
        assert_eq!(spec.event, cursor.event.grok_event());
        assert_eq!(spec.source_dir, Path::new("/work"));
    }
    let shell = specs
        .iter()
        .find(|s| {
            s.cursor
                .as_ref()
                .is_some_and(|c| c.event == CursorEvent::BeforeShellExecution)
        })
        .expect("beforeShellExecution spec");
    assert_eq!(shell.event, HookEventName::PreToolUse);
    assert_eq!(shell.name, "hooks:before_shell_execution[0].hooks[0]");
}

#[test]
fn entry_options_follow_cursor_defaults() {
    let (specs, errors) = parse(serde_json::json!({
        "version": 1,
        "hooks": {
            "stop": [
                {"command": "./a.sh"},
                {"command": "./b.sh", "loop_limit": null, "timeout": 2, "failClosed": true},
                {"command": "./c.sh", "loop_limit": 10, "matcher": "*"}
            ]
        }
    }));
    assert!(errors.is_empty(), "{errors:?}");
    let cursor: Vec<&CursorHook> = specs.iter().filter_map(|s| s.cursor.as_ref()).collect();
    assert_eq!(cursor.len(), 3);
    assert_eq!(cursor.first().map(|c| c.loop_limit), Some(Some(5)));
    assert_eq!(cursor.get(1).map(|c| c.loop_limit), Some(None));
    assert_eq!(cursor.get(1).map(|c| c.fail_closed), Some(true));
    assert_eq!(specs.get(1).map(|s| s.timeout_ms), Some(2000));
    assert_eq!(cursor.get(2).map(|c| c.loop_limit), Some(Some(10)));
    assert_eq!(
        cursor.get(2).map(|c| c.matcher.clone()),
        Some(None),
        "\"*\" matches everything"
    );
}

#[test]
fn rejects_bad_entries_and_skips_unsupported_events() {
    let (specs, errors) = parse(serde_json::json!({
        "version": 1,
        "hooks": {
            "afterAgentThought": [{"command": "./t.sh"}],
            "workspaceOpen": [{"command": "./w.sh"}],
            "preToolUse": [
                {"command": "./ok.sh"},
                {"type": "prompt", "prompt": "is this safe?"},
                {"command": "./bad.sh", "matcher": "("},
                {"matcher": "Shell"}
            ]
        }
    }));
    assert_eq!(specs.len(), 1);
    assert_eq!(errors.len(), 3, "{errors:?}");
}

#[test]
fn working_dir_is_project_root_for_project_files_and_the_dir_otherwise() {
    let home = Path::new("/home/me");
    assert_eq!(
        working_dir_for(Path::new("/repo/.cursor/hooks.json"), Some(home)),
        Path::new("/repo")
    );
    assert_eq!(
        working_dir_for(Path::new("/home/me/.cursor/hooks.json"), Some(home)),
        Path::new("/home/me/.cursor")
    );
    assert_eq!(
        working_dir_for(Path::new("/etc/cursor/hooks.json"), Some(home)),
        Path::new("/etc/cursor")
    );
    assert_eq!(
        working_dir_for(Path::new("/plugins/p/hooks/hooks.json"), Some(home)),
        Path::new("/plugins/p/hooks")
    );
}

#[test]
fn tool_events_filter_by_tool_kind_and_match_cursor_targets() {
    let shell = pre_tool(
        "run_terminal_command",
        serde_json::json!({"command": "curl x"}),
    );
    let read = pre_tool("read_file", serde_json::json!({"path": "a.txt"}));
    let mcp = pre_tool("linear__create_issue", serde_json::json!({"title": "t"}));

    assert!(hook(CursorEvent::PreToolUse, Some("Shell")).applies(&shell));
    assert!(!hook(CursorEvent::PreToolUse, Some("Shell")).applies(&read));
    assert!(hook(CursorEvent::PreToolUse, Some("MCP:create_issue")).applies(&mcp));

    assert!(hook(CursorEvent::BeforeShellExecution, Some("curl|wget")).applies(&shell));
    assert!(!hook(CursorEvent::BeforeShellExecution, Some("rm ")).applies(&shell));
    assert!(!hook(CursorEvent::BeforeShellExecution, None).applies(&read));

    assert!(hook(CursorEvent::BeforeMcpExecution, None).applies(&mcp));
    assert!(!hook(CursorEvent::BeforeMcpExecution, None).applies(&shell));

    assert!(hook(CursorEvent::BeforeReadFile, Some("Read")).applies(&read));
    assert!(!hook(CursorEvent::BeforeReadFile, None).applies(&shell));

    let edit = post_tool(
        "search_replace",
        serde_json::json!({"file_path": "src/a.rs", "old_string": "a", "new_string": "b"}),
        serde_json::json!("ok"),
    );
    assert!(hook(CursorEvent::AfterFileEdit, Some("Write")).applies(&edit));
    assert!(!hook(CursorEvent::AfterShellExecution, None).applies(&edit));
}

#[test]
fn stdin_carries_cursor_common_and_event_fields() {
    let shell = pre_tool(
        "run_terminal_command",
        serde_json::json!({"command": "npm test", "working_directory": "/work/app"}),
    );
    let json = hook(CursorEvent::BeforeShellExecution, None).stdin_json(&shell, "h");
    assert_eq!(json["hook_event_name"], "beforeShellExecution");
    assert_eq!(json["conversation_id"], "cursor-test-session");
    assert_eq!(json["generation_id"], "gen-1");
    assert_eq!(json["workspace_roots"], serde_json::json!(["/work"]));
    assert_eq!(json["transcript_path"], "/tmp/transcript.jsonl");
    assert_eq!(json["command"], "npm test");
    assert_eq!(json["cwd"], "/work/app");
    assert_eq!(json["sandbox"], false);

    let pre = hook(CursorEvent::PreToolUse, None).stdin_json(&shell, "h");
    assert_eq!(pre["tool_name"], "Shell");
    assert_eq!(pre["tool_input"]["command"], "npm test");
    assert_eq!(pre["tool_use_id"], "tu-1");

    let mcp = pre_tool("linear__create_issue", serde_json::json!({"title": "t"}));
    let before_mcp = hook(CursorEvent::BeforeMcpExecution, None).stdin_json(&mcp, "h");
    assert_eq!(before_mcp["tool_name"], "create_issue");
    assert_eq!(before_mcp["mcp_server_name"], "linear");
    assert_eq!(before_mcp["tool_input"], r#"{"title":"t"}"#);

    let edit = post_tool(
        "search_replace",
        serde_json::json!({"file_path": "src/a.rs", "old_string": "a", "new_string": "b"}),
        serde_json::json!("ok"),
    );
    let after_edit = hook(CursorEvent::AfterFileEdit, None).stdin_json(&edit, "h");
    assert_eq!(after_edit["file_path"], "/work/src/a.rs");
    assert_eq!(
        after_edit["edits"],
        serde_json::json!([{"old_string": "a", "new_string": "b"}])
    );

    let post = hook(CursorEvent::PostToolUse, None).stdin_json(&edit, "h");
    assert_eq!(post["tool_name"], "Write");
    assert_eq!(post["tool_output"], "ok");
    assert_eq!(post["duration"], 42);
}

#[test]
fn permission_output_maps_to_gate_results() {
    let shell = pre_tool("run_terminal_command", serde_json::json!({"command": "ls"}));
    let pre = hook(CursorEvent::PreToolUse, None);
    let run = |h: &CursorHook, stdout: &str, exit: i32| {
        h.parse_output(GateKind::Tool, stdout, "", exit, "h", &shell)
    };

    assert!(matches!(
        run(&pre, r#"{"permission":"deny","agent_message":"no network","user_message":"blocked"}"#, 0),
        HookRunnerResult::Deny { ref reason, .. } if reason == "no network"
    ));
    assert!(matches!(
        run(&pre, r#"{"permission":"allow","updated_input":{"command":"ls -la"}}"#, 0),
        HookRunnerResult::Allow { updated_input: Some(ref input), .. } if input["command"] == "ls -la"
    ));
    assert!(matches!(run(&pre, "", 0), HookRunnerResult::Allow { .. }));
    assert!(matches!(
        run(&pre, "not json", 0),
        HookRunnerResult::Deny { .. }
    ));
    assert!(matches!(
        run(&pre, r#"{"permission":"maybe"}"#, 0),
        HookRunnerResult::Deny { .. }
    ));
    assert!(matches!(run(&pre, "", 2), HookRunnerResult::Deny { .. }));
    assert!(matches!(run(&pre, "", 1), HookRunnerResult::Failed(_)));
    assert!(
        matches!(
            run(&pre, r#"{"permission":"ask"}"#, 0),
            HookRunnerResult::Allow { .. }
        ),
        "preToolUse accepts ask but does not enforce it"
    );

    let shell_hook = hook(CursorEvent::BeforeShellExecution, None);
    assert!(matches!(
        run(&shell_hook, r#"{"permission":"ask","user_message":"confirm?"}"#, 0),
        HookRunnerResult::Ask { reason: Some(ref r), .. } if r == "confirm?"
    ));

    let closed = CursorHook {
        fail_closed: true,
        ..hook(CursorEvent::BeforeShellExecution, None)
    };
    let failed = run(&closed, "", 1);
    assert!(matches!(
        closed.apply_fail_closed(failed, "h"),
        HookRunnerResult::Deny { .. }
    ));
    let timed_out = closed.apply_fail_closed(HookRunnerResult::Failed("timed out".into()), "h");
    assert!(matches!(timed_out, HookRunnerResult::Deny { .. }));
    assert!(matches!(
        hook(CursorEvent::BeforeShellExecution, None)
            .apply_fail_closed(HookRunnerResult::Failed("timed out".into()), "h"),
        HookRunnerResult::Failed(_)
    ));
}

#[test]
fn prompt_and_post_tool_outputs_translate() {
    let prompt = envelope(
        HookEventName::UserPromptSubmit,
        HookPayload::UserPromptSubmit {
            prompt: Some("hi".into()),
            subagent_type: None,
        },
    );
    let submit = hook(CursorEvent::BeforeSubmitPrompt, None);
    assert!(matches!(
        submit.parse_output(GateKind::Prompt, r#"{"continue":false,"user_message":"no"}"#, "", 0, "h", &prompt),
        HookRunnerResult::Block { ref reason, .. } if reason == "no"
    ));
    assert!(matches!(
        submit.parse_output(
            GateKind::Prompt,
            r#"{"continue":true}"#,
            "",
            0,
            "h",
            &prompt
        ),
        HookRunnerResult::Success
    ));

    let mcp = post_tool(
        "linear__get",
        serde_json::json!({}),
        serde_json::json!({"a": 1}),
    );
    let post = hook(CursorEvent::PostToolUse, None);
    match post.parse_output(
        GateKind::PostTool,
        r#"{"additional_context":"note","updated_mcp_tool_output":{"b":2}}"#,
        "",
        0,
        "h",
        &mcp,
    ) {
        HookRunnerResult::PostToolUse { outcome, .. } => {
            assert_eq!(outcome.additional_context.as_deref(), Some("note"));
            let replacement = outcome.output_replacement.expect("mcp replacement");
            assert_eq!(replacement.kind, ReplacementKind::Mcp);
            assert_eq!(replacement.value, serde_json::json!({"b": 2}));
        }
        other => panic!("unexpected {other:?}"),
    }
    let builtin = post_tool("read_file", serde_json::json!({}), serde_json::json!("x"));
    match post.parse_output(
        GateKind::PostTool,
        r#"{"updated_mcp_tool_output":{"b":2}}"#,
        "",
        0,
        "h",
        &builtin,
    ) {
        HookRunnerResult::PostToolUse { outcome, .. } => {
            assert!(outcome.output_replacement.is_none(), "MCP-only field");
        }
        other => panic!("unexpected {other:?}"),
    }
}

#[test]
fn stop_followups_respect_loop_limit_and_reset_on_a_fresh_stop() {
    let limited = CursorHook {
        loop_limit: Some(2),
        ..hook(CursorEvent::Stop, None)
    };
    let session = "loop-limit-session";
    let followup = r#"{"followup_message":"keep going"}"#;
    let mut continued = 0;
    for turn in 0..4 {
        let env = stop(session, turn > 0);
        let stdin = limited.stdin_json(&env, "stop-hook");
        assert_eq!(stdin["loop_count"], continued);
        match limited.parse_output(GateKind::Stop, followup, "", 0, "stop-hook", &env) {
            HookRunnerResult::Stop(outcome) if outcome.additional_context.is_some() => {
                assert_eq!(outcome.additional_context.as_deref(), Some("keep going"));
                continued += 1;
            }
            HookRunnerResult::Stop(_) => {}
            other => panic!("unexpected {other:?}"),
        }
    }
    assert_eq!(continued, 2);

    let fresh = stop(session, false);
    assert_eq!(limited.stdin_json(&fresh, "stop-hook")["loop_count"], 0);
}

#[test]
fn session_start_env_reaches_later_hooks() {
    let mut start = envelope(
        HookEventName::SessionStart,
        HookPayload::SessionStart {
            source: "new".into(),
            model_id: None,
            agent_type: None,
        },
    );
    start.session_id = "env-session".into();
    let h = hook(CursorEvent::SessionStart, None);
    assert!(matches!(
        h.parse_output(
            GateKind::Observe,
            r#"{"env":{"TEAM":"core"}}"#,
            "",
            0,
            "h",
            &start
        ),
        HookRunnerResult::Success
    ));
    let env = h.process_env(&start);
    assert_eq!(env.get("TEAM").map(String::as_str), Some("core"));
    assert_eq!(
        env.get("CURSOR_TRANSCRIPT_PATH").map(String::as_str),
        Some("/tmp/transcript.jsonl")
    );
}

#[test]
fn dedup_keeps_distinct_cursor_events_sharing_a_grok_event() {
    let (specs, errors) = parse(serde_json::json!({
        "version": 1,
        "hooks": {
            "preToolUse": [{"command": "./audit.sh"}],
            "beforeShellExecution": [{"command": "./audit.sh"}]
        }
    }));
    assert!(errors.is_empty());
    let registry = registry_from_specs_deduped(specs);
    assert_eq!(registry.hooks_for(HookEventName::PreToolUse).len(), 2);
}

#[cfg(unix)]
mod process {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn ctx(root: &str) -> RunContext<'_> {
        RunContext {
            session_id: "cursor-process-session",
            workspace_root: root,
            process_scope: None,
            disabled: Default::default(),
        }
    }

    fn script(dir: &Path, name: &str, body: &str) {
        let path = dir.join(name);
        std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    fn load(project: &Path, hooks: serde_json::Value) -> HookRegistry {
        let file = project.join(".cursor").join("hooks.json");
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(&file, hooks.to_string()).unwrap();
        let (specs, errors) =
            crate::config::parse_hook_file(&std::fs::read_to_string(&file).unwrap(), &file);
        assert!(errors.is_empty(), "{errors:?}");
        registry_from_specs_deduped(specs)
    }

    #[tokio::test]
    async fn before_shell_hook_reads_cursor_stdin_and_denies() {
        let project = tempfile::tempdir().unwrap();
        let hooks_dir = project.path().join(".cursor").join("hooks");
        std::fs::create_dir_all(&hooks_dir).unwrap();
        script(
            &hooks_dir,
            "guard.sh",
            r#"input=$(cat)
pwd > "$CURSOR_PROJECT_DIR/.cursor/cwd.txt"
printf '%s' "$input" > "$CURSOR_PROJECT_DIR/.cursor/stdin.json"
case "$input" in
  *'"command":"curl'*) echo '{"permission":"deny","agent_message":"network calls are blocked"}' ;;
  *) echo '{"permission":"allow"}' ;;
esac"#,
        );
        let registry = load(
            project.path(),
            serde_json::json!({
                "version": 1,
                "hooks": {"beforeShellExecution": [{"command": ".cursor/hooks/guard.sh"}]}
            }),
        );
        let root = project.path().to_string_lossy().into_owned();
        let mut env = pre_tool(
            "run_terminal_command",
            serde_json::json!({"command": "curl x"}),
        );
        env.workspace_root = root.clone();
        env.cwd = root.clone();

        let result = crate::dispatcher::dispatch_pre_tool_use(&registry, &env, &ctx(&root)).await;
        assert!(
            matches!(result.decision, HookDecision::Deny { ref reason, .. } if reason == "network calls are blocked"),
            "{:?}",
            result.decision
        );
        let stdin: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(project.path().join(".cursor/stdin.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(stdin["hook_event_name"], "beforeShellExecution");
        assert_eq!(stdin["command"], "curl x");
        let cwd = std::fs::read_to_string(project.path().join(".cursor/cwd.txt")).unwrap();
        assert_eq!(
            dunce::canonicalize(cwd.trim()).unwrap(),
            dunce::canonicalize(project.path()).unwrap(),
            "project hooks run from the project root"
        );

        let read = pre_tool("read_file", serde_json::json!({"path": "a"}));
        let result = crate::dispatcher::dispatch_pre_tool_use(&registry, &read, &ctx(&root)).await;
        assert_eq!(result.decision, HookDecision::Allow, "not a shell tool");
    }

    #[tokio::test]
    async fn exit_two_blocks_a_prompt() {
        let project = tempfile::tempdir().unwrap();
        let registry = load(
            project.path(),
            serde_json::json!({
                "version": 1,
                "hooks": {"beforeSubmitPrompt": [{"command": "echo 'no secrets' >&2; exit 2"}]}
            }),
        );
        let root = project.path().to_string_lossy().into_owned();
        let prompt = envelope(
            HookEventName::UserPromptSubmit,
            HookPayload::UserPromptSubmit {
                prompt: Some("my password is hunter2".into()),
                subagent_type: None,
            },
        );
        let result = crate::dispatcher::dispatch_prompt_gate(&registry, &prompt, &ctx(&root)).await;
        assert!(
            matches!(result.decision, PromptDecision::Block { ref reason, .. } if reason == "no secrets"),
            "{:?}",
            result.decision
        );
    }

    #[tokio::test]
    async fn stop_followup_keeps_the_agent_working() {
        let project = tempfile::tempdir().unwrap();
        let registry = load(
            project.path(),
            serde_json::json!({
                "version": 1,
                "hooks": {"stop": [{"command": "echo '{\"followup_message\":\"run the tests\"}'"}]}
            }),
        );
        let root = project.path().to_string_lossy().into_owned();
        let env = stop("cursor-process-stop", false);
        let result =
            crate::dispatcher::dispatch_stop(&registry, HookEventName::Stop, &env, &ctx(&root))
                .await;
        assert!(result.wants_continuation());
        assert_eq!(result.feedback(), "run the tests");
    }
}
