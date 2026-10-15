//! Cursor Cloud Agents HTTP API. ACP `session/prompt` does not create a cloud agent.
//! A message that starts with `&` and a task is sent here. `&` and `& status` only read the latest run.
//! `& cancel` stops that run. A run that is still going is followed on its event stream.
//!
//! Auth matches `agent login`. `CURSOR_AUTH_TOKEN` is sent as Bearer, then `CURSOR_API_KEY` as Basic,
//! then the access token `agent login` stores.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::time::Duration;

use base64::Engine;
use serde::Deserialize;
use serde_json::{Value, json};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CloudAgent {
    pub id: String,
    pub url: Option<String>,
    pub run_id: Option<String>,
    pub run_status: Option<String>,
    pub result: Option<String>,
    pub pr_url: Option<String>,
    pub branch: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum CloudEvent {
    Assistant(String),
    Tool(CloudTool),
    Result {
        status: String,
        text: String,
        pr_url: Option<String>,
        branch: Option<String>,
    },
    Thinking(String),
    Error { message: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CloudTool {
    pub call_id: String,
    pub name: String,
    pub phase: CloudToolPhase,
    pub raw_input: Option<String>,
    pub raw_output: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CloudToolPhase {
    Running,
    Completed,
    Failed,
}

pub(crate) enum CloudPiece {
    Text(String),
    Tool(CloudTool),
}

pub(crate) fn run_is_terminal(status: Option<&str>) -> bool {
    matches!(
        status,
        Some("FINISHED" | "ERROR" | "CANCELLED" | "EXPIRED")
    )
}

pub(crate) fn event_text(event: &CloudEvent, saw_assistant: bool) -> Option<String> {
    match event {
        CloudEvent::Assistant(text) => Some(text.clone()),
        CloudEvent::Tool(_) => None,
        CloudEvent::Result {
            status,
            text,
            pr_url,
            branch,
        } => {
            let mut msg = format!("Run {status}.\n");
            if !saw_assistant && !text.is_empty() {
                msg.push_str(text);
                if !text.ends_with('\n') {
                    msg.push('\n');
                }
            }
            if let Some(url) = pr_url {
                msg.push_str(url);
                msg.push('\n');
            }
            if let Some(branch) = branch {
                msg.push_str(branch);
                msg.push('\n');
            }
            Some(msg)
        }
        CloudEvent::Thinking(text) => Some(text.clone()),
        CloudEvent::Error { message } => Some(format!("Cloud agent run failed. {message}\n")),
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum CloudAuth {
    Basic(String),
    Bearer(String),
}

#[derive(Debug, Clone, Deserialize, Default)]
pub(crate) struct StoredLogin {
    #[serde(default, rename = "accessToken")]
    access_token: Option<String>,
    #[serde(default, rename = "apiKey")]
    api_key: Option<String>,
}

const LOGIN_REQUIRED: &str =
    "Run `agent login`, or set CURSOR_API_KEY, to send this to a Cursor cloud agent.";

pub(crate) fn resolve_cloud_auth() -> Result<CloudAuth, String> {
    let auth_token = std::env::var("CURSOR_AUTH_TOKEN").ok();
    let api_key = std::env::var("CURSOR_API_KEY").ok();
    let stored = read_stored_login()?;
    cloud_auth_from_sources(
        auth_token.as_deref(),
        api_key.as_deref(),
        stored.as_ref(),
    )
}

pub(crate) fn cloud_auth_from_sources(
    auth_token: Option<&str>,
    api_key: Option<&str>,
    stored: Option<&StoredLogin>,
) -> Result<CloudAuth, String> {
    if let Some(token) = nonempty(auth_token) {
        return Ok(CloudAuth::Bearer(token.to_owned()));
    }
    if let Some(key) = nonempty(api_key) {
        return Ok(CloudAuth::Basic(key.to_owned()));
    }
    if let Some(stored) = stored {
        if let Some(token) = nonempty(stored.access_token.as_deref()) {
            return Ok(CloudAuth::Bearer(token.to_owned()));
        }
        if let Some(key) = nonempty(stored.api_key.as_deref()) {
            return Ok(CloudAuth::Basic(key.to_owned()));
        }
    }
    Err(LOGIN_REQUIRED.to_owned())
}

fn nonempty(value: Option<&str>) -> Option<&str> {
    value.map(str::trim).filter(|text| !text.is_empty())
}

fn read_stored_login() -> Result<Option<StoredLogin>, String> {
    #[cfg(target_os = "macos")]
    if keychain_is_default_store()
        && let Some(login) = read_macos_keychain()
        && (nonempty(login.access_token.as_deref()).is_some()
            || nonempty(login.api_key.as_deref()).is_some())
    {
        return Ok(Some(login));
    }
    read_auth_file()
}

fn read_auth_file() -> Result<Option<StoredLogin>, String> {
    let path = cursor_auth_path();
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(_) => {
            return Err(format!(
                "Cursor login file at {} could not be read. Run `agent login` again.",
                path.display()
            ));
        }
    };
    serde_json::from_str(&text).map(Some).map_err(|_| {
        format!(
            "Cursor login file at {} could not be read. Run `agent login` again.",
            path.display()
        )
    })
}

fn cursor_auth_path() -> PathBuf {
    let home = std::env::var("HOME")
        .or_else(|_| std::env::var("USERPROFILE"))
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("."));
    let xdg = std::env::var("XDG_CONFIG_HOME")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .map(PathBuf::from);
    let appdata = std::env::var("APPDATA")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .map(PathBuf::from);
    cursor_auth_path_for(
        std::env::consts::OS,
        &home,
        xdg.as_deref(),
        appdata.as_deref(),
    )
}

fn cursor_auth_path_for(
    os: &str,
    home: &Path,
    xdg_config_home: Option<&Path>,
    appdata: Option<&Path>,
) -> PathBuf {
    match os {
        "windows" => {
            let base = appdata
                .map(Path::to_path_buf)
                .unwrap_or_else(|| home.join("AppData").join("Roaming"));
            base.join("Cursor").join("auth.json")
        }
        "macos" => home.join(".cursor").join("auth.json"),
        _ => {
            let base = xdg_config_home
                .map(Path::to_path_buf)
                .unwrap_or_else(|| home.join(".config"));
            base.join("cursor").join("auth.json")
        }
    }
}

#[cfg(target_os = "macos")]
fn keychain_is_default_store() -> bool {
    !matches!(
        std::env::var("AGENT_CLI_CREDENTIAL_STORE")
            .ok()
            .as_deref(),
        Some("file" | "memory")
    )
}

#[cfg(target_os = "macos")]
fn read_macos_keychain() -> Option<StoredLogin> {
    Some(StoredLogin {
        access_token: keychain_secret("cursor-access-token"),
        api_key: keychain_secret("cursor-api-key"),
    })
}

#[cfg(target_os = "macos")]
fn keychain_secret(service: &str) -> Option<String> {
    let output = std::process::Command::new("security")
        .args([
            "find-generic-password",
            "-s",
            service,
            "-a",
            "cursor-user",
            "-w",
        ])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let secret = String::from_utf8(output.stdout).ok()?;
    let secret = secret.trim();
    if secret.is_empty() {
        None
    } else {
        Some(secret.to_owned())
    }
}

fn authorization_header(auth: &CloudAuth) -> String {
    match auth {
        CloudAuth::Basic(key) => format!("Basic {}", basic_token(key)),
        CloudAuth::Bearer(token) => format!("Bearer {token}"),
    }
}

fn curl_auth_args(auth: &CloudAuth) -> Vec<String> {
    match auth {
        CloudAuth::Basic(key) => vec!["-u".to_owned(), format!("{key}:")],
        CloudAuth::Bearer(token) => {
            vec!["-H".to_owned(), format!("Authorization: Bearer {token}")]
        }
    }
}

pub(crate) fn handoff_text(prompt: &str) -> Option<String> {
    let rest = prompt.trim().strip_prefix('&')?.trim();
    if rest.is_empty() {
        None
    } else {
        Some(rest.to_owned())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CloudRepo {
    pub url: String,
    pub starting_ref: Option<String>,
}

/// One image on a cloud prompt. The API takes either bytes or an http(s) URL, at most five.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CloudImage {
    pub data: String,
    pub mime_type: String,
    pub url: Option<String>,
}

const CLOUD_IMAGE_LIMIT: usize = 5;
const MAX_CLOUD_IMAGE_BYTES: usize = 15 * 1024 * 1024;

pub(crate) fn images_from_prompt(prompt: Option<&Value>) -> Vec<CloudImage> {
    let Some(blocks) = prompt.and_then(Value::as_array) else {
        return Vec::new();
    };
    let mut images = Vec::new();
    for block in blocks {
        if images.len() == CLOUD_IMAGE_LIMIT {
            break;
        }
        if block.get("type").and_then(Value::as_str) != Some("image") {
            continue;
        }
        if let Some(image) = cloud_image(block) {
            images.push(image);
        }
    }
    images
}

fn cloud_image(block: &Value) -> Option<CloudImage> {
    let mime = block
        .get("mimeType")
        .or_else(|| block.get("mime_type"))
        .and_then(Value::as_str)
        .unwrap_or("");
    let data = block.get("data").and_then(Value::as_str).unwrap_or("");
    let url = http_image_url(block.get("uri").and_then(Value::as_str));
    if supported_cloud_mime(mime)
        && !data.is_empty()
        && base64_payload_bytes(data) <= MAX_CLOUD_IMAGE_BYTES
    {
        return Some(CloudImage {
            data: data.to_owned(),
            mime_type: mime.to_owned(),
            url: None,
        });
    }
    url.map(|url| CloudImage {
        data: String::new(),
        mime_type: String::new(),
        url: Some(url),
    })
}

fn supported_cloud_mime(mime: &str) -> bool {
    matches!(mime, "image/png" | "image/jpeg" | "image/gif" | "image/webp")
}

fn http_image_url(uri: Option<&str>) -> Option<String> {
    let uri = uri?.trim();
    if uri.starts_with("https://") || uri.starts_with("http://") {
        Some(uri.to_owned())
    } else {
        None
    }
}

fn base64_payload_bytes(data: &str) -> usize {
    data.trim_end_matches('=').len().saturating_mul(3) / 4
}

fn image_json(image: &CloudImage) -> Value {
    match &image.url {
        Some(url) if image.data.is_empty() => json!({ "url": url }),
        _ => json!({ "data": image.data, "mimeType": image.mime_type }),
    }
}

pub(crate) struct CloudModel {
    pub id: String,
    pub params: Vec<(String, String)>,
}

pub(crate) fn cloud_model_from_id(model_id: &str) -> Option<CloudModel> {
    let model_id = model_id.trim();
    if model_id.is_empty() {
        return None;
    }
    let (id, params) = match model_id.split_once('[') {
        Some((id, rest)) => {
            let rest = rest.strip_suffix(']').unwrap_or(rest);
            let params = rest
                .split(',')
                .filter_map(|part| {
                    let part = part.trim();
                    if part.is_empty() {
                        return None;
                    }
                    let (key, value) = part.split_once('=').unwrap_or((part, "true"));
                    let key = key.trim();
                    if key.is_empty() {
                        None
                    } else {
                        Some((key.to_owned(), value.trim().to_owned()))
                    }
                })
                .collect();
            (id.trim(), params)
        }
        None => (model_id, Vec::new()),
    };
    if id.is_empty() {
        None
    } else {
        Some(CloudModel {
            id: id.to_owned(),
            params,
        })
    }
}

pub(crate) fn create_body(
    text: &str,
    repo: Option<&CloudRepo>,
    mode: Option<&str>,
    model: Option<&str>,
    images: &[CloudImage],
) -> String {
    let mut body = json!({ "prompt": { "text": text } });
    if let Some(repo) = repo {
        let mut entry = json!({ "url": repo.url });
        if let Some(starting_ref) = &repo.starting_ref {
            entry["startingRef"] = json!(starting_ref);
        }
        body["repos"] = json!([entry]);
    }
    attach_cloud_mode(&mut body, mode);
    attach_cloud_model(&mut body, model);
    attach_images(&mut body, images);
    body.to_string()
}

fn attach_cloud_model(body: &mut Value, model: Option<&str>) {
    let Some(model) = model.and_then(cloud_model_from_id) else {
        return;
    };
    let mut entry = json!({ "id": model.id });
    if !model.params.is_empty() {
        let params: Vec<Value> = model
            .params
            .iter()
            .map(|(id, value)| json!({ "id": id, "value": value }))
            .collect();
        entry["params"] = Value::Array(params);
    }
    body["model"] = entry;
}

fn attach_cloud_mode(body: &mut Value, mode: Option<&str>) {
    if let Some(mode) = mode
        && (mode == "agent" || mode == "plan")
    {
        body["mode"] = json!(mode);
    }
}

fn attach_images(body: &mut Value, images: &[CloudImage]) {
    if images.is_empty() {
        return;
    }
    let Some(prompt) = body.get_mut("prompt").and_then(Value::as_object_mut) else {
        return;
    };
    prompt.insert(
        "images".to_owned(),
        Value::Array(images.iter().map(image_json).collect()),
    );
}

pub(crate) fn normalize_git_remote(remote: &str) -> Option<String> {
    let remote = remote.trim();
    if remote.is_empty() {
        return None;
    }
    let url = if let Some(rest) = remote.strip_prefix("git@") {
        let (host, path) = rest.split_once(':')?;
        if host.is_empty() || path.is_empty() || host.contains('/') {
            return None;
        }
        format!("https://{host}/{path}")
    } else if let Some(rest) = remote.strip_prefix("ssh://") {
        let rest = rest.strip_prefix("git@").unwrap_or(rest);
        format!("https://{rest}")
    } else if let Some(rest) = remote.strip_prefix("http://") {
        format!("https://{rest}")
    } else if remote.starts_with("https://") {
        remote.to_owned()
    } else {
        return None;
    };
    let url = strip_userinfo(&url)?;
    let url = trim_repo_url(url);
    let without = url.strip_prefix("https://")?;
    let (host, path) = without.split_once('/')?;
    if host.is_empty() || path.is_empty() {
        None
    } else {
        Some(url)
    }
}

fn strip_userinfo(url: &str) -> Option<String> {
    let rest = url.strip_prefix("https://")?;
    if let Some((userinfo, hostpath)) = rest.split_once('@')
        && !userinfo.contains('/')
    {
        return Some(format!("https://{hostpath}"));
    }
    Some(url.to_owned())
}

fn trim_repo_url(mut url: String) -> String {
    while url.ends_with('/') {
        url.pop();
    }
    if let Some(stripped) = url.strip_suffix(".git") {
        url = stripped.to_owned();
    }
    while url.ends_with('/') {
        url.pop();
    }
    url
}

pub(crate) fn repo_from_cwd(cwd: &std::path::Path) -> Option<CloudRepo> {
    let remote = git_output(cwd, &["remote", "get-url", "origin"])?;
    let url = normalize_git_remote(&remote)?;
    let starting_ref = git_output(cwd, &["symbolic-ref", "--short", "HEAD"]).and_then(|name| {
        let name = name.trim();
        if name.is_empty() || name == "HEAD" {
            None
        } else {
            Some(name.to_owned())
        }
    });
    Some(CloudRepo { url, starting_ref })
}

fn git_output(cwd: &std::path::Path, args: &[&str]) -> Option<String> {
    let output = std::process::Command::new("git")
        .arg("-C")
        .arg(cwd)
        .args(args)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8(output.stdout).ok()?;
    let text = text.trim();
    if text.is_empty() {
        None
    } else {
        Some(text.to_owned())
    }
}

pub(crate) fn follow_up_body(text: &str, mode: Option<&str>, images: &[CloudImage]) -> String {
    let mut body = json!({ "prompt": { "text": text } });
    attach_cloud_mode(&mut body, mode);
    attach_images(&mut body, images);
    body.to_string()
}

pub(crate) fn parse_cloud_response(body: &str) -> Result<CloudAgent, String> {
    let value: Value = serde_json::from_str(body).map_err(|err| err.to_string())?;
    let agent = value.get("agent").unwrap_or(&value);
    let id = agent
        .get("id")
        .and_then(Value::as_str)
        .or_else(|| {
            value
                .pointer("/run/agentId")
                .and_then(Value::as_str)
        })
        .ok_or_else(|| "cloud agent response has no id".to_owned())?
        .to_owned();
    let url = agent
        .get("url")
        .and_then(Value::as_str)
        .map(str::to_owned);
    let run_id = value
        .pointer("/run/id")
        .and_then(Value::as_str)
        .or_else(|| agent.get("latestRunId").and_then(Value::as_str))
        .map(str::to_owned);
    let run_status = value
        .pointer("/run/status")
        .or_else(|| agent.get("status"))
        .and_then(Value::as_str)
        .map(str::to_owned);
    let result = value
        .pointer("/run/result")
        .and_then(Value::as_str)
        .map(str::to_owned);
    Ok(CloudAgent {
        id,
        url,
        run_id,
        run_status,
        result,
        pr_url: pr_url(&value),
        branch: git_place(&value),
    })
}

fn git_branches(value: &Value) -> Option<&Vec<Value>> {
    for pointer in ["/run/git/branches", "/git/branches"] {
        if let Some(branches) = value.pointer(pointer).and_then(Value::as_array)
            && !branches.is_empty()
        {
            return Some(branches);
        }
    }
    None
}

fn pr_url(value: &Value) -> Option<String> {
    let urls: Vec<&str> = git_branches(value)?
        .iter()
        .filter_map(|branch| branch.get("prUrl").and_then(Value::as_str))
        .filter(|url| !url.is_empty())
        .collect();
    if urls.is_empty() {
        None
    } else {
        Some(urls.join("\n"))
    }
}

fn git_place(value: &Value) -> Option<String> {
    let lines: Vec<String> = git_branches(value)?
        .iter()
        .filter_map(|branch| {
            let name = branch.get("branch").and_then(Value::as_str).unwrap_or("");
            let repo = branch.get("repoUrl").and_then(Value::as_str).unwrap_or("");
            match (repo.is_empty(), name.is_empty()) {
                (true, true) => None,
                (false, true) => Some(repo.to_owned()),
                (true, false) => Some(name.to_owned()),
                (false, false) => Some(format!("{repo} {name}")),
            }
        })
        .collect();
    if lines.is_empty() {
        None
    } else {
        Some(lines.join("\n"))
    }
}

fn refresh(api_base: &str, auth: &CloudAuth, agent: &mut CloudAgent) -> Result<(), String> {
    let record = request(
        "GET",
        &format!("{api_base}/v1/agents/{}", agent.id),
        auth,
        None,
    )?;
    let value: Value = serde_json::from_str(&record).map_err(|err| err.to_string())?;
    if agent.url.is_none() {
        agent.url = value
            .get("url")
            .and_then(Value::as_str)
            .map(str::to_owned);
    }
    let run_id = value
        .get("latestRunId")
        .and_then(Value::as_str)
        .map(str::to_owned)
        .or_else(|| agent.run_id.clone());
    let Some(run_id) = run_id else {
        return Ok(());
    };
    let run = request(
        "GET",
        &format!("{api_base}/v1/agents/{}/runs/{run_id}", agent.id),
        auth,
        None,
    )?;
    let run: Value = serde_json::from_str(&run).map_err(|err| err.to_string())?;
    agent.run_id = Some(run_id);
    agent.run_status = run
        .get("status")
        .and_then(Value::as_str)
        .map(str::to_owned)
        .or(agent.run_status.take());
    agent.result = run
        .get("result")
        .and_then(Value::as_str)
        .map(str::to_owned)
        .or(agent.result.take());
    if let Some(url) = pr_url(&run) {
        agent.pr_url = Some(url);
    }
    if let Some(place) = git_place(&run) {
        agent.branch = Some(place);
    }
    Ok(())
}

pub(crate) fn refresh_existing(
    api_base: &str,
    auth: &CloudAuth,
    agent_id: &str,
) -> Result<CloudAgent, String> {
    let mut agent = CloudAgent {
        id: agent_id.to_owned(),
        url: None,
        run_id: None,
        run_status: None,
        result: None,
        pr_url: None,
        branch: None,
    };
    refresh(api_base, auth, &mut agent)?;
    Ok(agent)
}

pub(crate) fn cancel_existing(
    api_base: &str,
    auth: &CloudAuth,
    agent_id: &str,
) -> Result<CloudAgent, String> {
    let mut agent = refresh_existing(api_base, auth, agent_id)?;
    if run_is_terminal(agent.run_status.as_deref()) {
        return Ok(agent);
    }
    let Some(run_id) = agent.run_id.clone() else {
        return Err("cloud agent has no run to cancel".to_owned());
    };
    let _ = request(
        "POST",
        &format!("{api_base}/v1/agents/{}/runs/{run_id}/cancel", agent.id),
        auth,
        None,
    )?;
    let _ = refresh(api_base, auth, &mut agent);
    Ok(agent)
}

pub(crate) fn dispatch(
    api_base: &str,
    auth: &CloudAuth,
    agent_id: Option<&str>,
    text: &str,
    repo: Option<&CloudRepo>,
    mode: Option<&str>,
    model: Option<&str>,
    images: &[CloudImage],
) -> Result<CloudAgent, String> {
    let (url, body) = match agent_id {
        Some(id) => (
            format!("{api_base}/v1/agents/{id}/runs"),
            follow_up_body(text, mode, images),
        ),
        None => (
            format!("{api_base}/v1/agents"),
            create_body(text, repo, mode, model, images),
        ),
    };
    let raw = match request("POST", &url, auth, Some(&body)) {
        Ok(raw) => raw,
        Err(err) if is_agent_busy(&err) => {
            let Some(id) = agent_id else {
                return Err(err);
            };
            let mut agent = refresh_existing(api_base, auth, id)?;
            if agent.result.is_none() {
                agent.result = Some(
                    "Still running, so this task was not sent. Send & status to read it, or & cancel to stop it."
                        .to_owned(),
                );
            }
            return Ok(agent);
        }
        Err(err) => return Err(err),
    };
    let mut agent = parse_cloud_response(&raw)?;
    let _ = refresh(api_base, auth, &mut agent);
    Ok(agent)
}

pub(crate) fn recovered_run_text(agent: &CloudAgent, include_reply: bool) -> Option<String> {
    let terminal = run_is_terminal(agent.run_status.as_deref());
    let reply = agent
        .result
        .as_deref()
        .filter(|text| include_reply && !text.is_empty());
    if agent.run_status.is_none() && reply.is_none() && agent.pr_url.is_none() && agent.branch.is_none()
    {
        return None;
    }
    if !terminal && reply.is_none() && agent.pr_url.is_none() && agent.branch.is_none() {
        return agent
            .run_status
            .as_deref()
            .map(|status| format!("Run {status}.\n"));
    }
    let mut msg = format!("Run {}.\n", agent.run_status.as_deref().unwrap_or(""));
    if let Some(text) = reply {
        msg.push_str(text);
        if !text.ends_with('\n') {
            msg.push('\n');
        }
    }
    if let Some(url) = &agent.pr_url {
        msg.push_str(url);
        msg.push('\n');
    }
    if let Some(branch) = &agent.branch {
        msg.push_str(branch);
        msg.push('\n');
    }
    Some(msg)
}

pub(crate) fn follow_run_or_recover(
    api_base: &str,
    auth: &CloudAuth,
    agent_id: &str,
    run_id: &str,
    mut on_piece: impl FnMut(CloudPiece),
) -> Result<(), String> {
    let mut saw_assistant = false;
    let mut saw_result = false;
    let followed = follow_run(api_base, auth, agent_id, run_id, |event| {
        if matches!(event, CloudEvent::Result { .. }) {
            saw_result = true;
        }
        let text = event_text(&event, saw_assistant);
        if matches!(event, CloudEvent::Assistant(_)) {
            saw_assistant = true;
        }
        if let CloudEvent::Tool(tool) = &event {
            on_piece(CloudPiece::Tool(tool.clone()));
        }
        if let Some(text) = text {
            on_piece(CloudPiece::Text(text));
        }
    });
    if matches!(followed, Ok(()) if saw_result) {
        return Ok(());
    }
    let err = followed.err();
    match refresh_existing(api_base, auth, agent_id) {
        Ok(agent) => {
            if let Some(text) = recovered_run_text(&agent, !saw_assistant) {
                on_piece(CloudPiece::Text(text));
                Ok(())
            } else if let Some(err) = err {
                Err(err)
            } else {
                Ok(())
            }
        }
        Err(refresh_err) => Err(err.unwrap_or(refresh_err)),
    }
}

pub(crate) fn follow_run(
    api_base: &str,
    auth: &CloudAuth,
    agent_id: &str,
    run_id: &str,
    on_event: impl FnMut(CloudEvent),
) -> Result<(), String> {
    let url = format!("{api_base}/v1/agents/{agent_id}/runs/{run_id}/stream");
    if url.starts_with("https://") {
        return follow_via_curl(&url, auth, on_event);
    }
    let body = request("GET", &url, auth, None)?;
    emit_sse(&body, on_event);
    Ok(())
}

fn emit_sse(body: &str, mut on_event: impl FnMut(CloudEvent)) {
    let mut parser = SseParser::default();
    for line in body.split_inclusive('\n') {
        let line = line.trim_end_matches(['\r', '\n']);
        if let Some(event) = parser.push_line(line) {
            on_event(event);
        }
    }
    if let Some(event) = parser.finish() {
        on_event(event);
    }
}

#[derive(Default)]
struct SseParser {
    event: String,
    data: String,
}

impl SseParser {
    fn push_line(&mut self, line: &str) -> Option<CloudEvent> {
        if line.is_empty() {
            return self.finish();
        }
        if let Some(rest) = line.strip_prefix("event:") {
            self.event = rest.trim().to_owned();
        } else if let Some(rest) = line.strip_prefix("data:") {
            if !self.data.is_empty() {
                self.data.push('\n');
            }
            self.data.push_str(rest.trim_start());
        }
        None
    }

    fn finish(&mut self) -> Option<CloudEvent> {
        let event = std::mem::take(&mut self.event);
        let data = std::mem::take(&mut self.data);
        if event.is_empty() && data.is_empty() {
            None
        } else {
            decode_event(&event, &data)
        }
    }
}

fn json_field(value: &Value, key: &str) -> Option<String> {
    let field = value.get(key)?;
    if field.is_null() {
        None
    } else {
        Some(field.to_string())
    }
}

fn decode_event(event: &str, data: &str) -> Option<CloudEvent> {
    let value: Value = serde_json::from_str(data).unwrap_or(Value::Null);
    match event {
        "assistant" => value
            .get("text")
            .and_then(Value::as_str)
            .map(|text| CloudEvent::Assistant(text.to_owned())),
        "tool_call" => {
            let phase = match value.get("status").and_then(Value::as_str) {
                Some("running") => CloudToolPhase::Running,
                Some("completed") => CloudToolPhase::Completed,
                Some("error" | "failed") => CloudToolPhase::Failed,
                _ => return None,
            };
            let name = value
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or("tool")
                .to_owned();
            let call_id = value
                .get("callId")
                .and_then(Value::as_str)
                .unwrap_or(&name)
                .to_owned();
            Some(CloudEvent::Tool(CloudTool {
                call_id,
                name,
                phase,
                raw_input: json_field(&value, "args"),
                raw_output: json_field(&value, "result"),
            }))
        }
        "thinking" => value
            .get("text")
            .and_then(Value::as_str)
            .filter(|text| !text.is_empty())
            .map(|text| CloudEvent::Thinking(text.to_owned())),
        "result" => Some(CloudEvent::Result {
            status: value
                .get("status")
                .and_then(Value::as_str)
                .unwrap_or("FINISHED")
                .to_owned(),
            text: value
                .get("text")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_owned(),
            pr_url: pr_url(&value),
            branch: git_place(&value),
        }),
        "error" => Some(CloudEvent::Error {
            message: value
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("unknown error")
                .to_owned(),
        }),
        _ => None,
    }
}

fn follow_via_curl(
    url: &str,
    auth: &CloudAuth,
    mut on_event: impl FnMut(CloudEvent),
) -> Result<(), String> {
    let mut args = vec![
        "-N".to_owned(),
        "-sS".to_owned(),
        "-f".to_owned(),
        "-X".to_owned(),
        "GET".to_owned(),
    ];
    args.extend(curl_auth_args(auth));
    args.extend([
        "-H".to_owned(),
        "Accept: text/event-stream".to_owned(),
        url.to_owned(),
    ]);
    let mut child = std::process::Command::new("curl").args(args)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|err| err.to_string())?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| "curl has no stdout".to_owned())?;
    let mut parser = SseParser::default();
    for line in BufReader::new(stdout).lines() {
        let line = line.map_err(|err| err.to_string())?;
        if let Some(event) = parser.push_line(&line) {
            on_event(event);
        }
    }
    if let Some(event) = parser.finish() {
        on_event(event);
    }
    let status = child.wait().map_err(|err| err.to_string())?;
    if status.success() {
        Ok(())
    } else {
        let mut err = String::new();
        if let Some(mut stderr) = child.stderr.take() {
            let _ = std::io::Read::read_to_string(&mut stderr, &mut err);
        }
        Err(if err.trim().is_empty() {
            format!("curl exited {status}")
        } else {
            err.trim().to_owned()
        })
    }
}

fn is_agent_busy(err: &str) -> bool {
    err.contains(" 409") || err.contains("HTTP 409")
}

fn request(
    method: &str,
    url: &str,
    auth: &CloudAuth,
    body: Option<&str>,
) -> Result<String, String> {
    if url.starts_with("https://") {
        return request_via_curl(method, url, auth, body);
    }
    request_via_tcp(method, url, auth, body)
}

fn request_via_curl(
    method: &str,
    url: &str,
    auth: &CloudAuth,
    body: Option<&str>,
) -> Result<String, String> {
    let mut args = vec!["-sS".to_owned(), "-X".to_owned(), method.to_owned()];
    args.extend(curl_auth_args(auth));
    args.extend([
        "-H".to_owned(),
        "Accept: application/json".to_owned(),
        "-w".to_owned(),
        "\n%{http_code}".to_owned(),
        url.to_owned(),
    ]);
    let mut cmd = std::process::Command::new("curl");
    cmd.args(args);
    if let Some(body) = body {
        cmd.args(["-H", "Content-Type: application/json", "-d", body]);
    }
    let output = cmd.output().map_err(|err| err.to_string())?;
    if !output.status.success() {
        return Err(String::from_utf8_lossy(&output.stderr).trim().to_owned());
    }
    let text = String::from_utf8_lossy(&output.stdout);
    let (payload, code) = text
        .rsplit_once('\n')
        .ok_or_else(|| "curl returned no status".to_owned())?;
    if !code.starts_with('2') {
        return Err(format!("cloud agent HTTP {code}: {payload}"));
    }
    Ok(payload.to_owned())
}

fn request_via_tcp(
    method: &str,
    url: &str,
    auth: &CloudAuth,
    body: Option<&str>,
) -> Result<String, String> {
    let rest = url
        .strip_prefix("http://")
        .ok_or_else(|| format!("unsupported cloud API url {url}"))?;
    let (host_port, path) = rest
        .split_once('/')
        .ok_or_else(|| format!("cloud API url {url} has no path"))?;
    let path = format!("/{path}");
    let mut stream = TcpStream::connect(host_port).map_err(|err| err.to_string())?;
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .map_err(|err| err.to_string())?;
    let header = authorization_header(auth);
    let body = body.unwrap_or("");
    let request = format!(
        "{method} {path} HTTP/1.1\r\nHost: {host_port}\r\nAuthorization: {header}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    stream
        .write_all(request.as_bytes())
        .map_err(|err| err.to_string())?;
    let mut raw = Vec::new();
    stream.read_to_end(&mut raw).map_err(|err| err.to_string())?;
    let text = String::from_utf8_lossy(&raw);
    let (head, payload) = text
        .split_once("\r\n\r\n")
        .ok_or_else(|| "cloud API response has no body".to_owned())?;
    let status = head
        .lines()
        .next()
        .ok_or_else(|| "cloud API response has no status".to_owned())?;
    if !status.contains(" 2") {
        return Err(format!("{status}: {payload}"));
    }
    Ok(payload.to_owned())
}

fn basic_token(api_key: &str) -> String {
    base64::engine::general_purpose::STANDARD.encode(format!("{api_key}:"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::thread;

    #[test]
    fn create_and_follow_up_round_trip_against_a_local_server() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        thread::spawn(move || {
            let replies = [
                r#"{"agent":{"id":"bc-1","url":"https://cursor.com/agents/bc-1","status":"ACTIVE"},"run":{"id":"run-1","status":"CREATING"}}"#,
                r#"{"id":"bc-1","url":"https://cursor.com/agents/bc-1","status":"ACTIVE","latestRunId":"run-1"}"#,
                r#"{"id":"run-1","agentId":"bc-1","status":"FINISHED","result":"Refactored auth."}"#,
                r#"{"run":{"id":"run-2","agentId":"bc-1","status":"CREATING"}}"#,
                r#"{"id":"bc-1","url":"https://cursor.com/agents/bc-1","status":"IDLE","latestRunId":"run-2"}"#,
                r#"{"id":"run-2","agentId":"bc-1","status":"FINISHED","result":"Added the tests."}"#,
            ];
            for body in replies {
                let (mut sock, _) = listener.accept().unwrap();
                let mut buf = [0_u8; 8192];
                let _ = sock.read(&mut buf).unwrap();
                let resp = format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                sock.write_all(resp.as_bytes()).unwrap();
            }
        });
        let base = format!("http://{addr}");
        let created = dispatch(&base, &CloudAuth::Basic("test-key".to_owned()), None, "refactor the auth module", None, None, None, &[]).unwrap();
        assert_eq!(created.id, "bc-1");
        assert_eq!(
            created.url.as_deref(),
            Some("https://cursor.com/agents/bc-1")
        );
        assert_eq!(created.run_status.as_deref(), Some("FINISHED"));
        assert_eq!(created.result.as_deref(), Some("Refactored auth."));
        let follow = dispatch(&base, &CloudAuth::Basic("test-key".to_owned()), Some("bc-1"), "add tests", None, None, None, &[]).unwrap();
        assert_eq!(follow.id, "bc-1");
        assert_eq!(follow.run_status.as_deref(), Some("FINISHED"));
        assert_eq!(follow.result.as_deref(), Some("Added the tests."));
    }

    #[test]
    fn refresh_reads_the_latest_run_without_posting() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        thread::spawn(move || {
            let replies = [
                r#"{"id":"bc-1","url":"https://cursor.com/agents/bc-1","status":"IDLE","latestRunId":"run-9"}"#,
                r#"{"id":"run-9","agentId":"bc-1","status":"FINISHED","result":"Still the same answer."}"#,
            ];
            for body in replies {
                let (mut sock, _) = listener.accept().unwrap();
                let mut buf = [0_u8; 8192];
                let n = sock.read(&mut buf).unwrap();
                let req = String::from_utf8_lossy(&buf[..n]);
                let ok = req.starts_with("GET ");
                let payload = if ok {
                    body.to_owned()
                } else {
                    r#"{"error":"posted"}"#.to_owned()
                };
                let status = if ok { "200 OK" } else { "400 Bad Request" };
                let resp = format!(
                    "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{payload}",
                    payload.len()
                );
                sock.write_all(resp.as_bytes()).unwrap();
            }
        });
        let base = format!("http://{addr}");
        let agent = refresh_existing(&base, &CloudAuth::Basic("test-key".to_owned()), "bc-1").unwrap();
        assert_eq!(agent.id, "bc-1");
        assert_eq!(
            agent.url.as_deref(),
            Some("https://cursor.com/agents/bc-1")
        );
        assert_eq!(agent.run_id.as_deref(), Some("run-9"));
        assert_eq!(agent.run_status.as_deref(), Some("FINISHED"));
        assert_eq!(agent.result.as_deref(), Some("Still the same answer."));
    }

    #[test]
    fn a_running_cloud_agent_streams_its_reply() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let stream = "\
event: status
data: {\"runId\":\"run-1\",\"status\":\"RUNNING\"}

event: assistant
data: {\"text\":\"I'll update the README now.\"}

event: tool_call
data: {\"callId\":\"call-1\",\"name\":\"read_file\",\"status\":\"running\"}

event: tool_call
data: {\"callId\":\"call-1\",\"name\":\"read_file\",\"status\":\"completed\"}

event: result
data: {\"status\":\"FINISHED\",\"text\":\"Added README.md.\",\"git\":{\"branches\":[{\"prUrl\":\"https://github.com/acme/app/pull/123\"}]}}

event: done
data: {}
";
        thread::spawn(move || {
            let replies = [
                (
                    "POST",
                    r#"{"agent":{"id":"bc-1","url":"https://cursor.com/agents/bc-1","status":"ACTIVE"},"run":{"id":"run-1","status":"CREATING"}}"#,
                ),
                (
                    "GET",
                    r#"{"id":"bc-1","url":"https://cursor.com/agents/bc-1","status":"ACTIVE","latestRunId":"run-1"}"#,
                ),
                (
                    "GET",
                    r#"{"id":"run-1","agentId":"bc-1","status":"RUNNING"}"#,
                ),
                ("GET", stream),
            ];
            for (method, body) in replies {
                let (mut sock, _) = listener.accept().unwrap();
                let mut buf = [0_u8; 8192];
                let n = sock.read(&mut buf).unwrap();
                let req = String::from_utf8_lossy(&buf[..n]);
                let ok = req.starts_with(method);
                let payload = if ok {
                    body.to_owned()
                } else {
                    r#"{"error":"wrong method"}"#.to_owned()
                };
                let status = if ok { "200 OK" } else { "400 Bad Request" };
                let resp = format!(
                    "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{payload}",
                    payload.len()
                );
                sock.write_all(resp.as_bytes()).unwrap();
            }
        });
        let base = format!("http://{addr}");
        let created = dispatch(&base, &CloudAuth::Basic("test-key".to_owned()), None, "update the readme", None, None, None, &[]).unwrap();
        assert_eq!(created.run_status.as_deref(), Some("RUNNING"));
        assert_eq!(created.result, None);
        assert!(!run_is_terminal(created.run_status.as_deref()));
        let mut events = Vec::new();
        follow_run(&base, &CloudAuth::Basic("test-key".to_owned()), "bc-1", "run-1", |event| events.push(event)).unwrap();
        assert_eq!(
            events,
            vec![
                CloudEvent::Assistant("I'll update the README now.".to_owned()),
                CloudEvent::Tool(CloudTool {
                    call_id: "call-1".to_owned(),
                    name: "read_file".to_owned(),
                    phase: CloudToolPhase::Running,
                    raw_input: None,
                    raw_output: None,
                }),
                CloudEvent::Tool(CloudTool {
                    call_id: "call-1".to_owned(),
                    name: "read_file".to_owned(),
                    phase: CloudToolPhase::Completed,
                    raw_input: None,
                    raw_output: None,
                }),
                CloudEvent::Result {
                    status: "FINISHED".to_owned(),
                    text: "Added README.md.".to_owned(),
                    pr_url: Some("https://github.com/acme/app/pull/123".to_owned()),
                    branch: None,
                },
            ]
        );
        let mut saw_assistant = false;
        let texts: Vec<String> = events
            .iter()
            .filter_map(|event| {
                let text = event_text(event, saw_assistant);
                if matches!(event, CloudEvent::Assistant(_)) {
                    saw_assistant = true;
                }
                text
            })
            .collect();
        assert_eq!(
            texts,
            vec![
                "I'll update the README now.".to_owned(),
                "Run FINISHED.\nhttps://github.com/acme/app/pull/123\n".to_owned(),
            ]
        );
    }

    #[test]
    fn a_stream_result_names_the_pushed_branch_and_keeps_thinking() {
        let thinking = super::decode_event("thinking", r#"{"text":"Looking at the README."}"#).unwrap();
        let result = super::decode_event(
            "result",
            r#"{"status":"FINISHED","text":"Added README.md","git":{"branches":[{"repoUrl":"github.com/your-org/your-repo","branch":"cursor/add-readme-a1b2"}]}}"#,
        )
        .unwrap();
        assert_eq!(
            event_text(&thinking, false).as_deref(),
            Some("Looking at the README.")
        );
        assert_eq!(
            event_text(&result, false).as_deref(),
            Some("Run FINISHED.\nAdded README.md\ngithub.com/your-org/your-repo cursor/add-readme-a1b2\n")
        );
        assert_eq!(
            event_text(&result, true).as_deref(),
            Some("Run FINISHED.\ngithub.com/your-org/your-repo cursor/add-readme-a1b2\n")
        );
    }

    #[test]
    fn a_stacked_run_names_every_branch_and_pull_request() {
        let result = super::decode_event(
            "result",
            r#"{"status":"FINISHED","text":"Stacked.","git":{"branches":[{"repoUrl":"github.com/acme/app","branch":"cursor/one","prUrl":"https://github.com/acme/app/pull/1"},{"repoUrl":"github.com/acme/app","branch":"cursor/two","prUrl":"https://github.com/acme/app/pull/2"}]}}"#,
        )
        .unwrap();
        assert_eq!(
            event_text(&result, false).as_deref(),
            Some(
                "Run FINISHED.\nStacked.\nhttps://github.com/acme/app/pull/1\nhttps://github.com/acme/app/pull/2\ngithub.com/acme/app cursor/one\ngithub.com/acme/app cursor/two\n"
            )
        );
    }

    #[test]
    fn a_failed_stream_still_reads_the_finished_run() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        thread::spawn(move || {
            let replies = [
                ("500 Internal Server Error", r#"{"error":"stream_expired"}"#),
                (
                    "200 OK",
                    r#"{"id":"bc-1","url":"https://cursor.com/agents/bc-1","latestRunId":"run-1"}"#,
                ),
                (
                    "200 OK",
                    r#"{"id":"run-1","status":"FINISHED","result":"Added the tests.","git":{"branches":[{"repoUrl":"github.com/acme/app","branch":"cursor/tests"}]}}"#,
                ),
            ];
            for (status, body) in replies {
                let (mut sock, _) = listener.accept().unwrap();
                let mut buf = [0_u8; 8192];
                let _ = sock.read(&mut buf).unwrap();
                let resp = format!(
                    "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                sock.write_all(resp.as_bytes()).unwrap();
            }
        });
        let base = format!("http://{addr}");
        let mut texts = Vec::new();
        follow_run_or_recover(
            &base,
            &CloudAuth::Basic("test-key".to_owned()),
            "bc-1",
            "run-1",
            |piece| {
                if let CloudPiece::Text(text) = piece {
                    texts.push(text);
                }
            },
        )
        .unwrap();
        assert_eq!(
            texts,
            vec!["Run FINISHED.\nAdded the tests.\ngithub.com/acme/app cursor/tests\n".to_owned()]
        );
    }

    #[test]
    fn an_empty_stream_reads_the_run_status() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        thread::spawn(move || {
            let replies = [
                ("200 OK", ""),
                (
                    "200 OK",
                    r#"{"id":"bc-1","url":"https://cursor.com/agents/bc-1","latestRunId":"run-1"}"#,
                ),
                ("200 OK", r#"{"id":"run-1","status":"RUNNING"}"#),
            ];
            for (status, body) in replies {
                let (mut sock, _) = listener.accept().unwrap();
                let mut buf = [0_u8; 8192];
                let _ = sock.read(&mut buf).unwrap();
                let resp = format!(
                    "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                sock.write_all(resp.as_bytes()).unwrap();
            }
        });
        let base = format!("http://{addr}");
        let mut texts = Vec::new();
        follow_run_or_recover(
            &base,
            &CloudAuth::Basic("test-key".to_owned()),
            "bc-1",
            "run-1",
            |piece| {
                if let CloudPiece::Text(text) = piece {
                    texts.push(text);
                }
            },
        )
        .unwrap();
        assert_eq!(texts, vec!["Run RUNNING.\n".to_owned()]);
    }

    #[test]
    fn cancel_stops_a_running_cloud_run() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        thread::spawn(move || {
            let replies = [
                r#"{"id":"bc-1","url":"https://cursor.com/agents/bc-1","status":"ACTIVE","latestRunId":"run-1"}"#,
                r#"{"id":"run-1","agentId":"bc-1","status":"RUNNING"}"#,
                r#"{"id":"run-1"}"#,
                r#"{"id":"bc-1","url":"https://cursor.com/agents/bc-1","status":"IDLE","latestRunId":"run-1"}"#,
                r#"{"id":"run-1","agentId":"bc-1","status":"CANCELLED","result":"Stopped."}"#,
            ];
            for (i, body) in replies.into_iter().enumerate() {
                let (mut sock, _) = listener.accept().unwrap();
                let mut buf = [0_u8; 8192];
                let n = sock.read(&mut buf).unwrap();
                let req = String::from_utf8_lossy(&buf[..n]);
                let ok = if i == 2 {
                    req.starts_with("POST ") && req.contains("/cancel")
                } else {
                    req.starts_with("GET ")
                };
                let payload = if ok {
                    body.to_owned()
                } else {
                    r#"{"error":"wrong method"}"#.to_owned()
                };
                let status = if ok { "200 OK" } else { "400 Bad Request" };
                let resp = format!(
                    "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{payload}",
                    payload.len()
                );
                sock.write_all(resp.as_bytes()).unwrap();
            }
        });
        let base = format!("http://{addr}");
        let agent = cancel_existing(&base, &CloudAuth::Basic("test-key".to_owned()), "bc-1").unwrap();
        assert_eq!(agent.id, "bc-1");
        assert_eq!(agent.run_id.as_deref(), Some("run-1"));
        assert_eq!(agent.run_status.as_deref(), Some("CANCELLED"));
        assert_eq!(agent.result.as_deref(), Some("Stopped."));
    }

    #[test]
    fn cancel_leaves_a_finished_run_alone() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        thread::spawn(move || {
            let replies = [
                r#"{"id":"bc-1","url":"https://cursor.com/agents/bc-1","status":"IDLE","latestRunId":"run-1"}"#,
                r#"{"id":"run-1","agentId":"bc-1","status":"FINISHED","result":"Already done."}"#,
            ];
            for body in replies {
                let (mut sock, _) = listener.accept().unwrap();
                let mut buf = [0_u8; 8192];
                let _ = sock.read(&mut buf).unwrap();
                let resp = format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                sock.write_all(resp.as_bytes()).unwrap();
            }
        });
        let base = format!("http://{addr}");
        let agent = cancel_existing(&base, &CloudAuth::Basic("test-key".to_owned()), "bc-1").unwrap();
        assert_eq!(agent.run_status.as_deref(), Some("FINISHED"));
        assert_eq!(agent.result.as_deref(), Some("Already done."));
    }

    #[test]
    fn follow_up_while_the_agent_is_busy_reads_the_running_run() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        thread::spawn(move || {
            let replies = [
                ("409 Conflict", r#"{"code":"agent_busy"}"#),
                (
                    "200 OK",
                    r#"{"id":"bc-1","url":"https://cursor.com/agents/bc-1","status":"ACTIVE","latestRunId":"run-1"}"#,
                ),
                (
                    "200 OK",
                    r#"{"id":"run-1","agentId":"bc-1","status":"RUNNING"}"#,
                ),
            ];
            for (i, (status, body)) in replies.into_iter().enumerate() {
                let (mut sock, _) = listener.accept().unwrap();
                let mut buf = [0_u8; 8192];
                let n = sock.read(&mut buf).unwrap();
                let req = String::from_utf8_lossy(&buf[..n]);
                let ok = if i == 0 {
                    req.starts_with("POST ")
                } else {
                    req.starts_with("GET ")
                };
                let (status, payload) = if ok {
                    (status, body.to_owned())
                } else {
                    ("400 Bad Request", r#"{"error":"wrong method"}"#.to_owned())
                };
                let resp = format!(
                    "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{payload}",
                    payload.len()
                );
                sock.write_all(resp.as_bytes()).unwrap();
            }
        });
        let base = format!("http://{addr}");
        let agent = dispatch(&base, &CloudAuth::Basic("test-key".to_owned()), Some("bc-1"), "add tests", None, None, None, &[]).unwrap();
        assert_eq!(agent.id, "bc-1");
        assert_eq!(agent.run_status.as_deref(), Some("RUNNING"));
        assert_eq!(
            agent.result.as_deref(),
            Some(
                "Still running, so this task was not sent. Send & status to read it, or & cancel to stop it."
            )
        );
    }

    #[test]
    fn login_sources_prefer_the_cli_order() {
        let stored = serde_json::from_str::<StoredLogin>(
            r#"{"accessToken":"login-token","refreshToken":"refresh","apiKey":"stored-key"}"#,
        )
        .unwrap();
        assert_eq!(
            cloud_auth_from_sources(Some("env-token"), Some("env-key"), Some(&stored)).unwrap(),
            CloudAuth::Bearer("env-token".to_owned())
        );
        assert_eq!(
            cloud_auth_from_sources(None, Some("env-key"), Some(&stored)).unwrap(),
            CloudAuth::Basic("env-key".to_owned())
        );
        assert_eq!(
            cloud_auth_from_sources(None, None, Some(&stored)).unwrap(),
            CloudAuth::Bearer("login-token".to_owned())
        );
        let key_only = serde_json::from_str::<StoredLogin>(r#"{"apiKey":"stored-key"}"#).unwrap();
        assert_eq!(
            cloud_auth_from_sources(None, Some("  "), Some(&key_only)).unwrap(),
            CloudAuth::Basic("stored-key".to_owned())
        );
        let err = cloud_auth_from_sources(None, None, None).unwrap_err();
        assert!(err.contains("agent login"), "{err}");
    }

    #[test]
    fn auth_file_path_matches_the_cli() {
        let home = std::path::Path::new("/home/u");
        assert_eq!(
            cursor_auth_path_for("linux", home, None, None),
            std::path::PathBuf::from("/home/u/.config/cursor/auth.json")
        );
        assert_eq!(
            cursor_auth_path_for("linux", home, Some(std::path::Path::new("/xdg")), None),
            std::path::PathBuf::from("/xdg/cursor/auth.json")
        );
        assert_eq!(
            cursor_auth_path_for("macos", home, None, None),
            std::path::PathBuf::from("/home/u/.cursor/auth.json")
        );
        assert_eq!(
            cursor_auth_path_for("windows", home, None, Some(std::path::Path::new("/App"))),
            std::path::PathBuf::from("/App/Cursor/auth.json")
        );
        assert_eq!(
            cursor_auth_path_for("windows", home, None, None),
            std::path::PathBuf::from("/home/u/AppData/Roaming/Cursor/auth.json")
        );
    }

    #[test]
    fn login_token_is_sent_as_bearer() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        thread::spawn(move || {
            let (mut sock, _) = listener.accept().unwrap();
            let mut buf = [0_u8; 8192];
            let n = sock.read(&mut buf).unwrap();
            let req = String::from_utf8_lossy(&buf[..n]).to_string();
            let body = if req.contains("Authorization: Bearer login-token")
                && !req.contains("Authorization: Basic")
            {
                r#"{"agent":{"id":"bc-1","url":"https://cursor.com/agents/bc-1"},"run":{"id":"run-1","status":"FINISHED","result":"from login"}}"#
            } else {
                r#"{"error":"missing bearer"}"#
            };
            let status = if body.contains("bc-1") { "200 OK" } else { "401 Unauthorized" };
            let resp = format!(
                "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            sock.write_all(resp.as_bytes()).unwrap();
        });
        let base = format!("http://{addr}");
        let created = dispatch(
            &base,
            &CloudAuth::Bearer("login-token".to_owned()),
            None,
            "refactor the auth module",
            None,
            None,
            None,
            &[],
        )
        .unwrap();
        assert_eq!(created.id, "bc-1");
        assert_eq!(created.result.as_deref(), Some("from login"));
    }

    #[test]
    fn plan_mode_is_sent_and_ask_is_left_off() {
        let created: Value = serde_json::from_str(&create_body(
            "draft it",
            Some(&CloudRepo {
                url: "https://github.com/acme/app".to_owned(),
                starting_ref: None,
            }),
            Some("plan"),
            None,
            &[],
        ))
        .unwrap();
        assert_eq!(created["mode"], "plan");
        assert_eq!(created["prompt"]["text"], "draft it");
        assert_eq!(created["repos"][0]["url"], "https://github.com/acme/app");
        let follow: Value =
            serde_json::from_str(&follow_up_body("ship it", Some("agent"), &[])).unwrap();
        assert_eq!(follow["mode"], "agent");
        assert_eq!(follow["prompt"]["text"], "ship it");
        let ask: Value =
            serde_json::from_str(&create_body("look", None, Some("ask"), None, &[])).unwrap();
        assert!(ask.get("mode").is_none());
        let plain: Value = serde_json::from_str(&follow_up_body("look", None, &[])).unwrap();
        assert!(plain.get("mode").is_none());
    }

    #[test]
    fn a_parameterized_model_id_becomes_cloud_model_params() {
        let body: Value = serde_json::from_str(&create_body(
            "draft it",
            None,
            Some("plan"),
            Some("grok-4.7[context=256k,fast=true]"),
            &[],
        ))
        .unwrap();
        assert_eq!(body["model"]["id"], "grok-4.7");
        assert_eq!(body["model"]["params"][0]["id"], "context");
        assert_eq!(body["model"]["params"][0]["value"], "256k");
        assert_eq!(body["model"]["params"][1]["id"], "fast");
        assert_eq!(body["model"]["params"][1]["value"], "true");
        assert_eq!(body["mode"], "plan");
        let plain: Value =
            serde_json::from_str(&create_body("draft it", None, None, Some("composer-2"), &[]))
                .unwrap();
        assert_eq!(plain["model"]["id"], "composer-2");
        assert!(plain["model"].get("params").is_none());
        let follow: Value =
            serde_json::from_str(&follow_up_body("ship it", None, &[])).unwrap();
        assert!(follow.get("model").is_none());
    }

    #[test]
    fn prompt_images_keep_five_supported_pictures() {
        let with_url = images_from_prompt(Some(&json!([{
            "type": "image",
            "data": "abc",
            "mimeType": "image/jpeg",
            "uri": "https://example.com/a.jpg"
        }])));
        assert_eq!(with_url[0].data, "abc");
        assert!(with_url[0].url.is_none());

        let mut blocks = vec![
            json!({"type": "image", "data": "abc", "mimeType": "image/png"}),
            json!({"type": "image", "data": "zzz", "mimeType": "image/bmp"}),
            json!({"type": "image", "uri": "https://example.com/shot.webp"}),
            json!({"type": "image", "uri": "file:///tmp/shot.png"}),
        ];
        for _ in 0..4 {
            blocks.push(json!({"type": "image", "data": "more", "mimeType": "image/gif"}));
        }
        let images = images_from_prompt(Some(&Value::Array(blocks)));
        assert_eq!(images.len(), 5);
        assert_eq!(images[0].mime_type, "image/png");
        assert_eq!(images[1].url.as_deref(), Some("https://example.com/shot.webp"));
        assert_eq!(images[2].mime_type, "image/gif");

        let body: Value =
            serde_json::from_str(&create_body("fix it", None, None, None, &images[..2])).unwrap();
        assert_eq!(body["prompt"]["images"][0]["data"], "abc");
        assert_eq!(body["prompt"]["images"][0]["mimeType"], "image/png");
        assert!(body["prompt"]["images"][0].get("url").is_none());
        assert_eq!(body["prompt"]["images"][1]["url"], "https://example.com/shot.webp");
        assert!(body["prompt"]["images"][1].get("data").is_none());

        let huge = "A".repeat(MAX_CLOUD_IMAGE_BYTES / 3 * 4 + 16);
        let dropped = images_from_prompt(Some(&json!([{
            "type": "image",
            "data": huge,
            "mimeType": "image/png"
        }])));
        assert!(dropped.is_empty());
    }

    #[test]
    fn git_remotes_become_cloud_repo_urls() {
        assert_eq!(
            normalize_git_remote("git@github.com:acme/payments.git").as_deref(),
            Some("https://github.com/acme/payments")
        );
        assert_eq!(
            normalize_git_remote("ssh://git@github.com/acme/payments.git").as_deref(),
            Some("https://github.com/acme/payments")
        );
        assert_eq!(
            normalize_git_remote("https://user:secret@github.com/acme/payments.git/").as_deref(),
            Some("https://github.com/acme/payments")
        );
        assert_eq!(normalize_git_remote("/tmp/local.git"), None);
    }

    #[test]
    fn a_session_git_repo_is_sent_on_create() {
        let dir = std::env::temp_dir().join(format!("grok-cloud-repo-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let init = std::process::Command::new("git")
            .args(["init", "-b", "feature"])
            .current_dir(&dir)
            .output()
            .unwrap();
        assert!(init.status.success(), "{}", String::from_utf8_lossy(&init.stderr));
        let remote = std::process::Command::new("git")
            .args(["remote", "add", "origin", "git@github.com:acme/payments.git"])
            .current_dir(&dir)
            .status()
            .unwrap();
        assert!(remote.success());
        let repo = repo_from_cwd(&dir).unwrap();
        assert_eq!(repo.url, "https://github.com/acme/payments");
        assert_eq!(repo.starting_ref.as_deref(), Some("feature"));

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        thread::spawn(move || {
            let (mut sock, _) = listener.accept().unwrap();
            let mut buf = [0_u8; 8192];
            let n = sock.read(&mut buf).unwrap();
            let req = String::from_utf8_lossy(&buf[..n]).to_string();
            let ok = req.contains("https://github.com/acme/payments")
                && req.contains("\"startingRef\":\"feature\"");
            let body = if ok {
                r#"{"agent":{"id":"bc-1","url":"https://cursor.com/agents/bc-1"},"run":{"id":"run-1","status":"FINISHED","result":"on the repo"}}"#
            } else {
                r#"{"error":"missing repo"}"#
            };
            let status = if ok { "200 OK" } else { "400 Bad Request" };
            let resp = format!(
                "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            sock.write_all(resp.as_bytes()).unwrap();
        });
        let base = format!("http://{addr}");
        let created = dispatch(
            &base,
            &CloudAuth::Basic("test-key".to_owned()),
            None,
            "refactor the auth module",
            Some(&repo),
            None,
            None,
            &[],
        )
        .unwrap();
        assert_eq!(created.result.as_deref(), Some("on the repo"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
