//! `--resume` / `--continue` against Cursor's chat store instead of grok's local session files.

use std::process::Stdio;
use std::time::Duration;

use anyhow::{Context, Result};
use serde::Deserialize;
use serde_json::json;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

use super::CursorAgentCommand;
use crate::app::session_startup::{MaterializedStartup, SessionStartupIntent};

const LIST_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct CursorSession {
    pub(crate) session_id: String,
    #[serde(default)]
    pub(crate) title: Option<String>,
    #[serde(default)]
    pub(crate) updated_at: Option<String>,
}

/// Resolve CLI session intent for the Cursor backend.
pub async fn materialize_startup(
    command: &CursorAgentCommand,
    intent: SessionStartupIntent,
    cwd: &str,
) -> Result<MaterializedStartup> {
    match intent {
        SessionStartupIntent::NewAuto => Ok(MaterializedStartup::NewAuto),
        SessionStartupIntent::NewWithId { .. } => {
            anyhow::bail!(
                "--session-id is not supported by the Cursor backend (Cursor assigns chat ids)"
            )
        }
        SessionStartupIntent::ForkFrom { .. } => {
            anyhow::bail!("--fork-session is not supported by the Cursor backend")
        }
        SessionStartupIntent::Resume {
            session_id: Some(target),
            ..
        } if uuid::Uuid::try_parse(&target).is_ok() => Ok(resume(target, None)),
        SessionStartupIntent::Resume {
            session_id,
            most_recent_for_cwd,
        } => {
            let sessions = list_sessions(command, cwd).await?;
            let picked = match session_id.as_deref() {
                Some(title) => pick_by_title(&sessions, title)
                    .with_context(|| format!("No Cursor chat titled '{title}' in {cwd}"))?,
                None if most_recent_for_cwd => most_recent(&sessions)
                    .with_context(|| format!("No Cursor chats found in {cwd}"))?,
                None => anyhow::bail!("nothing to resume"),
            };
            Ok(resume(picked.session_id.clone(), picked.title.clone()))
        }
    }
}

fn resume(session_id: String, title: Option<String>) -> MaterializedStartup {
    MaterializedStartup::Resume {
        session_id,
        original_cwd: None,
        title,
        deferred_local_miss: false,
        suppress_code_restore: true,
    }
}

fn most_recent(sessions: &[CursorSession]) -> Option<&CursorSession> {
    sessions
        .iter()
        .max_by(|a, b| a.updated_at.cmp(&b.updated_at))
}

fn pick_by_title<'a>(sessions: &'a [CursorSession], title: &str) -> Option<&'a CursorSession> {
    let title = title.trim();
    sessions
        .iter()
        .filter(|s| {
            s.title
                .as_deref()
                .is_some_and(|t| t.trim().eq_ignore_ascii_case(title))
        })
        .max_by(|a, b| a.updated_at.cmp(&b.updated_at))
}

/// One-shot `cursor-agent acp` call: `initialize` then `session/list` for `cwd`.
pub(crate) async fn list_sessions(
    command: &CursorAgentCommand,
    cwd: &str,
) -> Result<Vec<CursorSession>> {
    tokio::time::timeout(LIST_TIMEOUT, list_sessions_inner(command, cwd))
        .await
        .context("timed out listing Cursor chats")?
}

async fn list_sessions_inner(
    command: &CursorAgentCommand,
    cwd: &str,
) -> Result<Vec<CursorSession>> {
    let mut cmd = command.tokio_command(false);
    cmd.current_dir(cwd).stderr(Stdio::null());
    let mut child = cmd
        .spawn()
        .with_context(|| format!("failed to start {}", command.program.display()))?;
    let mut stdin = child.stdin.take().context("cursor-agent stdin")?;
    let stdout = child.stdout.take().context("cursor-agent stdout")?;

    let requests = [
        json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":1,"clientCapabilities":{}}}),
        json!({"jsonrpc":"2.0","id":2,"method":"session/list","params":{"cwd":cwd}}),
    ];
    for req in requests {
        stdin.write_all(format!("{req}\n").as_bytes()).await?;
    }
    stdin.flush().await?;

    let mut lines = BufReader::new(stdout).lines();
    while let Some(line) = lines.next_line().await? {
        let Ok(msg) = serde_json::from_str::<serde_json::Value>(&line) else {
            continue;
        };
        if msg.get("id") != Some(&json!(2)) {
            continue;
        }
        let _ = child.start_kill();
        if let Some(error) = msg.get("error") {
            anyhow::bail!("Cursor session/list failed: {error}");
        }
        let sessions = msg
            .pointer("/result/sessions")
            .cloned()
            .unwrap_or_else(|| json!([]));
        return serde_json::from_value(sessions).context("parse Cursor session/list");
    }
    anyhow::bail!("cursor-agent exited before listing chats")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn session(id: &str, title: &str, updated: &str) -> CursorSession {
        CursorSession {
            session_id: id.into(),
            title: Some(title.into()),
            updated_at: Some(updated.into()),
        }
    }

    #[test]
    fn picks_most_recent_and_by_title() {
        let sessions = vec![
            session("a", "Fix Bug", "2026-10-14T10:00:00Z"),
            session("b", "Other", "2026-10-14T12:00:00Z"),
            session("c", "fix bug", "2026-10-14T11:00:00Z"),
        ];
        assert_eq!(most_recent(&sessions).unwrap().session_id, "b");
        assert_eq!(pick_by_title(&sessions, "FIX BUG").unwrap().session_id, "c");
        assert!(pick_by_title(&sessions, "missing").is_none());
    }
}
