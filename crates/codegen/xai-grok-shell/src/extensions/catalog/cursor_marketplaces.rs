//! Cursor's local plugin marketplaces (`~/.cursor/plugins/local-marketplaces.json`) as marketplace-modal sources.
//!
//! `cursor-agent` loads the plugins this store marks enabled, so installing from one of these sources is the
//! install that takes effect for a Cursor session. Every other source goes through the grok marketplace actions.

use std::path::{Path, PathBuf};

use xai_grok_agent::plugins::cursor_marketplace::{
    self, CursorMarketplaceError, LocalMarketplaces,
};
use xai_hooks_plugins_types::{
    ActionOutcome, MarketplaceAction, MarketplacePluginEntry, MarketplaceScanResult, OutcomeStatus,
};

use crate::extensions::marketplace::{PluginReloader, run_marketplace_action};

/// `source_kind` of a Cursor local marketplace.
const SOURCE_KIND: &str = "cursor";
/// Prefixes the marketplace name in `source_url_or_path`, so the id never collides with a grok source for the same repo.
const SOURCE_PREFIX: &str = "cursor-marketplace:";

pub(super) async fn list() -> Vec<MarketplaceScanResult> {
    let Some(config_dir) = xai_grok_agent::plugins::cursor::config_dir() else {
        return Vec::new();
    };
    tokio::task::spawn_blocking(move || scan(&config_dir))
        .await
        .unwrap_or_default()
}

fn scan(config_dir: &Path) -> Vec<MarketplaceScanResult> {
    LocalMarketplaces::load(config_dir)
        .marketplaces
        .iter()
        .map(|marketplace| MarketplaceScanResult {
            source_name: marketplace.name.clone(),
            source_kind: SOURCE_KIND.to_owned(),
            source_url_or_path: format!("{SOURCE_PREFIX}{}", marketplace.name),
            plugins: marketplace
                .plugins
                .iter()
                .map(|plugin| MarketplacePluginEntry {
                    name: plugin.name.clone(),
                    version: None,
                    description: plugin.description.clone(),
                    category: None,
                    author: None,
                    tags: Vec::new(),
                    keywords: Vec::new(),
                    domains: Vec::new(),
                    homepage: None,
                    relative_path: plugin.name.clone(),
                    skill_count: plugin.skills.len(),
                    has_hooks: !plugin.hooks.is_empty(),
                    has_agents: !plugin.subagents.is_empty(),
                    has_mcp: !plugin.mcp_servers.is_empty(),
                    install_status: if plugin.enabled {
                        "installed"
                    } else {
                        "not_installed"
                    }
                    .to_owned(),
                    installed_version: None,
                    components: None,
                    remote_url: Some(plugin.source_url(marketplace).to_owned()),
                    remote_ref: plugin.source_ref(marketplace).map(str::to_owned),
                    remote_sha: marketplace.commit_sha.clone(),
                    remote_subdir: plugin.git_path.clone(),
                })
                .collect(),
            error: None,
        })
        .collect()
}

/// Run `action`, sending Cursor sources to Cursor's store and everything else to the grok marketplace actions.
pub(super) async fn run_action(
    reloader: &dyn PluginReloader,
    action: MarketplaceAction,
) -> ActionOutcome {
    let Some(config_dir) = xai_grok_agent::plugins::cursor::config_dir() else {
        return run_marketplace_action(reloader, action).await;
    };
    let Some(op) = CursorOp::from_action(&action) else {
        return match action {
            MarketplaceAction::Refresh {
                source_url_or_path: None,
            } => refresh_all(reloader, action, config_dir).await,
            action => run_marketplace_action(reloader, action).await,
        };
    };
    let done = run_blocking(config_dir, op).await;
    finish(reloader, done).await
}

/// A Cursor-store operation and the marketplace (and plugin) it targets.
#[derive(Debug, Clone, PartialEq, Eq)]
enum CursorOp {
    Install { marketplace: String, plugin: String },
    Update { marketplace: String, plugin: String },
    Uninstall { marketplace: String, plugin: String },
    Refresh { marketplace: String },
    RemoveSource { marketplace: String },
}

impl CursorOp {
    fn from_action(action: &MarketplaceAction) -> Option<Self> {
        let marketplace = |source: &str| source.strip_prefix(SOURCE_PREFIX).map(str::to_owned);
        Some(match action {
            MarketplaceAction::Install {
                source_url_or_path,
                plugin_relative_path,
            } => Self::Install {
                marketplace: marketplace(source_url_or_path)?,
                plugin: plugin_relative_path.clone(),
            },
            MarketplaceAction::Update {
                source_url_or_path,
                plugin_relative_path,
            } => Self::Update {
                marketplace: marketplace(source_url_or_path)?,
                plugin: plugin_relative_path.clone(),
            },
            MarketplaceAction::Uninstall {
                source_url_or_path,
                plugin_relative_path,
            } => Self::Uninstall {
                marketplace: marketplace(source_url_or_path)?,
                plugin: plugin_relative_path.clone(),
            },
            MarketplaceAction::Refresh {
                source_url_or_path: Some(source),
            } => Self::Refresh {
                marketplace: marketplace(source)?,
            },
            MarketplaceAction::RemoveSource { source_url_or_path } => Self::RemoveSource {
                marketplace: marketplace(source_url_or_path)?,
            },
            MarketplaceAction::Refresh {
                source_url_or_path: None,
            }
            | MarketplaceAction::AddSource { .. } => return None,
        })
    }
}

/// What a Cursor-store operation did: the outcome, and whether installed plugin files changed.
struct Done {
    outcome: ActionOutcome,
    plugins_changed: bool,
}

async fn run_blocking(config_dir: PathBuf, op: CursorOp) -> Done {
    tokio::task::spawn_blocking(move || apply(&config_dir, op))
        .await
        .unwrap_or_else(|error| Done {
            outcome: outcome(
                OutcomeStatus::InternalError,
                format!("Cursor marketplace task failed: {error}"),
            ),
            plugins_changed: false,
        })
}

fn apply(config_dir: &Path, op: CursorOp) -> Done {
    let result = match op {
        CursorOp::Install {
            marketplace,
            plugin,
        } => cursor_marketplace::install_plugin(config_dir, &format!("{plugin}@{marketplace}"))
            .map(|installed| {
                format!(
                    "Installed {}@{} into Cursor's plugin store. New Cursor sessions load it.",
                    installed.plugin, installed.marketplace
                )
            }),
        CursorOp::Update {
            marketplace,
            plugin,
        } => cursor_marketplace::update(config_dir, Some(&format!("{plugin}@{marketplace}")))
            .map(|refreshed| refreshed_message(&refreshed)),
        CursorOp::Uninstall {
            marketplace,
            plugin,
        } => cursor_marketplace::uninstall_plugin(config_dir, &format!("{plugin}@{marketplace}"))
            .map(|(marketplace, plugin)| {
                format!("Uninstalled {plugin}@{marketplace} from Cursor's plugin store.")
            }),
        CursorOp::Refresh { marketplace } => {
            cursor_marketplace::update(config_dir, Some(&marketplace))
                .map(|refreshed| refreshed_message(&refreshed))
        }
        CursorOp::RemoveSource { marketplace } => {
            cursor_marketplace::remove_marketplace(config_dir, &marketplace)
                .map(|name| format!("Removed Cursor marketplace {name}."))
        }
    };
    match result {
        Ok(message) => Done {
            outcome: outcome(OutcomeStatus::Success, message),
            plugins_changed: true,
        },
        Err(error) => Done {
            outcome: outcome(error_status(&error), error.to_string()),
            plugins_changed: false,
        },
    }
}

/// Refresh every grok source, then re-index every Cursor marketplace.
async fn refresh_all(
    reloader: &dyn PluginReloader,
    action: MarketplaceAction,
    config_dir: PathBuf,
) -> ActionOutcome {
    let grok = run_marketplace_action(reloader, action).await;
    let has_cursor_sources = {
        let config_dir = config_dir.clone();
        tokio::task::spawn_blocking(move || {
            !LocalMarketplaces::load(&config_dir).marketplaces.is_empty()
        })
        .await
        .unwrap_or(false)
    };
    if !has_cursor_sources {
        return grok;
    }
    let cursor = tokio::task::spawn_blocking(move || {
        cursor_marketplace::update(&config_dir, None).map(|refreshed| refreshed_message(&refreshed))
    })
    .await;
    let cursor_line = match cursor {
        Ok(Ok(message)) => message,
        Ok(Err(error)) => format!("Cursor marketplaces: {error}"),
        Err(error) => format!("Cursor marketplaces: refresh task failed: {error}"),
    };
    ActionOutcome {
        message: format!("{}\n{cursor_line}", grok.message),
        ..grok
    }
}

async fn finish(reloader: &dyn PluginReloader, done: Done) -> ActionOutcome {
    let Done {
        mut outcome,
        plugins_changed,
    } = done;
    if plugins_changed && !reloader.reload_plugins().await {
        outcome.requires_restart = true;
        outcome
            .message
            .push_str(" Restart to refresh the plugin list.");
    }
    outcome
}

fn refreshed_message(refreshed: &[cursor_marketplace::MaterializedPlugin]) -> String {
    if refreshed.is_empty() {
        return "Cursor marketplace re-indexed; no installed plugins to refresh.".to_owned();
    }
    let names: Vec<String> = refreshed
        .iter()
        .map(|p| format!("{}@{}", p.plugin, p.marketplace))
        .collect();
    format!("Refreshed {}.", names.join(", "))
}

fn error_status(error: &CursorMarketplaceError) -> OutcomeStatus {
    match error {
        CursorMarketplaceError::Invalid(_) => OutcomeStatus::NotFound,
        CursorMarketplaceError::Git(_)
        | CursorMarketplaceError::Io { .. }
        | CursorMarketplaceError::Parse { .. } => OutcomeStatus::InternalError,
    }
}

fn outcome(status: OutcomeStatus, message: String) -> ActionOutcome {
    ActionOutcome {
        status,
        message,
        requires_reload: false,
        requires_restart: false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use pretty_assertions::assert_eq;

    fn write_store(config_dir: &Path) {
        let plugins = config_dir.join("plugins");
        std::fs::create_dir_all(&plugins).unwrap();
        std::fs::write(
            plugins.join("local-marketplaces.json"),
            serde_json::json!({
                "version": 1,
                "marketplaces": [{
                    "name": "team",
                    "gitUrl": "https://github.com/acme/plugins",
                    "commitSha": "0123456789abcdef0123456789abcdef01234567",
                    "plugins": [
                        {
                            "name": "lint",
                            "description": "Lint helpers",
                            "gitPath": "plugins/lint",
                            "skills": [{"name": "lint"}],
                            "hooks": [{"event": "stop"}],
                            "enabled": true
                        },
                        { "name": "deploy", "enabled": false }
                    ]
                }]
            })
            .to_string(),
        )
        .unwrap();
    }

    #[test]
    fn local_marketplaces_list_as_cursor_sources() {
        let dir = tempfile::tempdir().unwrap();
        write_store(dir.path());

        let sources = scan(dir.path());

        assert_eq!(1, sources.len());
        let source = &sources[0];
        assert_eq!("team", source.source_name);
        assert_eq!(SOURCE_KIND, source.source_kind);
        assert_eq!("cursor-marketplace:team", source.source_url_or_path);
        let lint = &source.plugins[0];
        assert_eq!(
            ("lint", "installed", 1, true, false),
            (
                lint.relative_path.as_str(),
                lint.install_status.as_str(),
                lint.skill_count,
                lint.has_hooks,
                lint.has_mcp,
            )
        );
        assert_eq!(Some("plugins/lint"), lint.remote_subdir.as_deref());
        assert_eq!(
            Some("https://github.com/acme/plugins"),
            lint.remote_url.as_deref()
        );
        assert_eq!("not_installed", source.plugins[1].install_status);
    }

    #[test]
    fn no_store_lists_no_sources() {
        let dir = tempfile::tempdir().unwrap();
        assert!(scan(dir.path()).is_empty());
    }

    #[test]
    fn only_cursor_sources_become_cursor_ops() {
        let install = MarketplaceAction::Install {
            source_url_or_path: "cursor-marketplace:team".to_owned(),
            plugin_relative_path: "lint".to_owned(),
        };
        assert_eq!(
            Some(CursorOp::Install {
                marketplace: "team".to_owned(),
                plugin: "lint".to_owned(),
            }),
            CursorOp::from_action(&install)
        );
        let grok = MarketplaceAction::Install {
            source_url_or_path: "https://github.com/acme/plugins".to_owned(),
            plugin_relative_path: "lint".to_owned(),
        };
        assert_eq!(None, CursorOp::from_action(&grok));
        assert_eq!(
            None,
            CursorOp::from_action(&MarketplaceAction::AddSource {
                url: "https://github.com/acme/plugins".to_owned(),
            })
        );
        assert_eq!(
            None,
            CursorOp::from_action(&MarketplaceAction::Refresh {
                source_url_or_path: None,
            })
        );
        assert_eq!(
            Some(CursorOp::RemoveSource {
                marketplace: "team".to_owned(),
            }),
            CursorOp::from_action(&MarketplaceAction::RemoveSource {
                source_url_or_path: "cursor-marketplace:team".to_owned(),
            })
        );
    }

    #[test]
    fn uninstall_and_remove_source_edit_cursors_store() {
        let dir = tempfile::tempdir().unwrap();
        write_store(dir.path());

        let uninstalled = apply(
            dir.path(),
            CursorOp::Uninstall {
                marketplace: "team".to_owned(),
                plugin: "lint".to_owned(),
            },
        );
        assert_eq!(OutcomeStatus::Success, uninstalled.outcome.status);
        assert!(uninstalled.plugins_changed);
        assert_eq!(
            "not_installed",
            scan(dir.path())[0].plugins[0].install_status
        );

        let removed = apply(
            dir.path(),
            CursorOp::RemoveSource {
                marketplace: "team".to_owned(),
            },
        );
        assert_eq!(OutcomeStatus::Success, removed.outcome.status);
        assert!(scan(dir.path()).is_empty());
    }

    #[test]
    fn an_unknown_plugin_is_not_found() {
        let dir = tempfile::tempdir().unwrap();
        write_store(dir.path());

        let missing = apply(
            dir.path(),
            CursorOp::Uninstall {
                marketplace: "team".to_owned(),
                plugin: "nope".to_owned(),
            },
        );
        assert_eq!(OutcomeStatus::NotFound, missing.outcome.status);
        assert!(!missing.plugins_changed);
    }
}
