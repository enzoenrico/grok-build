//! Reads Cursor CLI permission files into Grok rules.
//! `~/.cursor/cli-config.json` and `<project>/.cursor/cli.json` hold `permissions.allow` / `permissions.deny` lists of
//! `Shell(..)`, `Read(..)`, `Write(..)`, `WebFetch(..)`, and `Mcp(server:tool)` entries. Deny beats allow in both engines.
//!
//! A project file loads only in a trusted folder; [`project_cursor_cli_present`] is the folder-trust gate's probe, so a
//! clone shipping one cannot approve its own commands without a trust decision.

use std::path::{Path, PathBuf};

use tracing::{debug, warn};

use crate::resolution::SkippedPermission;
use crate::types::{
    PatternMode, PermissionRule, RequirementSource, RuleAction, Sourced, ToolFilter,
};

/// Rules from `~/.cursor/cli-config.json` and, when `project_trusted`, each `.cursor/cli.json` from `cwd` up to the repo root.
pub(crate) fn load_cursor_cli_permissions(
    cwd: &Path,
    project_trusted: bool,
) -> (Vec<Sourced<PermissionRule>>, Vec<SkippedPermission>) {
    let mut rules = Vec::new();
    let mut skipped = Vec::new();
    let mut files = Vec::new();
    if let Some(home) = xai_dirs::home_dir() {
        files.push(home.join(".cursor").join("cli-config.json"));
    }
    if project_trusted {
        files.extend(project_cli_json_paths(cwd));
    }
    for path in files {
        load_file(&path, &mut rules, &mut skipped);
    }
    (rules, skipped)
}

/// Whether a project `.cursor/cli.json` exists on the walk the loader reads; the folder-trust gate shares it so they cannot drift.
pub fn project_cursor_cli_present(cwd: &Path) -> bool {
    project_cli_json_paths(cwd)
        .iter()
        .any(|p| crate::util::path_present_or_uncertain(p))
}

/// `.cursor/cli.json` in `cwd` and each parent up to the repo root, cwd first. Outside a repo only `cwd` is probed.
fn project_cli_json_paths(cwd: &Path) -> Vec<PathBuf> {
    let repo_root = cwd
        .ancestors()
        .find(|dir| dir.join(".git").exists())
        .filter(|root| !crate::trust::is_home_dir(root))
        .unwrap_or(cwd);
    let mut paths = Vec::new();
    for dir in cwd.ancestors() {
        paths.push(dir.join(".cursor").join("cli.json"));
        if dir == repo_root {
            break;
        }
    }
    paths
}

fn load_file(
    path: &Path,
    rules: &mut Vec<Sourced<PermissionRule>>,
    skipped: &mut Vec<SkippedPermission>,
) {
    // Opening a FIFO blocks until a writer appears, so only regular files are read
    if !std::fs::metadata(path).is_ok_and(|m| m.is_file()) {
        return;
    }
    let Some(value) = std::fs::read_to_string(path)
        .ok()
        .and_then(|content| serde_json::from_str::<serde_json::Value>(&content).ok())
    else {
        warn!(path = %path.display(), "Cursor CLI config is not valid JSON; permissions ignored");
        return;
    };
    let Some(permissions) = value.get("permissions") else {
        return;
    };
    let source = RequirementSource::Settings {
        path: path.to_path_buf(),
    };
    let before = rules.len();
    for (label, action) in [("allow", RuleAction::Allow), ("deny", RuleAction::Deny)] {
        let Some(entries) = permissions.get(label).and_then(|v| v.as_array()) else {
            continue;
        };
        for entry in entries {
            let Some(entry) = entry.as_str() else {
                skipped.push(SkippedPermission {
                    rule: format!("permissions.{label}: {entry}"),
                    reason: "expected a string".to_string(),
                });
                continue;
            };
            match translate(entry, action) {
                Ok(translated) => rules.extend(translated.into_iter().map(|rule| Sourced {
                    value: rule,
                    source: source.clone(),
                })),
                Err(reason) => skipped.push(SkippedPermission {
                    rule: format!("permissions.{label}: {entry}"),
                    reason,
                }),
            }
        }
    }
    debug!(path = %path.display(), rules = rules.len() - before, "Loaded Cursor CLI permission rules");
}

/// One Cursor entry becomes one or two Grok rules (a deny with a globbed command name, an exact-host `WebFetch` allow).
fn translate(entry: &str, action: RuleAction) -> Result<Vec<PermissionRule>, String> {
    let (kind, arg) = entry
        .trim()
        .strip_suffix(')')
        .and_then(|s| s.split_once('('))
        .ok_or_else(|| "expected Kind(argument)".to_string())?;
    let arg = arg.trim();
    if arg.is_empty() {
        return Err("empty argument".to_string());
    }
    let rules = |tool: ToolFilter, patterns: Vec<Option<String>>| {
        patterns
            .into_iter()
            .map(|pattern| PermissionRule {
                action,
                tool: tool.clone(),
                pattern,
                pattern_mode: PatternMode::Glob,
            })
            .collect()
    };
    match kind.trim() {
        "Shell" => Ok(rules(ToolFilter::Bash, shell_patterns(arg, action)?)),
        "Read" => Ok(rules(ToolFilter::Read, vec![path_pattern(arg, action)])),
        "Write" => Ok(rules(ToolFilter::Edit, vec![path_pattern(arg, action)])),
        "Mcp" => Ok(rules(ToolFilter::Mcp, vec![mcp_pattern(arg)?])),
        "WebFetch" => web_fetch_rules(arg, action),
        other => Err(format!("unknown Cursor permission kind: {other}")),
    }
}

/// Cursor matches the command name against the first word; Grok prefix-matches or globs the whole command line.
/// `cmd:args` is Cursor's command-and-arguments form, and a colon after whitespace is part of an argument (`docker run img:tag`).
/// Where Grok's match would be wider than Cursor's, an allow is refused and a deny keeps the wider form.
fn shell_patterns(arg: &str, action: RuleAction) -> Result<Vec<Option<String>>, String> {
    if arg == "*" {
        return Ok(vec![None]);
    }
    let (base, args) = match arg.split_once(':') {
        Some((cmd, args)) if !cmd.is_empty() && !cmd.contains(char::is_whitespace) => {
            (cmd, Some(args.trim()))
        }
        _ => (arg, None),
    };
    let allow = action == RuleAction::Allow;
    let name = base.split_whitespace().next().unwrap_or(base);
    if name.contains(['*', '?', '[']) {
        if allow {
            return Err(
                "a glob in the command name would match the whole command line".to_string(),
            );
        }
        return Ok(match args {
            None | Some("*") => vec![Some(base.to_string()), Some(format!("{base} *"))],
            Some(args) => vec![Some(format!("{base} {args}"))],
        });
    }
    match args {
        None | Some("*") => Ok(vec![Some(base.to_string())]),
        Some("") => Err("empty arguments after ':'".to_string()),
        Some(args) if allow && !args.ends_with('*') => Err(
            "exact arguments would also allow extra trailing arguments; end them with *"
                .to_string(),
        ),
        Some(args) => Ok(vec![Some(format!("{base} {args}"))]),
    }
}

/// Cursor scopes relative paths to the workspace, so a relative allow is rooted at the session directory.
/// A deny stays unrooted, which matches the path at any depth.
fn path_pattern(arg: &str, action: RuleAction) -> Option<String> {
    if action != RuleAction::Allow {
        return (arg != "*").then(|| arg.to_string());
    }
    if arg == "*" {
        return Some("./**".to_string());
    }
    if arg.starts_with(['/', '~', '.']) {
        Some(arg.to_string())
    } else {
        Some(format!("./{arg}"))
    }
}

/// Cursor's `server:tool` maps onto Grok's qualified `<server>__<tool>` MCP names.
fn mcp_pattern(arg: &str) -> Result<Option<String>, String> {
    let (server, tool) = arg
        .split_once(':')
        .map(|(server, tool)| (server.trim(), tool.trim()))
        .filter(|(server, tool)| !server.is_empty() && !tool.is_empty())
        .ok_or_else(|| "expected Mcp(server:tool)".to_string())?;
    Ok((server != "*" || tool != "*").then(|| format!("{server}__{tool}")))
}

/// Cursor: `*` is every host, `*.example.com` its subdomains, and `example.com` that host only.
/// Grok's domain mode also matches subdomains, so it is exact enough for deny (wider is safer) and for `*.` (adds the apex).
/// An exact-host allow uses scheme-anchored URL globs so `example.com` does not allow `evil.example.com`.
fn web_fetch_rules(arg: &str, action: RuleAction) -> Result<Vec<PermissionRule>, String> {
    let rule = |pattern: Option<String>, pattern_mode| PermissionRule {
        action,
        tool: ToolFilter::WebFetch,
        pattern,
        pattern_mode,
    };
    if arg == "*" {
        return Ok(vec![rule(None, PatternMode::Glob)]);
    }
    let (host, subdomains) = match arg.strip_prefix("*.") {
        Some(host) => (host, true),
        None => (arg, false),
    };
    let host = host.to_ascii_lowercase();
    let valid = !host.is_empty()
        && host
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-');
    if !valid {
        return Err("expected *, *.domain, or domain".to_string());
    }
    // Grok normalizes `www.` away, so `*.www.example.com` would allow all of `example.com`
    if action == RuleAction::Allow && host.starts_with("www.") {
        return Err("a www. host would allow the whole parent domain".to_string());
    }
    if subdomains || action != RuleAction::Allow {
        return Ok(vec![rule(Some(host), PatternMode::Domain)]);
    }
    Ok(["https", "http"]
        .into_iter()
        .map(|scheme| rule(Some(format!("{scheme}://{host}/*")), PatternMode::Glob))
        .collect())
}

#[cfg(test)]
#[path = "cursor_cli_tests.rs"]
mod tests;
