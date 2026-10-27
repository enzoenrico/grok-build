//! Cursor Cloud Agents API, the same launch, follow, status, and cancel path the Agent SDK and
//! Cursor CLI use. ACP `session/prompt` does not create a cloud agent. A message that starts with
//! `&` and a task is sent here. The pager owns the job list, so one session can have many runs.
//! `& <task>` starts another job, or follows the job `/cloud <id>` focused. `&` and `& status` read
//! that focused job. `& cancel` stops it. A run that is still going is followed on its event stream.
//!
//! A dropped stream resumes with `Last-Event-ID`. `410 stream_expired` is read back with Get A Run.
//! Cancel is a no-op once the run is terminal, including `409 run_not_cancellable`.
//!
//! Auth matches the CLI, then the Agent SDK service account. `CURSOR_AUTH_TOKEN` is Bearer,
//! `CURSOR_API_KEY` and `CURSOR_SERVICE_ACCOUNT_KEY` are Basic, then the access token `agent login` stores.

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
    pub branch: Option<String>,
    pub pr_url: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum CloudEvent {
    Assistant(String),
    Thinking(String),
    ToolDone {
        name: String,
    },
    Result {
        status: String,
        text: String,
        pr_url: Option<String>,
        branch: Option<String>,
    },
    Error {
        message: String,
    },
}

pub(crate) fn run_is_terminal(status: Option<&str>) -> bool {
    matches!(status, Some("FINISHED" | "ERROR" | "CANCELLED" | "EXPIRED"))
}

pub(crate) fn event_text(event: &CloudEvent, saw_assistant: bool) -> Option<String> {
    match event {
        CloudEvent::Assistant(text) | CloudEvent::Thinking(text) => Some(text.clone()),
        CloudEvent::ToolDone { name } => Some(format!("Tool {name}\n")),
        CloudEvent::Result {
            status,
            text,
            pr_url,
            branch: _,
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
            Some(msg)
        }
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

pub(crate) const LOGIN_REQUIRED: &str =
    "Run `agent login`, or set CURSOR_API_KEY, to send this to a Cursor cloud agent.";

pub(crate) fn resolve_cloud_auth() -> Result<CloudAuth, String> {
    let auth_token = std::env::var("CURSOR_AUTH_TOKEN").ok();
    let api_key = std::env::var("CURSOR_API_KEY").ok();
    let service_account_key = std::env::var("CURSOR_SERVICE_ACCOUNT_KEY").ok();
    let stored = read_stored_login()?;
    cloud_auth_from_sources(
        auth_token.as_deref(),
        api_key.as_deref(),
        service_account_key.as_deref(),
        stored.as_ref(),
    )
}

pub(crate) fn cloud_auth_from_sources(
    auth_token: Option<&str>,
    api_key: Option<&str>,
    service_account_key: Option<&str>,
    stored: Option<&StoredLogin>,
) -> Result<CloudAuth, String> {
    if let Some(token) = nonempty(auth_token) {
        return Ok(CloudAuth::Bearer(token.to_owned()));
    }
    if let Some(key) = nonempty(api_key) {
        return Ok(CloudAuth::Basic(key.to_owned()));
    }
    if let Some(key) = nonempty(service_account_key) {
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
        std::env::var("AGENT_CLI_CREDENTIAL_STORE").ok().as_deref(),
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

pub(crate) fn create_body(text: &str, repo: Option<&CloudRepo>, model_id: Option<&str>) -> String {
    let mut body = json!({ "prompt": { "text": text } });
    if let Some(repo) = repo {
        let mut entry = json!({ "url": repo.url });
        if let Some(starting_ref) = &repo.starting_ref {
            entry["startingRef"] = json!(starting_ref);
        }
        body["repos"] = json!([entry]);
    }
    attach_cloud_model(&mut body, model_id);
    body.to_string()
}

/// Same create body as [`create_body`], plus a distinct `branchName` for one swarm member.
fn create_body_on_branch(
    text: &str,
    repo: Option<&CloudRepo>,
    model_id: Option<&str>,
    branch_name: &str,
) -> String {
    let mut body = json!({ "prompt": { "text": text } });
    if let Some(repo) = repo {
        let mut entry = json!({ "url": repo.url });
        if let Some(starting_ref) = &repo.starting_ref {
            entry["startingRef"] = json!(starting_ref);
        }
        entry["branchName"] = json!(branch_name);
        body["repos"] = json!([entry]);
    }
    body["target"] = json!({ "branchName": branch_name });
    attach_cloud_model(&mut body, model_id);
    body.to_string()
}

fn attach_cloud_model(body: &mut Value, model_id: Option<&str>) {
    let Some(model) = model_id.and_then(cloud_model_from_id) else {
        return;
    };
    body["model"] = model;
}

pub(crate) fn cloud_model_from_id(raw: &str) -> Option<Value> {
    let raw = raw.trim();
    if raw.is_empty() {
        return None;
    }
    let Some((id, rest)) = raw.split_once('[') else {
        return Some(json!({ "id": raw }));
    };
    let id = id.trim();
    if id.is_empty() {
        return None;
    }
    let inside = rest.strip_suffix(']').unwrap_or(rest);
    let params: Vec<Value> = inside
        .split(',')
        .filter_map(|pair| {
            let (key, value) = pair.split_once('=')?;
            let key = key.trim();
            let value = value.trim();
            if key.is_empty() || value.is_empty() {
                None
            } else {
                Some(json!({ "id": key, "value": value }))
            }
        })
        .collect();
    if params.is_empty() {
        Some(json!({ "id": id }))
    } else {
        Some(json!({ "id": id, "params": params }))
    }
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

pub(crate) fn follow_up_body(text: &str, model_id: Option<&str>) -> String {
    let mut body = json!({ "prompt": { "text": text } });
    attach_cloud_model(&mut body, model_id);
    body.to_string()
}

pub(crate) fn parse_cloud_response(body: &str) -> Result<CloudAgent, String> {
    let value: Value = serde_json::from_str(body).map_err(|err| err.to_string())?;
    let agent = value.get("agent").unwrap_or(&value);
    let id = agent
        .get("id")
        .and_then(Value::as_str)
        .or_else(|| value.pointer("/run/agentId").and_then(Value::as_str))
        .ok_or_else(|| "cloud agent response has no id".to_owned())?
        .to_owned();
    let url = agent.get("url").and_then(Value::as_str).map(str::to_owned);
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
    let (branch, pr_url) = git_fields(&value);
    Ok(CloudAgent {
        id,
        url,
        run_id,
        run_status,
        result,
        branch,
        pr_url,
    })
}

/// Branch name and pull request URL from a cloud agent or run payload.
fn git_fields(value: &Value) -> (Option<String>, Option<String>) {
    for pointer in ["/run/git/branches", "/git/branches"] {
        let Some(branches) = value.pointer(pointer).and_then(Value::as_array) else {
            continue;
        };
        let branch = branches.iter().find_map(|entry| {
            entry
                .get("branch")
                .and_then(Value::as_str)
                .or_else(|| entry.get("branchName").and_then(Value::as_str))
                .or_else(|| entry.get("name").and_then(Value::as_str))
                .filter(|name| !name.is_empty())
                .map(str::to_owned)
        });
        let pr = branches
            .iter()
            .find_map(|entry| entry.get("prUrl").and_then(Value::as_str))
            .map(str::to_owned);
        if branch.is_some() || pr.is_some() {
            return (branch, pr);
        }
    }
    (None, None)
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
        agent.url = value.get("url").and_then(Value::as_str).map(str::to_owned);
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
    let (branch, pr) = git_fields(&run);
    if agent.branch.is_none() {
        agent.branch = branch;
    }
    if agent.pr_url.is_none() {
        agent.pr_url = pr;
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
        branch: None,
        pr_url: None,
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
    if let Err(err) = request(
        "POST",
        &format!("{api_base}/v1/agents/{}/runs/{run_id}/cancel", agent.id),
        auth,
        None,
    ) {
        if !is_run_not_cancellable(&err) {
            return Err(err);
        }
        let _ = refresh(api_base, auth, &mut agent);
        if !run_is_terminal(agent.run_status.as_deref()) {
            return Err(err);
        }
        return Ok(agent);
    }
    let _ = refresh(api_base, auth, &mut agent);
    Ok(agent)
}

fn is_run_not_cancellable(err: &str) -> bool {
    err.contains("run_not_cancellable")
}

pub(crate) fn dispatch(
    api_base: &str,
    auth: &CloudAuth,
    agent_id: Option<&str>,
    text: &str,
    repo: Option<&CloudRepo>,
    model_id: Option<&str>,
) -> Result<CloudAgent, String> {
    let (url, body) = match agent_id {
        Some(id) => (
            format!("{api_base}/v1/agents/{id}/runs"),
            follow_up_body(text, model_id),
        ),
        None => (
            format!("{api_base}/v1/agents"),
            create_body(text, repo, model_id),
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

/// Create one cloud agent on `branch_name`. A non-2xx response is an error for this member only.
pub(crate) fn create_agent(
    api_base: &str,
    auth: &CloudAuth,
    text: &str,
    repo: Option<&CloudRepo>,
    model_id: Option<&str>,
    branch_name: &str,
) -> Result<CloudAgent, String> {
    let url = format!("{api_base}/v1/agents");
    let body = create_body_on_branch(text, repo, model_id, branch_name);
    let raw = request("POST", &url, auth, Some(&body))?;
    let mut agent = parse_cloud_response(&raw)?;
    let _ = refresh(api_base, auth, &mut agent);
    if agent.branch.is_none() {
        agent.branch = Some(branch_name.to_owned());
    }
    Ok(agent)
}

const STREAM_ATTEMPTS: u32 = 7;

enum StreamStop {
    Result,
    Done,
    Expired,
    Dropped,
}

struct SseFrame {
    id: Option<String>,
    event: Option<CloudEvent>,
    done: bool,
}

/// Follow one run the way `Run.stream()` does. A drop resumes with `Last-Event-ID`.
/// `410 stream_expired` is not retried; the terminal run is read with Get A Run.
pub(crate) fn follow_run(
    api_base: &str,
    auth: &CloudAuth,
    agent_id: &str,
    run_id: &str,
    mut on_event: impl FnMut(CloudEvent),
) -> Result<(), String> {
    let url = format!("{api_base}/v1/agents/{agent_id}/runs/{run_id}/stream");
    let mut last_event_id = None;
    for attempt in 0..STREAM_ATTEMPTS {
        let resume_from = last_event_id.clone();
        let stop = read_stream(
            &url,
            auth,
            resume_from.as_deref(),
            &mut last_event_id,
            &mut on_event,
        )?;
        match stop {
            StreamStop::Result | StreamStop::Done => return Ok(()),
            StreamStop::Expired => {
                return emit_terminal_run(api_base, auth, agent_id, run_id, &mut on_event);
            }
            StreamStop::Dropped => {
                if emit_finished_run(api_base, auth, agent_id, run_id, &mut on_event)? {
                    return Ok(());
                }
                if attempt + 1 == STREAM_ATTEMPTS {
                    break;
                }
                std::thread::sleep(stream_backoff(attempt));
            }
        }
    }
    Err("cloud agent stream dropped before the run finished".to_owned())
}

/// First reconnect is immediate. Later ones use the SDK backoff, capped at 30s.
fn stream_backoff(retry: u32) -> Duration {
    if retry == 0 {
        return Duration::from_millis(0);
    }
    let shift = retry.saturating_sub(1).min(5);
    Duration::from_millis((1_000 * 2u64.pow(shift)).min(30_000))
}

fn read_stream(
    url: &str,
    auth: &CloudAuth,
    last_event_id: Option<&str>,
    resume_id: &mut Option<String>,
    on_event: &mut impl FnMut(CloudEvent),
) -> Result<StreamStop, String> {
    if url.starts_with("https://") {
        follow_via_curl(url, auth, last_event_id, resume_id, on_event)
    } else {
        follow_via_tcp(url, auth, last_event_id, resume_id, on_event)
    }
}

fn follow_via_tcp(
    url: &str,
    auth: &CloudAuth,
    last_event_id: Option<&str>,
    resume_id: &mut Option<String>,
    on_event: &mut impl FnMut(CloudEvent),
) -> Result<StreamStop, String> {
    let mut headers = vec![("Accept", "text/event-stream")];
    if let Some(id) = last_event_id {
        headers.push(("Last-Event-ID", id));
    }
    let (status, body) = exchange_tcp("GET", url, auth, None, &headers)?;
    let code = status_code(&status);
    if code == 410 {
        return Ok(StreamStop::Expired);
    }
    if !(200..300).contains(&code) {
        return Err(format!("{status}: {body}"));
    }
    Ok(consume_sse(&body, resume_id, on_event))
}

fn follow_via_curl(
    url: &str,
    auth: &CloudAuth,
    last_event_id: Option<&str>,
    resume_id: &mut Option<String>,
    on_event: &mut impl FnMut(CloudEvent),
) -> Result<StreamStop, String> {
    let header_path = std::env::temp_dir().join(format!(
        "grok-cloud-stream-{}-{}.hdr",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|elapsed| elapsed.as_nanos())
            .unwrap_or(0)
    ));
    let _header_file = HeaderFile(header_path.clone());
    let mut args = vec![
        "-N".to_owned(),
        "-sS".to_owned(),
        "-D".to_owned(),
        header_path.display().to_string(),
        "-X".to_owned(),
        "GET".to_owned(),
    ];
    args.extend(curl_auth_args(auth));
    args.extend(["-H".to_owned(), "Accept: text/event-stream".to_owned()]);
    if let Some(id) = last_event_id {
        args.extend(["-H".to_owned(), format!("Last-Event-ID: {id}")]);
    }
    args.push(url.to_owned());
    let mut child = std::process::Command::new("curl")
        .args(args)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|err| err.to_string())?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| "curl has no stdout".to_owned())?;
    let mut parser = SseParser::default();
    let mut stop = StreamStop::Dropped;
    for line in BufReader::new(stdout).lines() {
        let line = line.map_err(|err| err.to_string())?;
        if let Some(frame) = parser.push_line(&line)
            && apply_frame(&frame, resume_id, on_event, &mut stop)
        {
            let _ = child.kill();
            let _ = child.wait();
            return Ok(stop);
        }
    }
    if let Some(frame) = parser.finish() {
        let _ = apply_frame(&frame, resume_id, on_event, &mut stop);
    }
    let exit = child.wait().map_err(|err| err.to_string())?;
    let code = std::fs::read_to_string(&header_path)
        .ok()
        .and_then(|headers| status_code_in(&headers));
    if code == Some(410) {
        return Ok(StreamStop::Expired);
    }
    if !matches!(stop, StreamStop::Dropped) {
        return Ok(stop);
    }
    match code {
        Some(code) if (200..300).contains(&code) => Ok(stop),
        Some(code) => Err(format!("cloud agent HTTP {code}")),
        None if exit.success() => Ok(stop),
        None => {
            let mut err = String::new();
            if let Some(mut stderr) = child.stderr.take() {
                let _ = std::io::Read::read_to_string(&mut stderr, &mut err);
            }
            Err(if err.trim().is_empty() {
                format!("curl exited {exit}")
            } else {
                err.trim().to_owned()
            })
        }
    }
}

struct HeaderFile(PathBuf);

impl Drop for HeaderFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

fn consume_sse(
    body: &str,
    resume_id: &mut Option<String>,
    on_event: &mut impl FnMut(CloudEvent),
) -> StreamStop {
    let mut parser = SseParser::default();
    let mut stop = StreamStop::Dropped;
    for line in body.split_inclusive('\n') {
        let line = line.trim_end_matches(['\r', '\n']);
        if let Some(frame) = parser.push_line(line)
            && apply_frame(&frame, resume_id, on_event, &mut stop)
        {
            return stop;
        }
    }
    if let Some(frame) = parser.finish() {
        let _ = apply_frame(&frame, resume_id, on_event, &mut stop);
    }
    stop
}

fn apply_frame(
    frame: &SseFrame,
    resume_id: &mut Option<String>,
    on_event: &mut impl FnMut(CloudEvent),
    stop: &mut StreamStop,
) -> bool {
    if let Some(id) = frame.id.as_deref().filter(|id| !id.is_empty()) {
        *resume_id = Some(id.to_owned());
    }
    if frame.done {
        *stop = StreamStop::Done;
        return true;
    }
    if let Some(event) = frame.event.clone() {
        let finished = matches!(event, CloudEvent::Result { .. });
        on_event(event);
        if finished {
            *stop = StreamStop::Result;
            return true;
        }
    }
    false
}

fn emit_terminal_run(
    api_base: &str,
    auth: &CloudAuth,
    agent_id: &str,
    run_id: &str,
    on_event: &mut impl FnMut(CloudEvent),
) -> Result<(), String> {
    let event = run_result_event(api_base, auth, agent_id, run_id)?;
    let still_running = match &event {
        CloudEvent::Result { status, .. } => !run_is_terminal(Some(status.as_str())),
        _ => true,
    };
    if still_running {
        let status = match &event {
            CloudEvent::Result { status, .. } => status.as_str(),
            _ => "unknown",
        };
        return Err(format!(
            "cloud agent stream expired while the run was still {status}"
        ));
    }
    on_event(event);
    Ok(())
}

/// `Ok(true)` when the run has already finished and its result was emitted.
fn emit_finished_run(
    api_base: &str,
    auth: &CloudAuth,
    agent_id: &str,
    run_id: &str,
    on_event: &mut impl FnMut(CloudEvent),
) -> Result<bool, String> {
    let event = match run_result_event(api_base, auth, agent_id, run_id) {
        Ok(event) => event,
        Err(_) => return Ok(false),
    };
    let CloudEvent::Result { status, .. } = &event else {
        return Ok(false);
    };
    if !run_is_terminal(Some(status.as_str())) {
        return Ok(false);
    }
    on_event(event);
    Ok(true)
}

fn run_result_event(
    api_base: &str,
    auth: &CloudAuth,
    agent_id: &str,
    run_id: &str,
) -> Result<CloudEvent, String> {
    let body = request(
        "GET",
        &format!("{api_base}/v1/agents/{agent_id}/runs/{run_id}"),
        auth,
        None,
    )?;
    let value: Value = serde_json::from_str(&body).map_err(|err| err.to_string())?;
    Ok(result_from_run(&value))
}

fn result_from_run(value: &Value) -> CloudEvent {
    let (branch, pr_url) = git_fields(value);
    CloudEvent::Result {
        status: value
            .get("status")
            .and_then(Value::as_str)
            .unwrap_or("FINISHED")
            .to_owned(),
        text: value
            .get("result")
            .or_else(|| value.get("text"))
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_owned(),
        pr_url,
        branch,
    }
}

fn status_code(status_line: &str) -> u16 {
    status_code_in(status_line).unwrap_or(0)
}

fn status_code_in(headers: &str) -> Option<u16> {
    headers.lines().rev().find_map(|line| {
        let line = line.trim();
        if !line.starts_with("HTTP/") {
            return None;
        }
        line.split_whitespace().nth(1)?.parse().ok()
    })
}

#[derive(Default)]
struct SseParser {
    event: String,
    data: String,
    id: Option<String>,
}

impl SseParser {
    fn push_line(&mut self, line: &str) -> Option<SseFrame> {
        if line.is_empty() {
            return self.finish();
        }
        if let Some(rest) = line.strip_prefix("id:") {
            self.id = Some(rest.trim().to_owned());
        } else if let Some(rest) = line.strip_prefix("event:") {
            self.event = rest.trim().to_owned();
        } else if let Some(rest) = line.strip_prefix("data:") {
            if !self.data.is_empty() {
                self.data.push('\n');
            }
            self.data.push_str(rest.trim_start());
        }
        None
    }

    fn finish(&mut self) -> Option<SseFrame> {
        let event_name = std::mem::take(&mut self.event);
        let data = std::mem::take(&mut self.data);
        let id = self.id.take();
        if event_name.is_empty() && data.is_empty() && id.is_none() {
            return None;
        }
        if event_name == "done" {
            return Some(SseFrame {
                id,
                event: None,
                done: true,
            });
        }
        let event = if event_name.is_empty() && data.is_empty() {
            None
        } else {
            decode_event(&event_name, &data)
        };
        if event.is_none() && id.is_none() {
            None
        } else {
            Some(SseFrame {
                id,
                event,
                done: false,
            })
        }
    }
}

fn decode_event(event: &str, data: &str) -> Option<CloudEvent> {
    let value: Value = serde_json::from_str(data).unwrap_or(Value::Null);
    match event {
        "assistant" => value
            .get("text")
            .and_then(Value::as_str)
            .map(|text| CloudEvent::Assistant(text.to_owned())),
        "thinking" => value
            .get("text")
            .and_then(Value::as_str)
            .filter(|text| !text.is_empty())
            .map(|text| CloudEvent::Thinking(text.to_owned())),
        "tool_call" => {
            let name = value
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or("tool")
                .to_owned();
            match value.get("status").and_then(Value::as_str) {
                Some("completed") => Some(CloudEvent::ToolDone { name }),
                Some("error") => Some(CloudEvent::ToolDone {
                    name: format!("{name} failed"),
                }),
                _ => None,
            }
        }
        "result" => Some(result_from_run(&value)),
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
    let (status, payload) = exchange_tcp(method, url, auth, body, &[])?;
    if !status.contains(" 2") {
        return Err(format!("{status}: {payload}"));
    }
    Ok(payload)
}

fn exchange_tcp(
    method: &str,
    url: &str,
    auth: &CloudAuth,
    body: Option<&str>,
    extra_headers: &[(&str, &str)],
) -> Result<(String, String), String> {
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
    let mut extra = String::new();
    for (name, value) in extra_headers {
        extra.push_str(name);
        extra.push_str(": ");
        extra.push_str(value);
        extra.push_str("\r\n");
    }
    let request = format!(
        "{method} {path} HTTP/1.1\r\nHost: {host_port}\r\nAuthorization: {header}\r\n{extra}Content-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    stream
        .write_all(request.as_bytes())
        .map_err(|err| err.to_string())?;
    let mut raw = Vec::new();
    stream
        .read_to_end(&mut raw)
        .map_err(|err| err.to_string())?;
    let text = String::from_utf8_lossy(&raw);
    let (head, payload) = text
        .split_once("\r\n\r\n")
        .ok_or_else(|| "cloud API response has no body".to_owned())?;
    let status = head
        .lines()
        .next()
        .ok_or_else(|| "cloud API response has no status".to_owned())?
        .to_owned();
    Ok((status, payload.to_owned()))
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
        let created = dispatch(
            &base,
            &CloudAuth::Basic("test-key".to_owned()),
            None,
            "refactor the auth module",
            None,
            None,
        )
        .unwrap();
        assert_eq!(created.id, "bc-1");
        assert_eq!(
            created.url.as_deref(),
            Some("https://cursor.com/agents/bc-1")
        );
        assert_eq!(created.run_status.as_deref(), Some("FINISHED"));
        assert_eq!(created.result.as_deref(), Some("Refactored auth."));
        let follow = dispatch(
            &base,
            &CloudAuth::Basic("test-key".to_owned()),
            Some("bc-1"),
            "add tests",
            None,
            None,
        )
        .unwrap();
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
        let agent =
            refresh_existing(&base, &CloudAuth::Basic("test-key".to_owned()), "bc-1").unwrap();
        assert_eq!(agent.id, "bc-1");
        assert_eq!(agent.url.as_deref(), Some("https://cursor.com/agents/bc-1"));
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
        let created = dispatch(
            &base,
            &CloudAuth::Basic("test-key".to_owned()),
            None,
            "update the readme",
            None,
            None,
        )
        .unwrap();
        assert_eq!(created.run_status.as_deref(), Some("RUNNING"));
        assert_eq!(created.result, None);
        assert!(!run_is_terminal(created.run_status.as_deref()));
        let mut events = Vec::new();
        follow_run(
            &base,
            &CloudAuth::Basic("test-key".to_owned()),
            "bc-1",
            "run-1",
            |event| events.push(event),
        )
        .unwrap();
        assert_eq!(
            events,
            vec![
                CloudEvent::Assistant("I'll update the README now.".to_owned()),
                CloudEvent::ToolDone {
                    name: "read_file".to_owned()
                },
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
                "Tool read_file\n".to_owned(),
                "Run FINISHED.\nhttps://github.com/acme/app/pull/123\n".to_owned(),
            ]
        );
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
        let agent =
            cancel_existing(&base, &CloudAuth::Basic("test-key".to_owned()), "bc-1").unwrap();
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
        let agent =
            cancel_existing(&base, &CloudAuth::Basic("test-key".to_owned()), "bc-1").unwrap();
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
        let agent = dispatch(
            &base,
            &CloudAuth::Basic("test-key".to_owned()),
            Some("bc-1"),
            "add tests",
            None,
            None,
        )
        .unwrap();
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
            cloud_auth_from_sources(
                Some("env-token"),
                Some("env-key"),
                Some("sa-key"),
                Some(&stored)
            )
            .unwrap(),
            CloudAuth::Bearer("env-token".to_owned())
        );
        assert_eq!(
            cloud_auth_from_sources(None, Some("env-key"), Some("sa-key"), Some(&stored)).unwrap(),
            CloudAuth::Basic("env-key".to_owned())
        );
        assert_eq!(
            cloud_auth_from_sources(None, None, Some("sa-key"), Some(&stored)).unwrap(),
            CloudAuth::Basic("sa-key".to_owned())
        );
        assert_eq!(
            cloud_auth_from_sources(None, None, None, Some(&stored)).unwrap(),
            CloudAuth::Bearer("login-token".to_owned())
        );
        let key_only = serde_json::from_str::<StoredLogin>(r#"{"apiKey":"stored-key"}"#).unwrap();
        assert_eq!(
            cloud_auth_from_sources(None, Some("  "), None, Some(&key_only)).unwrap(),
            CloudAuth::Basic("stored-key".to_owned())
        );
        let err = cloud_auth_from_sources(None, None, None, None).unwrap_err();
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
            let status = if body.contains("bc-1") {
                "200 OK"
            } else {
                "401 Unauthorized"
            };
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
        )
        .unwrap();
        assert_eq!(created.id, "bc-1");
        assert_eq!(created.result.as_deref(), Some("from login"));
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
        assert!(
            init.status.success(),
            "{}",
            String::from_utf8_lossy(&init.stderr)
        );
        let remote = std::process::Command::new("git")
            .args([
                "remote",
                "add",
                "origin",
                "git@github.com:acme/payments.git",
            ])
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
                && req.contains("\"startingRef\":\"feature\"")
                && req.contains("\"id\":\"grok-4.7\"")
                && req.contains("\"value\":\"256k\"");
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
            Some("grok-4.7[context=256k,fast=true]"),
        )
        .unwrap();
        assert_eq!(created.result.as_deref(), Some("on the repo"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn cursor_model_ids_turn_into_cloud_model_params() {
        assert_eq!(
            cloud_model_from_id("grok-4.7[context=256k,fast=true]"),
            Some(json!({
                "id": "grok-4.7",
                "params": [
                    { "id": "context", "value": "256k" },
                    { "id": "fast", "value": "true" }
                ]
            }))
        );
        assert_eq!(
            cloud_model_from_id("claude-opus-5-5"),
            Some(json!({ "id": "claude-opus-5-5" }))
        );
        assert_eq!(cloud_model_from_id("  "), None);
    }

    #[test]
    fn active_cloud_runs_stay_open_until_they_terminate() {
        assert!(!run_is_terminal(Some("CREATING")));
        assert!(!run_is_terminal(Some("RUNNING")));
        assert!(!run_is_terminal(Some("WAITING_FOR_BACKGROUND_WORK")));
        assert!(run_is_terminal(Some("FINISHED")));
        assert!(run_is_terminal(Some("ERROR")));
        assert!(run_is_terminal(Some("CANCELLED")));
        assert!(run_is_terminal(Some("EXPIRED")));
    }

    #[test]
    fn sdk_stream_frames_show_thinking_and_a_failed_tool() {
        let thinking = decode_event("thinking", r#"{"text":"Checking auth."}"#).unwrap();
        assert_eq!(
            event_text(&thinking, false).as_deref(),
            Some("Checking auth.")
        );
        let failed = decode_event(
            "tool_call",
            r#"{"callId":"call-1","name":"read_file","status":"error"}"#,
        )
        .unwrap();
        assert_eq!(
            event_text(&failed, false).as_deref(),
            Some("Tool read_file failed\n")
        );
        assert_eq!(
            decode_event(
                "tool_call",
                r#"{"callId":"call-1","name":"read_file","status":"running"}"#,
            ),
            None
        );
    }

    #[test]
    fn status_reads_the_pushed_branch_and_pull_request() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        thread::spawn(move || {
            let replies = [
                r#"{"id":"bc-1","url":"https://cursor.com/agents/bc-1","status":"IDLE","latestRunId":"run-1"}"#,
                r#"{"id":"run-1","agentId":"bc-1","status":"FINISHED","result":"Added README.md with installation instructions.","git":{"branches":[{"repoUrl":"github.com/acme/app","branch":"cursor/add-readme-a1b2","prUrl":"https://github.com/acme/app/pull/123"}]}}"#,
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
        let agent =
            refresh_existing(&base, &CloudAuth::Basic("test-key".to_owned()), "bc-1").unwrap();
        assert_eq!(agent.run_status.as_deref(), Some("FINISHED"));
        assert_eq!(
            agent.result.as_deref(),
            Some("Added README.md with installation instructions.")
        );
        assert_eq!(agent.branch.as_deref(), Some("cursor/add-readme-a1b2"));
        assert_eq!(
            agent.pr_url.as_deref(),
            Some("https://github.com/acme/app/pull/123")
        );
    }

    #[test]
    fn a_dropped_stream_resumes_with_the_last_event_id() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        thread::spawn(move || {
            for turn in 0..3 {
                let (mut sock, _) = listener.accept().unwrap();
                let mut buf = [0_u8; 8192];
                let n = sock.read(&mut buf).unwrap();
                let req = String::from_utf8_lossy(&buf[..n]);
                let body = match turn {
                    0 => {
                        assert!(req.contains("/stream"), "{req}");
                        assert!(!req.contains("Last-Event-ID"), "{req}");
                        "id: 1713033000000-0\nevent: assistant\ndata: {\"text\":\"I'll update the README now.\"}\n\n"
                    }
                    1 => {
                        assert!(req.starts_with("GET "), "{req}");
                        assert!(req.contains("/runs/run-1"), "{req}");
                        assert!(!req.contains("/stream"), "{req}");
                        r#"{"id":"run-1","agentId":"bc-1","status":"RUNNING"}"#
                    }
                    _ => {
                        assert!(req.contains("Last-Event-ID: 1713033000000-0"), "{req}");
                        "event: result\ndata: {\"status\":\"FINISHED\",\"text\":\"Added README.md.\",\"git\":{\"branches\":[{\"branch\":\"cursor/add-readme-a1b2\",\"prUrl\":\"https://github.com/acme/app/pull/123\"}]}}\n\n"
                    }
                };
                let resp = format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                sock.write_all(resp.as_bytes()).unwrap();
            }
        });
        let base = format!("http://{addr}");
        let mut events = Vec::new();
        follow_run(
            &base,
            &CloudAuth::Basic("test-key".to_owned()),
            "bc-1",
            "run-1",
            |event| events.push(event),
        )
        .unwrap();
        assert_eq!(
            events,
            vec![
                CloudEvent::Assistant("I'll update the README now.".to_owned()),
                CloudEvent::Result {
                    status: "FINISHED".to_owned(),
                    text: "Added README.md.".to_owned(),
                    pr_url: Some("https://github.com/acme/app/pull/123".to_owned()),
                    branch: Some("cursor/add-readme-a1b2".to_owned()),
                },
            ]
        );
    }

    #[test]
    fn an_expired_stream_reads_the_finished_run() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        thread::spawn(move || {
            for turn in 0..2 {
                let (mut sock, _) = listener.accept().unwrap();
                let mut buf = [0_u8; 8192];
                let n = sock.read(&mut buf).unwrap();
                let req = String::from_utf8_lossy(&buf[..n]);
                let (status, body) = if turn == 0 {
                    assert!(req.contains("/stream"), "{req}");
                    (
                        "410 Gone",
                        r#"{"code":"stream_expired","message":"stream expired"}"#,
                    )
                } else {
                    assert!(!req.contains("/stream"), "{req}");
                    (
                        "200 OK",
                        r#"{"id":"run-1","agentId":"bc-1","status":"FINISHED","result":"Added README.md with installation instructions.","git":{"branches":[{"repoUrl":"github.com/acme/app","branch":"cursor/add-readme-a1b2","prUrl":"https://github.com/acme/app/pull/123"}]}}"#,
                    )
                };
                let resp = format!(
                    "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                sock.write_all(resp.as_bytes()).unwrap();
            }
        });
        let base = format!("http://{addr}");
        let mut events = Vec::new();
        follow_run(
            &base,
            &CloudAuth::Basic("test-key".to_owned()),
            "bc-1",
            "run-1",
            |event| events.push(event),
        )
        .unwrap();
        assert_eq!(
            events,
            vec![CloudEvent::Result {
                status: "FINISHED".to_owned(),
                text: "Added README.md with installation instructions.".to_owned(),
                pr_url: Some("https://github.com/acme/app/pull/123".to_owned()),
                branch: Some("cursor/add-readme-a1b2".to_owned()),
            }]
        );
    }

    #[test]
    fn cancel_of_an_uncancellable_run_reads_the_finished_run() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        thread::spawn(move || {
            let steps = [
                (
                    "GET",
                    "200 OK",
                    r#"{"id":"bc-1","url":"https://cursor.com/agents/bc-1","status":"ACTIVE","latestRunId":"run-1"}"#,
                ),
                (
                    "GET",
                    "200 OK",
                    r#"{"id":"run-1","agentId":"bc-1","status":"RUNNING"}"#,
                ),
                (
                    "POST",
                    "409 Conflict",
                    r#"{"code":"run_not_cancellable","message":"run is already finished"}"#,
                ),
                (
                    "GET",
                    "200 OK",
                    r#"{"id":"bc-1","url":"https://cursor.com/agents/bc-1","status":"IDLE","latestRunId":"run-1"}"#,
                ),
                (
                    "GET",
                    "200 OK",
                    r#"{"id":"run-1","agentId":"bc-1","status":"FINISHED","result":"Already done."}"#,
                ),
            ];
            for (method, status, body) in steps {
                let (mut sock, _) = listener.accept().unwrap();
                let mut buf = [0_u8; 8192];
                let n = sock.read(&mut buf).unwrap();
                let req = String::from_utf8_lossy(&buf[..n]);
                let ok = req.starts_with(method);
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
        let agent =
            cancel_existing(&base, &CloudAuth::Basic("test-key".to_owned()), "bc-1").unwrap();
        assert_eq!(agent.run_status.as_deref(), Some("FINISHED"));
        assert_eq!(agent.result.as_deref(), Some("Already done."));
    }

    #[test]
    fn service_account_key_is_sent_as_basic() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let expected = authorization_header(&CloudAuth::Basic("sa-key".to_owned()));
        thread::spawn(move || {
            let (mut sock, _) = listener.accept().unwrap();
            let mut buf = [0_u8; 8192];
            let n = sock.read(&mut buf).unwrap();
            let req = String::from_utf8_lossy(&buf[..n]).to_string();
            let body = if req.contains(&expected) && !req.contains("Authorization: Bearer") {
                r#"{"agent":{"id":"bc-1","url":"https://cursor.com/agents/bc-1"},"run":{"id":"run-1","status":"FINISHED","result":"from service account"}}"#
            } else {
                r#"{"error":"missing basic"}"#
            };
            let status = if body.contains("bc-1") {
                "200 OK"
            } else {
                "401 Unauthorized"
            };
            let resp = format!(
                "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            sock.write_all(resp.as_bytes()).unwrap();
        });
        let base = format!("http://{addr}");
        let auth = cloud_auth_from_sources(None, None, Some("sa-key"), None).unwrap();
        let created = dispatch(&base, &auth, None, "refactor the auth module", None, None).unwrap();
        assert_eq!(created.result.as_deref(), Some("from service account"));
    }
}
