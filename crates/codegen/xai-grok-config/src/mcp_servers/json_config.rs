use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use agent_client_protocol as acp;
use indexmap::IndexMap;

use crate::compat::CompatConfig;
use crate::mcp_servers::server_list::{DefinitionResolver, McpServerResolution};
use crate::mcp_servers::{
    McpServerSkipReason, McpServerSource, McpServerSources, SkippedMcpServer,
};
use crate::{ClaudeImport, McpConfig, McpOAuthConfigMap, McpPreferencesFile, McpServerConfig};

/// `mcp_json_files` lists the repo root first.
/// The file nearest the cwd wins a name that two files define.
pub fn load_mcp_json_servers(
    mcp_json_files: &[PathBuf],
    claude_import: ClaudeImport,
) -> Vec<acp::McpServer> {
    let preferences = crate::mcp_servers::load_mcp_preferences().file();
    load_acp_servers(&preferences, |resolver| {
        mcp_json_resolutions(mcp_json_files, claude_import, resolver)
    })
}

/// The returned list starts with the file nearest the cwd.
/// `mcp_json_files` lists the repo root first.
pub(crate) fn mcp_json_resolutions(
    mcp_json_files: &[PathBuf],
    claude_import: ClaudeImport,
    resolver: &DefinitionResolver<'_>,
) -> Vec<McpServerResolution> {
    if claude_import == ClaudeImport::Imported {
        return Vec::new();
    }
    mcp_json_files
        .iter()
        .rev()
        .flat_map(|path| {
            mcp_json_file_resolutions(
                path,
                McpServerSource::McpJson { path: path.clone() },
                resolver,
            )
        })
        .collect()
}

pub fn load_mcp_json_servers_as_configs(
    mcp_json_files: &[PathBuf],
    claude_import: ClaudeImport,
) -> IndexMap<String, McpServerConfig> {
    if claude_import == ClaudeImport::Imported {
        return IndexMap::new();
    }
    load_mcp_json_servers_as_configs_unfiltered(mcp_json_files)
}

/// The `/import-claude` scanner calls this to let users re-import servers they skipped before.
pub fn load_mcp_json_servers_as_configs_unfiltered(
    mcp_json_files: &[PathBuf],
) -> IndexMap<String, McpServerConfig> {
    let mut result = IndexMap::new();
    for mcp_path in mcp_json_files.iter().rev() {
        if let Some(config) = read_mcp_json(mcp_path) {
            for (name, cfg) in config.mcp_servers {
                result.entry(name).or_insert(cfg);
            }
        }
    }
    result
}

/// `projects.<cwd>.mcpServers` comes before the top-level `mcpServers`.
pub(crate) fn claude_json_resolutions(
    sources: &McpServerSources,
    resolver: &DefinitionResolver<'_>,
    home: Option<&Path>,
) -> Vec<McpServerResolution> {
    if !sources.compat.claude.mcps || sources.claude_import == ClaudeImport::Imported {
        return Vec::new();
    }
    let Some(home) = home else {
        return Vec::new();
    };
    claude_json_file_resolutions(&home.join(".claude.json"), &sources.cwd, resolver)
}

pub(crate) fn load_claude_json_mcp_servers_for_attribution(
    cwd: &Path,
    home: Option<&Path>,
    preferences: &McpPreferencesFile,
) -> Vec<acp::McpServer> {
    let Some(home) = home else {
        return Vec::new();
    };
    load_claude_json_mcp_servers_from(&home.join(".claude.json"), cwd, preferences)
}

pub fn load_claude_json_mcp_servers_as_configs(
    cwd: &Path,
    compat: &CompatConfig,
    claude_import: ClaudeImport,
) -> IndexMap<String, McpServerConfig> {
    if !compat.claude.mcps || claude_import == ClaudeImport::Imported {
        return IndexMap::new();
    }
    load_claude_json_mcp_servers_as_configs_unfiltered(cwd)
}

/// The `/import-claude` scanner calls this to let users re-import servers they skipped before.
pub fn load_claude_json_mcp_servers_as_configs_unfiltered(
    cwd: &Path,
) -> IndexMap<String, McpServerConfig> {
    let Some(home) = xai_dirs::home_dir() else {
        return IndexMap::new();
    };
    load_claude_json_mcp_servers_from_as_configs(&home.join(".claude.json"), cwd)
}

fn load_claude_json_mcp_servers_from_as_configs(
    claude_json_path: &Path,
    cwd: &Path,
) -> IndexMap<String, McpServerConfig> {
    let content = match std::fs::read_to_string(claude_json_path) {
        Ok(c) => c,
        Err(e) => {
            tracing::debug!(
                path = %claude_json_path.display(),
                error = %e,
                "failed to read ~/.claude.json"
            );
            return IndexMap::new();
        }
    };
    let value: serde_json::Value = match serde_json::from_str(&content) {
        Ok(v) => v,
        Err(e) => {
            tracing::debug!(
                path = %claude_json_path.display(),
                error = %e,
                "failed to parse ~/.claude.json"
            );
            return IndexMap::new();
        }
    };
    let config = claude_json_mcp_from_value(&value);

    let mut result = IndexMap::new();
    let cwd_key = cwd.to_string_lossy();
    if let Some(project) = config.projects.get(cwd_key.as_ref()) {
        for (name, cfg) in &project.mcp_servers {
            result.insert(name.clone(), cfg.clone());
        }
    }
    for (name, cfg) in &config.user_mcp.mcp_servers {
        result.entry(name.clone()).or_insert(cfg.clone());
    }

    tracing::info!(
        project_count = config
            .projects
            .get(cwd_key.as_ref())
            .map_or(0, |p| p.mcp_servers.len()),
        user_level_count = config.user_mcp.mcp_servers.len(),
        total_count = result.len(),
        "MCP servers loaded from ~/.claude.json"
    );

    result
}

fn load_claude_json_mcp_servers_from(
    claude_json_path: &Path,
    cwd: &Path,
    preferences: &McpPreferencesFile,
) -> Vec<acp::McpServer> {
    // One name can be a project URL and a user URL. The vendor block needs both.
    claude_json_file_resolutions(
        claude_json_path,
        cwd,
        &DefinitionResolver::expanding_env(preferences),
    )
    .into_iter()
    .filter_map(McpServerResolution::into_server)
    .map(acp::McpServer::from)
    .collect()
}

fn claude_json_file_resolutions(
    claude_json_path: &Path,
    cwd: &Path,
    resolver: &DefinitionResolver<'_>,
) -> Vec<McpServerResolution> {
    let Ok(content) = std::fs::read_to_string(claude_json_path) else {
        return Vec::new();
    };
    let value: serde_json::Value = match serde_json::from_str(&content) {
        Ok(v) => v,
        Err(e) => {
            tracing::debug!(
                path = %claude_json_path.display(),
                error = %e,
                "failed to parse claude.json"
            );
            return Vec::new();
        }
    };

    let path = claude_json_path.to_path_buf();
    let project = value
        .get("projects")
        .and_then(|projects| projects.get(cwd.to_string_lossy().as_ref()));
    let mut resolutions = Vec::new();
    if let Some(project) = project {
        resolutions.extend(resolve_mcp_json(
            parse_mcp_json_value(project),
            &McpServerSource::ClaudeProject { path: path.clone() },
            resolver,
        ));
    }
    resolutions.extend(resolve_mcp_json(
        parse_mcp_json_value(&value),
        &McpServerSource::ClaudeUser { path },
        resolver,
    ));
    resolutions
}

struct ClaudeJsonMcp {
    user_mcp: McpConfig,
    projects: HashMap<String, McpConfig>,
}

fn claude_json_mcp_from_value(value: &serde_json::Value) -> ClaudeJsonMcp {
    let user_mcp = mcp_config_from_json_value(value);
    let mut projects = HashMap::new();
    if let Some(entries) = value.get("projects").and_then(|v| v.as_object()) {
        for (path, project) in entries {
            projects.insert(path.clone(), mcp_config_from_json_value(project));
        }
    }
    ClaudeJsonMcp { user_mcp, projects }
}

pub(crate) fn load_cursor_mcp_servers(
    cwd: &Path,
    compat: &CompatConfig,
    home: Option<&Path>,
    preferences: &McpPreferencesFile,
) -> Vec<acp::McpServer> {
    load_acp_servers(preferences, |resolver| {
        cursor_resolutions(cwd, compat, resolver, home)
    })
}

/// Nearest project `.cursor/mcp.json` comes first, then the user file, then `plugins/local/*/mcp.json`.
/// Marketplace installs in `plugins/cache` load their MCP servers through the plugin registry instead.
/// Callers keep the server from the first file when two files define the same name.
pub(crate) fn cursor_resolutions(
    cwd: &Path,
    compat: &CompatConfig,
    resolver: &DefinitionResolver<'_>,
    home: Option<&Path>,
) -> Vec<McpServerResolution> {
    if !compat.cursor.mcps {
        return Vec::new();
    }
    let config_dir = cursor_user_config_dir(home);
    cursor_mcp_files(cwd, config_dir.as_deref())
        .into_iter()
        .flat_map(|file| mcp_json_file_resolutions(&file.path, file.source, resolver))
        .collect()
}

pub fn load_cursor_mcp_servers_as_configs(
    cwd: &Path,
    compat: &CompatConfig,
) -> IndexMap<String, McpServerConfig> {
    if !compat.cursor.mcps {
        return IndexMap::new();
    }
    let mut result = IndexMap::new();
    let config_dir = cursor_user_config_dir(xai_dirs::home_dir().as_deref());
    for file in cursor_mcp_files(cwd, config_dir.as_deref()) {
        if let Some(config) = read_mcp_json(&file.path) {
            for (name, cfg) in config.mcp_servers {
                result.entry(name).or_insert(cfg);
            }
        }
    }
    result
}

const MAX_CURSOR_PROJECT_WALK: usize = 32;
const MAX_CURSOR_PLUGINS: usize = 64;

/// `CURSOR_CONFIG_DIR` when set, otherwise `<home>/.cursor`.
pub fn cursor_user_config_dir(home: Option<&Path>) -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os("CURSOR_CONFIG_DIR").filter(|dir| !dir.is_empty()) {
        return Some(PathBuf::from(dir));
    }
    home.map(|home| home.join(".cursor"))
        .or_else(|| xai_dirs::home_dir().map(|home| home.join(".cursor")))
}

/// One Cursor plugin installed under `plugins/cache`, as named by `.cloud-plugin-manifest.json`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CursorInstalledPlugin {
    pub name: String,
    /// Marketplace slug. Absent installs are still usable; callers can label them `cursor`.
    pub marketplace: Option<String>,
    pub root: PathBuf,
    pub description: Option<String>,
    pub skill_count: usize,
    pub command_count: usize,
    pub has_hooks: bool,
    pub has_agents: bool,
    pub has_mcp: bool,
}

/// Plugins recorded in Cursor's cloud plugin manifest whose cache directory exists.
pub fn cursor_installed_plugins(config_dir: &Path) -> Vec<CursorInstalledPlugin> {
    let manifest_path = config_dir
        .join("plugins")
        .join("cache")
        .join(".cloud-plugin-manifest.json");
    let Ok(text) = std::fs::read_to_string(&manifest_path) else {
        return Vec::new();
    };
    let Ok(value) = serde_json::from_str::<serde_json::Value>(&text) else {
        return Vec::new();
    };
    let Some(plugins) = value.get("plugins").and_then(|entry| entry.as_array()) else {
        return Vec::new();
    };
    let mut installed = Vec::new();
    for plugin in plugins.iter().take(MAX_CURSOR_PLUGINS) {
        let Some(installed_plugin) = cursor_installed_plugin(config_dir, plugin) else {
            continue;
        };
        installed.push(installed_plugin);
    }
    installed
}

fn cursor_installed_plugin(
    config_dir: &Path,
    plugin: &serde_json::Value,
) -> Option<CursorInstalledPlugin> {
    let name = plugin.get("name").and_then(|value| value.as_str())?;
    if name.is_empty() || !cursor_path_segment_ok(name) {
        return None;
    }
    let root = cursor_cache_plugin_root(config_dir, plugin)?;
    let capabilities = plugin.get("declaredCapabilityPaths");
    let skill_count = capability_len(capabilities, "skill");
    let command_count = capability_len(capabilities, "command");
    let has_agents = capability_len(capabilities, "subagent") > 0 || root.join("agents").is_dir();
    let has_hooks = root.join("hooks").join("hooks.json").is_file();
    let has_mcp = root.join("mcp.json").is_file() || root.join(".mcp.json").is_file();
    let marketplace = plugin
        .get("marketplaceSlug")
        .and_then(|value| value.as_str())
        .filter(|slug| cursor_path_segment_ok(slug))
        .map(str::to_string);
    Some(CursorInstalledPlugin {
        name: name.to_string(),
        marketplace,
        description: cursor_plugin_description(&root),
        root,
        skill_count,
        command_count,
        has_hooks,
        has_agents,
        has_mcp,
    })
}

fn capability_len(capabilities: Option<&serde_json::Value>, key: &str) -> usize {
    capabilities
        .and_then(|value| value.get(key))
        .and_then(|value| value.as_array())
        .map(|entries| entries.len())
        .unwrap_or(0)
}

fn cursor_plugin_description(root: &Path) -> Option<String> {
    let path = root.join(".cursor-plugin").join("plugin.json");
    let text = std::fs::read_to_string(path).ok()?;
    let value: serde_json::Value = serde_json::from_str(&text).ok()?;
    value
        .get("description")
        .and_then(|entry| entry.as_str())
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .map(str::to_string)
}

fn cursor_path_segment_ok(segment: &str) -> bool {
    !segment.is_empty()
        && !segment.contains("..")
        && !segment.contains('/')
        && !segment.contains('\\')
}

fn cursor_cache_plugin_root(config_dir: &Path, plugin: &serde_json::Value) -> Option<PathBuf> {
    let slug = plugin
        .get("marketplaceSlug")
        .and_then(|value| value.as_str())?;
    let id = plugin.get("pluginId").and_then(|value| value.as_str())?;
    if !cursor_path_segment_ok(slug) || !cursor_path_segment_ok(id) {
        return None;
    }
    let base = config_dir.join("plugins").join("cache").join(slug).join(id);
    let mut refs = Vec::new();
    if let Some(sha) = plugin
        .get("resolvedCommitSha")
        .and_then(|value| value.as_str())
        .filter(|sha| cursor_path_segment_ok(sha))
    {
        refs.push(sha.to_string());
    }
    if let Some(git_ref) = plugin
        .get("gitRef")
        .and_then(|value| value.as_str())
        .filter(|git_ref| cursor_path_segment_ok(git_ref))
    {
        refs.push(git_ref.to_string());
    }
    if let Some(tag) = plugin
        .get("releaseTag")
        .and_then(|value| value.as_str())
        .filter(|tag| cursor_path_segment_ok(tag))
    {
        refs.push(format!("release_{tag}"));
        refs.push(tag.to_string());
    }
    refs.into_iter()
        .map(|git_ref| base.join(git_ref))
        .find(|path| path.is_dir())
}

/// Direct children of `plugins/local`, capped and sorted.
pub fn cursor_local_plugin_dirs(config_dir: &Path) -> Vec<PathBuf> {
    let root = config_dir.join("plugins").join("local");
    let Ok(entries) = std::fs::read_dir(&root) else {
        return Vec::new();
    };
    let mut dirs: Vec<PathBuf> = entries
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.is_dir())
        .collect();
    dirs.sort();
    dirs.truncate(MAX_CURSOR_PLUGINS);
    dirs
}

/// Directories from `cwd` up through the git root or `$HOME`, whichever comes first.
fn cursor_project_dirs(cwd: &Path) -> Vec<PathBuf> {
    let home = xai_dirs::home_dir();
    let mut dirs = Vec::new();
    let mut current = cwd.to_path_buf();
    for _ in 0..MAX_CURSOR_PROJECT_WALK {
        dirs.push(current.clone());
        if current.join(".git").exists() {
            break;
        }
        if home.as_ref().is_some_and(|home| &current == home) {
            break;
        }
        let Some(parent) = current.parent() else {
            break;
        };
        if parent == current {
            break;
        }
        current = parent.to_path_buf();
    }
    dirs
}

struct CursorMcpFile {
    path: PathBuf,
    source: McpServerSource,
}

/// Project files nearest the cwd, then the user `mcp.json`, then each `plugins/local/*/mcp.json`.
fn cursor_mcp_files(cwd: &Path, config_dir: Option<&Path>) -> Vec<CursorMcpFile> {
    let user_mcp = config_dir.map(|dir| dir.join("mcp.json"));
    let mut files = Vec::new();
    for dir in cursor_project_dirs(cwd) {
        let path = dir.join(".cursor").join("mcp.json");
        if user_mcp.as_ref().is_some_and(|user| user == &path) {
            continue;
        }
        if path.is_file() {
            files.push(CursorMcpFile {
                source: McpServerSource::CursorProject { path: path.clone() },
                path,
            });
        }
    }
    let Some(config_dir) = config_dir else {
        return files;
    };
    let user = config_dir.join("mcp.json");
    if user.is_file() {
        files.push(CursorMcpFile {
            source: McpServerSource::CursorUser { path: user.clone() },
            path: user,
        });
    }
    let plugins_root = config_dir.join("plugins").join("local");
    if let Ok(entries) = std::fs::read_dir(&plugins_root) {
        let mut plugin_dirs: Vec<_> = entries.filter_map(Result::ok).collect();
        plugin_dirs.sort_by_key(|entry| entry.file_name());
        for entry in plugin_dirs.into_iter().take(MAX_CURSOR_PLUGINS) {
            if !entry.file_type().is_ok_and(|kind| kind.is_dir()) {
                continue;
            }
            let path = entry.path().join("mcp.json");
            if !path.is_file() {
                continue;
            }
            let plugin_name = entry.file_name().to_string_lossy().into_owned();
            files.push(CursorMcpFile {
                source: McpServerSource::Plugin {
                    plugin_name,
                    path: path.clone(),
                    scope: super::McpServerScope::User,
                },
                path,
            });
        }
    }
    files
}

/// One Cursor MCP server as `grok mcp list` and the `/mcps` modal show it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CursorMcpListing {
    pub name: String,
    /// `stdio`, `http`, or `sse`.
    pub transport: &'static str,
    /// `project`, `user`, or `plugin`.
    pub scope: &'static str,
    pub location: PathBuf,
    /// Command line for stdio, URL for HTTP and SSE.
    pub detail: String,
    pub enabled: bool,
    pub plugin_name: Option<String>,
}

impl CursorMcpListing {
    pub fn is_connector(&self) -> bool {
        matches!(self.transport, "http" | "sse")
    }

    pub fn source_label(&self) -> String {
        if self.is_connector() {
            format!("connector: {}", self.scope)
        } else {
            format!("cursor: {}", self.scope)
        }
    }
}

/// Cursor MCP servers visible from `cwd`, including ones Cursor marks `disabled`.
/// The nearest project file wins a shared name. This reads the files Cursor's CLI reads;
/// it does not apply grok's `compat.cursor.mcps` switch.
pub fn list_cursor_mcp_for_display(cwd: &Path) -> Vec<CursorMcpListing> {
    let config_dir = cursor_user_config_dir(xai_dirs::home_dir().as_deref());
    list_cursor_mcp_in(cwd, config_dir.as_deref())
}

fn list_cursor_mcp_in(cwd: &Path, config_dir: Option<&Path>) -> Vec<CursorMcpListing> {
    let mut seen = HashSet::new();
    let mut listed = Vec::new();
    for file in cursor_mcp_files(cwd, config_dir) {
        let Some(config) = read_mcp_json(&file.path) else {
            continue;
        };
        let (scope, plugin_name) = cursor_listing_scope(&file.source);
        for (name, cfg) in config.mcp_servers {
            if !seen.insert(name.clone()) {
                continue;
            }
            let (transport, detail) = cursor_transport_detail(&cfg);
            listed.push(CursorMcpListing {
                name,
                transport,
                scope,
                location: file.path.clone(),
                detail,
                enabled: cfg.enabled,
                plugin_name: plugin_name.clone(),
            });
        }
    }
    listed
}

fn cursor_listing_scope(source: &McpServerSource) -> (&'static str, Option<String>) {
    match source {
        McpServerSource::CursorProject { .. } => ("project", None),
        McpServerSource::CursorUser { .. } => ("user", None),
        McpServerSource::Plugin { plugin_name, .. } => ("plugin", Some(plugin_name.clone())),
        _ => ("user", None),
    }
}

fn cursor_transport_detail(config: &McpServerConfig) -> (&'static str, String) {
    match config.resolve_transport("") {
        Some(super::McpServerTransport::Stdio { command, args, .. }) => {
            let command = command.display().to_string();
            let detail = if args.is_empty() {
                command
            } else {
                format!("{command} {}", args.join(" "))
            };
            ("stdio", detail)
        }
        Some(super::McpServerTransport::Http { url, .. }) => ("http", url),
        Some(super::McpServerTransport::Sse { url, .. }) => ("sse", url),
        None => ("stdio", String::new()),
    }
}

pub fn load_mcp_json_file(path: &Path) -> Vec<acp::McpServer> {
    let preferences = crate::mcp_servers::load_mcp_preferences().file();
    load_acp_servers(&preferences, |resolver| {
        mcp_json_file_resolutions(
            path,
            McpServerSource::McpJson {
                path: path.to_path_buf(),
            },
            resolver,
        )
    })
}

fn load_acp_servers(
    preferences: &McpPreferencesFile,
    resolve: impl FnOnce(&DefinitionResolver<'_>) -> Vec<McpServerResolution>,
) -> Vec<acp::McpServer> {
    let mut seen_names = HashSet::new();
    resolve(&DefinitionResolver::expanding_env(preferences))
        .into_iter()
        .filter_map(McpServerResolution::into_server)
        .filter(|server| seen_names.insert(server.name.clone()))
        .map(acp::McpServer::from)
        .collect()
}

fn mcp_json_file_resolutions(
    path: &Path,
    source: McpServerSource,
    resolver: &DefinitionResolver<'_>,
) -> Vec<McpServerResolution> {
    if !path.is_file() {
        return Vec::new();
    }
    let Some(value) = read_mcp_json_value(path) else {
        return Vec::new();
    };
    resolve_mcp_json(parse_mcp_json_value(&value), &source, resolver)
}

fn resolve_mcp_json(
    parsed: ParsedMcpJson,
    source: &McpServerSource,
    resolver: &DefinitionResolver<'_>,
) -> Vec<McpServerResolution> {
    let invalid = parsed.invalid.into_iter().map(|(name, reason)| {
        McpServerResolution::Skipped(SkippedMcpServer {
            name,
            source: source.clone(),
            reason: McpServerSkipReason::InvalidEntry { reason },
        })
    });
    let resolved = parsed
        .config
        .mcp_servers
        .iter()
        .map(|(name, config)| resolver.resolve(name, config, source.clone()));
    invalid.chain(resolved).collect()
}

pub fn read_mcp_json(path: &Path) -> Option<McpConfig> {
    Some(mcp_config_from_json_value(&read_mcp_json_value(path)?))
}

fn read_mcp_json_value(path: &Path) -> Option<serde_json::Value> {
    let content = std::fs::read_to_string(path)
        .map_err(|e| {
            tracing::warn!(error = %e, "failed to read MCP JSON");
        })
        .ok()?;
    serde_json::from_str(&content)
        .map_err(|e| {
            tracing::warn!(error = %e, "failed to parse MCP JSON");
        })
        .ok()
}

/// Cursor writes `disabled: true` and no `enabled` field. Grok's schema uses `enabled`.
fn normalize_cursor_disabled(entry: &serde_json::Value) -> serde_json::Value {
    let Some(obj) = entry.as_object() else {
        return entry.clone();
    };
    if obj.contains_key("enabled")
        || obj.get("disabled").and_then(|value| value.as_bool()) != Some(true)
    {
        return entry.clone();
    }
    let mut obj = obj.clone();
    obj.insert("enabled".to_owned(), serde_json::Value::Bool(false));
    serde_json::Value::Object(obj)
}

/// An `mcpServers` entry that fails to deserialize is skipped without dropping the others.
pub fn mcp_config_from_json_value(value: &serde_json::Value) -> McpConfig {
    parse_mcp_json_value(value).config
}

struct ParsedMcpJson {
    config: McpConfig,
    invalid: Vec<(String, String)>,
}

fn parse_mcp_json_value(value: &serde_json::Value) -> ParsedMcpJson {
    let mut mcp_servers = IndexMap::new();
    let mut invalid = Vec::new();
    if let Some(entries) = value.get("mcpServers").and_then(|v| v.as_object()) {
        for (name, entry) in entries {
            match serde_json::from_value::<McpServerConfig>(normalize_cursor_disabled(entry)) {
                Ok(config) => {
                    mcp_servers.insert(name.clone(), config);
                }
                Err(error) => {
                    tracing::warn!(
                        server = %name,
                        error = %error,
                        "skipping invalid MCP server entry in JSON config"
                    );
                    invalid.push((name.clone(), error.to_string()));
                }
            }
        }
    }
    ParsedMcpJson {
        config: McpConfig { mcp_servers },
        invalid,
    }
}

pub fn parse_mcp_config_with_oauth(
    config: &McpConfig,
    source_label: &str,
    sub: &dyn Fn(&str) -> String,
) -> (Vec<acp::McpServer>, McpOAuthConfigMap) {
    let preferences = crate::mcp_servers::load_mcp_preferences().file();
    let mut servers = Vec::new();
    let mut oauth_configs = McpOAuthConfigMap::new();
    for (name, server_config) in &config.mcp_servers {
        let Some(mut server_config) =
            crate::mcp_servers::resolve_mcp_setup(name, server_config, &preferences)
        else {
            continue;
        };
        server_config.expand_strings(sub);
        if let Some(oauth) = server_config.oauth_config() {
            oauth_configs.insert(name.clone(), oauth);
        }
        if let Some(server) = server_config.to_acp_mcp_server(name.clone()) {
            servers.push(server);
        } else {
            tracing::warn!(
                source = source_label,
                server = name,
                "MCP server has no 'command' (stdio) or 'url' (http/sse); skipping"
            );
        }
    }

    if !servers.is_empty() {
        tracing::info!(
            source = source_label,
            count = servers.len(),
            "loaded MCP servers"
        );
    }

    (servers, oauth_configs)
}

/// Write Cursor's `disabled` flag into the `mcp.json` that defines `name`.
/// `Ok(None)` means the name is not a Cursor server. Cursor's CLI reads this flag;
/// grok's `disabled_mcp_servers` list is a separate store.
pub fn set_cursor_mcp_server_disabled(
    cwd: &Path,
    name: &str,
    disabled: bool,
) -> std::io::Result<Option<PathBuf>> {
    let Some(listing) = list_cursor_mcp_for_display(cwd)
        .into_iter()
        .find(|row| row.name == name)
    else {
        return Ok(None);
    };
    if !set_cursor_mcp_disabled_at(&listing.location, name, disabled)? {
        return Ok(None);
    }
    Ok(Some(listing.location))
}

/// Set or clear `disabled` on one server inside `path`. Returns whether the file changed.
pub fn set_cursor_mcp_disabled_at(
    path: &Path,
    name: &str,
    disabled: bool,
) -> std::io::Result<bool> {
    let text = std::fs::read_to_string(path)?;
    let mut value: serde_json::Value = serde_json::from_str(&text).map_err(|error| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("mcp.json at {} is not JSON: {error}", path.display()),
        )
    })?;
    let Some(entry) = value
        .get_mut("mcpServers")
        .and_then(serde_json::Value::as_object_mut)
        .and_then(|servers| servers.get_mut(name))
        .and_then(serde_json::Value::as_object_mut)
    else {
        return Ok(false);
    };
    if disabled {
        let already = entry.get("disabled").and_then(serde_json::Value::as_bool) == Some(true)
            && !entry.contains_key("enabled");
        if already {
            return Ok(false);
        }
        entry.insert("disabled".to_owned(), serde_json::Value::Bool(true));
        entry.remove("enabled");
    } else {
        let already = entry.get("disabled").is_none()
            && entry.get("enabled").and_then(serde_json::Value::as_bool) != Some(false);
        if already {
            return Ok(false);
        }
        entry.remove("disabled");
        if entry.get("enabled").and_then(serde_json::Value::as_bool) == Some(false) {
            entry.remove("enabled");
        }
    }
    let rendered = serde_json::to_string_pretty(&value)
        .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error.to_string()))?;
    std::fs::write(path, rendered + "\n")?;
    Ok(true)
}

/// `ready` when an enabled server looks reachable, `unavailable` otherwise.
/// HTTP and SSE get a short TCP connect. Stdio checks that the command exists.
/// Nothing is spawned: a project server could be untrusted.
pub fn cursor_mcp_connection_status(listing: &CursorMcpListing) -> &'static str {
    if !listing.enabled {
        return "unavailable";
    }
    let reachable = match listing.transport {
        "http" | "sse" => tcp_reachable(&listing.detail),
        "stdio" => stdio_command_exists(&listing.detail),
        _ => false,
    };
    if reachable { "ready" } else { "unavailable" }
}

/// Parallel reachability for a display list. Disabled rows stay `unavailable` and are not probed.
pub fn cursor_mcp_connection_statuses(rows: &[CursorMcpListing]) -> Vec<&'static str> {
    let mut out = vec!["unavailable"; rows.len()];
    std::thread::scope(|scope| {
        let mut handles = Vec::new();
        for (index, row) in rows.iter().enumerate() {
            if !row.enabled {
                continue;
            }
            handles.push(scope.spawn(move || (index, cursor_mcp_connection_status(row))));
        }
        for handle in handles {
            if let Ok((index, status)) = handle.join()
                && let Some(slot) = out.get_mut(index)
            {
                *slot = status;
            }
        }
    });
    out
}

fn tcp_reachable(url: &str) -> bool {
    use std::net::ToSocketAddrs;
    let Some((host, port)) = http_host_port(url) else {
        return false;
    };
    let Ok(addrs) = (host.as_str(), port).to_socket_addrs() else {
        return false;
    };
    let timeout = std::time::Duration::from_millis(300);
    for addr in addrs.take(2) {
        if std::net::TcpStream::connect_timeout(&addr, timeout).is_ok() {
            return true;
        }
    }
    false
}

fn http_host_port(url: &str) -> Option<(String, u16)> {
    let (body, default_port) = url
        .strip_prefix("https://")
        .map(|rest| (rest, 443))
        .or_else(|| url.strip_prefix("http://").map(|rest| (rest, 80)))?;
    let authority = body.split(['/', '?', '#']).next().unwrap_or(body);
    if authority.is_empty() {
        return None;
    }
    if let Some(host) = authority.strip_prefix('[') {
        let (host, rest) = host.split_once(']')?;
        let port = rest
            .strip_prefix(':')
            .and_then(|value| value.parse().ok())
            .unwrap_or(default_port);
        return Some((host.to_owned(), port));
    }
    if let Some((host, port)) = authority.rsplit_once(':')
        && !host.is_empty()
        && let Ok(port) = port.parse::<u16>()
    {
        return Some((host.to_owned(), port));
    }
    Some((authority.to_owned(), default_port))
}

fn stdio_command_exists(detail: &str) -> bool {
    let command = detail.split_whitespace().next().unwrap_or("");
    if command.is_empty() {
        return false;
    }
    let path = Path::new(command);
    if command.contains('/') || command.contains('\\') {
        return path.is_file();
    }
    std::env::var_os("PATH")
        .is_some_and(|paths| std::env::split_paths(&paths).any(|dir| dir.join(command).is_file()))
}

#[cfg(test)]
#[path = "json_config_tests.rs"]
mod tests;
