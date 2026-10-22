//! Route a pager session to a Grok bot that already lives on the computer hub.
//!
//! The route is not a cloud job. `/bot` lists and selects. Later plain prompts
//! call `bot_send_prompt` then `bot_await_turn` until `/bot off`.

mod hub;
mod image;
mod sdk;
mod session;

#[cfg(test)]
mod tests;

pub use hub::{AwaitedTurn, BotHub, HubBot, HubError, OnBusy, SendAccepted};
pub use image::{BOT_ICON_ASSET, BOT_ICON_PNG, GUTTER_COLS, composer_escapes};
pub use session::{BotSession, LOGIN_HINT, SubmitOutcome, format_bot_reply};

/// One line the session can interpret. Slash commands other than `/bot` never arrive here.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BotInput {
    /// Arguments after `/bot`. Empty lists the roster.
    Command(String),
    /// A plain prompt while a route may be set.
    Plain(String),
}

/// Session plus the scrollback outcome after one hub-backed submit.
#[derive(Clone, Debug, PartialEq)]
pub struct HubRun {
    pub session: BotSession,
    pub outcome: SubmitOutcome,
}

pub use sdk::run_with_hub;
