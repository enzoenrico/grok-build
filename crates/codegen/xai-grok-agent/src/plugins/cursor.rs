//! Cursor's on-disk plugin layout, read so plugins installed in Cursor load in grok unchanged.
//!
//! Everything lives under `${CURSOR_CONFIG_DIR:-~/.cursor}/plugins`:
//! - `cache/<marketplace>/<pluginId>/<version>/` holds one materialized plugin root per version; a `.cache-complete`
//!   marker means the copy finished. Path segments go through [`sanitize_segment`], versions through [`sanitize_version`].
//!   The version is the resolved commit SHA, or `release/<tag>` for release-asset plugins.
//! - `cache/.cloud-plugin-manifest.json` lists the plugins the account has installed (written by Cursor's cloud setup).
//!   When it is present it is authoritative for which cache entries are installed.
//! - `local/<name>/` holds hand-placed plugins.
//! - `local-marketplaces.json` records marketplaces added with `agent plugin marketplace add` and which of their plugins
//!   are installed (`enabled`); see [`super::cursor_marketplace`].

use std::path::{Path, PathBuf};

use serde::Deserialize;

pub const CACHE_COMPLETE_MARKER: &str = ".cache-complete";
const CLOUD_MANIFEST_FILE: &str = ".cloud-plugin-manifest.json";
const MAX_CURSOR_PLUGINS: usize = 256;

/// `CURSOR_CONFIG_DIR` when set and non-empty, otherwise `~/.cursor`.
pub fn config_dir() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os("CURSOR_CONFIG_DIR").filter(|dir| !dir.is_empty()) {
        return Some(PathBuf::from(dir));
    }
    xai_dirs::home_dir().map(|home| home.join(".cursor"))
}

pub fn plugins_dir(config_dir: &Path) -> PathBuf {
    config_dir.join("plugins")
}

pub fn cache_dir(config_dir: &Path) -> PathBuf {
    plugins_dir(config_dir).join("cache")
}

/// Cursor's cache path segment rule for marketplace and plugin ids: anything outside `[A-Za-z0-9_-]` becomes `_`.
pub fn sanitize_segment(segment: &str) -> String {
    segment
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// Cursor's cache version rule: like [`sanitize_segment`] but periods survive (`release/v0.1.4` -> `release_v0.1.4`).
pub fn sanitize_version(version: &str) -> String {
    let mapped: String = version
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' || c == '.' {
                c
            } else {
                '_'
            }
        })
        .collect();
    if mapped.is_empty() || mapped.chars().all(|c| c == '.') {
        "_".to_string()
    } else {
        mapped
    }
}

/// Root of one cached plugin version.
pub fn cache_version_dir(
    config_dir: &Path,
    marketplace: &str,
    plugin_id: &str,
    version: &str,
) -> PathBuf {
    cache_dir(config_dir)
        .join(sanitize_segment(marketplace))
        .join(sanitize_segment(plugin_id))
        .join(sanitize_version(version))
}

/// A plugin installed through Cursor and materialized in its cache.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CursorInstall {
    /// Plugin name recorded by Cursor. `None` for cache entries found by scanning; the manifest name applies then.
    pub name: Option<String>,
    /// Marketplace name as Cursor recorded it (`cursor-public`, or a `local-marketplaces.json` entry name).
    pub marketplace: String,
    pub root: PathBuf,
}

#[derive(Debug, Deserialize)]
struct CloudManifest {
    #[serde(default)]
    plugins: Vec<CloudManifestPlugin>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct CloudManifestPlugin {
    name: Option<String>,
    plugin_id: Option<String>,
    marketplace_slug: Option<String>,
    resolved_commit_sha: Option<String>,
    git_ref: Option<String>,
    release_tag: Option<String>,
}

impl CloudManifestPlugin {
    /// Mirrors Cursor's version key: `release/<tag>` for release assets, otherwise the resolved SHA.
    fn version_candidates(&self) -> Vec<String> {
        let mut versions = Vec::new();
        if let Some(sha) = self
            .resolved_commit_sha
            .as_deref()
            .filter(|s| !s.is_empty())
        {
            versions.push(sha.to_string());
        }
        if let Some(tag) = self.release_tag.as_deref().filter(|s| !s.is_empty()) {
            versions.push(format!("release/{tag}"));
        }
        if let Some(git_ref) = self.git_ref.as_deref().filter(|s| !s.is_empty()) {
            versions.push(git_ref.to_string());
        }
        versions
    }
}

/// Installed Cursor plugins, in a stable order.
///
/// With a cloud manifest, its entries name the installs. Without one (desktop Cursor), every complete cache entry is an
/// install, picking the most recently written version of each plugin. Either way, `local-marketplaces.json` plugins that
/// are not `enabled` are skipped, and enabled ones are added from the cache when the manifest does not list them.
pub fn installed_plugins(config_dir: &Path) -> Vec<CursorInstall> {
    let local = super::cursor_marketplace::LocalMarketplaces::load(config_dir);
    let mut installs = match read_cloud_manifest(config_dir) {
        Some(manifest) => cloud_manifest_installs(config_dir, &manifest),
        None => scan_cache_installs(config_dir, &local),
    };
    for marketplace in &local.marketplaces {
        for plugin in marketplace.plugins.iter().filter(|p| p.enabled) {
            let Some(root) = newest_complete_version(
                &cache_dir(config_dir)
                    .join(sanitize_segment(&marketplace.name))
                    .join(sanitize_segment(&plugin.name)),
            ) else {
                continue;
            };
            if installs.iter().any(|install| install.root == root) {
                continue;
            }
            installs.push(CursorInstall {
                name: Some(plugin.name.clone()),
                marketplace: marketplace.name.clone(),
                root,
            });
        }
    }
    installs.truncate(MAX_CURSOR_PLUGINS);
    installs
}

fn read_cloud_manifest(config_dir: &Path) -> Option<CloudManifest> {
    let text = std::fs::read_to_string(cache_dir(config_dir).join(CLOUD_MANIFEST_FILE)).ok()?;
    match serde_json::from_str(&text) {
        Ok(manifest) => Some(manifest),
        Err(error) => {
            tracing::warn!(%error, "ignoring unreadable Cursor cloud plugin manifest");
            None
        }
    }
}

fn cloud_manifest_installs(config_dir: &Path, manifest: &CloudManifest) -> Vec<CursorInstall> {
    let mut installs = Vec::new();
    for plugin in &manifest.plugins {
        let (Some(marketplace), Some(plugin_id)) = (
            plugin.marketplace_slug.as_deref().filter(|s| !s.is_empty()),
            plugin.plugin_id.as_deref().filter(|s| !s.is_empty()),
        ) else {
            continue;
        };
        let root = plugin
            .version_candidates()
            .iter()
            .map(|version| cache_version_dir(config_dir, marketplace, plugin_id, version))
            .find(|dir| dir.join(CACHE_COMPLETE_MARKER).is_file());
        let Some(root) = root else {
            tracing::debug!(
                marketplace,
                plugin_id,
                "Cursor plugin listed but not cached; skipping"
            );
            continue;
        };
        installs.push(CursorInstall {
            name: plugin.name.clone().filter(|n| !n.is_empty()),
            marketplace: marketplace.to_string(),
            root,
        });
    }
    installs
}

fn scan_cache_installs(
    config_dir: &Path,
    local: &super::cursor_marketplace::LocalMarketplaces,
) -> Vec<CursorInstall> {
    let mut installs = Vec::new();
    for marketplace_dir in sorted_child_dirs(&cache_dir(config_dir)) {
        let Some(marketplace) = dir_name(&marketplace_dir) else {
            continue;
        };
        for plugin_dir in sorted_child_dirs(&marketplace_dir) {
            let Some(plugin_id) = dir_name(&plugin_dir) else {
                continue;
            };
            if local.is_known_disabled(&marketplace, &plugin_id) {
                continue;
            }
            if let Some(root) = newest_complete_version(&plugin_dir) {
                installs.push(CursorInstall {
                    name: None,
                    marketplace: marketplace.clone(),
                    root,
                });
            }
        }
    }
    installs
}

/// The complete version dir with the newest marker, so a half-written upgrade never shadows the working copy.
pub fn newest_complete_version(plugin_dir: &Path) -> Option<PathBuf> {
    sorted_child_dirs(plugin_dir)
        .into_iter()
        .filter_map(|dir| {
            let modified = std::fs::metadata(dir.join(CACHE_COMPLETE_MARKER))
                .and_then(|meta| meta.modified())
                .ok()?;
            Some((modified, dir))
        })
        .max_by(|(a, a_dir), (b, b_dir)| a.cmp(b).then_with(|| a_dir.cmp(b_dir)))
        .map(|(_, dir)| dir)
}

/// Hand-placed plugins in `plugins/local`. Dot-directories are skipped, and like Cursor a symlinked plugin must
/// resolve inside `plugins/local`.
pub fn local_plugin_dirs(config_dir: &Path) -> Vec<PathBuf> {
    let local = plugins_dir(config_dir).join("local");
    let Ok(canonical_local) = dunce::canonicalize(&local) else {
        return Vec::new();
    };
    let mut dirs: Vec<PathBuf> = sorted_child_dirs(&local)
        .into_iter()
        .filter(|dir| {
            if !dir.is_symlink() {
                return true;
            }
            let inside = dunce::canonicalize(dir).is_ok_and(|target| target.starts_with(&canonical_local));
            if !inside {
                tracing::warn!(path = %dir.display(), "Cursor local plugin symlink points outside plugins/local; skipping");
            }
            inside
        })
        .collect();
    dirs.truncate(MAX_CURSOR_PLUGINS);
    dirs
}

fn sorted_child_dirs(dir: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut dirs: Vec<PathBuf> = entries
        .filter_map(Result::ok)
        .filter(|entry| !entry.file_name().to_string_lossy().starts_with('.'))
        .map(|entry| entry.path())
        .filter(|path| path.is_dir())
        .collect();
    dirs.sort();
    dirs
}

fn dir_name(dir: &Path) -> Option<String> {
    dir.file_name()?.to_str().map(str::to_string)
}

#[cfg(test)]
#[path = "cursor_tests.rs"]
mod tests;
