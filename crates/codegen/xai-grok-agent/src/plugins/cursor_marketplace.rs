//! Cursor's local plugin marketplaces: `${CURSOR_CONFIG_DIR:-~/.cursor}/plugins/local-marketplaces.json`.
//!
//! The file and layout match what `agent plugin marketplace add|list|remove|update` writes when Cursor's local plugin
//! store is in use, so either tool can manage the same marketplaces:
//! - The file is `{"version":1,"marketplaces":[{name,gitUrl,gitRef,commitSha,plugins:[{name,gitUrl?,gitRef?,gitPath?,
//!   displayName?,description?,logoUrl?,skills,subagents,hooks,rules,mcpServers,commands,variables?,enabled}]}]}`.
//!   A re-index keeps each plugin's `enabled` flag; `enabled` is what "installed" means.
//! - Marketplace repos are cloned to `plugins/marketplaces/<host>/<owner>/<repo>/<commitSha>/`.
//! - An installed plugin is copied (its `gitPath` subtree) to `plugins/cache/<marketplace>/<plugin>/<sha>/` with a
//!   `.cache-complete` marker, the same place Cursor materializes it.
//!
//! Marketplaces attached to a Cursor account or team live on Cursor's servers and need Cursor's login; they are not here.

use std::path::{Component, Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::cursor::{
    CACHE_COMPLETE_MARKER, cache_dir, cache_version_dir, plugins_dir, sanitize_segment,
    sanitize_version,
};
use super::git_install::{clone_repo, is_full_commit_sha, run_git_in_capture};
use super::manifest::{ManifestDialect, ManifestLoadResult, PluginManifest, load_manifest_for};

const STORE_FILE: &str = "local-marketplaces.json";
const STORE_VERSION: u32 = 1;
const MARKETPLACE_MANIFESTS: &[&str] = &[
    ".cursor-plugin/marketplace.json",
    ".claude-plugin/marketplace.json",
];

#[derive(Debug, thiserror::Error)]
pub enum CursorMarketplaceError {
    #[error("{0}")]
    Invalid(String),
    #[error("git: {0}")]
    Git(String),
    #[error("I/O error on {path}: {source}")]
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("failed to parse {path}: {message}")]
    Parse { path: PathBuf, message: String },
}

type Result<T> = std::result::Result<T, CursorMarketplaceError>;

fn io_err(path: &Path) -> impl FnOnce(std::io::Error) -> CursorMarketplaceError + '_ {
    move |source| CursorMarketplaceError::Io {
        path: path.to_path_buf(),
        source,
    }
}

// ── Store ─────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct LocalMarketplaces {
    #[serde(default)]
    pub marketplaces: Vec<LocalMarketplace>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LocalMarketplace {
    pub name: String,
    pub git_url: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub git_ref: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub commit_sha: Option<String>,
    #[serde(default)]
    pub plugins: Vec<LocalMarketplacePlugin>,
    /// Fields a newer Cursor writes are carried through untouched.
    #[serde(flatten)]
    pub extra: serde_json::Map<String, serde_json::Value>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LocalMarketplacePlugin {
    pub name: String,
    /// Set for plugins hosted outside the marketplace repo.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub git_url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub git_ref: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub git_path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub logo_url: Option<String>,
    /// Component summaries, `[{name, description?}]`, as Cursor's indexer records them.
    #[serde(default)]
    pub skills: Vec<serde_json::Value>,
    #[serde(default)]
    pub subagents: Vec<serde_json::Value>,
    #[serde(default)]
    pub hooks: Vec<serde_json::Value>,
    #[serde(default)]
    pub rules: Vec<serde_json::Value>,
    #[serde(default)]
    pub mcp_servers: Vec<serde_json::Value>,
    #[serde(default)]
    pub commands: Vec<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub variables: Option<serde_json::Value>,
    #[serde(default)]
    pub enabled: bool,
    #[serde(flatten)]
    pub extra: serde_json::Map<String, serde_json::Value>,
}

impl LocalMarketplacePlugin {
    fn non_empty(value: &Option<String>) -> Option<&str> {
        value.as_deref().filter(|s| !s.is_empty())
    }

    /// Repo holding this plugin's files: its own `gitUrl`, or the marketplace repo.
    pub fn source_url<'a>(&'a self, marketplace: &'a LocalMarketplace) -> &'a str {
        Self::non_empty(&self.git_url).unwrap_or(&marketplace.git_url)
    }

    /// Ref Cursor pins this plugin to: its own ref, else the indexed marketplace commit, else the marketplace ref.
    pub fn source_ref<'a>(&'a self, marketplace: &'a LocalMarketplace) -> Option<&'a str> {
        Self::non_empty(&self.git_ref)
            .or_else(|| Self::non_empty(&marketplace.commit_sha))
            .or_else(|| Self::non_empty(&marketplace.git_ref))
    }

    fn is_external(&self) -> bool {
        Self::non_empty(&self.git_url).is_some()
    }
}

impl LocalMarketplaces {
    pub fn path(config_dir: &Path) -> PathBuf {
        plugins_dir(config_dir).join(STORE_FILE)
    }

    /// Missing or unreadable files read as empty, like Cursor's own reader.
    pub fn load(config_dir: &Path) -> Self {
        let path = Self::path(config_dir);
        let Ok(text) = std::fs::read_to_string(&path) else {
            return Self::default();
        };
        match serde_json::from_str(&text) {
            Ok(store) => store,
            Err(error) => {
                tracing::warn!(path = %path.display(), %error, "ignoring unreadable Cursor local-marketplaces.json");
                Self::default()
            }
        }
    }

    pub fn save(&self, config_dir: &Path) -> Result<()> {
        let path = Self::path(config_dir);
        let parent = plugins_dir(config_dir);
        std::fs::create_dir_all(&parent).map_err(io_err(&parent))?;
        let body =
            serde_json::json!({ "version": STORE_VERSION, "marketplaces": self.marketplaces });
        let text =
            serde_json::to_string_pretty(&body).map_err(|e| CursorMarketplaceError::Parse {
                path: path.clone(),
                message: e.to_string(),
            })? + "\n";
        let tmp = parent.join(format!(".{STORE_FILE}.{}.tmp", std::process::id()));
        std::fs::write(&tmp, text).map_err(io_err(&tmp))?;
        std::fs::rename(&tmp, &path).map_err(io_err(&path))
    }

    /// Insert or replace by name, keeping each plugin's existing `enabled` flag.
    pub fn upsert(&mut self, mut marketplace: LocalMarketplace) {
        match self
            .marketplaces
            .iter_mut()
            .find(|m| m.name == marketplace.name)
        {
            Some(existing) => {
                for plugin in &mut marketplace.plugins {
                    if let Some(old) = existing.plugins.iter().find(|p| p.name == plugin.name) {
                        plugin.enabled = old.enabled;
                    }
                }
                *existing = marketplace;
            }
            None => self.marketplaces.push(marketplace),
        }
    }

    pub fn set_plugin_enabled(&mut self, marketplace: &str, plugin: &str, enabled: bool) -> bool {
        let Some(entry) = self
            .marketplaces
            .iter_mut()
            .find(|m| m.name == marketplace)
            .and_then(|m| m.plugins.iter_mut().find(|p| p.name == plugin))
        else {
            return false;
        };
        entry.enabled = enabled;
        true
    }

    pub fn remove(&mut self, name: &str) -> bool {
        let before = self.marketplaces.len();
        self.marketplaces.retain(|m| m.name != name);
        self.marketplaces.len() != before
    }

    /// By exact name, or by git URL when `name_or_url` looks like one.
    pub fn find(&self, name_or_url: &str) -> Option<&LocalMarketplace> {
        let wanted = parse_git_url(name_or_url).ok().map(|repo| repo.key());
        self.marketplaces.iter().find(|m| {
            m.name == name_or_url
                || wanted.as_ref().is_some_and(|key| {
                    parse_git_url(&m.git_url).is_ok_and(|repo| &repo.key() == key)
                })
        })
    }

    /// True when the cache dirs belong to a plugin this store lists as not installed.
    pub fn is_known_disabled(&self, marketplace_dir: &str, plugin_dir: &str) -> bool {
        self.marketplaces
            .iter()
            .filter(|m| sanitize_segment(&m.name) == marketplace_dir)
            .flat_map(|m| m.plugins.iter())
            .any(|p| sanitize_segment(&p.name) == plugin_dir && !p.enabled)
    }

    /// `(marketplace, plugin)` pairs named `plugin`, optionally limited to one marketplace.
    fn plugin_matches(&self, plugin: &str, marketplace: Option<&str>) -> Vec<(String, String)> {
        self.marketplaces
            .iter()
            .filter(|m| marketplace.is_none_or(|wanted| m.name == wanted))
            .flat_map(|m| {
                m.plugins
                    .iter()
                    .filter(|p| p.name == plugin)
                    .map(|p| (m.name.clone(), p.name.clone()))
            })
            .collect()
    }

    fn get(
        &self,
        marketplace: &str,
        plugin: &str,
    ) -> Option<(&LocalMarketplace, &LocalMarketplacePlugin)> {
        let m = self.marketplaces.iter().find(|m| m.name == marketplace)?;
        let p = m.plugins.iter().find(|p| p.name == plugin)?;
        Some((m, p))
    }
}

// ── Git URLs ──────────────────────────────────────────────────────────

/// A git repository named by URL, SSH address, or GitHub `owner/repo` shorthand.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitRepoUrl {
    /// URL handed to git.
    pub url: String,
    pub host: String,
    pub owner: String,
    pub repo: String,
}

impl GitRepoUrl {
    fn key(&self) -> (String, String, String) {
        (
            self.host.to_ascii_lowercase(),
            self.owner.to_ascii_lowercase(),
            self.repo.to_ascii_lowercase(),
        )
    }
}

pub fn parse_git_url(input: &str) -> Result<GitRepoUrl> {
    let input = input.trim();
    let invalid = || CursorMarketplaceError::Invalid(format!("not a git repository URL: {input}"));
    let (url, host, path) = if let Some(path) = input.strip_prefix("file://") {
        (input.to_string(), "file".to_string(), path.to_string())
    } else if let Some(rest) = input
        .strip_prefix("https://")
        .or_else(|| input.strip_prefix("http://"))
    {
        let rest = rest.rsplit_once('@').map_or(rest, |(_, after)| after);
        let (host, path) = rest.split_once('/').ok_or_else(invalid)?;
        (input.to_string(), host.to_string(), path.to_string())
    } else if let Some(rest) = input.strip_prefix("ssh://") {
        let rest = rest.split_once('@').map_or(rest, |(_, after)| after);
        let (host, path) = rest.split_once('/').ok_or_else(invalid)?;
        let host = host.split(':').next().unwrap_or(host);
        (input.to_string(), host.to_string(), path.to_string())
    } else if let Some((user_host, path)) = input.split_once(':')
        && let Some((_, host)) = user_host.split_once('@')
        && !path.starts_with("//")
    {
        (input.to_string(), host.to_string(), path.to_string())
    } else if input.matches('/').count() == 1 && !input.starts_with('.') && !input.contains(':') {
        let url = format!("https://github.com/{input}");
        (url, "github.com".to_string(), input.to_string())
    } else {
        return Err(invalid());
    };
    let path = path.trim_end_matches('/');
    let path = path.strip_suffix(".git").unwrap_or(path);
    let mut parts = path.rsplitn(2, '/');
    let repo = parts.next().filter(|s| !s.is_empty()).ok_or_else(invalid)?;
    let owner = parts.next().filter(|s| !s.is_empty()).ok_or_else(invalid)?;
    let owner = if host == "file" {
        owner.rsplit('/').next().unwrap_or(owner)
    } else {
        owner.trim_start_matches('/')
    };
    if owner.is_empty() || host.is_empty() || host.starts_with('-') {
        return Err(invalid());
    }
    Ok(GitRepoUrl {
        url,
        host,
        owner: owner.to_string(),
        repo: repo.to_string(),
    })
}

/// Cursor's rule for clone path segments: version sanitizing, trailing dots and spaces stripped, reserved names escaped.
fn clone_path_segment(segment: &str) -> String {
    let cleaned = sanitize_version(segment)
        .trim_end_matches(['.', ' '])
        .to_string();
    let cleaned = if cleaned.is_empty() {
        "_".to_string()
    } else {
        cleaned
    };
    const RESERVED: &[&str] = &[
        "con", "prn", "aux", "nul", "com1", "com2", "com3", "com4", "com5", "com6", "com7", "com8",
        "com9", "lpt1", "lpt2", "lpt3", "lpt4", "lpt5", "lpt6", "lpt7", "lpt8", "lpt9",
    ];
    let stem = cleaned.split('.').next().unwrap_or("").to_ascii_lowercase();
    if RESERVED.contains(&stem.as_str()) {
        format!("_{cleaned}")
    } else {
        cleaned
    }
}

pub fn marketplace_clone_dir(config_dir: &Path, repo: &GitRepoUrl, sha: &str) -> PathBuf {
    let mut dir = plugins_dir(config_dir).join("marketplaces");
    dir.push(clone_path_segment(&repo.host));
    for segment in repo.owner.split('/') {
        dir.push(clone_path_segment(segment));
    }
    dir.push(clone_path_segment(&repo.repo));
    dir.push(clone_path_segment(sha));
    dir
}

// ── Git operations ────────────────────────────────────────────────────

/// What a remote's `HEAD` and a requested ref resolve to.
struct ResolvedRef {
    sha: String,
    default_branch: Option<String>,
}

fn git_scratch_dir() -> PathBuf {
    std::env::temp_dir()
}

/// Resolves `git_ref` (branch, tag, or full SHA; `None` for the default branch) on `url` without cloning.
fn resolve_remote_ref(url: &str, git_ref: Option<&str>) -> Result<ResolvedRef> {
    if let Some(sha) = git_ref.filter(|r| is_full_commit_sha(r)) {
        return Ok(ResolvedRef {
            sha: sha.to_ascii_lowercase(),
            default_branch: None,
        });
    }
    let scratch = git_scratch_dir();
    let head = run_git_in_capture(&scratch, &["ls-remote", "--symref", "--", url, "HEAD"])
        .map_err(CursorMarketplaceError::Git)?;
    let head = String::from_utf8_lossy(&head.stdout).into_owned();
    let default_branch = head.lines().find_map(|line| {
        line.strip_prefix("ref: refs/heads/")
            .and_then(|rest| rest.split_whitespace().next())
            .map(str::to_string)
    });
    let Some(git_ref) = git_ref else {
        let sha = head
            .lines()
            .filter(|line| !line.starts_with("ref:"))
            .find_map(|line| line.split_whitespace().next())
            .ok_or_else(|| CursorMarketplaceError::Git(format!("{url} has no HEAD")))?;
        return Ok(ResolvedRef {
            sha: sha.to_string(),
            default_branch,
        });
    };
    let listing = run_git_in_capture(&scratch, &["ls-remote", "--", url, git_ref])
        .map_err(CursorMarketplaceError::Git)?;
    let listing = String::from_utf8_lossy(&listing.stdout).into_owned();
    let refs: Vec<(&str, &str)> = listing
        .lines()
        .filter_map(|line| line.split_once('\t'))
        .collect();
    let pick = |name: String| refs.iter().find(|(_, r)| *r == name).map(|(sha, _)| *sha);
    let sha = pick(format!("refs/heads/{git_ref}"))
        .or_else(|| pick(format!("refs/tags/{git_ref}^{{}}")))
        .or_else(|| pick(format!("refs/tags/{git_ref}")))
        .or_else(|| refs.first().map(|(sha, _)| *sha))
        .ok_or_else(|| CursorMarketplaceError::Git(format!("ref {git_ref} not found on {url}")))?;
    Ok(ResolvedRef {
        sha: sha.to_string(),
        default_branch,
    })
}

/// Clones `url` at `sha` into `target` (atomic: a sibling temp dir is renamed into place).
fn clone_at(url: &str, sha: &str, target: &Path) -> Result<()> {
    if target.join(".git").exists() {
        return Ok(());
    }
    let parent = target.parent().ok_or_else(|| {
        CursorMarketplaceError::Invalid(format!("bad clone target {}", target.display()))
    })?;
    std::fs::create_dir_all(parent).map_err(io_err(parent))?;
    let tmp = parent.join(format!(".clone-{}-{sha}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    clone_repo(url, None, Some(sha), &tmp)
        .map_err(|e| CursorMarketplaceError::Git(e.to_string()))?;
    if target.exists() {
        let _ = std::fs::remove_dir_all(target);
    }
    std::fs::rename(&tmp, target).map_err(io_err(target))
}

// ── Marketplace indexing ──────────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct MarketplaceManifest {
    name: String,
    #[serde(default)]
    metadata: Option<MarketplaceMetadata>,
    #[serde(default)]
    plugins: Vec<MarketplaceEntry>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct MarketplaceMetadata {
    #[serde(default)]
    plugin_root: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct MarketplaceEntry {
    name: String,
    #[serde(default)]
    source: Option<serde_json::Value>,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    display_name: Option<String>,
    #[serde(default)]
    logo: Option<String>,
}

/// Where a marketplace entry's files come from.
#[derive(Debug, Clone, PartialEq, Eq)]
enum EntrySource {
    /// A directory inside the marketplace repo.
    Local { git_path: String },
    /// Another repo, optionally a subdirectory of it.
    External {
        git_url: String,
        git_ref: Option<String>,
        git_path: Option<String>,
    },
}

fn normalize_rel_path(path: &str) -> Option<String> {
    let path = Path::new(path.trim());
    let mut parts = Vec::new();
    for component in path.components() {
        match component {
            Component::Normal(part) => parts.push(part.to_str()?.to_string()),
            Component::CurDir => {}
            _ => return None,
        }
    }
    Some(parts.join("/"))
}

fn entry_source(
    source: Option<&serde_json::Value>,
    plugin_root: Option<&str>,
    name: &str,
) -> Option<EntrySource> {
    let join_root = |rel: &str| {
        let rel = normalize_rel_path(rel)?;
        match plugin_root
            .and_then(normalize_rel_path)
            .filter(|r| !r.is_empty())
        {
            Some(root) if !rel.is_empty() => Some(format!("{root}/{rel}")),
            Some(root) => Some(root),
            None => Some(rel),
        }
    };
    let Some(source) = source else {
        return join_root(name).map(|git_path| EntrySource::Local { git_path });
    };
    if let Some(rel) = source.as_str() {
        return join_root(rel).map(|git_path| EntrySource::Local { git_path });
    }
    let field = |key: &str| {
        source
            .get(key)
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
    };
    let git_ref = field("sha").or_else(|| field("ref")).map(str::to_string);
    match field("source").or_else(|| field("type"))? {
        "github" => Some(EntrySource::External {
            git_url: format!("https://github.com/{}", field("repo")?),
            git_ref,
            git_path: None,
        }),
        "url" | "git" => Some(EntrySource::External {
            git_url: field("url")?.to_string(),
            git_ref,
            git_path: field("path")
                .and_then(normalize_rel_path)
                .filter(|p| !p.is_empty()),
        }),
        "git-subdir" => Some(EntrySource::External {
            git_url: field("url")?.to_string(),
            git_ref,
            git_path: Some(normalize_rel_path(field("path")?)?).filter(|p| !p.is_empty()),
        }),
        "local" => join_root(field("path")?).map(|git_path| EntrySource::Local { git_path }),
        _ => None,
    }
}

/// Cursor's plugin name rule: lowercase kebab-case, periods allowed inside.
fn is_valid_cursor_plugin_name(name: &str) -> bool {
    let bytes = name.as_bytes();
    !bytes.is_empty()
        && bytes
            .iter()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || *b == b'-' || *b == b'.')
        && bytes.first().is_some_and(|b| b.is_ascii_alphanumeric())
        && bytes.last().is_some_and(|b| b.is_ascii_alphanumeric())
}

fn named(names: impl IntoIterator<Item = String>) -> Vec<serde_json::Value> {
    let mut names: Vec<String> = names.into_iter().collect();
    names.sort();
    names.dedup();
    names
        .into_iter()
        .map(|name| serde_json::json!({ "name": name }))
        .collect()
}

fn file_stems(dirs: &[PathBuf], extensions: &[&str]) -> Vec<String> {
    dirs.iter()
        .filter_map(|dir| std::fs::read_dir(dir).ok())
        .flatten()
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.path())
        .filter(|path| {
            path.is_file()
                && path
                    .extension()
                    .and_then(|e| e.to_str())
                    .is_some_and(|e| extensions.iter().any(|x| e.eq_ignore_ascii_case(x)))
        })
        .filter_map(|path| path.file_stem()?.to_str().map(str::to_string))
        .collect()
}

fn json_keys(path: Option<PathBuf>, key: &str) -> Vec<String> {
    path.and_then(|p| std::fs::read_to_string(p).ok())
        .and_then(|text| serde_json::from_str::<serde_json::Value>(&text).ok())
        .and_then(|value| {
            value
                .get(key)
                .and_then(|v| v.as_object())
                .map(|o| o.keys().cloned().collect())
        })
        .unwrap_or_default()
}

/// Loads a plugin root in Cursor's dialect, falling back to a manifest-less one named `fallback_name`.
fn cursor_manifest(root: &Path, fallback_name: &str) -> PluginManifest {
    match load_manifest_for(root, ManifestDialect::Cursor) {
        Ok(ManifestLoadResult::Found(manifest)) => *manifest,
        _ => PluginManifest {
            name: fallback_name.to_string(),
            dialect: ManifestDialect::Cursor,
            ..Default::default()
        },
    }
}

/// Fills a plugin entry's component summaries from its files.
fn summarize_components(plugin: &mut LocalMarketplacePlugin, root: &Path) {
    let manifest = cursor_manifest(root, &plugin.name);
    let mut skills: Vec<String> = manifest
        .skill_dirs(root)
        .iter()
        .flat_map(|dir| {
            let own = dir
                .join("SKILL.md")
                .is_file()
                .then(|| dir.file_name().and_then(|n| n.to_str()).map(str::to_string))
                .flatten();
            let children = std::fs::read_dir(dir)
                .into_iter()
                .flatten()
                .filter_map(|entry| entry.ok())
                .map(|entry| entry.path())
                .filter(|path| path.join("SKILL.md").is_file())
                .filter_map(|path| path.file_name()?.to_str().map(str::to_string));
            own.into_iter().chain(children).collect::<Vec<_>>()
        })
        .collect();
    if root.join("SKILL.md").is_file() {
        skills.push(plugin.name.clone());
    }
    plugin.skills = named(skills);
    plugin.subagents = named(file_stems(
        &manifest.agent_dirs(root),
        &["md", "mdc", "markdown"],
    ));
    plugin.commands = named(file_stems(
        &manifest.command_dirs(root),
        ManifestDialect::Cursor.command_extensions(),
    ));
    plugin.rules = named(file_stems(
        &manifest.rule_dirs(root),
        &["md", "mdc", "markdown"],
    ));
    plugin.hooks = named(json_keys(manifest.hooks_path(root), "hooks"));
    plugin.mcp_servers = named(json_keys(manifest.mcp_config_path(root), "mcpServers"));
    if plugin.description.is_none() {
        plugin.description = manifest.description.clone().filter(|d| !d.is_empty());
    }
}

/// Reads a cloned marketplace repo into store entries (all `enabled: false`; [`LocalMarketplaces::upsert`] restores flags).
pub fn index_marketplace_repo(
    clone_root: &Path,
    repo: &GitRepoUrl,
) -> Result<(String, Vec<LocalMarketplacePlugin>)> {
    let manifest_path = MARKETPLACE_MANIFESTS
        .iter()
        .map(|rel| clone_root.join(rel))
        .find(|path| path.is_file());
    let Some(manifest_path) = manifest_path else {
        // A repo without a marketplace manifest is one plugin at its root.
        let manifest = cursor_manifest(clone_root, &repo.repo.to_ascii_lowercase());
        let mut plugin = LocalMarketplacePlugin {
            name: manifest.name.clone(),
            description: manifest.description.clone(),
            ..Default::default()
        };
        summarize_components(&mut plugin, clone_root);
        return Ok((manifest.name, vec![plugin]));
    };
    let text = std::fs::read_to_string(&manifest_path).map_err(io_err(&manifest_path))?;
    let manifest: MarketplaceManifest =
        serde_json::from_str(&text).map_err(|e| CursorMarketplaceError::Parse {
            path: manifest_path.clone(),
            message: e.to_string(),
        })?;
    let plugin_root = manifest
        .metadata
        .as_ref()
        .and_then(|m| m.plugin_root.as_deref());
    let mut plugins = Vec::new();
    for entry in manifest.plugins {
        let name = entry.name.trim().to_ascii_lowercase();
        if !is_valid_cursor_plugin_name(&name) {
            tracing::warn!(plugin = %entry.name, "skipping Cursor marketplace entry with an invalid name");
            continue;
        }
        let Some(source) = entry_source(entry.source.as_ref(), plugin_root, &name) else {
            tracing::warn!(plugin = %name, "skipping Cursor marketplace entry with an unsupported source");
            continue;
        };
        let mut plugin = LocalMarketplacePlugin {
            name,
            display_name: entry.display_name,
            description: entry.description,
            logo_url: entry.logo.filter(|logo| logo.starts_with("http")),
            ..Default::default()
        };
        match source {
            EntrySource::Local { git_path } => {
                let root = clone_root.join(&git_path);
                if !root.is_dir() {
                    tracing::warn!(plugin = %plugin.name, path = %git_path, "skipping Cursor marketplace entry whose directory is missing");
                    continue;
                }
                summarize_components(&mut plugin, &root);
                plugin.git_path = Some(git_path).filter(|p| !p.is_empty());
            }
            EntrySource::External {
                git_url,
                git_ref,
                git_path,
            } => {
                plugin.git_url = Some(git_url);
                plugin.git_ref = git_ref;
                plugin.git_path = git_path;
            }
        }
        plugins.push(plugin);
    }
    Ok((manifest.name, plugins))
}

// ── Commands ──────────────────────────────────────────────────────────

/// Adds (or re-indexes) the marketplace at `url`, cloning it at the resolved commit like `agent plugin marketplace add`.
pub fn add_marketplace(
    config_dir: &Path,
    url: &str,
    git_ref: Option<&str>,
) -> Result<LocalMarketplace> {
    let repo = parse_git_url(url)?;
    let resolved = resolve_remote_ref(&repo.url, git_ref)?;
    let clone_dir = marketplace_clone_dir(config_dir, &repo, &resolved.sha);
    clone_at(&repo.url, &resolved.sha, &clone_dir)?;
    let (name, plugins) = index_marketplace_repo(&clone_dir, &repo)?;
    if plugins.is_empty() {
        return Err(CursorMarketplaceError::Invalid(format!(
            "no installable plugins found in {}",
            repo.url
        )));
    }
    let marketplace = LocalMarketplace {
        name,
        git_url: repo.url.clone(),
        git_ref: git_ref
            .map(str::to_string)
            .or(resolved.default_branch)
            .or_else(|| Some(resolved.sha.clone())),
        commit_sha: Some(resolved.sha),
        plugins,
        extra: Default::default(),
    };
    let mut store = LocalMarketplaces::load(config_dir);
    store.upsert(marketplace.clone());
    store.save(config_dir)?;
    Ok(store
        .find(&marketplace.name)
        .cloned()
        .unwrap_or(marketplace))
}

/// Removes a marketplace by name or URL, along with the cached copies of its plugins.
pub fn remove_marketplace(config_dir: &Path, name_or_url: &str) -> Result<String> {
    let mut store = LocalMarketplaces::load(config_dir);
    let name = store
        .find(name_or_url)
        .map(|m| m.name.clone())
        .ok_or_else(|| no_marketplace(&store, name_or_url))?;
    store.remove(&name);
    store.save(config_dir)?;
    let cached = cache_dir(config_dir).join(sanitize_segment(&name));
    if cached.is_dir() {
        std::fs::remove_dir_all(&cached).map_err(io_err(&cached))?;
    }
    Ok(name)
}

fn no_marketplace(store: &LocalMarketplaces, wanted: &str) -> CursorMarketplaceError {
    let available: Vec<&str> = store.marketplaces.iter().map(|m| m.name.as_str()).collect();
    CursorMarketplaceError::Invalid(if available.is_empty() {
        format!(
            "no Cursor marketplace matches \"{wanted}\" (none added; run `grok plugin cursor marketplace add <git-url>`)"
        )
    } else {
        format!(
            "no Cursor marketplace matches \"{wanted}\". Available: {}",
            available.join(", ")
        )
    })
}

/// A plugin copied into Cursor's cache.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MaterializedPlugin {
    pub marketplace: String,
    pub plugin: String,
    pub root: PathBuf,
}

fn resolve_plugin(store: &LocalMarketplaces, spec: &str) -> Result<(String, String)> {
    let (plugin, marketplace) = match spec.split_once('@') {
        Some((plugin, marketplace)) => (plugin, Some(marketplace)),
        None => (spec, None),
    };
    let matches = store.plugin_matches(plugin, marketplace);
    match matches.as_slice() {
        [one] => Ok(one.clone()),
        [] => Err(CursorMarketplaceError::Invalid(format!(
            "no plugin named \"{plugin}\" in {}",
            marketplace.map_or("any added Cursor marketplace".to_string(), |m| format!(
                "Cursor marketplace \"{m}\""
            ))
        ))),
        many => Err(CursorMarketplaceError::Invalid(format!(
            "\"{plugin}\" is in several marketplaces ({}); use {plugin}@<marketplace>",
            many.iter()
                .map(|(m, _)| m.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        ))),
    }
}

/// Copies `src` into `dst`, skipping `.git`; symlinks are recreated on Unix and skipped elsewhere.
fn copy_tree(src: &Path, dst: &Path) -> Result<()> {
    std::fs::create_dir_all(dst).map_err(io_err(dst))?;
    for entry in std::fs::read_dir(src).map_err(io_err(src))? {
        let entry = entry.map_err(io_err(src))?;
        let name = entry.file_name();
        if name == ".git" {
            continue;
        }
        let from = entry.path();
        let to = dst.join(&name);
        let kind = entry.file_type().map_err(io_err(&from))?;
        if kind.is_symlink() {
            #[cfg(unix)]
            {
                let target = std::fs::read_link(&from).map_err(io_err(&from))?;
                std::os::unix::fs::symlink(target, &to).map_err(io_err(&to))?;
            }
        } else if kind.is_dir() {
            copy_tree(&from, &to)?;
        } else {
            std::fs::copy(&from, &to).map_err(io_err(&from))?;
        }
    }
    Ok(())
}

/// Puts one marketplace plugin into Cursor's cache at its pinned commit and drops older cached versions.
fn materialize(
    config_dir: &Path,
    marketplace: &LocalMarketplace,
    plugin: &LocalMarketplacePlugin,
) -> Result<MaterializedPlugin> {
    let url = plugin.source_url(marketplace).to_string();
    let resolved = resolve_remote_ref(&url, plugin.source_ref(marketplace))?;
    let target = cache_version_dir(config_dir, &marketplace.name, &plugin.name, &resolved.sha);
    let done = MaterializedPlugin {
        marketplace: marketplace.name.clone(),
        plugin: plugin.name.clone(),
        root: target.clone(),
    };
    if !target.join(CACHE_COMPLETE_MARKER).is_file() {
        let repo = parse_git_url(&url)?;
        let source_root = if !plugin.is_external() {
            let dir = marketplace_clone_dir(config_dir, &repo, &resolved.sha);
            clone_at(&url, &resolved.sha, &dir)?;
            dir
        } else {
            let dir =
                cache_dir(config_dir).join(format!(".src-{}-{}", std::process::id(), resolved.sha));
            let _ = std::fs::remove_dir_all(&dir);
            clone_repo(&url, None, Some(&resolved.sha), &dir)
                .map_err(|e| CursorMarketplaceError::Git(e.to_string()))?;
            dir
        };
        let subtree = match plugin.git_path.as_deref().filter(|p| !p.is_empty()) {
            Some(rel) => {
                let rel = normalize_rel_path(rel).ok_or_else(|| {
                    CursorMarketplaceError::Invalid(format!(
                        "plugin path {rel} escapes its repository"
                    ))
                })?;
                source_root.join(rel)
            }
            None => source_root.clone(),
        };
        let staged = target.with_extension(format!("tmp-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&staged);
        let copied = if subtree.is_dir() {
            copy_tree(&subtree, &staged)
        } else {
            Err(CursorMarketplaceError::Invalid(format!(
                "plugin directory {} is missing in {url}",
                plugin.git_path.as_deref().unwrap_or(".")
            )))
        };
        if plugin.is_external() {
            let _ = std::fs::remove_dir_all(&source_root);
        }
        if let Err(error) = copied {
            let _ = std::fs::remove_dir_all(&staged);
            return Err(error);
        }
        let _ = std::fs::remove_dir_all(&target);
        std::fs::rename(&staged, &target).map_err(io_err(&target))?;
        let marker = target.join(CACHE_COMPLETE_MARKER);
        std::fs::write(&marker, "").map_err(io_err(&marker))?;
    }
    prune_other_versions(&target);
    Ok(done)
}

fn prune_other_versions(keep: &Path) {
    let (Some(parent), Some(keep_name)) = (keep.parent(), keep.file_name()) else {
        return;
    };
    let Ok(entries) = std::fs::read_dir(parent) else {
        return;
    };
    for entry in entries.filter_map(|entry| entry.ok()) {
        if entry.file_name() != keep_name && entry.path().is_dir() {
            let _ = std::fs::remove_dir_all(entry.path());
        }
    }
}

/// Installs `plugin` or `plugin@marketplace`: marks it enabled and materializes it in the cache.
pub fn install_plugin(config_dir: &Path, spec: &str) -> Result<MaterializedPlugin> {
    let mut store = LocalMarketplaces::load(config_dir);
    let (marketplace, plugin) = resolve_plugin(&store, spec)?;
    let (m, p) = store.get(&marketplace, &plugin).ok_or_else(|| {
        CursorMarketplaceError::Invalid(format!("{plugin}@{marketplace} vanished"))
    })?;
    let installed = materialize(config_dir, m, p)?;
    store.set_plugin_enabled(&marketplace, &plugin, true);
    store.save(config_dir)?;
    Ok(installed)
}

/// Uninstalls `plugin` or `plugin@marketplace`: marks it not enabled and deletes its cached copies.
pub fn uninstall_plugin(config_dir: &Path, spec: &str) -> Result<(String, String)> {
    let mut store = LocalMarketplaces::load(config_dir);
    let (marketplace, plugin) = resolve_plugin(&store, spec)?;
    store.set_plugin_enabled(&marketplace, &plugin, false);
    store.save(config_dir)?;
    let cached = cache_dir(config_dir)
        .join(sanitize_segment(&marketplace))
        .join(sanitize_segment(&plugin));
    if cached.is_dir() {
        std::fs::remove_dir_all(&cached).map_err(io_err(&cached))?;
    }
    Ok((marketplace, plugin))
}

/// Re-indexes marketplaces from their tracked ref and refreshes installed plugins.
/// `target` names a marketplace, a plugin (`plugin` or `plugin@marketplace`), or everything when `None`.
pub fn update(config_dir: &Path, target: Option<&str>) -> Result<Vec<MaterializedPlugin>> {
    let store = LocalMarketplaces::load(config_dir);
    let (marketplaces, only_plugin): (Vec<LocalMarketplace>, Option<String>) = match target {
        None => (store.marketplaces.clone(), None),
        Some(wanted) => match store.find(wanted) {
            Some(m) => (vec![m.clone()], None),
            None => {
                let (marketplace, plugin) = resolve_plugin(&store, wanted)?;
                let m = store
                    .marketplaces
                    .iter()
                    .find(|m| m.name == marketplace)
                    .cloned()
                    .into_iter()
                    .collect();
                (m, Some(plugin))
            }
        },
    };
    if marketplaces.is_empty() {
        return Err(no_marketplace(&store, target.unwrap_or("")));
    }
    let mut refreshed = Vec::new();
    for marketplace in marketplaces {
        let tracked_ref = marketplace
            .git_ref
            .as_deref()
            .filter(|r| !is_full_commit_sha(r));
        let updated = add_marketplace(config_dir, &marketplace.git_url, tracked_ref)?;
        for plugin in updated.plugins.iter().filter(|p| p.enabled) {
            if only_plugin
                .as_deref()
                .is_some_and(|only| only != plugin.name)
            {
                continue;
            }
            refreshed.push(materialize(config_dir, &updated, plugin)?);
        }
    }
    Ok(refreshed)
}

#[cfg(test)]
#[path = "cursor_marketplace_tests.rs"]
mod tests;
