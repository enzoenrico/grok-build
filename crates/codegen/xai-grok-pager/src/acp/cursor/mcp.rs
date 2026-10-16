//! `/mcps` on the Cursor backend. Cursor loads MCP servers itself (`.cursor/mcp.json`, `~/.cursor/mcp.json`, its
//! approval list), so the list and the enable switch go through Cursor's own `agent mcp` commands for the session's
//! cwd instead of grok's MCP config, which Cursor never reads.

use std::path::Path;
use std::time::Duration;

use serde_json::{Value, json};

pub(crate) const MCP_LIST: &str = "x.ai/mcp/list";
pub(crate) const MCP_TOGGLE: &str = "x.ai/mcp/toggle";

const CLI_TIMEOUT: Duration = Duration::from_secs(30);
const NO_SERVERS: &str = "No MCP servers configured";

/// One server row from `agent mcp list`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CursorMcpServer {
    pub name: String,
    pub status: CursorMcpStatus,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum CursorMcpStatus {
    Ready,
    /// `disabled`, or `not loaded (needs approval)`: Cursor will not start it until `agent mcp enable`.
    Off,
    Loading,
    /// Anything else Cursor prints, such as `Error: Connection failed`.
    Unavailable,
}

/// `{servers}` in the shape the pager's `/mcps` modal reads, tools included for the servers Cursor reports ready.
pub(crate) async fn list(program: &Path, cwd: &Path) -> Result<Value, String> {
    let servers = parse_list(&run(program, cwd, &["mcp", "list"]).await?);
    let mut lookups = tokio::task::JoinSet::new();
    for (index, server) in servers.iter().enumerate() {
        if server.status != CursorMcpStatus::Ready {
            continue;
        }
        let (program, cwd, name) = (
            program.to_path_buf(),
            cwd.to_path_buf(),
            server.name.clone(),
        );
        lookups.spawn(async move {
            let listed = run(&program, &cwd, &["mcp", "list-tools", &name]).await;
            (index, listed)
        });
    }
    let mut tools = vec![Vec::new(); servers.len()];
    while let Some(joined) = lookups.join_next().await {
        if let Ok((index, Ok(output))) = joined
            && let Some(slot) = tools.get_mut(index)
        {
            *slot = parse_tools(&output);
        }
    }
    let rows: Vec<Value> = servers
        .iter()
        .zip(tools)
        .map(|(server, tools)| server_row(server, tools))
        .collect();
    Ok(json!({ "servers": rows }))
}

/// `agent mcp enable|disable <name>`; Cursor applies it to the sessions it starts next.
pub(crate) async fn toggle(program: &Path, cwd: &Path, params: &Value) -> Result<Value, String> {
    let name = params
        .get("server_name")
        .or_else(|| params.get("serverName"))
        .and_then(Value::as_str)
        .filter(|name| !name.is_empty())
        .ok_or_else(|| "missing server_name".to_owned())?;
    let enabled = params
        .get("enabled")
        .and_then(Value::as_bool)
        .ok_or_else(|| "missing enabled".to_owned())?;
    let verb = if enabled { "enable" } else { "disable" };
    run(program, cwd, &["mcp", verb, name]).await?;
    Ok(json!({}))
}

fn server_row(server: &CursorMcpServer, tools: Vec<String>) -> Value {
    let (enabled, status) = match server.status {
        CursorMcpStatus::Ready => (true, "ready"),
        CursorMcpStatus::Off => (false, "unavailable"),
        CursorMcpStatus::Loading => (true, "initializing"),
        CursorMcpStatus::Unavailable => (true, "unavailable"),
    };
    let tools: Vec<Value> = tools
        .into_iter()
        .map(|name| json!({ "name": name }))
        .collect();
    json!({
        "name": server.name,
        "source": "local",
        "sourceLabel": "Cursor",
        "session": { "enabled": enabled, "status": status, "tools": tools },
    })
}

pub(crate) fn parse_list(output: &str) -> Vec<CursorMcpServer> {
    output
        .lines()
        .filter(|line| !line.trim_start().starts_with(NO_SERVERS))
        .filter_map(|line| {
            let (name, status) = line.split_once(": ")?;
            let name = name.trim();
            if name.is_empty() {
                return None;
            }
            let status = status.trim().to_ascii_lowercase();
            let status = if status == "ready" {
                CursorMcpStatus::Ready
            } else if status == "disabled" || status.starts_with("not loaded") {
                CursorMcpStatus::Off
            } else if status.starts_with("loading")
                || status.starts_with("connecting")
                || status.starts_with("starting")
            {
                CursorMcpStatus::Loading
            } else {
                CursorMcpStatus::Unavailable
            };
            Some(CursorMcpServer {
                name: name.to_owned(),
                status,
            })
        })
        .collect()
}

/// Tool names from `agent mcp list-tools`: one `- name (args)` line per tool.
pub(crate) fn parse_tools(output: &str) -> Vec<String> {
    output
        .lines()
        .filter_map(|line| line.trim_start().strip_prefix("- "))
        .filter_map(|rest| {
            let name = rest.split(" (").next().unwrap_or(rest).trim();
            (!name.is_empty()).then(|| name.to_owned())
        })
        .collect()
}

async fn run(program: &Path, cwd: &Path, args: &[&str]) -> Result<String, String> {
    let mut cmd = tokio::process::Command::new(program);
    cmd.args(args)
        .current_dir(cwd)
        .stdin(std::process::Stdio::null())
        .kill_on_drop(true);
    let output = tokio::time::timeout(CLI_TIMEOUT, cmd.output())
        .await
        .map_err(|_| format!("`{} {}` timed out", program.display(), args.join(" ")))?
        .map_err(|error| format!("couldn't run `{}`: {error}", program.display()))?;
    let stdout = plain_text(&output.stdout);
    if !output.status.success() {
        let stderr = plain_text(&output.stderr);
        let detail = [stderr.trim(), stdout.trim()]
            .into_iter()
            .find(|text| !text.is_empty())
            .unwrap_or("no output");
        return Err(format!("`agent {}` failed: {detail}", args.join(" ")));
    }
    Ok(stdout)
}

fn plain_text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(&strip_ansi_escapes::strip(bytes)).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn list_output_becomes_server_rows() {
        let servers = parse_list(
            "echo: ready\nweb: not loaded (needs approval)\noff: disabled\nbroken: Error: Connection failed\n",
        );
        assert_eq!(
            servers,
            [
                ("echo", CursorMcpStatus::Ready),
                ("web", CursorMcpStatus::Off),
                ("off", CursorMcpStatus::Off),
                ("broken", CursorMcpStatus::Unavailable),
            ]
            .map(|(name, status)| CursorMcpServer {
                name: name.to_owned(),
                status,
            })
        );
        assert!(
            parse_list(
                "No MCP servers configured (expected in .cursor/mcp.json or ~/.cursor/mcp.json)"
            )
            .is_empty()
        );
    }

    #[test]
    fn tool_lines_become_tool_names() {
        assert_eq!(
            parse_tools("Tools for echo (2):\n- ping ()\n- search (query, limit)\n"),
            ["ping", "search"]
        );
    }

    #[test]
    fn rows_match_the_mcps_modal_shape() {
        let ready = server_row(
            &CursorMcpServer {
                name: "echo".to_owned(),
                status: CursorMcpStatus::Ready,
            },
            vec!["ping".to_owned()],
        );
        assert_eq!(
            ready,
            json!({
                "name": "echo",
                "source": "local",
                "sourceLabel": "Cursor",
                "session": { "enabled": true, "status": "ready", "tools": [{ "name": "ping" }] },
            })
        );
        let parsed: crate::views::mcps_modal::McpsListResponse =
            serde_json::from_value(json!({ "servers": [ready] })).unwrap();
        let info = crate::views::mcps_modal::convert_list_response(parsed);
        assert_eq!(info[0].tool_count, 1);
        assert!(info[0].enabled);
    }
}
