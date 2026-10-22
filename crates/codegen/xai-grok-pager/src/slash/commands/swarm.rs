//! `/swarm <n> <prompt>` starts n Cursor cloud jobs on the shared job list.
//! `/swarm` prints one rollup. One member failing does not cancel the others.

use crate::acp::cursor::cloud_jobs;
use crate::slash::command::{CommandExecCtx, CommandResult, SlashCommand, slash_meta};

/// Fan out up to 8 Cursor cloud jobs and print one rollup.
pub struct SwarmCommand;

impl SlashCommand for SwarmCommand {
    slash_meta! {
        name: "swarm",
        description: "Run up to 8 Cursor cloud jobs and print one rollup",
        usage: "/swarm [<n> <prompt>]",
        takes_args: true,
        session_scoped: true,
        arg_placeholder: "<n> <prompt>",
    }

    fn run(&self, ctx: &mut CommandExecCtx, args: &str) -> CommandResult {
        let Some(session_id) = ctx.session_id else {
            return CommandResult::Error("No active session".to_string());
        };
        match cloud_jobs::run_swarm(session_id.0.as_ref(), args) {
            Ok(text) => CommandResult::Message(text),
            Err(err) => CommandResult::Error(err),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::acp::model_state::ModelState;
    use crate::app::bundle::BundleState;
    use crate::settings::PagerLocalSnapshot;

    static DEFAULT_BUNDLE_STATE: BundleState = BundleState {
        has_cache: false,
        version: String::new(),
        personas: Vec::new(),
        roles: Vec::new(),
        agents: Vec::new(),
        skills: Vec::new(),
        persona_details: Vec::new(),
        role_details: Vec::new(),
    };

    fn run_with_session(
        sid: Option<&agent_client_protocol::SessionId>,
        args: &str,
    ) -> CommandResult {
        let models = ModelState::default();
        let mut ctx = CommandExecCtx {
            models: &models,
            session_id: sid,
            bundle_state: &DEFAULT_BUNDLE_STATE,
            screen_mode: crate::app::ScreenMode::Minimal,
            billing_surface_visible: true,
            usage_command_visible: true,
            pager_state: PagerLocalSnapshot::default(),
        };
        SwarmCommand.run(&mut ctx, args)
    }

    #[test]
    fn no_session_errors() {
        match run_with_session(None, "") {
            CommandResult::Error(msg) => assert!(msg.contains("No active session")),
            other => panic!("expected Error, got {other:?}"),
        }
    }
}
