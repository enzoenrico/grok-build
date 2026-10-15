//! Cursor backend: runs `cursor-agent acp` as the agent and bridges it into the pager's ACP channels.
//!
//! Cursor owns models, auth, the agent loop, and tool execution; the pager keeps its UI. Selected with
//! `--backend cursor`, `GROK_BACKEND=cursor`, or `[cli] backend = "cursor"` in `config.toml`.

mod adapter;
mod cloud;
pub(crate) mod protocol;
mod startup;

pub use startup::materialize_startup;

fn cloud_handoff_notice(
    session_id: &str,
    request: &CloudRequest,
    agent_id: Option<&str>,
    repo: Option<&cloud::CloudRepo>,
) -> (String, Option<cloud::CloudAgent>) {
    let base = std::env::var("CURSOR_CLOUD_API")
        .unwrap_or_else(|_| "https://api.cursor.com".to_owned());
    let outcome = match cloud::resolve_cloud_auth() {
        Err(err) => Err(err),
        Ok(auth) => match request {
            CloudRequest::Send(task) => cloud::dispatch(&base, &auth, agent_id, task, repo),
            CloudRequest::Refresh => match agent_id {
                Some(id) => cloud::refresh_existing(&base, &auth, id),
                None => Err(
                    "No cloud agent on this session yet. Send & and a task to start one."
                        .to_owned(),
                ),
            },
            CloudRequest::Cancel => match agent_id {
                Some(id) => cloud::cancel_existing(&base, &auth, id),
                None => Err(
                    "No cloud agent on this session yet. Send & and a task to start one."
                        .to_owned(),
                ),
            },
        },
    };
    let (text, recorded) = match outcome {
        Ok(agent) => {
            let url = agent.url.clone().unwrap_or_default();
            let status = agent.run_status.clone().unwrap_or_default();
            let reply = agent.result.clone().unwrap_or_default();
            let mut text = format!("Cloud agent {} {url}\nRun status {status}.\n{reply}", agent.id);
            if let Some(pr) = &agent.pr_url {
                text.push('\n');
                text.push_str(pr);
            }
            if let Some(branch) = &agent.branch {
                text.push('\n');
                text.push_str(branch);
            }
            (text, Some(agent))
        }
        Err(err) => (format!("Cloud agent handoff failed. {err}\n"), None),
    };
    (
        serde_json::json!({
            "jsonrpc": "2.0",
            "method": "session/update",
            "params": {
                "sessionId": session_id,
                "update": {
                    "sessionUpdate": "agent_message_chunk",
                    "content": { "type": "text", "text": text }
                }
            }
        })
        .to_string(),
        recorded,
    )
}

fn watch_cloud_run(
    session_id: String,
    agent_id: String,
    run_id: String,
    prompt_rpc_id: Option<serde_json::Value>,
    prompt_id: Option<String>,
    tx: mpsc::UnboundedSender<String>,
) {
    // The stream stays open until the cloud agent finishes. Reading it here would stall the local agent.
    std::thread::spawn(move || {
        let base = std::env::var("CURSOR_CLOUD_API")
            .unwrap_or_else(|_| "https://api.cursor.com".to_owned());
        let mut saw_assistant = false;
        let result = (|| {
            let auth = cloud::resolve_cloud_auth()?;
            cloud::follow_run(&base, &auth, &agent_id, &run_id, |event| {
                let text = cloud::event_text(&event, saw_assistant);
                if matches!(event, cloud::CloudEvent::Assistant(_)) {
                    saw_assistant = true;
                }
                if let Some(text) = text {
                    let _ = tx.send(cloud_text_notice(&session_id, &text));
                }
            })
        })();
        if let Err(err) = result {
            let _ = tx.send(cloud_text_notice(
                &session_id,
                &format!("Cloud agent run failed. {err}\n"),
            ));
        }
        if let Some(id) = prompt_rpc_id {
            let _ = tx.send(protocol::prompt_end(&id, prompt_id.as_deref()).to_string());
        }
    });
}

fn cloud_text_notice(session_id: &str, text: &str) -> String {
    serde_json::json!({
        "jsonrpc": "2.0",
        "method": "session/update",
        "params": {
            "sessionId": session_id,
            "update": {
                "sessionUpdate": "agent_message_chunk",
                "content": { "type": "text", "text": text }
            }
        }
    })
    .to_string()
}

use std::path::PathBuf;
use std::process::Stdio;
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;
use xai_grok_shell::leader::ReconnectPolicy;

use super::leader_bridge::bridge_channels;
use super::spawn::{SpawnedAgent, spawn_runtime_thread};
use adapter::{CloudRequest, CursorAdapter, Routed};

pub const CURSOR_AGENT_BIN_ENV: &str = "GROK_CURSOR_AGENT_BIN";
const DEFAULT_CURSOR_AGENT_BIN: &str = "cursor-agent";
const AGENT_BIN_FALLBACK: &str = "agent";
const CHILD_EXIT_GRACE: std::time::Duration = std::time::Duration::from_secs(2);

fn resolve_cursor_program(configured: &std::path::Path) -> std::path::PathBuf {
    if configured.components().nth(1).is_some() {
        return configured.to_path_buf();
    }
    if configured != std::path::Path::new(DEFAULT_CURSOR_AGENT_BIN) {
        return configured.to_path_buf();
    }
    let name = pick_default_cursor_bin(bin_on_path(DEFAULT_CURSOR_AGENT_BIN), bin_on_path(AGENT_BIN_FALLBACK));
    std::path::PathBuf::from(name)
}

fn pick_default_cursor_bin(cursor_agent_on_path: bool, agent_on_path: bool) -> &'static str {
    if cursor_agent_on_path || !agent_on_path {
        DEFAULT_CURSOR_AGENT_BIN
    } else {
        AGENT_BIN_FALLBACK
    }
}

fn bin_on_path(name: &str) -> bool {
    std::env::var_os("PATH").is_some_and(|path| {
        std::env::split_paths(&path).any(|dir| {
            let candidate = dir.join(name);
            candidate.is_file()
        })
    })
}

/// How to launch the Cursor agent (`<program> <args...> acp`) and how to talk to it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CursorAgentCommand {
    pub program: PathBuf,
    pub args: Vec<String>,
    /// `[cursor] model`: model for new sessions, kept apart from grok's `[models] default`.
    pub default_model: Option<String>,
    /// `[cursor] forward_mcp_servers`: also pass grok's MCP server list to Cursor.
    pub forward_mcp_servers: bool,
}

/// Persist the pager's default-model choice as `[cursor] model`; an empty id clears it.
pub fn persist_default_model(model_id: &str) -> Result<()> {
    let value = if model_id.is_empty() {
        None
    } else {
        Some(toml_edit::Value::from(model_id))
    };
    crate::config_toml_edit::set_config_key("cursor", "model", value).map_err(anyhow::Error::from)
}

impl CursorAgentCommand {
    /// `bin_env` (`GROK_CURSOR_AGENT_BIN`) or `[cursor] bin` pick the binary. `[cursor] args` stay as written.
    pub fn from_config(raw_config: &toml::Value, bin_env: Option<std::ffi::OsString>) -> Self {
        let section = raw_config.get("cursor");
        let default_model = section
            .and_then(|s| s.get("model"))
            .and_then(toml::Value::as_str)
            .map(str::trim)
            .filter(|m| !m.is_empty())
            .map(str::to_owned);
        let forward_mcp_servers = section
            .and_then(|s| s.get("forward_mcp_servers"))
            .and_then(toml::Value::as_bool)
            .unwrap_or(false);
        let program = bin_env
            .map(PathBuf::from)
            .or_else(|| {
                section
                    .and_then(|s| s.get("bin"))
                    .and_then(toml::Value::as_str)
                    .map(PathBuf::from)
            })
            .unwrap_or_else(|| PathBuf::from(DEFAULT_CURSOR_AGENT_BIN));
        let args = section
            .and_then(|s| s.get("args"))
            .and_then(toml::Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(toml::Value::as_str)
                    .map(str::to_owned)
                    .collect()
            })
            .unwrap_or_default();
        Self {
            program,
            args,
            default_model,
            forward_mcp_servers,
        }
    }

    /// Program, configured args, `--yolo` when asked and not already forced, then `acp`.
    pub fn tokio_command(&self, yolo: bool) -> tokio::process::Command {
        let mut cmd = tokio::process::Command::new(resolve_cursor_program(&self.program));
        cmd.args(&self.args);
        if yolo
            && !self
                .args
                .iter()
                .any(|a| matches!(a.as_str(), "--yolo" | "--force" | "-f"))
        {
            cmd.arg("--yolo");
        }
        cmd.arg("acp")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .kill_on_drop(true);
        xai_tty_utils::detach_command(&mut cmd);
        cmd
    }
}

/// Spawn `cursor-agent acp` and bridge its stdio into an in-process ACP channel.
///
/// The returned thread owns the subprocess: cancelling `SpawnedAgent::cancel` kills Cursor and joins the bridge.
/// If Cursor exits on its own, `app_cancel` fires so the pager shuts down instead of waiting on a dead agent.
pub async fn spawn_cursor_agent(
    command: CursorAgentCommand,
    app_cancel: &CancellationToken,
    auth_manager: Arc<xai_grok_login::AuthManager>,
    yolo: bool,
) -> Result<SpawnedAgent> {
    let agent_cancel = app_cancel.child_token();
    let (to_agent_tx, to_agent_rx) = mpsc::unbounded_channel::<String>();
    let (to_client_tx, to_client_rx) = mpsc::unbounded_channel::<String>();
    let bridge = bridge_channels(
        to_agent_tx,
        to_client_rx,
        agent_cancel.clone(),
        None,
        ReconnectPolicy::bounded(),
    )?;

    let (spawned_tx, spawned_rx) = oneshot::channel();
    let io = CursorIo {
        command,
        yolo,
        to_agent_rx,
        to_client_tx,
        cancel: agent_cancel.clone(),
        app_cancel: app_cancel.clone(),
    };
    let bridge_thread = bridge.thread_handle;
    let thread_handle = spawn_runtime_thread("cursor-agent-io", move |rt| {
        let result = rt.block_on(io.run(spawned_tx));
        drop(rt);
        if bridge_thread.join().is_err() {
            tracing::warn!("cursor backend: bridge thread panicked");
        }
        result
    })
    .await
    .inspect_err(|_| agent_cancel.cancel())?;

    match spawned_rx.await {
        Ok(Ok(())) => {}
        Ok(Err(error)) => {
            agent_cancel.cancel();
            let _ = thread_handle.join();
            return Err(error);
        }
        Err(_) => {
            agent_cancel.cancel();
            let _ = thread_handle.join();
            anyhow::bail!("cursor-agent I/O thread exited before the agent started");
        }
    }

    Ok(SpawnedAgent {
        thread_handle,
        channel: bridge.channel,
        cancel: agent_cancel,
        auth_manager,
    })
}

struct CursorIo {
    command: CursorAgentCommand,
    yolo: bool,
    to_agent_rx: mpsc::UnboundedReceiver<String>,
    to_client_tx: mpsc::UnboundedSender<String>,
    cancel: CancellationToken,
    app_cancel: CancellationToken,
}

impl CursorIo {
    async fn run(self, spawned_tx: oneshot::Sender<Result<()>>) -> Result<()> {
        let CursorIo {
            command,
            yolo,
            mut to_agent_rx,
            to_client_tx,
            cancel,
            app_cancel,
        } = self;

        let mut cmd = command.tokio_command(yolo);
        cmd.stderr(Stdio::piped());
        let mut child = match cmd.spawn().with_context(|| {
            format!(
                "failed to start the Cursor agent ({}); install it with `curl https://cursor.com/install -fsS | bash` \
                 or set {CURSOR_AGENT_BIN_ENV}",
                command.program.display()
            )
        }) {
            Ok(child) => child,
            Err(error) => {
                let message = format!("{error:#}");
                let _ = spawned_tx.send(Err(error));
                return Err(anyhow::anyhow!(message));
            }
        };
        tracing::info!(program = %command.program.display(), args = ?command.args, pid = ?child.id(), "cursor backend: agent started");
        let _ = spawned_tx.send(Ok(()));

        let mut stdin = child.stdin.take().context("cursor-agent stdin")?;
        let stdout = child.stdout.take().context("cursor-agent stdout")?;
        let stderr = child.stderr.take().context("cursor-agent stderr")?;
        let adapter = Arc::new(Mutex::new(CursorAdapter::new(
            command.default_model.clone(),
            command.forward_mcp_servers,
        )));
        let (stdin_tx, mut stdin_rx) = mpsc::unbounded_channel::<String>();

        let deliver = {
            let to_client_tx = to_client_tx.clone();
            let stdin_tx = stdin_tx.clone();
            let adapter = adapter.clone();
            move |routed: Vec<Routed>| {
                for r in routed {
                    match r {
                        Routed::ToAgent(line) => {
                            let _ = stdin_tx.send(line);
                        }
                        Routed::ToClient(line) => {
                            let _ = to_client_tx.send(line);
                        }
                        Routed::Cloud {
                            session_id,
                            request,
                            agent_id,
                            repo,
                            prompt_rpc_id,
                            prompt_id,
                        } => {
                            let (notice, recorded) = cloud_handoff_notice(
                                &session_id,
                                &request,
                                agent_id.as_deref(),
                                repo.as_ref(),
                            );
                            if let Some(agent) = &recorded {
                                adapter
                                    .lock()
                                    .unwrap_or_else(|p| p.into_inner())
                                    .note_cloud_agent(&session_id, &agent.id);
                                if !cloud::run_is_terminal(agent.run_status.as_deref())
                                    && let Some(run_id) = agent.run_id.clone()
                                {
                                    watch_cloud_run(
                                        session_id.clone(),
                                        agent.id.clone(),
                                        run_id,
                                        prompt_rpc_id.clone(),
                                        prompt_id.clone(),
                                        to_client_tx.clone(),
                                    );
                                    let _ = to_client_tx.send(notice);
                                    continue;
                                }
                            }
                            let _ = to_client_tx.send(notice);
                            if let Some(id) = prompt_rpc_id {
                                let _ = to_client_tx.send(
                                    protocol::prompt_end(&id, prompt_id.as_deref()).to_string(),
                                );
                            }
                        }
                    }
                }
            }
        };

        let stdin_task = tokio::spawn(async move {
            while let Some(line) = stdin_rx.recv().await {
                tracing::trace!(target: "cursor_agent", %line, "-> cursor");
                if stdin.write_all(line.as_bytes()).await.is_err()
                    || stdin.write_all(b"\n").await.is_err()
                    || stdin.flush().await.is_err()
                {
                    break;
                }
            }
        });

        let outbound_task = {
            let adapter = adapter.clone();
            let deliver = deliver.clone();
            tokio::spawn(async move {
                while let Some(line) = to_agent_rx.recv().await {
                    let routed = adapter
                        .lock()
                        .unwrap_or_else(|p| p.into_inner())
                        .outbound(&line);
                    let started_login = routed.iter().any(|routed| {
                        matches!(
                            routed,
                            Routed::ToAgent(line)
                                if line.contains("grok-cursor-bridge-auth")
                                    && line.contains("cursor_login")
                        )
                    });
                    deliver(routed);
                    if started_login {
                        let adapter = adapter.clone();
                        let deliver = deliver.clone();
                        tokio::spawn(async move {
                            tokio::time::sleep(std::time::Duration::from_secs(8)).await;
                            let notices = adapter
                                .lock()
                                .unwrap_or_else(|p| p.into_inner())
                                .auth_timed_out();
                            if !notices.is_empty() {
                                deliver(notices.into_iter().map(Routed::ToClient).collect());
                            }
                        });
                    }
                }
            })
        };

        let mut inbound_task = {
            let adapter = adapter.clone();
            tokio::spawn(async move {
                let mut lines = BufReader::new(stdout).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    let trimmed = line.trim();
                    if trimmed.is_empty() {
                        continue;
                    }
                    if !trimmed.starts_with('{') {
                        tracing::warn!(target: "cursor_agent", line = %trimmed, "cursor backend: non-JSON stdout line dropped");
                        continue;
                    }
                    tracing::trace!(target: "cursor_agent", line = %trimmed, "<- cursor");
                    let routed = adapter
                        .lock()
                        .unwrap_or_else(|p| p.into_inner())
                        .inbound(trimmed);
                    deliver(routed);
                }
            })
        };

        let stderr_task = tokio::spawn(async move {
            let mut lines = BufReader::new(stderr).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                tracing::debug!(target: "cursor_agent", "{line}");
            }
        });

        let exited_on_its_own = tokio::select! {
            () = cancel.cancelled() => false,
            status = child.wait() => {
                tracing::error!(?status, "cursor backend: cursor-agent exited");
                true
            }
            _ = &mut inbound_task => {
                tracing::error!("cursor backend: cursor-agent closed its stdout");
                true
            }
        };

        cancel.cancel();
        if exited_on_its_own {
            app_cancel.cancel();
        }
        let _ = child.start_kill();
        let _ = tokio::time::timeout(CHILD_EXIT_GRACE, child.wait()).await;
        for task in [stdin_task, outbound_task, inbound_task, stderr_task] {
            task.abort();
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::ffi::OsString;

    use super::*;

    fn config(toml_text: &str) -> toml::Value {
        toml::from_str(toml_text).unwrap()
    }

    fn argv(cmd: &CursorAgentCommand, yolo: bool) -> Vec<String> {
        cmd.tokio_command(yolo)
            .as_std()
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect()
    }

    #[test]
    fn command_reads_config_and_bin_env() {
        let cfg = config(
            "[cursor]\nbin = \"/opt/agent\"\nargs = [\"--trust\"]\nmodel = \"gpt-5.5\"\nforward_mcp_servers = true",
        );
        let cmd = CursorAgentCommand::from_config(&cfg, None);
        assert_eq!(cmd.program, PathBuf::from("/opt/agent"));
        assert_eq!(cmd.args, vec!["--trust"]);
        assert_eq!(cmd.default_model.as_deref(), Some("gpt-5.5"));
        assert!(cmd.forward_mcp_servers);

        let from_env = CursorAgentCommand::from_config(&cfg, Some(OsString::from("/env/agent")));
        assert_eq!(from_env.program, PathBuf::from("/env/agent"));
        assert_eq!(from_env.args, vec!["--trust"]);

        let cmd = CursorAgentCommand::from_config(&config(""), None);
        assert_eq!(cmd.program, PathBuf::from(DEFAULT_CURSOR_AGENT_BIN));
        assert!(cmd.args.is_empty());
        assert_eq!(cmd.default_model, None);
        assert!(!cmd.forward_mcp_servers);
    }

    #[test]
    fn tokio_command_appends_yolo_and_acp() {
        let cmd =
            CursorAgentCommand::from_config(&config("[cursor]\nargs = [\"--trust\"]\n"), None);
        assert_eq!(argv(&cmd, true), ["--trust", "--yolo", "acp"]);
        assert_eq!(argv(&cmd, false), ["--trust", "acp"]);

        let forced =
            CursorAgentCommand::from_config(&config("[cursor]\nargs = [\"--force\"]\n"), None);
        assert_eq!(argv(&forced, true), ["--force", "acp"]);

        let already = CursorAgentCommand::from_config(&config("[cursor]\nargs = [\"-f\"]\n"), None);
        assert_eq!(argv(&already, true), ["-f", "acp"]);
    }

    #[test]
    fn default_bin_uses_agent_when_cursor_agent_is_missing() {
        assert_eq!(pick_default_cursor_bin(true, true), "cursor-agent");
        assert_eq!(pick_default_cursor_bin(true, false), "cursor-agent");
        assert_eq!(pick_default_cursor_bin(false, false), "cursor-agent");
        assert_eq!(pick_default_cursor_bin(false, true), "agent");
    }
}
