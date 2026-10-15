//! Which agent runtime serves the pager.
//!
//! Selected with `--backend`, `GROK_BACKEND`, or `[cli] backend` in `config.toml`.

use anyhow::Result;
use tokio_util::sync::CancellationToken;
use xai_grok_shell::agent::config::Config as AgentConfig;

use super::cursor::{self, CursorAgentCommand};
use super::spawn::{self, SpawnedAgent};

pub const BACKEND_ENV: &str = "GROK_BACKEND";

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
fn resolve_kind(
    cli: Option<BackendKind>,
    env: Option<&str>,
    raw_config: &toml::Value,
) -> BackendKind {
    if let Some(kind) = cli {
        return kind;
    }
    if let Some(kind) = env.and_then(BackendKind::parse) {
        return kind;
    }
    raw_config
        .get("cli")
        .and_then(|cli| cli.get("backend"))
        .and_then(toml::Value::as_str)
        .and_then(BackendKind::parse)
        .unwrap_or_default()
}

/// Resolved runtime for this process. Built once at the edge, then passed down.
#[derive(Debug, Clone, Default)]
pub enum AgentBackend {
    #[default]
    Grok,
    Cursor(CursorAgentCommand),
}

impl AgentBackend {
    /// Reads `GROK_BACKEND` and `GROK_CURSOR_AGENT_BIN` once, here at the edge.
    pub fn resolve(cli: Option<BackendKind>, raw_config: &toml::Value) -> Self {
        let env = std::env::var(BACKEND_ENV).ok();
        match resolve_kind(cli, env.as_deref(), raw_config) {
            BackendKind::Grok => Self::Grok,
            BackendKind::Cursor => {
                let bin_env = std::env::var_os(cursor::CURSOR_AGENT_BIN_ENV);
                Self::Cursor(CursorAgentCommand::from_config(raw_config, bin_env))
            }
        }
    }

    pub fn kind(&self) -> BackendKind {
        match self {
            Self::Grok => BackendKind::Grok,
            Self::Cursor(_) => BackendKind::Cursor,
        }
    }

    pub async fn spawn(
        &self,
        agent_config: AgentConfig,
        cancel: &CancellationToken,
        yolo: bool,
    ) -> Result<SpawnedAgent> {
        match self {
            Self::Grok => {
                let memory_config = agent_config.memory_config.clone();
                spawn::spawn_grok_shell(agent_config, cancel, memory_config).await
            }
            Self::Cursor(cmd) => {
                let auth_manager = spawn::boot_auth_manager(
                    &xai_grok_shell::util::grok_home::grok_home(),
                    &agent_config,
                );
                cursor::spawn_cursor_agent(cmd.clone(), cancel, auth_manager, yolo).await
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(toml_text: &str) -> toml::Value {
        toml::from_str(toml_text).unwrap()
    }

    #[test]
    fn cli_flag_beats_env_and_config() {
        let cfg = config("[cli]\nbackend = \"cursor\"");
        assert_eq!(
            resolve_kind(Some(BackendKind::Grok), Some("cursor"), &cfg),
            BackendKind::Grok
        );
    }

    #[test]
    fn env_beats_config() {
        let cfg = config("[cli]\nbackend = \"grok\"");
        assert_eq!(
            resolve_kind(None, Some("cursor"), &cfg),
            BackendKind::Cursor
        );
        assert_eq!(
            resolve_kind(None, Some("cursor-agent"), &config("")),
            BackendKind::Cursor
        );
        assert_eq!(resolve_kind(None, Some("shell"), &cfg), BackendKind::Grok);
    }

    #[test]
    fn config_selects_cursor_and_blank_defaults_to_grok() {
        let cfg = config("[cli]\nbackend = \"cursor\"");
        assert_eq!(resolve_kind(None, None, &cfg), BackendKind::Cursor);
        assert_eq!(resolve_kind(None, Some("nope"), &cfg), BackendKind::Cursor);
        assert_eq!(resolve_kind(None, None, &config("")), BackendKind::Grok);
        assert_eq!(
            resolve_kind(None, Some("  "), &config("")),
            BackendKind::Grok
        );
    }
}
