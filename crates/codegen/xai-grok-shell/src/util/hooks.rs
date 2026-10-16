use std::path::Path;
use std::sync::Arc;

use xai_grok_hooks::discovery::{DiscoveryOptions, HookRegistry};
use xai_grok_hooks::error::HookError;
use xai_grok_hooks::trust::DisabledHooks;
use xai_grok_hooks::trust::Trust;
use xai_grok_workspace::hook_inputs::ProcessHookInputs;
use xai_grok_workspace::permission::resolution::managed_settings;

pub(crate) fn process_hook_inputs() -> ProcessHookInputs {
    ProcessHookInputs::read(crate::claude_import::import_marker())
}

/// For a session that dispatches the hooks it discovers.
pub(crate) fn session_hook_inputs() -> (ProcessHookInputs, DisabledHooks) {
    ProcessHookInputs::read_with_disabled(managed_settings(), crate::claude_import::import_marker())
}

/// Every session startup and mid-session reload loads hooks through this function.
/// This is the one place that chooses which hook sources to load.
pub(crate) fn discover_hooks(
    inputs: &ProcessHookInputs,
    git_root: Option<&Path>,
    compat: &xai_grok_tools::types::compat::CompatConfig,
    trust: Trust,
) -> (xai_grok_hooks::discovery::HookRegistry, Vec<HookError>) {
    xai_grok_hooks::discovery::assemble_hooks(
        inputs.config_layers(),
        DiscoveryOptions {
            git_root,
            grok_home: inputs.grok_home(),
            home: inputs.home(),
            compat: compat.hooks(),
            claude_import: inputs.claude_import(),
            trust,
        },
    )
}

/// The hooks a session at `cwd` runs, as a mid-session reload rebuilds them.
pub(crate) struct CwdHooks {
    pub registry: Option<Arc<HookRegistry>>,
    pub disabled: DisabledHooks,
    pub load_errors: Vec<String>,
    pub project_trusted: bool,
}

/// Re-discover the hooks for `cwd`: folder trust is reconciled first (so a `/hooks-trust` or `--trust` grant counts),
/// project sources are gated on that verdict, and the plugin registry's hooks are appended.
pub(crate) fn hooks_for_cwd(
    cwd: &Path,
    compat: &xai_grok_tools::types::compat::CompatConfig,
    plugin_registry: Option<&xai_grok_agent::plugins::PluginRegistry>,
) -> CwdHooks {
    let git_root = xai_grok_workspace::session::git::find_git_root_from_path(cwd).ok();
    let project_trusted = crate::agent::folder_trust::resolve_and_record(cwd, None, false);
    let (inputs, disabled) = session_hook_inputs();
    let (registry, errors) = discover_hooks(
        &inputs,
        git_root.as_deref(),
        compat,
        Trust::from_verdict(project_trusted),
    );
    for err in &errors {
        tracing::warn!("hook reload error: {err}");
    }
    let registry = xai_grok_agent::plugins::hooks_adapter::with_plugin_hooks_filtered(
        (!registry.is_empty()).then(|| Arc::new(registry)),
        xai_grok_agent::plugins::hooks_adapter::PluginHookSource::Registry(plugin_registry),
        compat.cursor.hooks,
    );
    CwdHooks {
        registry,
        disabled,
        load_errors: errors.iter().map(ToString::to_string).collect(),
        project_trusted,
    }
}
