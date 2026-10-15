//! Cursor backend: runs `cursor-agent acp` as the agent and bridges it into the pager's ACP channels.
//!
//! Cursor owns models, auth, the agent loop, and tool execution; the pager keeps its UI. Selected with
//! `--backend cursor`, `GROK_BACKEND=cursor`, or `[cli] backend = "cursor"` in `config.toml`.

mod adapter;
mod startup;

pub use startup::materialize_startup;

use std::path::PathBuf;
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;
use xai_grok_shell::leader::ReconnectPolicy;

use super::leader_bridge::bridge_channels;
use super::spawn::{SpawnedAgent, spawn_runtime_thread};
use adapter::{CursorAdapter, Routed};

pub const BACKEND_ENV: &str = "GROK_BACKEND";
pub const CURSOR_AGENT_BIN_ENV: &str = "GROK_CURSOR_AGENT_BIN";
const DEFAULT_CURSOR_AGENT_BIN: &str = "cursor-agent";
const CHILD_EXIT_GRACE: std::time::Duration = std::time::Duration::from_secs(2);

/// Which agent runtime serves the pager.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, clap::ValueEnum)]
pub enum BackendKind {
    /// The embedded grok shell (or a grok leader).
    #[default]
    Grok,
    /// `cursor-agent acp` as a subprocess.
    Cursor,
}

impl BackendKind {
    fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "grok" | "shell" => Some(Self::Grok),
            "cursor" | "cursor-agent" => Some(Self::Cursor),
            _ => None,
        }
    }

    pub fn is_cursor(self) -> bool {
        self == Self::Cursor
    }
}

/// Precedence: `--backend`, then `GROK_BACKEND`, then `[cli] backend`, then grok.
pub fn resolve_backend(cli: Option<BackendKind>, raw_config: &toml::Value) -> BackendKind {
    if let Some(kind) = cli {
        return kind;
    }
    if let Some(kind) = std::env::var(BACKEND_ENV)
        .ok()
        .as_deref()
        .and_then(BackendKind::parse)
    {
        return kind;
    }
    raw_config
        .get("cli")
        .and_then(|cli| cli.get("backend"))
        .and_then(toml::Value::as_str)
        .and_then(BackendKind::parse)
        .unwrap_or_default()
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

static CURSOR_BACKEND_ACTIVE: AtomicBool = AtomicBool::new(false);

/// Whether this process is serving the pager through `cursor-agent`.
pub fn is_active() -> bool {
    CURSOR_BACKEND_ACTIVE.load(Ordering::Relaxed)
}

/// Persist the pager's default-model choice as `[cursor] model`; an empty id clears it.
pub fn persist_default_model(model_id: &str) -> Result<()> {
    let path =
        xai_grok_shell::util::grok_home::grok_home().join(xai_grok_config::USER_CONFIG_FILENAME);
    let content = std::fs::read_to_string(&path).unwrap_or_default();
    let mut doc = content
        .parse::<toml_edit::DocumentMut>()
        .with_context(|| format!("parse {}", path.display()))?;
    let section = doc
        .entry("cursor")
        .or_insert_with(|| toml_edit::Item::Table(toml_edit::Table::new()))
        .as_table_mut()
        .context("[cursor] in config.toml is not a table")?;
    if model_id.is_empty() {
        section.remove("model");
    } else {
        section.insert("model", toml_edit::value(model_id));
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&path, doc.to_string()).with_context(|| format!("write {}", path.display()))
}

impl CursorAgentCommand {
    /// `GROK_CURSOR_AGENT_BIN` or `[cursor] bin` pick the binary; `[cursor] args` are passed before `acp`.
    pub fn from_config(raw_config: &toml::Value, yolo: bool) -> Self {
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
        let program = std::env::var_os(CURSOR_AGENT_BIN_ENV)
            .map(PathBuf::from)
            .or_else(|| {
                section
                    .and_then(|s| s.get("bin"))
                    .and_then(toml::Value::as_str)
                    .map(PathBuf::from)
            })
            .unwrap_or_else(|| PathBuf::from(DEFAULT_CURSOR_AGENT_BIN));
        let mut args: Vec<String> = section
            .and_then(|s| s.get("args"))
            .and_then(toml::Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(toml::Value::as_str)
                    .map(str::to_owned)
                    .collect()
            })
            .unwrap_or_default();
        if yolo && !args.iter().any(|a| matches!(a.as_str(), "--yolo" | "--force" | "-f")) {
            args.push("--yolo".to_owned());
        }
        args.push("acp".to_owned());
        Self {
            program,
            args,
            default_model,
            forward_mcp_servers,
        }
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

    CURSOR_BACKEND_ACTIVE.store(true, Ordering::Relaxed);
    Ok(SpawnedAgent {
        thread_handle,
        channel: bridge.channel,
        cancel: agent_cancel,
        auth_manager,
    })
}

struct CursorIo {
    command: CursorAgentCommand,
    to_agent_rx: mpsc::UnboundedReceiver<String>,
    to_client_tx: mpsc::UnboundedSender<String>,
    cancel: CancellationToken,
    app_cancel: CancellationToken,
}

impl CursorIo {
    async fn run(self, spawned_tx: oneshot::Sender<Result<()>>) -> Result<()> {
        let CursorIo {
            command,
            mut to_agent_rx,
            to_client_tx,
            cancel,
            app_cancel,
        } = self;

        let mut cmd = tokio::process::Command::new(&command.program);
        cmd.args(&command.args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        xai_tty_utils::detach_command(&mut cmd);
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
            move |routed: Vec<Routed>| {
                for r in routed {
                    match r {
                        Routed::ToAgent(line) => {
                            let _ = stdin_tx.send(line);
                        }
                        Routed::ToClient(line) => {
                            let _ = to_client_tx.send(line);
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
                    deliver(routed);
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
    use super::*;

    fn config(toml: &str) -> toml::Value {
        toml::from_str(toml).unwrap()
    }

    #[test]
    fn cli_flag_beats_config() {
        let cfg = config("[cli]\nbackend = \"cursor\"");
        assert_eq!(resolve_backend(Some(BackendKind::Grok), &cfg), BackendKind::Grok);
    }

    #[test]
    fn config_selects_cursor() {
        if std::env::var_os(BACKEND_ENV).is_some() {
            return;
        }
        let cfg = config("[cli]\nbackend = \"cursor\"");
        assert_eq!(resolve_backend(None, &cfg), BackendKind::Cursor);
        assert_eq!(resolve_backend(None, &config("")), BackendKind::Grok);
    }

    #[test]
    fn command_uses_config_args_and_appends_acp() {
        if std::env::var_os(CURSOR_AGENT_BIN_ENV).is_some() {
            return;
        }
        let cfg = config(
            "[cursor]\nbin = \"/opt/agent\"\nargs = [\"--trust\"]\nmodel = \"gpt-5.5\"\nforward_mcp_servers = true",
        );
        let cmd = CursorAgentCommand::from_config(&cfg, true);
        assert_eq!(cmd.program, PathBuf::from("/opt/agent"));
        assert_eq!(cmd.args, vec!["--trust", "--yolo", "acp"]);
        assert_eq!(cmd.default_model.as_deref(), Some("gpt-5.5"));
        assert!(cmd.forward_mcp_servers);

        let cmd = CursorAgentCommand::from_config(&config(""), false);
        assert_eq!(cmd.program, PathBuf::from(DEFAULT_CURSOR_AGENT_BIN));
        assert_eq!(cmd.args, vec!["acp"]);
        assert_eq!(cmd.default_model, None);
        assert!(!cmd.forward_mcp_servers);
    }
}
