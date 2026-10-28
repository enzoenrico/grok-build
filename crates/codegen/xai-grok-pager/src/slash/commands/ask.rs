//! `/ask` toggles read-only ask mode.
//! `/ask <prompt>` enters ask mode and starts a turn with that prompt.

use crate::app::actions::Action;
use crate::slash::command::{CommandExecCtx, CommandResult, SlashCommand, slash_meta};

pub struct AskCommand;

impl SlashCommand for AskCommand {
    slash_meta! {
        name: "ask",
        description: "Ask without editing",
        usage: "/ask [prompt]",
        takes_args: true,
        session_scoped: true,
        arg_placeholder: "[prompt]",
    }

    fn run(&self, _ctx: &mut CommandExecCtx, args: &str) -> CommandResult {
        let trimmed = args.trim();
        let prompt = if trimmed.is_empty() {
            None
        } else {
            Some(trimmed.to_string())
        };
        CommandResult::Action(Action::EnterAskMode { prompt })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::acp::model_state::ModelState;
    use crate::app::bundle::BundleState;
    use crate::settings::PagerLocalSnapshot;

    fn ctx<'a>(models: &'a ModelState, bundle: &'a BundleState) -> CommandExecCtx<'a> {
        CommandExecCtx {
            models,
            session_id: None,
            bundle_state: bundle,
            screen_mode: crate::app::ScreenMode::Inline,
            billing_surface_visible: true,
            usage_command_visible: true,
            pager_state: PagerLocalSnapshot::default(),
        }
    }

    #[test]
    fn bare_ask_toggles_without_a_prompt() {
        let models = ModelState::default();
        let bundle = BundleState::default();
        let mut ctx = ctx(&models, &bundle);
        match AskCommand.run(&mut ctx, "  ") {
            CommandResult::Action(Action::EnterAskMode { prompt }) => {
                assert!(prompt.is_none());
            }
            other => panic!("expected EnterAskMode, got {other:?}"),
        }
    }

    #[test]
    fn ask_with_text_keeps_the_prompt() {
        let models = ModelState::default();
        let bundle = BundleState::default();
        let mut ctx = ctx(&models, &bundle);
        match AskCommand.run(&mut ctx, " where is main ") {
            CommandResult::Action(Action::EnterAskMode { prompt }) => {
                assert_eq!(prompt.as_deref(), Some("where is main"));
            }
            other => panic!("expected EnterAskMode, got {other:?}"),
        }
    }
}
