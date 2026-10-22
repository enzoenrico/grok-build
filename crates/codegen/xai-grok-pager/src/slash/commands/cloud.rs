//! `/cloud` lists Cursor cloud jobs for this repo, focuses one, or cancels one.
//! The jobs are the same list `&` writes and the tasks pane Cloud group reads.

use crate::acp::cursor::cloud_jobs;
use crate::slash::command::{CommandExecCtx, CommandResult, SlashCommand, slash_meta};

/// List, focus, or cancel Cursor cloud jobs for this repo.
pub struct CloudCommand;

impl SlashCommand for CloudCommand {
    slash_meta! {
        name: "cloud",
        description: "List, focus, or cancel Cursor cloud jobs for this repo",
        usage: "/cloud [id | cancel <id>]",
        takes_args: true,
        session_scoped: true,
        arg_placeholder: "id | cancel <id>",
    }

    fn run(&self, ctx: &mut CommandExecCtx, args: &str) -> CommandResult {
        let Some(session_id) = ctx.session_id else {
            return CommandResult::Error("No active session".to_string());
        };
        match cloud_jobs::run_command(session_id.0.as_ref(), args) {
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
        CloudCommand.run(&mut ctx, args)
    }

    #[test]
    fn no_session_errors() {
        match run_with_session(None, "") {
            CommandResult::Error(msg) => assert!(msg.contains("No active session")),
            other => panic!("expected Error, got {other:?}"),
        }
    }

    #[test]
    fn cloud_args_parse_list_focus_and_cancel() {
        assert!(matches!(
            cloud_jobs::parse_cloud_args("").unwrap(),
            cloud_jobs::CloudArgs::List
        ));
        assert!(matches!(
            cloud_jobs::parse_cloud_args("bc-1").unwrap(),
            cloud_jobs::CloudArgs::Focus(id) if id == "bc-1"
        ));
        assert!(matches!(
            cloud_jobs::parse_cloud_args("cancel bc-2").unwrap(),
            cloud_jobs::CloudArgs::Cancel(id) if id == "bc-2"
        ));
        assert!(cloud_jobs::parse_cloud_args("cancel").is_err());
    }
}
