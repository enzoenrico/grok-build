//! `/fast` (alias `/speed`) and `/max-mode` toggle or set fast mode and Max mode for the active model.

use crate::app::actions::{Action, ModelChoice};
use crate::slash::command::{
    AppCtx, ArgItem, CommandExecCtx, CommandResult, SlashCommand, slash_meta,
};
use xai_grok_shell::sampling::types::{parse_bool_or_speed_token, parse_bool_token};

pub struct FastCommand;

impl SlashCommand for FastCommand {
    slash_meta! {
        name: "fast",
        aliases: ["speed"],
        description: "Toggle or set fast mode for the current model",
        usage: "/fast [on|off|toggle]",
        takes_args: true,
        args_required: false,
        session_scoped: true,
        arg_placeholder: "[on|off|toggle]",
    }

    fn visible(&self, ctx: &AppCtx) -> bool {
        ctx.models.supports_fast_mode() || ctx.models.fast_mode.is_some()
    }

    fn suggest_args(&self, ctx: &AppCtx, _args_query: &str) -> Option<Vec<ArgItem>> {
        let current = ctx.models.fast_mode.unwrap_or(false);
        Some(toggle_arg_items(
            current,
            "Enable fast mode (higher output speed)",
            "Standard output speed",
        ))
    }

    fn run(&self, ctx: &mut CommandExecCtx, args: &str) -> CommandResult {
        let Some(model_id) = ctx.models.current.clone() else {
            return CommandResult::Error("No active model".into());
        };

        let trimmed = args.trim();
        let enabled = if trimmed.is_empty() || trimmed.eq_ignore_ascii_case("toggle") {
            !ctx.models.fast_mode.unwrap_or(false)
        } else if let Some(val) = parse_bool_or_speed_token(trimmed) {
            val
        } else {
            return CommandResult::Error(format!(
                "Usage: /fast [on|off|toggle] (unknown option '{trimmed}')"
            ));
        };

        CommandResult::Action(Action::SwitchModel(ModelChoice {
            effort: ctx.models.reasoning_effort,
            context_window_selection: ctx
                .models
                .get_context_window()
                .and_then(std::num::NonZeroU64::new),
            fast_mode: Some(enabled),
            max_mode: ctx.models.max_mode,
            model_parameters: ctx.models.model_parameters.clone(),
            ..ModelChoice::new(model_id)
        }))
    }
}

pub struct MaxModeCommand;

impl SlashCommand for MaxModeCommand {
    slash_meta! {
        name: "max-mode",
        description: "Toggle or set Max mode for the current model",
        usage: "/max-mode [on|off|toggle]",
        takes_args: true,
        args_required: false,
        session_scoped: true,
        arg_placeholder: "[on|off|toggle]",
    }

    fn visible(&self, ctx: &AppCtx) -> bool {
        ctx.models.supports_max_mode() || ctx.models.max_mode.is_some()
    }

    fn suggest_args(&self, ctx: &AppCtx, _args_query: &str) -> Option<Vec<ArgItem>> {
        let current = ctx.models.max_mode.unwrap_or(false);
        Some(toggle_arg_items(
            current,
            "Enable Max mode (expanded context & limits)",
            "Disable Max mode",
        ))
    }

    fn run(&self, ctx: &mut CommandExecCtx, args: &str) -> CommandResult {
        let Some(model_id) = ctx.models.current.clone() else {
            return CommandResult::Error("No active model".into());
        };

        let trimmed = args.trim();
        let enabled = if trimmed.is_empty() || trimmed.eq_ignore_ascii_case("toggle") {
            !ctx.models.max_mode.unwrap_or(false)
        } else if let Some(val) = parse_bool_token(trimmed) {
            val
        } else {
            return CommandResult::Error(format!(
                "Usage: /max-mode [on|off|toggle] (unknown option '{trimmed}')"
            ));
        };

        CommandResult::Action(Action::SwitchModel(ModelChoice {
            effort: ctx.models.reasoning_effort,
            context_window_selection: ctx
                .models
                .get_context_window()
                .and_then(std::num::NonZeroU64::new),
            fast_mode: ctx.models.fast_mode,
            max_mode: Some(enabled),
            model_parameters: ctx.models.model_parameters.clone(),
            ..ModelChoice::new(model_id)
        }))
    }
}

fn toggle_arg_items(current: bool, on_desc: &str, off_desc: &str) -> Vec<ArgItem> {
    let on_suffix = if current { " (active)" } else { "" };
    let off_suffix = if !current { " (active)" } else { "" };
    vec![
        ArgItem {
            display: format!("on{on_suffix}"),
            match_text: "a on".to_string(),
            insert_text: "on".to_string(),
            description: on_desc.to_string(),
        },
        ArgItem {
            display: format!("off{off_suffix}"),
            match_text: "b off".to_string(),
            insert_text: "off".to_string(),
            description: off_desc.to_string(),
        },
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::acp::model_state::ModelState;
    use crate::slash::commands::tests::make_ctx;
    use agent_client_protocol as acp;
    use std::sync::Arc;

    fn sample_models() -> ModelState {
        let mut state = ModelState::default();
        let id = acp::ModelId::new(Arc::from("claude-opus-4-6"));
        let mut meta = serde_json::Map::new();
        meta.insert("supportsFastMode".into(), serde_json::Value::Bool(true));
        meta.insert("supportsMaxMode".into(), serde_json::Value::Bool(true));
        state.available.insert(
            id.clone(),
            acp::ModelInfo::new(id.clone(), "Claude 4.6 Opus").meta(Some(meta)),
        );
        state.current = Some(id);
        state
    }

    #[test]
    fn fast_command_toggles_and_sets_explicit_state() {
        let mut models = sample_models();
        let cmd = FastCommand;

        let mut ctx = make_ctx(&models);
        match cmd.run(&mut ctx, "") {
            CommandResult::Action(Action::SwitchModel(choice)) => {
                assert_eq!(choice.fast_mode, Some(true));
            }
            other => panic!("expected SwitchModel, got {other:?}"),
        }

        models.fast_mode = Some(true);
        let mut ctx = make_ctx(&models);
        match cmd.run(&mut ctx, "off") {
            CommandResult::Action(Action::SwitchModel(choice)) => {
                assert_eq!(choice.fast_mode, Some(false));
            }
            other => panic!("expected SwitchModel, got {other:?}"),
        }
    }

    #[test]
    fn max_mode_command_toggles_and_sets_explicit_state() {
        let mut models = sample_models();
        let cmd = MaxModeCommand;

        let mut ctx = make_ctx(&models);
        match cmd.run(&mut ctx, "on") {
            CommandResult::Action(Action::SwitchModel(choice)) => {
                assert_eq!(choice.max_mode, Some(true));
            }
            other => panic!("expected SwitchModel, got {other:?}"),
        }

        models.max_mode = Some(true);
        let mut ctx = make_ctx(&models);
        match cmd.run(&mut ctx, "toggle") {
            CommandResult::Action(Action::SwitchModel(choice)) => {
                assert_eq!(choice.max_mode, Some(false));
            }
            other => panic!("expected SwitchModel, got {other:?}"),
        }
    }
}
