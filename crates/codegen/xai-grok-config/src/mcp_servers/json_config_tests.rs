use super::*;

#[test]
fn json_map_skips_bad_entry_and_keeps_the_rest() {
    let value = serde_json::json!({
        "mcpServers": {
            "bad": { "enabled": false },
            "good": { "command": "npx", "args": ["-y", "pkg"] }
        }
    });

    let config = mcp_config_from_json_value(&value);
    assert_eq!(vec!["good"], config.mcp_servers.keys().collect::<Vec<_>>());
}

#[test]
fn mcp_json_oauth_block_parsed_into_oauth_config() {
    let json = r#"{
        "mcpServers": {
            "slack": {
                "type": "http",
                "url": "https://mcp.slack.example/mcp",
                "oauth": { "clientId": "slack-byo-client", "callbackPort": 3118 }
            }
        }
    }"#;

    let config: McpConfig = serde_json::from_str(json).unwrap();
    let slack = config.mcp_servers.get("slack").unwrap();
    let oauth = slack.oauth_config().unwrap();
    assert_eq!(
        (Some("slack-byo-client"), Some(3118)),
        (oauth.client_id.as_deref(), oauth.callback_port)
    );
}

#[test]
fn read_mcp_json_of_a_missing_file_is_none() {
    let dir = tempfile::tempdir().unwrap();
    assert!(read_mcp_json(&dir.path().join("mcp.json")).is_none());
}

#[test]
fn claude_json_loads_user_and_project_servers() {
    let dir = tempfile::tempdir().unwrap();
    let claude_json = dir.path().join("claude.json");
    let cwd = dir.path().join("proj");
    let cwd_key = cwd.to_string_lossy().into_owned();
    let fixture = serde_json::json!({
        "mcpServers": {
            "killswitch-claude": { "command": "true" }
        },
        "projects": {
            cwd_key: {
                "mcpServers": {
                    "killswitch-claude-proj": { "command": "true" }
                }
            }
        }
    });
    std::fs::write(&claude_json, serde_json::to_string(&fixture).unwrap()).unwrap();

    let servers = load_claude_json_mcp_servers_from(
        &claude_json,
        &cwd,
        &crate::McpPreferencesFile::default(),
    );
    assert_eq!(
        vec!["killswitch-claude-proj", "killswitch-claude"],
        servers
            .iter()
            .map(crate::mcp_servers::mcp_server_name)
            .collect::<Vec<_>>()
    );
}

#[test]
fn claude_attribution_keeps_both_urls_for_one_name() {
    let dir = tempfile::tempdir().unwrap();
    let claude_json = dir.path().join("claude.json");
    let cwd = dir.path().join("proj");
    let cwd_key = cwd.to_string_lossy().into_owned();
    let fixture = serde_json::json!({
        "mcpServers": {
            "shared": { "url": "https://user.example/mcp" }
        },
        "projects": {
            cwd_key: {
                "mcpServers": {
                    "shared": { "url": "https://project.example/mcp" }
                }
            }
        }
    });
    std::fs::write(&claude_json, serde_json::to_string(&fixture).unwrap()).unwrap();

    let servers = load_claude_json_mcp_servers_from(
        &claude_json,
        &cwd,
        &crate::McpPreferencesFile::default(),
    );

    let urls: Vec<&str> = servers
        .iter()
        .filter_map(|server| match server {
            acp::McpServer::Http(http) => Some(http.url.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(
        vec!["https://project.example/mcp", "https://user.example/mcp"],
        urls
    );
}

#[test]
fn mcp_json_expands_an_unset_variable_to_its_default() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join(".mcp.json");
    std::fs::write(
        &path,
        r#"{"mcpServers": {"api": {"url": "${GROK_TEST_MCP_UNSET_VAR_12345:-https://fallback.example.com}/mcp"}}}"#,
    )
    .unwrap();

    let servers = load_mcp_json_file(&path);
    let [acp::McpServer::Http(acp::McpServerHttp { url, .. })] = servers.as_slice() else {
        panic!("expected one http server: {servers:?}");
    };
    assert_eq!("https://fallback.example.com/mcp", url);
}

#[test]
fn nearest_mcp_json_wins_a_shared_name() {
    let root = tempfile::tempdir().unwrap();
    let farther = root.path().join(".mcp.json");
    let nearer = root.path().join("pkg").join(".mcp.json");
    std::fs::create_dir_all(nearer.parent().unwrap()).unwrap();
    std::fs::write(
        &farther,
        r#"{"mcpServers": {"svc": {"command": "far"}, "far-only": {"command": "far"}}}"#,
    )
    .unwrap();
    std::fs::write(&nearer, r#"{"mcpServers": {"svc": {"command": "near"}}}"#).unwrap();

    let servers = load_mcp_json_servers(&[farther, nearer], ClaudeImport::NotImported);
    let commands: Vec<(&str, String)> = servers
        .iter()
        .map(|server| {
            let acp::McpServer::Stdio(stdio) = server else {
                panic!("expected stdio servers: {servers:?}");
            };
            (stdio.name.as_str(), stdio.command.display().to_string())
        })
        .collect();
    assert_eq!(
        vec![("svc", "near".to_owned()), ("far-only", "far".to_owned())],
        commands
    );
}

#[test]
fn cursor_disabled_true_maps_to_enabled_false() {
    let value = serde_json::json!({
        "mcpServers": {
            "off": { "command": "echo", "disabled": true },
            "on": { "command": "echo", "disabled": false }
        }
    });
    let config = mcp_config_from_json_value(&value);
    assert!(!config.mcp_servers["off"].enabled);
    assert!(config.mcp_servers["on"].enabled);
}

#[test]
fn cursor_parent_mcp_json_is_visible_and_nearest_wins() {
    let root = tempfile::tempdir().unwrap();
    let config_dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(root.path().join(".git")).unwrap();
    std::fs::create_dir_all(root.path().join(".cursor")).unwrap();
    std::fs::create_dir_all(root.path().join("pkg").join(".cursor")).unwrap();
    std::fs::create_dir_all(root.path().join("pkg").join("nested")).unwrap();
    std::fs::write(
        root.path().join(".cursor").join("mcp.json"),
        r#"{"mcpServers": {"shared": {"command": "parent"}, "parent-only": {"command": "parent"}}}"#,
    )
    .unwrap();
    std::fs::write(
        root.path().join("pkg").join(".cursor").join("mcp.json"),
        r#"{"mcpServers": {
            "shared": {"command": "child"},
            "remote": {"url": "https://mcp.example.test/mcp", "disabled": true},
            "events": {"url": "https://mcp.example.test/sse", "type": "sse"}
        }}"#,
    )
    .unwrap();
    std::fs::create_dir_all(
        config_dir
            .path()
            .join("plugins")
            .join("local")
            .join("linter"),
    )
    .unwrap();
    std::fs::write(
        config_dir.path().join("mcp.json"),
        r#"{"mcpServers": {"shared": {"command": "user"}, "user-only": {"command": "user"}}}"#,
    )
    .unwrap();
    std::fs::write(
        config_dir
            .path()
            .join("plugins")
            .join("local")
            .join("linter")
            .join("mcp.json"),
        r#"{"mcpServers": {"lint": {"command": "lint"}}}"#,
    )
    .unwrap();

    let nested = root.path().join("pkg").join("nested");
    let listed = list_cursor_mcp_in(&nested, Some(config_dir.path()));
    let by_name: std::collections::HashMap<_, _> =
        listed.iter().map(|row| (row.name.as_str(), row)).collect();

    assert_eq!(by_name["shared"].detail, "child");
    assert_eq!(by_name["shared"].scope, "project");
    assert_eq!(by_name["parent-only"].detail, "parent");
    assert_eq!(by_name["user-only"].scope, "user");
    assert_eq!(by_name["user-only"].source_label(), "cursor: user");
    assert!(!by_name["remote"].enabled);
    assert!(by_name["remote"].is_connector());
    assert_eq!(by_name["remote"].source_label(), "connector: project");
    assert_eq!(by_name["events"].transport, "sse");
    assert_eq!(by_name["lint"].scope, "plugin");
    assert_eq!(by_name["lint"].plugin_name.as_deref(), Some("linter"));
    assert_eq!(by_name.len(), 6);
}

#[test]
fn cursor_disabled_flag_is_written_into_mcp_json() {
    let root = tempfile::tempdir().unwrap();
    let config_dir = tempfile::tempdir().unwrap();
    let mcp = root.path().join(".cursor").join("mcp.json");
    std::fs::create_dir_all(mcp.parent().unwrap()).unwrap();
    std::fs::write(
        &mcp,
        r#"{"mcpServers":{"lint":{"command":"npx","args":["lint"],"env":{"A":"b"}}}}"#,
    )
    .unwrap();
    // Keep the user file out of this assertion: point config dir at an empty directory.
    let _ = config_dir;
    let changed = set_cursor_mcp_server_disabled(root.path(), "lint", true).unwrap();
    assert_eq!(changed.as_deref(), Some(mcp.as_path()));
    let listed = list_cursor_mcp_in(root.path(), Some(config_dir.path()));
    let lint = listed.iter().find(|row| row.name == "lint").unwrap();
    assert!(!lint.enabled);
    let raw = std::fs::read_to_string(&mcp).unwrap();
    assert!(raw.contains("\"disabled\": true"));
    assert!(raw.contains("\"A\": \"b\"") || raw.contains("\"A\":\"b\""));
    assert!(
        set_cursor_mcp_server_disabled(root.path(), "lint", false)
            .unwrap()
            .is_some()
    );
    let listed = list_cursor_mcp_in(root.path(), Some(config_dir.path()));
    assert!(
        listed
            .iter()
            .find(|row| row.name == "lint")
            .unwrap()
            .enabled
    );
}

#[test]
fn connection_status_checks_stdio_on_path_and_tcp() {
    let missing = CursorMcpListing {
        name: "missing".into(),
        transport: "stdio",
        scope: "user",
        location: std::path::PathBuf::from("mcp.json"),
        detail: "grok-mcp-probe-missing-bin".into(),
        enabled: true,
        plugin_name: None,
    };
    assert_eq!(cursor_mcp_connection_status(&missing), "unavailable");
    let shell = CursorMcpListing {
        detail: "sh -c true".into(),
        ..missing.clone()
    };
    assert_eq!(cursor_mcp_connection_status(&shell), "ready");
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let up = CursorMcpListing {
        transport: "http",
        detail: format!("http://127.0.0.1:{port}/mcp"),
        ..missing.clone()
    };
    assert_eq!(cursor_mcp_connection_status(&up), "ready");
    drop(listener);
    let down = CursorMcpListing {
        transport: "http",
        detail: "http://127.0.0.1:1/mcp".into(),
        ..missing
    };
    assert_eq!(cursor_mcp_connection_status(&down), "unavailable");
}
