//! The pager's catalog methods (skills, workflows, commands, hooks, plugins, marketplace) on the Cursor backend.
//!
//! `cursor-agent` implements none of them, so each is answered locally with the grok loaders and actions
//! ([`xai_grok_shell::extensions::catalog`]) for the session's cwd. The grok loaders already read Cursor's own
//! files (`.cursor/skills`, `hooks.json`, Cursor plugins and local marketplaces). Cursor's `available_commands_update`
//! catalog is the one list only Cursor knows (team and dashboard commands); its skills are merged into the skills
//! list and it stays the slash-command catalog.

use std::collections::HashSet;
use std::path::PathBuf;

use serde_json::{Map, Value, json};
use xai_grok_agent::prompt::skills::SkillInfo;
use xai_grok_shell::extensions::catalog as shell_catalog;

use super::protocol;

const COMMANDS_LIST: &str = "x.ai/commands/list";
const SKILLS_LIST: &str = "x.ai/skills/list";
const SKILLS_REFRESH_BASELINE: &str = "x.ai/skills/refresh-baseline";
const CURSOR_SKILL_PATH_PREFIX: &str = "cursor://skills/";

/// One catalog request, with the adapter state it needs captured so it can be answered off the I/O loop.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CatalogRequest {
    pub id: Value,
    pub method: &'static str,
    pub params: Value,
    /// The session's cwd: the directory every loader resolves project sources against.
    pub cwd: PathBuf,
    pub session_id: Option<String>,
    /// Every session's latest Cursor command catalog (normalized), keyed by session id.
    pub sessions: Vec<(String, Vec<Value>)>,
}

/// The bare method name when it is answered here.
pub(crate) fn catalog_method(method: &str) -> Option<&'static str> {
    let bare = method.strip_prefix('_').unwrap_or(method);
    if bare == COMMANDS_LIST {
        return Some(COMMANDS_LIST);
    }
    shell_catalog::catalog_method(bare)
}

/// Answer on a dedicated thread: the grok handlers are `!Send` and do blocking disk and git work.
pub(crate) fn spawn(
    request: CatalogRequest,
    to_client: tokio::sync::mpsc::UnboundedSender<String>,
) {
    let spawned = std::thread::Builder::new()
        .name("cursor-catalog".to_owned())
        .spawn(move || {
            for line in answer_blocking(request) {
                let _ = to_client.send(line);
            }
        });
    if let Err(error) = spawned {
        tracing::warn!(%error, "cursor backend: could not start the catalog thread");
    }
}

/// [`answer`] on a fresh current-thread runtime.
pub(crate) fn answer_blocking(request: CatalogRequest) -> Vec<String> {
    let id = request.id.clone();
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            return vec![error_line(
                &id,
                -32603,
                &format!("catalog runtime: {error}"),
            )];
        }
    };
    tokio::task::LocalSet::new().block_on(&runtime, answer(request))
}

/// The JSON-RPC response for `request`, then any notifications it implies.
pub(crate) async fn answer(request: CatalogRequest) -> Vec<String> {
    let agent_skills = cursor_skills(request_commands(&request));
    if request.method == COMMANDS_LIST {
        let commands = commands_list(&request, &agent_skills).await;
        return vec![result_line(&request.id, json!({ "commands": commands }))];
    }
    let reply =
        match shell_catalog::handle(request.method, &request.params, &request.cwd, &agent_skills)
            .await
        {
            Ok(response) => serde_json::from_str::<Value>(response.0.get())
                .map(|result| result_line(&request.id, result))
                .unwrap_or_else(|error| error_line(&request.id, -32603, &error.to_string())),
            Err(error) => json!({ "jsonrpc": "2.0", "id": request.id, "error": error }).to_string(),
        };
    let mut out = vec![reply];
    if request.method == SKILLS_REFRESH_BASELINE {
        let disabled = shell_catalog::disabled_skills();
        out.extend(request.sessions.iter().map(|(session_id, commands)| {
            json!({
                "jsonrpc": "2.0",
                "method": "session/update",
                "params": {
                    "sessionId": session_id,
                    "update": {
                        "sessionUpdate": "available_commands_update",
                        "availableCommands": visible_commands(commands, &disabled),
                    },
                },
            })
            .to_string()
        }));
    }
    out
}

/// The requesting session's Cursor catalog, or every session's when the request names none.
fn request_commands(request: &CatalogRequest) -> Vec<&Value> {
    request
        .sessions
        .iter()
        .filter(|(session_id, _)| {
            request
                .session_id
                .as_deref()
                .is_none_or(|wanted| wanted == session_id)
        })
        .flat_map(|(_, commands)| commands)
        .collect()
}

/// Cursor's catalog with disabled skills hidden and grok's metadata (real `SKILL.md` path, description) filled in
/// for skills both sides know. Cursor's catalog stays authoritative: grok-only skills are not added, because a
/// slash command Cursor does not know never reaches a Cursor skill.
async fn commands_list(request: &CatalogRequest, agent_skills: &[SkillInfo]) -> Vec<Value> {
    let listed = shell_catalog::handle(
        SKILLS_LIST,
        &json!({ "cwd": request.cwd }),
        &request.cwd,
        agent_skills,
    )
    .await
    .ok()
    .and_then(|response| crate::acp::skills_listing::parse_reply(response.0.get()).ok())
    .map(|listing| listing.skills)
    .unwrap_or_default();
    let disabled: HashSet<String> = listed
        .iter()
        .filter(|skill| !skill.enabled)
        .map(|skill| skill.name.clone())
        .collect();
    let commands: Vec<Value> = request_commands(request).into_iter().cloned().collect();
    let mut seen = HashSet::new();
    visible_commands(&commands, &disabled)
        .into_iter()
        .filter(|cmd| seen.insert(command_name(cmd).unwrap_or_default().to_owned()))
        .map(|mut cmd| {
            enrich(&mut cmd, &listed);
            cmd
        })
        .collect()
}

/// `commands` without the skills named in `disabled`.
pub(crate) fn visible_commands(commands: &[Value], disabled: &HashSet<String>) -> Vec<Value> {
    commands
        .iter()
        .filter(|cmd| {
            !(is_skill(cmd) && command_name(cmd).is_some_and(|name| disabled.contains(name)))
        })
        .cloned()
        .collect()
}

/// The skills in Cursor's catalog, as skills-list entries.
pub(crate) fn cursor_skills<'a>(commands: impl IntoIterator<Item = &'a Value>) -> Vec<SkillInfo> {
    let mut seen = HashSet::new();
    commands
        .into_iter()
        .filter(|cmd| is_skill(cmd))
        .filter_map(|cmd| {
            let name = command_name(cmd)?;
            if !seen.insert(name.to_owned()) {
                return None;
            }
            let meta = cmd.get("_meta");
            let path = meta
                .and_then(|m| m.get("path"))
                .and_then(Value::as_str)
                .map(str::to_owned)
                .unwrap_or_else(|| format!("{CURSOR_SKILL_PATH_PREFIX}{name}"));
            let description = cmd
                .get("description")
                .and_then(Value::as_str)
                .filter(|d| !d.is_empty())
                .unwrap_or(name);
            let entry = |scope: &str| {
                serde_json::from_value::<SkillInfo>(json!({
                    "name": name,
                    "description": description,
                    "path": path,
                    "scope": scope,
                }))
                .ok()
            };
            meta.and_then(|m| m.get("scope"))
                .and_then(Value::as_str)
                .and_then(entry)
                .or_else(|| entry("user"))
        })
        .collect()
}

fn enrich(cmd: &mut Value, listed: &[SkillInfo]) {
    let Some(name) = command_name(cmd).map(str::to_owned) else {
        return;
    };
    let Some(skill) = listed
        .iter()
        .find(|skill| skill.name == name && !skill.path.starts_with(CURSOR_SKILL_PATH_PREFIX))
    else {
        return;
    };
    let Some(obj) = cmd.as_object_mut() else {
        return;
    };
    let placeholder = obj
        .get("_meta")
        .and_then(|m| m.get("path"))
        .and_then(Value::as_str)
        .is_none_or(|path| path.starts_with(CURSOR_SKILL_PATH_PREFIX));
    if placeholder {
        let meta = obj
            .entry("_meta")
            .or_insert_with(|| Value::Object(Map::new()));
        if let Some(meta) = meta.as_object_mut() {
            meta.insert("path".to_owned(), Value::String(skill.path.clone()));
        }
    }
    let description_missing = obj
        .get("description")
        .and_then(Value::as_str)
        .is_none_or(|d| d.is_empty() || d == name);
    if description_missing && !skill.description.is_empty() {
        obj.insert(
            "description".to_owned(),
            Value::String(skill.description.clone()),
        );
    }
}

fn is_skill(cmd: &Value) -> bool {
    cmd.as_object().is_some_and(protocol::command_is_skill)
}

fn command_name(cmd: &Value) -> Option<&str> {
    cmd.get("name").and_then(Value::as_str)
}

fn result_line(id: &Value, result: Value) -> String {
    json!({ "jsonrpc": "2.0", "id": id, "result": result }).to_string()
}

fn error_line(id: &Value, code: i64, message: &str) -> String {
    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } }).to_string()
}
