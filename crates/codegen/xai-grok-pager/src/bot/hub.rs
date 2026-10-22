//! Hub calls the `/bot` route uses. The session never talks to the socket itself.

use async_trait::async_trait;
use serde_json::Value;

/// One Grok bot on the computer hub.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HubBot {
    pub id: String,
    pub name: String,
    pub description: String,
    pub status: String,
}

/// `on_busy` for `bot_send_prompt`.
///
/// A normal send passes [`Self::Reject`] so a busy bot is not interrupted.
/// `--replace` passes [`Self::Supersede`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OnBusy {
    Reject,
    Supersede,
}

impl OnBusy {
    pub fn as_wire(self) -> &'static str {
        match self {
            Self::Reject => "reject",
            Self::Supersede => "supersede",
        }
    }
}

/// Handle returned by `bot_send_prompt`. Stored as JSON so a later `bot_await_turn` can send it back unchanged.
#[derive(Clone, Debug, PartialEq)]
pub struct SendAccepted {
    pub handle: Value,
}

/// Result of `bot_await_turn`.
#[derive(Clone, Debug, PartialEq)]
pub enum AwaitedTurn {
    Finished {
        reply: String,
    },
    /// `finished` is false. The handle is the one to resume with.
    Timeout {
        handle: Value,
    },
}

/// Failure from a hub call. [`Self::NeedsLogin`] is the only auth failure the CLI prints.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HubError {
    NeedsLogin,
    /// The bot already has a turn in flight and this send used `on_busy: reject`.
    Busy,
    /// The wait exceeded the hub's budget. The caller still has the handle it sent.
    TimedOut,
    Message(String),
}

impl std::fmt::Display for HubError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NeedsLogin => f.write_str("needs login"),
            Self::Busy => f.write_str("busy"),
            Self::TimedOut => f.write_str("timed out"),
            Self::Message(message) => f.write_str(message),
        }
    }
}

/// The five hub tools `/bot` is allowed to call. Voice tools stay unwired.
#[async_trait]
pub trait BotHub: Send + Sync {
    async fn list_agents(&self) -> Result<Vec<HubBot>, HubError>;
    async fn search_agents(&self, query: &str) -> Result<Vec<HubBot>, HubError>;
    async fn create_agent(&self, name: &str) -> Result<HubBot, HubError>;
    async fn send_prompt(
        &self,
        agent_id: &str,
        prompt: &str,
        on_busy: OnBusy,
    ) -> Result<SendAccepted, HubError>;
    async fn await_turn(
        &self,
        agent_id: &str,
        handle: Option<&Value>,
    ) -> Result<AwaitedTurn, HubError>;
}
