use super::*;

use crate::extensions::hook_actions::{HooksActionHost, run_hooks_action};
use crate::extensions::plugin_actions::{PluginsActionHost, run_plugins_action};

/// A live session as the host of the hooks/plugins modal actions.
struct SessionActionHost<'a>(&'a Arc<SessionActor>);

#[async_trait::async_trait(?Send)]
impl HooksActionHost for SessionActionHost<'_> {
    fn cwd(&self) -> &str {
        &self.0.session_info.cwd
    }

    fn is_managed_policy_hook(&self, name: &str) -> bool {
        self.0.is_managed_policy_hook(name)
    }

    fn managed_only(&self) -> bool {
        self.0.hook_disabled.borrow().managed_only()
    }

    async fn reload_hooks(&self) -> String {
        self.0.reload_hooks_impl().await
    }

    /// Trust flips the project-config gate: re-seed the repo MCP output cap so it applies (or drops) without waiting for
    /// a config edit. The resolver is trust-gated, so a revoke clears it.
    async fn trust_changed(&self) {
        self.0.reseed_mcp_output_cap().await;
    }

    fn hook_disabled_changed(&self) {
        self.0.refresh_hook_disabled();
    }
}

#[async_trait::async_trait(?Send)]
impl PluginsActionHost for SessionActionHost<'_> {
    fn cwd(&self) -> &str {
        &self.0.session_info.cwd
    }

    async fn reload_plugins(&self, force: bool) -> Option<String> {
        let handle = self.0.plugin_registry_handle.as_ref()?;
        Some(self.0.reload_plugins_impl(handle, force).await)
    }
}

impl SessionActor {
    /// Re-resolve the repo `[mcp] max_output_bytes` cap for this session's cwd and update the toolset's `TruncationCfg` resource to match.
    /// Only that field changes, so any other `TruncationCfg` fields a host seeded are preserved.
    /// `resolve_max_mcp_output_bytes_for_cwd` is trust-gated, so calling this after a trust change keeps the seeded cap matching the gate.
    pub(super) async fn reseed_mcp_output_cap(&self) {
        let resolved = crate::util::config::resolve_max_mcp_output_bytes_for_cwd(
            std::path::Path::new(&self.session_info.cwd),
        );
        let bridge = std::sync::Arc::clone(self.agent.borrow().tool_bridge());
        let toolset = bridge.toolset();
        let mut resources = toolset.resources.lock().await;
        let existing = resources
            .get::<xai_grok_tools::types::resources::TruncationCfg>()
            .map(|c| c.0.clone());
        match (resolved, existing) {
            (resolved, Some(mut cfg)) => {
                if cfg.mcp_max_output_bytes != resolved {
                    cfg.mcp_max_output_bytes = resolved;
                    resources.insert(xai_grok_tools::types::resources::TruncationCfg(cfg));
                }
            }
            (Some(v), None) => {
                resources.insert(xai_grok_tools::types::resources::TruncationCfg(
                    xai_grok_tools::types::context::TruncationConfig {
                        mcp_max_output_bytes: Some(v),
                        ..Default::default()
                    },
                ));
            }
            (None, None) => {}
        }
    }

    /// Whether `name` resolves to a managed-policy (non-disableable) hook in the live registry, keyed on the spec's typed `layer`, never on the name.
    /// Fails open on a missing registry or name: a disable entry that slips through is inert because the dispatcher re-checks provenance at run time.
    /// This modal check is UX; the dispatcher is the enforcement boundary.
    pub(super) fn is_managed_policy_hook(&self, name: &str) -> bool {
        self.hook_registry
            .borrow()
            .as_ref()
            .is_some_and(|registry| {
                registry
                    .find_by_name(name)
                    .is_some_and(|spec| spec.is_managed_policy())
            })
    }

    // ── Hooks/plugins action handlers (pager modal) ──────────────────

    /// Handle a hooks management action from the pager modal.
    pub(super) async fn handle_hooks_action(
        self: &Arc<Self>,
        action: xai_hooks_plugins_types::HooksAction,
    ) -> xai_hooks_plugins_types::ActionOutcome {
        run_hooks_action(&SessionActionHost(self), action).await
    }

    /// Handle a plugins management action from the pager modal.
    pub(super) async fn handle_plugins_action(
        self: &Arc<Self>,
        action: xai_hooks_plugins_types::PluginsAction,
    ) -> xai_hooks_plugins_types::ActionOutcome {
        run_plugins_action(&SessionActionHost(self), action).await
    }

    /// Reload hooks mid-session: re-discovers global and project hooks, re-evaluates project trust, and re-appends plugin-contributed hooks.
    /// `pub(super)` so the `SessionCommand::ReloadHooks` arm in `run_session` (parent module) can call it after an interactive folder-trust grant.
    pub(super) async fn reload_hooks_impl(self: &std::sync::Arc<Self>) -> String {
        // Clone the Arc out of the RefCell so the borrow is dropped immediately.
        let plugin_registry_snapshot = self.plugin_registry.borrow().clone();
        let crate::util::hooks::CwdHooks {
            registry,
            disabled,
            load_errors,
            project_trusted: is_trusted,
        } = crate::util::hooks::hooks_for_cwd(
            std::path::Path::new(&self.session_info.cwd),
            &self.rebuild_spec.compat,
            plugin_registry_snapshot.as_deref(),
        );
        *self.hook_load_errors.borrow_mut() = load_errors;
        let hook_count = registry.as_ref().map_or(0, |registry| registry.len());
        *self.hook_registry.borrow_mut() = registry;
        *self.hook_disabled.borrow_mut() = Arc::new(disabled);
        tracing::info!(hook_count, "hooks reloaded mid-session");

        // Notify pager about hooks change.
        // Extract all RefCell borrows into locals before the .await so no Ref guard is alive across the suspension point
        {
            let hooks = crate::extensions::hooks::current_hook_infos(
                self.hook_registry.borrow().as_deref(),
            );
            let load_errors = self.hook_load_errors.borrow().clone();
            let project_trusted = is_trusted;
            self.send_xai_notification(XaiSessionUpdate::HooksChanged {
                hooks,
                project_trusted,
                load_errors,
            })
            .await;
        }
        format!("Hooks reloaded: {hook_count} hook(s) loaded.")
    }

    /// Shared plugin reload logic used by enable/disable/add/remove and the explicit `/plugins reload` command.
    /// `force` is `true` only for the explicit `/plugins reload`, which forces a full re-copy of local installs.
    /// Incidental toggles pass `false` for the cheap skip-unchanged path.
    pub(super) async fn reload_plugins_impl(
        self: &Arc<Self>,
        handle: &xai_grok_agent::plugins::SharedPluginRegistryHandle,
        force: bool,
    ) -> String {
        let session_cwd = std::path::Path::new(&self.session_info.cwd);

        let sid = self.session_info.id.0.as_ref();
        xai_grok_telemetry::unified_log::info("reload_plugins_impl: start", Some(sid), None);

        // Folder-trust gates repo-local project plugins (hooks/MCP).
        // Resolve and record the verdict for this cwd; the plugins-config read below gates project paths on it.
        // commands/list and the fan-out order these the same way, so no gate read ever precedes the site's own resolve.
        let project_trusted =
            crate::agent::folder_trust::resolve_and_record(session_cwd, None, false);

        let t0 = std::time::Instant::now();
        // Resolve the effective [plugins] config: global, ancestor project configs, and the compat merge
        // Shared with commands/list and the eager fan-out so all paths discover the same plugins for this cwd
        let discovery_config = xai_grok_workspace::plugins::resolve_effective_plugins_config(
            xai_grok_workspace::plugins::PluginConfigInputs {
                effective_config: crate::config::load_effective_config().ok().as_ref(),
                home: xai_dirs::home_dir().as_deref(),
                grok_home: xai_grok_config::user_grok_home().as_deref(),
                cwd: session_cwd,
                trust: xai_grok_hooks::trust::Trust::from_verdict(project_trusted),
                claude_import: crate::claude_import::import_marker(),
            },
        );
        let config_read_ms = t0.elapsed().as_millis();

        let t2 = std::time::Instant::now();
        let count = handle.reload(Some(session_cwd), &discovery_config, project_trusted, force);
        let discover_ms = t2.elapsed().as_millis();

        xai_grok_telemetry::unified_log::info(
            "reload_plugins_impl: discovery done",
            Some(sid),
            Some(serde_json::json!({
                "config_read_ms": config_read_ms as u64,
                "discover_ms": discover_ms as u64,
                "total_ms": t0.elapsed().as_millis() as u64,
                "plugin_count": count,
            })),
        );

        // Adopt plugin hooks, MCP, and plugin-contributed skills into this session.
        // Sessions with `_meta.pluginDirs` rebuild their own view instead; the shared snapshot never carries them
        let session_dirs = self.session_plugin_dirs();
        let new_registry_snapshot = if session_dirs.is_empty() {
            handle.snapshot()
        } else {
            handle.build_for_cwd(
                session_cwd,
                &discovery_config,
                &session_dirs,
                project_trusted,
            )
        };
        let (hooks_reloaded, mcp_changed, skill_count) = self
            .apply_plugin_registry_snapshot(new_registry_snapshot)
            .await;

        let mcp_status = if mcp_changed {
            "MCP refreshed"
        } else {
            "MCP unchanged"
        };
        format!(
            "Plugin registry rebuilt: {count} plugin(s), {hooks_reloaded} hook(s) reloaded, \
             {mcp_status}, {skill_count} skill(s) refreshed."
        )
    }

    /// This session's `_meta.pluginDirs`, recovered from the registry it was built with; empty when the session has none.
    pub(crate) fn session_plugin_dirs(&self) -> Vec<std::path::PathBuf> {
        self.plugin_registry
            .borrow()
            .as_ref()
            .map(|r| r.session_plugin_dirs().to_vec())
            .unwrap_or_default()
    }

    /// Re-merge this session's `_meta.pluginDirs` into a registry rebuilt by a process-wide fan-out (which knows nothing about per-session dirs).
    pub(crate) fn preserve_session_plugin_dirs(
        &self,
        incoming: Option<std::sync::Arc<xai_grok_agent::plugins::PluginRegistry>>,
    ) -> Option<std::sync::Arc<xai_grok_agent::plugins::PluginRegistry>> {
        let dirs = self.session_plugin_dirs();
        if dirs.is_empty() {
            return incoming;
        }
        let Some(handle) = self.plugin_registry_handle.as_ref() else {
            return incoming;
        };
        let session_cwd = std::path::Path::new(&self.session_info.cwd);
        // Reads the stored verdict only; the session's spawn resolve already recorded this cwd with the real remote
        let project_trusted = crate::agent::folder_trust::project_scope_allowed(session_cwd);
        let disk_cfg = xai_grok_workspace::plugins::resolve_effective_plugins_config(
            xai_grok_workspace::plugins::PluginConfigInputs {
                effective_config: crate::config::load_effective_config().ok().as_ref(),
                home: xai_dirs::home_dir().as_deref(),
                grok_home: xai_grok_config::user_grok_home().as_deref(),
                cwd: session_cwd,
                trust: xai_grok_hooks::trust::Trust::from_verdict(project_trusted),
                claude_import: crate::claude_import::import_marker(),
            },
        );
        handle.build_for_cwd(session_cwd, &disk_cfg, &dirs, project_trusted)
    }

    /// Apply a pre-built plugin registry snapshot to this session.
    /// Called by `reload_plugins_impl` in the originating session and by the `ReloadPlugins` command when plugins change in another session.
    /// Returns `(hooks_reloaded, mcp_changed, skill_count)`.
    pub(super) async fn apply_plugin_registry_snapshot(
        self: &Arc<Self>,
        new_registry_snapshot: Option<std::sync::Arc<xai_grok_agent::plugins::PluginRegistry>>,
    ) -> (usize, bool, usize) {
        let sid = self.session_info.id.0.as_ref();
        let session_cwd = std::path::Path::new(&self.session_info.cwd);

        *self.plugin_registry.borrow_mut() = new_registry_snapshot.clone();

        // Reload hooks in the current session
        let t_hooks = std::time::Instant::now();
        let new_specs = new_registry_snapshot
            .as_deref()
            .map(|registry| {
                xai_grok_agent::plugins::hooks_adapter::active_plugin_hook_specs_filtered(
                    registry,
                    self.rebuild_spec.compat.cursor.hooks,
                )
            })
            .unwrap_or_default();
        let hooks_reloaded = new_specs.len();
        let current = self.hook_registry.borrow().clone();
        let base = match current {
            Some(registry) => Some(registry),
            None if new_specs.is_empty() => None,
            None => {
                // No registry yet: bootstrap config-layer and file hooks the way reload_hooks_impl does
                // Starting from empty sources instead would let a plugin-first snapshot drop config hooks
                let git_root =
                    xai_grok_workspace::session::git::find_git_root_from_path(session_cwd).ok();
                let is_trusted =
                    crate::agent::folder_trust::resolve_and_record(session_cwd, None, false);
                let (registry, _errs) = crate::util::hooks::discover_hooks(
                    &crate::util::hooks::process_hook_inputs(),
                    git_root.as_deref(),
                    &self.rebuild_spec.compat,
                    xai_grok_hooks::trust::Trust::from_verdict(is_trusted),
                );
                (!registry.is_empty()).then(|| Arc::new(registry))
            }
        };
        *self.hook_registry.borrow_mut() =
            xai_grok_agent::plugins::hooks_adapter::replace_plugin_hooks(base, new_specs);

        xai_grok_telemetry::unified_log::info(
            "reload_plugins_impl: hooks done",
            Some(sid),
            Some(serde_json::json!({
                "hooks_reload_ms": t_hooks.elapsed().as_millis() as u64,
                "hooks_reloaded": hooks_reloaded,
            })),
        );

        // Always re-merge plugin-contributed MCP servers and apply them via an order-insensitive diff.
        // Unchanged servers stay connected; only added, changed, or removed ones are re-initialized.
        // The order-sensitive `update_configs` would tear everything down instead, because merge order is non-deterministic.
        let t_mcp = std::time::Instant::now();
        let client_seed = self.initial_client_mcp_servers.borrow().clone();
        let new_mcp_servers = crate::session::agent_mcp::rematerialize_with_agent_overlay(
            crate::session::agent_mcp::RematerializeParams {
                initial_client_mcp_servers: client_seed,
                cwd: session_cwd,
                parent_cwd: self.startup_hints.parent_cwd.as_deref(),
                plugin_registry: new_registry_snapshot.as_deref(),
                compat: &self.rebuild_spec.compat,
                definition: self.agent.borrow().definition(),
            },
        );
        let (mcp_change, dispatch_event_tx) = {
            let mut mcp_state = self.mcp_state.lock().await;
            let change = self.update_mcp_configs(&mut mcp_state, new_mcp_servers);
            let tx = mcp_state.client_event_tx();
            (change, tx)
        };
        let mcp_changed = if let Some(change) = mcp_change {
            self.apply_mcp_config_diff(&change.diff, dispatch_event_tx);
            self.start_mcp_servers_after_config_change(change).await;
            true
        } else {
            false
        };

        xai_grok_telemetry::unified_log::info(
            "reload_plugins_impl: MCP done",
            Some(sid),
            Some(serde_json::json!({
                "mcp_merge_ms": t_mcp.elapsed().as_millis() as u64,
                "mcp_changed": mcp_changed,
            })),
        );

        let t_skills = std::time::Instant::now();
        let bridge = self.agent.borrow().tool_bridge().clone();
        let mut plugin_skills =
            xai_grok_agent::prompt::skills::collect_plugin_skills(new_registry_snapshot.as_deref());
        let disabled = crate::util::config::load_config().await.skills.disabled;
        for skill in &mut plugin_skills {
            if disabled.iter().any(|name| name == &skill.name) {
                skill.enabled = false;
            }
        }
        let skill_count = plugin_skills.len();
        bridge.update_plugin_skills(plugin_skills).await;
        match bridge.apply_pending_skill_update().await {
            Some(effects) => self.apply_skill_update_effects(effects).await,
            None => {
                self.send_available_commands_update(AdvertiseTrigger::SkillsReload)
                    .await
            }
        }
        xai_grok_telemetry::unified_log::info(
            "reload_plugins_impl: skills done",
            Some(sid),
            Some(serde_json::json!({
                "skills_ms": t_skills.elapsed().as_millis() as u64,
                "skill_count": skill_count,
            })),
        );

        // Notify pager about registry changes so the modal auto-refreshes.
        // Extract all RefCell borrows into locals before the .await so no Ref guard is alive across the suspension point
        // Otherwise send_xai_notification's Notification hooks, which also borrow these RefCells, panic with BorrowMutError
        let t_notify = std::time::Instant::now();
        {
            let hooks = crate::extensions::hooks::current_hook_infos(
                self.hook_registry.borrow().as_deref(),
            );
            let load_errors = self.hook_load_errors.borrow().clone();
            // Report the folder-trust verdict so the flag matches the gated registry.
            let project_trusted = crate::agent::folder_trust::project_scope_allowed(
                std::path::Path::new(&self.session_info.cwd),
            );
            self.send_xai_notification(XaiSessionUpdate::HooksChanged {
                hooks,
                project_trusted,
                load_errors,
            })
            .await;

            use crate::extensions::plugins::loaded_plugin_to_info;
            let plugins = {
                let reg = self.plugin_registry.borrow();
                match &*reg {
                    Some(registry) => registry
                        .list()
                        .iter()
                        .map(|p| loaded_plugin_to_info(p))
                        .collect(),
                    None => Vec::new(),
                }
            };
            self.send_xai_notification(XaiSessionUpdate::PluginsChanged { plugins })
                .await;
        }

        xai_grok_telemetry::unified_log::info(
            "apply_plugin_registry_snapshot: complete",
            Some(sid),
            Some(serde_json::json!({
                "notify_ms": t_notify.elapsed().as_millis() as u64,
                "hooks_reloaded": hooks_reloaded,
                "mcp_changed": mcp_changed,
                "skill_count": skill_count,
            })),
        );

        (hooks_reloaded, mcp_changed, skill_count)
    }
}
