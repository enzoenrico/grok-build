//! `x.ai/hooks/action` (the pager's hooks modal) against a host that owns the hook registry: a live grok session, or a
//! working directory answered without one (see [`super::catalog`]).

use xai_hooks_plugins_types::{ActionOutcome, HooksAction, OutcomeStatus};

pub(crate) const MANAGED_HOOKS_ONLY_REFUSAL: &str =
    "Hooks outside managed policy are disabled by your organization.";

/// What a hooks action needs from whoever holds the hook registry it changes.
#[async_trait::async_trait(?Send)]
pub(crate) trait HooksActionHost {
    fn cwd(&self) -> &str;

    /// Whether `name` resolves to a managed-policy (non-disableable) hook in the host's registry, keyed on the spec's
    /// typed `layer`, never on the name. Fails open on a missing registry or name: the dispatcher re-checks provenance at
    /// run time, so this is UX and the dispatcher is the enforcement boundary.
    fn is_managed_policy_hook(&self, name: &str) -> bool;

    /// `allow_managed_hooks_only` is in force.
    fn managed_only(&self) -> bool;

    /// Re-discover hooks after a source or trust change; returns the line appended to the outcome message.
    async fn reload_hooks(&self) -> String;

    /// Trust flipped the project-config gate.
    async fn trust_changed(&self);

    /// The disabled-hooks file changed.
    fn hook_disabled_changed(&self);
}

/// Path written (or the session key on auto-trust), never the raw git root.
fn hooks_trust_key(
    outcome: &xai_grok_workspace::folder_trust::GrantOutcome,
    cwd: &std::path::Path,
) -> std::path::PathBuf {
    match outcome {
        xai_grok_workspace::folder_trust::GrantOutcome::Granted { key, .. }
        | xai_grok_workspace::folder_trust::GrantOutcome::AlreadyDurable { key } => key.clone(),
        xai_grok_workspace::folder_trust::GrantOutcome::Refused { .. } => {
            xai_grok_workspace::trust::workspace_key(cwd)
        }
    }
}

/// Same as `--trust`: also allows repo-local MCP/LSP for this folder.
/// Still requires a git worktree, but grants `workspace_key(cwd)` (the session key `decide` uses), not the raw git root.
/// When `$HOME` is a checkout that root is UnsafeRoot and would look trusted without a record.
pub(crate) fn trust_project(cwd: &str) -> Result<std::path::PathBuf, String> {
    let cwd_path = std::path::Path::new(cwd);
    xai_grok_workspace::session::git::find_git_root_from_path(cwd_path).map_err(|_| {
        "Not in a git repository. Project hooks require a git worktree root.".to_string()
    })?;
    let outcome = xai_grok_workspace::folder_trust::grant_folder_trust(cwd_path);
    // Success only for this session key: durable / already-durable, or
    // auto-trust when cwd itself is unrecordable (cwd is $HOME / inert).
    if outcome.dismisses_gate() {
        return Ok(hooks_trust_key(&outcome, cwd_path));
    }
    Err(outcome.to_string())
}

/// Untrust the current project in the unified folder-trust store.
/// Returns (session workspace key, was_trusted).
pub(crate) fn untrust_project(cwd: &str) -> Result<(std::path::PathBuf, bool), String> {
    let cwd_path = std::path::Path::new(cwd);
    xai_grok_workspace::session::git::find_git_root_from_path(cwd_path)
        .map_err(|_| "Not in a git repository.".to_string())?;
    // Same key as grant: revoke_folder_trust runs workspace_key(cwd).
    let key = xai_grok_workspace::trust::workspace_key(cwd_path);
    let was_trusted = crate::agent::folder_trust::revoke_folder_trust(cwd_path);
    Ok((key, was_trusted))
}

fn outcome(status: OutcomeStatus, message: impl Into<String>) -> ActionOutcome {
    ActionOutcome {
        status,
        message: message.into(),
        requires_reload: false,
        requires_restart: false,
    }
}

/// Under `allow_managed_hooks_only`, enabling anything outside managed policy is refused; the one rule for the per-hook
/// and per-source enable. Managed hooks pass: the pin never blocks them, so enabling one changes nothing at dispatch.
fn refuse_enable_under_managed_only<'a>(
    host: &dyn HooksActionHost,
    names: impl IntoIterator<Item = &'a str>,
) -> Option<ActionOutcome> {
    if !host.managed_only() {
        return None;
    }
    names
        .into_iter()
        .any(|name| !host.is_managed_policy_hook(name))
        .then(|| outcome(OutcomeStatus::ValidationError, MANAGED_HOOKS_ONLY_REFUSAL))
}

pub(crate) async fn run_hooks_action(
    host: &dyn HooksActionHost,
    action: HooksAction,
) -> ActionOutcome {
    match action {
        HooksAction::Reload => {
            let reload_msg = host.reload_hooks().await;
            outcome(
                OutcomeStatus::Success,
                format!("Hooks reloaded.\n{reload_msg}"),
            )
        }
        HooksAction::Trust => match trust_project(host.cwd()) {
            Err(e) => outcome(OutcomeStatus::ValidationError, e),
            Ok(root) => {
                let reload_msg = host.reload_hooks().await;
                host.trust_changed().await;
                outcome(
                    OutcomeStatus::Success,
                    format!("Trusted: {}.\n{reload_msg}", root.display()),
                )
            }
        },
        HooksAction::Untrust => match untrust_project(host.cwd()) {
            Err(e) => outcome(OutcomeStatus::ValidationError, e),
            Ok((root, false)) => outcome(
                OutcomeStatus::NotFound,
                format!("Not currently trusted: {}", root.display()),
            ),
            Ok((root, true)) => {
                let reload_msg = host.reload_hooks().await;
                host.trust_changed().await;
                outcome(
                    OutcomeStatus::Success,
                    format!("Untrusted: {}.\n{reload_msg}", root.display()),
                )
            }
        },
        HooksAction::Add { path } => {
            if path.is_empty() {
                return outcome(OutcomeStatus::ValidationError, "Path is required.");
            }
            // CWE-427: add_hooks_path() validates path is under ~/.grok/.
            match crate::config::add_hooks_path(&path) {
                Ok(()) => {
                    let reload_msg = host.reload_hooks().await;
                    outcome(
                        OutcomeStatus::Success,
                        format!("Added hook path: {path}.\n{reload_msg}"),
                    )
                }
                Err(e) => outcome(
                    OutcomeStatus::ValidationError,
                    format!("Failed to add hook path: {e}"),
                ),
            }
        }
        HooksAction::Remove { path } => {
            if path.is_empty() {
                return outcome(OutcomeStatus::ValidationError, "Path is required.");
            }
            match crate::config::remove_hooks_path(&path) {
                Ok(true) => {
                    let reload_msg = host.reload_hooks().await;
                    outcome(
                        OutcomeStatus::Success,
                        format!("Removed hook path: {path}.\n{reload_msg}"),
                    )
                }
                // The message omits the path; the user's selection already identifies the source
                Ok(false) => outcome(
                    OutcomeStatus::NotFound,
                    "Only user-added hook directories can be removed here.",
                ),
                Err(e) => outcome(
                    OutcomeStatus::InternalError,
                    format!("Failed to remove hook path: {e}"),
                ),
            }
        }
        HooksAction::Disable { hook_name } => {
            // Internal spec names stay out of the user-facing message
            if host.is_managed_policy_hook(&hook_name) {
                return outcome(
                    OutcomeStatus::ValidationError,
                    "This hook is enforced by managed policy and cannot be disabled.",
                );
            }
            match xai_grok_hooks::trust::disable_hook(&hook_name) {
                Ok(()) => {
                    host.hook_disabled_changed();
                    outcome(OutcomeStatus::Success, "Hook disabled.")
                }
                Err(e) => outcome(
                    OutcomeStatus::InternalError,
                    format!("Failed to disable hook: {e}"),
                ),
            }
        }
        HooksAction::Enable { hook_name } => {
            if let Some(refused) = refuse_enable_under_managed_only(host, [hook_name.as_str()]) {
                return refused;
            }
            match xai_grok_hooks::trust::enable_hook(&hook_name) {
                Ok(true) => {
                    host.hook_disabled_changed();
                    outcome(OutcomeStatus::Success, "Hook enabled.")
                }
                Ok(false) => outcome(OutcomeStatus::NotFound, "Hook was not disabled."),
                Err(e) => outcome(
                    OutcomeStatus::InternalError,
                    format!("Failed to enable hook: {e}"),
                ),
            }
        }
        HooksAction::ToggleSource {
            hook_names,
            disable,
        } => {
            if !disable
                && let Some(refused) =
                    refuse_enable_under_managed_only(host, hook_names.iter().map(String::as_str))
            {
                return refused;
            }
            let mut toggled = 0usize;
            let mut managed_skipped = 0usize;
            for name in &hook_names {
                // Managed-policy hooks are exempt from bulk disable, same rule as the per-hook Disable action
                if disable && host.is_managed_policy_hook(name) {
                    managed_skipped += 1;
                    continue;
                }
                let ok = if disable {
                    xai_grok_hooks::trust::disable_hook(name).is_ok()
                } else {
                    // Only an actual removal counts (Ok(false) means it wasn't disabled)
                    xai_grok_hooks::trust::enable_hook(name) == Ok(true)
                };
                if ok {
                    toggled += 1;
                }
            }
            if toggled > 0 {
                host.hook_disabled_changed();
            }
            let action = if disable { "Disabled" } else { "Enabled" };
            let mut message = format!("{action} {toggled}/{} hooks", hook_names.len());
            if managed_skipped > 0 {
                message.push_str(&format!(
                    " ({managed_skipped} enforced by managed policy, not disabled)"
                ));
            }
            outcome(OutcomeStatus::Success, message)
        }
    }
}
