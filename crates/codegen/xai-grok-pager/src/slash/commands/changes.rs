//! `/changes`: review the files the agent changed this session.
//!
//! Grok binds Ctrl+R to the session picker, so Cursor's Ctrl+R review lives here as a slash command.

use crate::app::actions::Action;
use crate::slash::command::{CommandExecCtx, CommandResult, SlashCommand, slash_meta};

pub struct ChangesCommand;

impl SlashCommand for ChangesCommand {
    slash_meta! {
        name: "changes",
        description: "Review the files changed this session",
        usage: "/changes",
        session_scoped: true,
    }

    fn run(&self, _ctx: &mut CommandExecCtx, _args: &str) -> CommandResult {
        CommandResult::Action(Action::ShowChanges)
    }
}
