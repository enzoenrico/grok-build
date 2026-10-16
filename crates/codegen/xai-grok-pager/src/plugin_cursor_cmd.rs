//! `grok plugin cursor` CLI subcommand: manage plugins from Cursor marketplaces.
//!
//! Reads and writes Cursor's own plugin store (`${CURSOR_CONFIG_DIR:-~/.cursor}/plugins`), so plugins installed here
//! show up in the Cursor agent CLI and IDE and the reverse. Business logic lives in
//! `xai_grok_agent::plugins::{cursor, cursor_marketplace}`.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use clap::Subcommand;
use serde::Serialize;

use xai_grok_agent::plugins::cursor;
use xai_grok_agent::plugins::cursor_marketplace::{self, LocalMarketplaces};
use xai_grok_agent::plugins::manifest::{
    ManifestDialect, ManifestLoadResult, load_manifest_for, sanitize_cursor_name,
};

#[derive(Debug, clap::Args, Clone)]
pub struct CursorPluginArgs {
    #[command(subcommand)]
    pub command: CursorPluginCommand,
}

#[derive(Debug, Subcommand, Clone)]
pub enum CursorPluginCommand {
    /// List Cursor plugins grok loads (marketplace installs and `~/.cursor/plugins/local`)
    List {
        /// Emit machine-readable JSON output.
        #[arg(long)]
        json: bool,
    },
    /// Install a plugin from an added Cursor marketplace
    Install {
        /// Plugin name, or `plugin@marketplace` when several marketplaces offer it.
        plugin: String,
        /// Trust the plugin immediately (skip confirmation prompt).
        #[arg(long)]
        trust: bool,
    },
    /// Re-index marketplaces and refresh installed plugins
    Update {
        /// Marketplace name, plugin, or `plugin@marketplace`. Omit to update everything.
        target: Option<String>,
    },
    /// Uninstall a Cursor marketplace plugin
    #[command(visible_alias = "rm", visible_alias = "uninstall")]
    Remove {
        /// Plugin name, or `plugin@marketplace`.
        plugin: String,
    },
    /// Manage Cursor marketplaces (`local-marketplaces.json`)
    Marketplace(CursorMarketplaceArgs),
}

#[derive(Debug, clap::Args, Clone)]
pub struct CursorMarketplaceArgs {
    #[command(subcommand)]
    pub command: CursorMarketplaceCommand,
}

#[derive(Debug, Subcommand, Clone)]
pub enum CursorMarketplaceCommand {
    /// List added Cursor marketplaces and the plugins they offer
    List {
        /// Emit machine-readable JSON output.
        #[arg(long)]
        json: bool,
    },
    /// Add a marketplace from a git URL or GitHub `owner/repo`
    Add {
        /// Git URL, `owner/repo`, or `file://` path.
        url: String,
        /// Branch, tag, or commit to index. Defaults to the remote's default branch.
        #[arg(long = "ref")]
        git_ref: Option<String>,
    },
    /// Remove a marketplace and the cached copies of its plugins
    #[command(visible_alias = "rm")]
    Remove {
        /// Marketplace name or git URL.
        marketplace: String,
    },
    /// Re-index marketplace(s) and refresh their installed plugins
    Update {
        /// Marketplace name or git URL. Omit to update all.
        marketplace: Option<String>,
    },
}

#[derive(Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
struct InstalledEntry {
    name: String,
    /// Marketplace name, or `None` for a `plugins/local` plugin.
    marketplace: Option<String>,
    root: PathBuf,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct MarketplaceEntry<'a> {
    name: &'a str,
    git_url: &'a str,
    git_ref: Option<&'a str>,
    commit_sha: Option<&'a str>,
    plugins: Vec<MarketplacePluginEntry<'a>>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct MarketplacePluginEntry<'a> {
    name: &'a str,
    description: Option<&'a str>,
    installed: bool,
}

fn config_dir() -> Result<PathBuf> {
    cursor::config_dir().context("cannot locate Cursor's config dir: set CURSOR_CONFIG_DIR or HOME")
}

pub fn run(args: CursorPluginArgs) -> Result<()> {
    let config = config_dir()?;
    match args.command {
        CursorPluginCommand::List { json } => cmd_list(&config, json),
        CursorPluginCommand::Install { plugin, trust } => cmd_install(&config, &plugin, trust),
        CursorPluginCommand::Update { target } => cmd_update(&config, target.as_deref()),
        CursorPluginCommand::Remove { plugin } => cmd_remove(&config, &plugin),
        CursorPluginCommand::Marketplace(mp) => match mp.command {
            CursorMarketplaceCommand::List { json } => cmd_marketplace_list(&config, json),
            CursorMarketplaceCommand::Add { url, git_ref } => {
                cmd_marketplace_add(&config, &url, git_ref.as_deref())
            }
            CursorMarketplaceCommand::Remove { marketplace } => {
                let name = cursor_marketplace::remove_marketplace(&config, &marketplace)?;
                println!("Removed Cursor marketplace {name}.");
                Ok(())
            }
            CursorMarketplaceCommand::Update { marketplace } => {
                cmd_update(&config, marketplace.as_deref())
            }
        },
    }
}

fn manifest_name(root: &Path) -> Option<String> {
    match load_manifest_for(root, ManifestDialect::Cursor) {
        Ok(ManifestLoadResult::Found(manifest)) => Some(manifest.name.clone()),
        _ => None,
    }
}

fn dir_name(root: &Path) -> String {
    root.file_name()
        .map(|n| sanitize_cursor_name(&n.to_string_lossy()))
        .unwrap_or_default()
}

fn installed_entries(config: &Path) -> Vec<InstalledEntry> {
    let mut entries: Vec<InstalledEntry> = cursor::installed_plugins(config)
        .into_iter()
        .map(|install| InstalledEntry {
            name: install
                .name
                .as_deref()
                .map(sanitize_cursor_name)
                .or_else(|| manifest_name(&install.root))
                .unwrap_or_else(|| dir_name(&install.root)),
            marketplace: Some(install.marketplace),
            root: install.root,
        })
        .collect();
    entries.extend(
        cursor::local_plugin_dirs(config)
            .into_iter()
            .map(|root| InstalledEntry {
                name: manifest_name(&root).unwrap_or_else(|| dir_name(&root)),
                marketplace: None,
                root,
            }),
    );
    entries
}

fn cmd_list(config: &Path, json: bool) -> Result<()> {
    let entries = installed_entries(config);
    if json {
        println!("{}", serde_json::to_string_pretty(&entries)?);
    } else if entries.is_empty() {
        println!(
            "No Cursor plugins found under {}. Add a marketplace with `grok plugin cursor marketplace add <git-url>`.",
            cursor::plugins_dir(config).display()
        );
    } else {
        for entry in &entries {
            let source = entry.marketplace.as_deref().unwrap_or("local");
            println!("  {} ({source}) {}", entry.name, entry.root.display());
        }
    }
    Ok(())
}

fn cmd_install(config: &Path, plugin: &str, trust: bool) -> Result<()> {
    if !trust {
        eprintln!(
            "Installing Cursor plugin {plugin} requires confirmation.\n\
             Plugins can run hooks, MCP servers, and skills on your machine, and Cursor loads it too.\n\
             \n\
             To proceed, re-run with --trust:\n  grok plugin cursor install {plugin} --trust"
        );
        std::process::exit(1);
    }
    let installed = cursor_marketplace::install_plugin(config, plugin)?;
    println!(
        "Installed {}@{} to {}",
        installed.plugin,
        installed.marketplace,
        installed.root.display()
    );
    Ok(())
}

fn cmd_update(config: &Path, target: Option<&str>) -> Result<()> {
    let refreshed = cursor_marketplace::update(config, target)?;
    if refreshed.is_empty() {
        println!("Marketplaces re-indexed; no installed plugins to refresh.");
    }
    for plugin in &refreshed {
        println!(
            "Updated {}@{} ({})",
            plugin.plugin,
            plugin.marketplace,
            plugin.root.display()
        );
    }
    Ok(())
}

fn cmd_remove(config: &Path, plugin: &str) -> Result<()> {
    let (marketplace, plugin) = cursor_marketplace::uninstall_plugin(config, plugin)?;
    println!("Uninstalled {plugin}@{marketplace}.");
    Ok(())
}

fn cmd_marketplace_add(config: &Path, url: &str, git_ref: Option<&str>) -> Result<()> {
    let marketplace = cursor_marketplace::add_marketplace(config, url, git_ref)?;
    println!(
        "Added Cursor marketplace {} with {} plugin(s): {}",
        marketplace.name,
        marketplace.plugins.len(),
        marketplace
            .plugins
            .iter()
            .map(|p| p.name.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    );
    println!("Install one with `grok plugin cursor install <plugin> --trust`.");
    Ok(())
}

fn marketplace_entries(store: &LocalMarketplaces) -> Vec<MarketplaceEntry<'_>> {
    store
        .marketplaces
        .iter()
        .map(|m| MarketplaceEntry {
            name: &m.name,
            git_url: &m.git_url,
            git_ref: m.git_ref.as_deref(),
            commit_sha: m.commit_sha.as_deref(),
            plugins: m
                .plugins
                .iter()
                .map(|p| MarketplacePluginEntry {
                    name: &p.name,
                    description: p.description.as_deref(),
                    installed: p.enabled,
                })
                .collect(),
        })
        .collect()
}

fn cmd_marketplace_list(config: &Path, json: bool) -> Result<()> {
    let store = LocalMarketplaces::load(config);
    let entries = marketplace_entries(&store);
    if json {
        println!("{}", serde_json::to_string_pretty(&entries)?);
        return Ok(());
    }
    if entries.is_empty() {
        println!(
            "No Cursor marketplaces added. Run `grok plugin cursor marketplace add <git-url>`."
        );
        return Ok(());
    }
    for m in &entries {
        println!(
            "{} ({}{})",
            m.name,
            m.git_url,
            m.git_ref.map(|r| format!(" @ {r}")).unwrap_or_default()
        );
        for p in &m.plugins {
            let mark = if p.installed { "[installed] " } else { "" };
            println!(
                "  {mark}{}{}",
                p.name,
                p.description.map(|d| format!(": {d}")).unwrap_or_default()
            );
        }
    }
    Ok(())
}

#[cfg(test)]
#[path = "plugin_cursor_cmd_tests.rs"]
mod tests;
