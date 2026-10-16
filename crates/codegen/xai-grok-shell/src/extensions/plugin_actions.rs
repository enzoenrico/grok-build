//! `x.ai/plugins/action` (the pager's plugins modal) against a host that owns the plugin registry: a live grok session,
//! or a working directory answered without one (see [`super::catalog`]).

use xai_hooks_plugins_types::{ActionOutcome, OutcomeStatus, PluginsAction};

/// What a plugins action needs from whoever holds the plugin registry it changes.
#[async_trait::async_trait(?Send)]
pub(crate) trait PluginsActionHost {
    fn cwd(&self) -> &str;

    /// Rebuild the host's plugin registry and return a summary line, or `None` when the host has no registry to rebuild.
    /// `force` is `true` only for the explicit `/plugins reload`, which forces a full re-copy of local installs.
    async fn reload_plugins(&self, force: bool) -> Option<String>;
}

/// Resolve a potentially relative path against the host cwd.
fn resolve_path(cwd: &str, path: &str) -> std::path::PathBuf {
    let p = std::path::Path::new(path);
    if p.is_relative() {
        std::path::Path::new(cwd).join(p)
    } else {
        p.to_path_buf()
    }
}

fn outcome(status: OutcomeStatus, message: impl Into<String>) -> ActionOutcome {
    ActionOutcome {
        status,
        message: message.into(),
        requires_reload: false,
        requires_restart: false,
    }
}

async fn set_enabled(
    host: &dyn PluginsActionHost,
    plugin_id: String,
    enabled: bool,
) -> ActionOutcome {
    let (done, verb) = if enabled {
        ("Enabled", "enable")
    } else {
        ("Disabled", "disable")
    };
    match crate::config::run_set_plugin_enabled(plugin_id.clone(), enabled).await {
        Ok(()) => match host.reload_plugins(false).await {
            Some(reload_msg) => outcome(
                OutcomeStatus::Success,
                format!("{done}: {plugin_id}.\n{reload_msg}"),
            ),
            None => ActionOutcome {
                requires_restart: true,
                ..outcome(
                    OutcomeStatus::Success,
                    format!("{done}: {plugin_id}. Restart to apply."),
                )
            },
        },
        Err(e) => outcome(
            OutcomeStatus::InternalError,
            format!("Failed to {verb} plugin: {e}"),
        ),
    }
}

async fn edit_path(host: &dyn PluginsActionHost, path: String, add: bool) -> ActionOutcome {
    if path.is_empty() {
        return outcome(OutcomeStatus::ValidationError, "Path is required.");
    }
    let path_str = resolve_path(host.cwd(), &path).display().to_string();
    let (edited, done, verb) = if add {
        (
            crate::config::run_add_plugin_path(path_str.clone()).await,
            "Added",
            "add",
        )
    } else {
        (
            crate::config::run_remove_plugin_path(path_str.clone()).await,
            "Removed",
            "remove",
        )
    };
    match edited {
        Ok(()) => {
            let mut msg = format!("{done} plugin path: {path_str}");
            if let Some(reload_msg) = host.reload_plugins(false).await {
                msg.push('\n');
                msg.push_str(&reload_msg);
            }
            outcome(OutcomeStatus::Success, msg)
        }
        Err(e) => outcome(
            OutcomeStatus::InternalError,
            format!("Failed to {verb} plugin path: {e}"),
        ),
    }
}

pub(crate) async fn run_plugins_action(
    host: &dyn PluginsActionHost,
    action: PluginsAction,
) -> ActionOutcome {
    match action {
        PluginsAction::Reload => match host.reload_plugins(true).await {
            Some(msg) => outcome(OutcomeStatus::Success, msg),
            None => outcome(
                OutcomeStatus::Unsupported,
                "No plugin registry handle available.",
            ),
        },
        PluginsAction::Install { source } => {
            if source.is_empty() {
                return outcome(
                    OutcomeStatus::ValidationError,
                    "Source is required (git URL or local path).",
                );
            }
            // Shared gated pipeline (parse → registry flock → gate → save → auto-enable): this arm can't
            // bypass the lockdown; blocking work runs off the LocalSet (invariant: plugin/acquire.rs).
            let cwd = host.cwd().to_owned();
            let cloned_source = source.clone();
            let installed = tokio::task::spawn_blocking(move || {
                crate::plugin::install_plugin(&cloned_source, std::path::Path::new(&cwd))
            })
            .await;
            match installed {
                Ok(Ok(installed)) => {
                    let count = installed.plugin_names.len();
                    let mut msg = format!(
                        "Installed {count} plugin(s) from {source}: {}",
                        installed.plugin_names.join(", ")
                    );
                    for w in &installed.warnings {
                        msg.push_str(&format!(" (warning: {w})"));
                    }
                    ActionOutcome {
                        requires_reload: true,
                        ..outcome(OutcomeStatus::Success, msg)
                    }
                }
                // The policy refusal is a complete, validation-shaped message — match the marketplace-modal
                // sibling instead of wrapping it as an internal error.
                Ok(Err(crate::plugin::PluginInstallError::Blocked { reason })) => {
                    outcome(OutcomeStatus::ValidationError, reason)
                }
                Ok(Err(e)) => outcome(
                    OutcomeStatus::InternalError,
                    format!("Failed to install plugin: {e}"),
                ),
                Err(e) => outcome(
                    OutcomeStatus::InternalError,
                    format!("Install task failed: {e}"),
                ),
            }
        }
        PluginsAction::Uninstall {
            plugin_id,
            confirmed,
        } => {
            if plugin_id.is_empty() {
                return outcome(OutcomeStatus::ValidationError, "Plugin ID is required.");
            }
            // Shared uninstall keeps this arm aligned with the CLI path; registry + fs work runs on the
            // blocking pool (invariant: plugin/acquire.rs).
            let task = tokio::task::spawn_blocking(move || {
                // Extract plugin name from ID (last segment of "scope/hex8/name").
                let plugin_name = plugin_id.rsplit('/').next().unwrap_or(&plugin_id);
                crate::plugin::uninstall_plugin(plugin_name, confirmed, false)
            })
            .await;
            use crate::plugin::UninstallError;
            match task {
                Ok(Ok(removed)) => ActionOutcome {
                    requires_reload: true,
                    ..outcome(
                        OutcomeStatus::Success,
                        format!(
                            "Uninstalled repo \"{}\" ({} plugin(s): {})",
                            removed.repo_key,
                            removed.removed_plugins.len(),
                            removed.removed_plugins.join(", ")
                        ),
                    )
                },
                Ok(Err(UninstallError::NeedsConfirm {
                    name,
                    repo_key,
                    other_plugins,
                    total,
                })) => outcome(
                    OutcomeStatus::ConfirmationRequired,
                    format!(
                        "Repo \"{repo_key}\" contains {total} plugin(s): {}. Uninstalling will remove all of them.",
                        std::iter::once(name)
                            .chain(other_plugins)
                            .collect::<Vec<_>>()
                            .join(", ")
                    ),
                ),
                Ok(Err(UninstallError::NotFound { name })) => outcome(
                    OutcomeStatus::NotFound,
                    format!("Plugin \"{name}\" not found in install registry."),
                ),
                Ok(Err(UninstallError::RegistryLock { detail })) => outcome(
                    OutcomeStatus::InternalError,
                    format!("Another plugin operation is in progress: {detail}"),
                ),
                Ok(Err(e @ UninstallError::RegistrySave { .. })) => ActionOutcome {
                    // Files are gone even though the registry is stale.
                    requires_reload: true,
                    ..outcome(OutcomeStatus::InternalError, e.to_string())
                },
                Err(e) => outcome(
                    OutcomeStatus::InternalError,
                    format!("Uninstall task failed: {e}"),
                ),
            }
        }
        PluginsAction::Update { plugin_id } => {
            use crate::plugin::{RepoUpdateOutcome, UpdateError};

            // Shared gated update pipeline: this arm can't force-sync a blocked source; sync fetches
            // run off the LocalSet (invariant: plugin/acquire.rs).
            let name = plugin_id
                .as_deref()
                .map(|id| id.rsplit('/').next().unwrap_or(id).to_string());
            let task =
                tokio::task::spawn_blocking(move || crate::plugin::update_plugins(name.as_deref()))
                    .await;
            match task {
                Ok(Ok(outcomes)) if outcomes.is_empty() => {
                    outcome(OutcomeStatus::NotFound, "No installed plugins to update.")
                }
                Ok(Ok(outcomes)) => {
                    let any_updated = outcomes
                        .iter()
                        .any(crate::plugin::repo_update_requires_reload);
                    let messages: Vec<String> = outcomes
                        .iter()
                        .map(|o| match o {
                            RepoUpdateOutcome::Updated { repo_key, .. } => {
                                format!("{repo_key}: updated")
                            }
                            RepoUpdateOutcome::AlreadyUpToDate { repo_key } => {
                                format!("{repo_key}: already up to date")
                            }
                            RepoUpdateOutcome::Pinned { repo_key, ref_name } => {
                                format!("{repo_key}: pinned to {ref_name}")
                            }
                            RepoUpdateOutcome::LiveLocal { repo_key } => {
                                format!("{repo_key}: local symlink (already live)")
                            }
                            RepoUpdateOutcome::Failed { repo_key, error } => {
                                format!("{repo_key}: update failed: {error}")
                            }
                        })
                        .collect();
                    ActionOutcome {
                        requires_reload: any_updated,
                        ..outcome(OutcomeStatus::Success, messages.join("\n"))
                    }
                }
                Ok(Err(UpdateError::NotFound { name })) => outcome(
                    OutcomeStatus::NotFound,
                    format!("Plugin \"{name}\" not found."),
                ),
                Ok(Err(UpdateError::RegistryLock { detail })) => outcome(
                    OutcomeStatus::InternalError,
                    format!("Another plugin operation is in progress: {detail}"),
                ),
                Ok(Err(e @ UpdateError::RegistrySave { .. })) => ActionOutcome {
                    // Fetched updates may be live on disk despite the stale registry.
                    requires_reload: true,
                    ..outcome(OutcomeStatus::InternalError, e.to_string())
                },
                Err(e) => outcome(
                    OutcomeStatus::InternalError,
                    format!("Update task failed: {e}"),
                ),
            }
        }
        PluginsAction::Add { path } => edit_path(host, path, true).await,
        PluginsAction::Enable { plugin_id } => set_enabled(host, plugin_id, true).await,
        PluginsAction::Disable { plugin_id } => set_enabled(host, plugin_id, false).await,
        PluginsAction::Remove { path } => edit_path(host, path, false).await,
    }
}
