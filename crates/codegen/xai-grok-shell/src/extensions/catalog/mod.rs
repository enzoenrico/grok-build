//! The pager's catalog methods (skills, workflows, hooks, plugins, marketplace, prompt history) answered for a working
//! directory instead of a live grok session, for ACP backends with no grok session behind them (`--backend cursor`).
//!
//! Every method runs the loader or action the session path runs; only the host differs: the registries are
//! rebuilt from disk for `cwd` on each request instead of being read from a session actor.

mod cursor_marketplaces;

use std::path::{Path, PathBuf};
use std::sync::Arc;

use agent_client_protocol as acp;
use xai_grok_agent::plugins::{PluginRegistry, SharedPluginRegistryHandle};
use xai_grok_agent::prompt::skills::SkillInfo;
use xai_grok_hooks::discovery::HookRegistry;
use xai_grok_tools::types::compat::CompatConfig;
use xai_hooks_plugins_types::{
    HooksActionRequest, HooksListResponse, MarketplaceActionRequest, PluginsActionRequest,
    PluginsListResponse,
};

use super::hook_actions::{HooksActionHost, run_hooks_action};
use super::marketplace::PluginReloader;
use super::plugin_actions::{PluginsActionHost, run_plugins_action};
use super::{ExtResult, parse_params_str, to_ext_response};
use crate::agent::MvpAgent;

/// Methods [`handle`] answers, without the ACP `_` extension prefix.
pub const METHODS: &[&str] = &[
    "x.ai/skills/list",
    "x.ai/skills/toggle",
    "x.ai/skills/add",
    "x.ai/skills/remove",
    "x.ai/skills/reset",
    "x.ai/skills/config",
    "x.ai/skills/refresh-baseline",
    "x.ai/workflows/list",
    "x.ai/hooks/list",
    "x.ai/hooks/action",
    "x.ai/plugins/list",
    "x.ai/plugins/action",
    "x.ai/plugins/reload",
    "x.ai/plugins/notify-updates",
    "x.ai/marketplace/list",
    "x.ai/marketplace/action",
    "x.ai/prompt_history",
];

/// The method name without the ACP `_` extension prefix, when [`handle`] answers it.
pub fn catalog_method(method: &str) -> Option<&'static str> {
    let bare = method.strip_prefix('_').unwrap_or(method);
    METHODS.iter().copied().find(|m| *m == bare)
}

/// Answer one catalog method for `cwd`. `agent_skills` are skills the backing agent advertises itself; the skills
/// methods list and toggle them alongside the discovered ones, deduplicated by name.
pub async fn handle(
    method: &str,
    params: &serde_json::Value,
    cwd: &Path,
    agent_skills: &[SkillInfo],
) -> ExtResult {
    let Some(method) = catalog_method(method) else {
        return Err(acp::Error::method_not_found());
    };
    if method == "x.ai/prompt_history" {
        let args =
            acp::ExtRequest::new(method, Arc::from(serde_json::value::to_raw_value(params)?));
        return super::prompt_history::handle_prompt_history(&args).await;
    }
    let host = CwdHost::load(cwd).await;
    let raw = params.to_string();
    match method {
        "x.ai/skills/refresh-baseline" | "x.ai/plugins/notify-updates" => {
            to_ext_response(Ok(serde_json::json!({ "ok": true })))
        }
        "x.ai/workflows/list" => {
            let workflows = if host.workflows_enabled {
                crate::session::workflow::registry::list_workflows(Some(cwd))
            } else {
                Vec::new()
            };
            to_ext_response(Ok(serde_json::json!({ "workflows": workflows })))
        }
        "x.ai/hooks/list" => to_ext_response(Ok(host.hooks_list().await)),
        "x.ai/hooks/action" => {
            let req: HooksActionRequest = parse_params_str(&raw)?;
            to_ext_response(Ok(run_hooks_action(&host, req.action).await))
        }
        "x.ai/plugins/list" => {
            let plugins = host
                .plugin_registry()
                .map(|registry| {
                    registry
                        .list()
                        .iter()
                        .map(|p| super::plugins::loaded_plugin_to_info(p))
                        .collect()
                })
                .unwrap_or_default();
            to_ext_response(Ok(PluginsListResponse { plugins }))
        }
        "x.ai/plugins/action" => {
            let req: PluginsActionRequest = parse_params_str(&raw)?;
            to_ext_response(Ok(run_plugins_action(&host, req.action).await))
        }
        "x.ai/plugins/reload" => {
            PluginsActionHost::reload_plugins(&host, true).await;
            to_ext_response(Ok(serde_json::json!({ "ok": true })))
        }
        "x.ai/marketplace/list" => {
            let mut listing = super::marketplace::list_marketplaces().await;
            listing.sources.extend(cursor_marketplaces::list().await);
            to_ext_response(Ok(listing))
        }
        "x.ai/marketplace/action" => {
            let req: MarketplaceActionRequest = parse_params_str(&raw)?;
            to_ext_response(Ok(cursor_marketplaces::run_action(&host, req.action).await))
        }
        skills => {
            let params = with_absolute_cwd(params, cwd);
            let args =
                acp::ExtRequest::new(skills, Arc::from(serde_json::value::to_raw_value(&params)?));
            super::skills::handle_for_cwd(
                &args,
                host.plugin_registry().as_deref(),
                host.compat,
                agent_skills,
            )
            .await
        }
    }
}

/// Skills requests name their discovery `cwd` relative to the client (the pager sends `"."`); anchor it to `cwd`.
fn with_absolute_cwd(params: &serde_json::Value, cwd: &Path) -> serde_json::Value {
    let mut params = match params {
        serde_json::Value::Object(map) => map.clone(),
        _ => serde_json::Map::new(),
    };
    let anchored = match params.get("cwd").and_then(serde_json::Value::as_str) {
        Some(requested) if Path::new(requested).is_absolute() => PathBuf::from(requested),
        Some(requested) => cwd.join(requested),
        None => cwd.to_path_buf(),
    };
    let anchored = dunce::canonicalize(&anchored).unwrap_or(anchored);
    params.insert(
        "cwd".to_owned(),
        serde_json::Value::String(anchored.to_string_lossy().into_owned()),
    );
    serde_json::Value::Object(params)
}

/// Append a prompt the user sent to another backend's session to `cwd`'s prompt history, the file
/// `x.ai/prompt_history` reads, so up-arrow and Ctrl+R recall it the way they recall grok prompts.
/// Synthetic prompts (by `prompt_id` prefix) are skipped, as the grok session skips them.
pub fn record_prompt(cwd: &str, session_id: &str, prompt_id: Option<&str>, prompt: &str) {
    let human = prompt_id.is_none_or(|id| {
        crate::session::PromptOrigin::from_prompt_id(id)
            .policy()
            .analytics
            .is_human_prompt()
    });
    if !human || prompt.trim().is_empty() {
        return;
    }
    let entry = crate::session::prompt_history::PromptEntry {
        timestamp: chrono::Utc::now(),
        session_id: session_id.to_owned(),
        prompt: prompt.to_owned(),
        is_bash: false,
    };
    if let Err(error) = crate::session::prompt_history::append_prompt(cwd, &entry) {
        tracing::warn!(%error, "catalog: could not record the prompt in prompt history");
    }
}

/// `[skills].disabled` from the effective config: the skills a backend's slash menu should hide.
pub fn disabled_skills() -> std::collections::HashSet<String> {
    crate::config::load_agent_config_disk_only()
        .map(|cfg| cfg.skills.disabled.into_iter().collect())
        .unwrap_or_default()
}

/// A working directory as the host of the hooks/plugins/marketplace actions: the plugin registry is built for it
/// the way a session spawned there builds it, and every action re-reads disk instead of patching session state.
struct CwdHost {
    cwd: PathBuf,
    cwd_str: String,
    compat: CompatConfig,
    workflows_enabled: bool,
    plugins: SharedPluginRegistryHandle,
}

impl CwdHost {
    async fn load(cwd: &Path) -> Self {
        let cfg = crate::config::load_agent_config_disk_only().unwrap_or_else(|error| {
            tracing::warn!(%error, "catalog: config did not load; using defaults");
            crate::agent::config::Config::default()
        });
        let compat = xai_grok_config::compat::resolve_compat_config(
            &cfg.compat,
            &xai_grok_config::compat::CompatEnv::from_process(),
            cfg.remote_settings.as_ref(),
        );
        let plugins = SharedPluginRegistryHandle::new(None, Vec::new());
        let build_cwd = cwd.to_path_buf();
        let build_handle = plugins.clone();
        let registry = tokio::task::spawn_blocking(move || {
            let (trusted, disk_config) = MvpAgent::registry_build_inputs(&build_cwd, None);
            build_handle.build_for_cwd(&build_cwd, &disk_config, &[], trusted)
        })
        .await
        .inspect_err(|error| tracing::warn!(%error, "catalog: plugin registry build failed"))
        .ok()
        .flatten();
        plugins.publish(registry);
        Self {
            cwd: cwd.to_path_buf(),
            cwd_str: cwd.to_string_lossy().into_owned(),
            compat,
            workflows_enabled: cfg.resolve_workflows().value,
            plugins,
        }
    }

    fn plugin_registry(&self) -> Option<Arc<PluginRegistry>> {
        self.plugins.snapshot()
    }

    async fn hooks(&self) -> crate::util::hooks::CwdHooks {
        let cwd = self.cwd.clone();
        let compat = self.compat;
        let registry = self.plugin_registry();
        tokio::task::spawn_blocking(move || {
            crate::util::hooks::hooks_for_cwd(&cwd, &compat, registry.as_deref())
        })
        .await
        .unwrap_or_else(|error| {
            tracing::warn!(%error, "catalog: hook discovery failed");
            crate::util::hooks::CwdHooks {
                registry: None,
                disabled: Default::default(),
                load_errors: vec![format!("hook discovery failed: {error}")],
                project_trusted: false,
            }
        })
    }

    async fn hooks_list(&self) -> HooksListResponse {
        let hooks = self.hooks().await;
        HooksListResponse {
            hooks: super::hooks::current_hook_infos(hooks.registry.as_deref()),
            project_trusted: hooks.project_trusted,
            load_errors: hooks.load_errors,
        }
    }

    fn hook_registry(&self) -> Option<Arc<HookRegistry>> {
        crate::util::hooks::hooks_for_cwd(
            &self.cwd,
            &self.compat,
            self.plugin_registry().as_deref(),
        )
        .registry
    }
}

#[async_trait::async_trait(?Send)]
impl HooksActionHost for CwdHost {
    fn cwd(&self) -> &str {
        &self.cwd_str
    }

    fn is_managed_policy_hook(&self, name: &str) -> bool {
        self.hook_registry().is_some_and(|registry| {
            registry
                .find_by_name(name)
                .is_some_and(|spec| spec.is_managed_policy())
        })
    }

    fn managed_only(&self) -> bool {
        crate::util::hooks::session_hook_inputs().1.managed_only()
    }

    async fn reload_hooks(&self) -> String {
        let count = self
            .hooks()
            .await
            .registry
            .map_or(0, |registry| registry.len());
        format!("Hooks reloaded: {count} hook(s) loaded.")
    }

    async fn trust_changed(&self) {}

    fn hook_disabled_changed(&self) {}
}

#[async_trait::async_trait(?Send)]
impl PluginsActionHost for CwdHost {
    fn cwd(&self) -> &str {
        &self.cwd_str
    }

    async fn reload_plugins(&self, force: bool) -> Option<String> {
        let cwd = self.cwd.clone();
        let handle = self.plugins.clone();
        let rebuilt = tokio::task::spawn_blocking(move || {
            let (trusted, disk_config) = MvpAgent::registry_build_inputs(&cwd, None);
            handle.reload(Some(&cwd), &disk_config, trusted, force)
        })
        .await;
        Some(match rebuilt {
            Ok(count) => format!("Plugin registry rebuilt: {count} plugin(s)."),
            Err(error) => format!("Plugin registry rebuild failed: {error}"),
        })
    }
}

#[async_trait::async_trait(?Send)]
impl PluginReloader for CwdHost {
    async fn reload_plugins(&self) -> bool {
        PluginsActionHost::reload_plugins(self, false)
            .await
            .is_some()
    }
}

#[cfg(test)]
mod tests;
