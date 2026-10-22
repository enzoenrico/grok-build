//! `/bot` routes this session to a Grok bot on the computer hub.
//!
//! The command sends nothing until a name is followed by text. Later plain
//! prompts go to the routed bot. `/bot off` clears the route.

use crate::app::actions::Action;
use crate::slash::command::{CommandExecCtx, CommandResult, SlashCommand, slash_meta};

pub struct BotCommand;

impl SlashCommand for BotCommand {
    slash_meta! {
        name: "bot",
        description: "Route this session to a Grok bot on your computer",
        usage: "/bot [name|off|new] [text]",
        takes_args: true,
        session_scoped: true,
        arg_placeholder: "name | off | new",
    }

    fn run(&self, ctx: &mut CommandExecCtx, args: &str) -> CommandResult {
        if ctx.session_id.is_none() {
            return CommandResult::Error("No active session".to_string());
        }
        CommandResult::Action(Action::BotCommand(args.to_string()))
    }
}
