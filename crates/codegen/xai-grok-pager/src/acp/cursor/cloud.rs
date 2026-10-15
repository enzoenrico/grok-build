//! Cursor Cloud Agents HTTP API. ACP `session/prompt` does not create a cloud agent.
//! A message that starts with `&` and a task is sent here. `&` and `& status` only read the latest run.
//! `& cancel` stops that run. A run that is still going is followed on its event stream.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::time::Duration;

use serde_json::{Value, json};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CloudAgent {
    pub id: String,
    pub url: Option<String>,
    pub run_id: Option<String>,
    pub run_status: Option<String>,
    pub result: Option<String>,
    pub pr_url: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum CloudEvent {
    Assistant(String),
    ToolDone { name: String },
    Result {
        status: String,
        text: String,
        pr_url: Option<String>,
    },
    Error { message: String },
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
        CloudEvent::ToolDone { name } => Some(format!("Tool {name}\n")),
        CloudEvent::Result {
            status,
            text,
            pr_url,
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

pub(crate) fn handoff_text(prompt: &str) -> Option<String> {
    let rest = prompt.trim().strip_prefix('&')?.trim();
    if rest.is_empty() {
        None
    } else {
        Some(rest.to_owned())
    }
}

pub(crate) fn create_body(text: &str) -> String {
    json!({ "prompt": { "text": text } }).to_string()
}

pub(crate) fn follow_up_body(text: &str) -> String {
    json!({ "prompt": { "text": text } }).to_string()
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
    })
}

fn pr_url(value: &Value) -> Option<String> {
    for pointer in ["/run/git/branches", "/git/branches"] {
        let Some(branches) = value.pointer(pointer).and_then(Value::as_array) else {
            continue;
        };
        if let Some(url) = branches
            .iter()
            .find_map(|branch| branch.get("prUrl").and_then(Value::as_str))
        {
            return Some(url.to_owned());
        }
    }
    None
}

fn refresh(api_base: &str, api_key: &str, agent: &mut CloudAgent) -> Result<(), String> {
    let record = request(
        "GET",
        &format!("{api_base}/v1/agents/{}", agent.id),
        api_key,
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
        api_key,
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
    if agent.pr_url.is_none() {
        agent.pr_url = pr_url(&run);
    }
    Ok(())
}

pub(crate) fn refresh_existing(
    api_base: &str,
    api_key: &str,
    agent_id: &str,
) -> Result<CloudAgent, String> {
    let mut agent = CloudAgent {
        id: agent_id.to_owned(),
        url: None,
        run_id: None,
        run_status: None,
        result: None,
        pr_url: None,
    };
    refresh(api_base, api_key, &mut agent)?;
    Ok(agent)
}

pub(crate) fn cancel_existing(
    api_base: &str,
    api_key: &str,
    agent_id: &str,
) -> Result<CloudAgent, String> {
    let mut agent = refresh_existing(api_base, api_key, agent_id)?;
    if run_is_terminal(agent.run_status.as_deref()) {
        return Ok(agent);
    }
    let Some(run_id) = agent.run_id.clone() else {
        return Err("cloud agent has no run to cancel".to_owned());
    };
    let _ = request(
        "POST",
        &format!("{api_base}/v1/agents/{}/runs/{run_id}/cancel", agent.id),
        api_key,
        None,
    )?;
    let _ = refresh(api_base, api_key, &mut agent);
    Ok(agent)
}

pub(crate) fn dispatch(
    api_base: &str,
    api_key: &str,
    agent_id: Option<&str>,
    text: &str,
) -> Result<CloudAgent, String> {
    let (url, body) = match agent_id {
        Some(id) => (
            format!("{api_base}/v1/agents/{id}/runs"),
            follow_up_body(text),
        ),
        None => (format!("{api_base}/v1/agents"), create_body(text)),
    };
    let raw = match request("POST", &url, api_key, Some(&body)) {
        Ok(raw) => raw,
        Err(err) if is_agent_busy(&err) => {
            let Some(id) = agent_id else {
                return Err(err);
            };
            let mut agent = refresh_existing(api_base, api_key, id)?;
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
    let _ = refresh(api_base, api_key, &mut agent);
    Ok(agent)
}

pub(crate) fn follow_run(
    api_base: &str,
    api_key: &str,
    agent_id: &str,
    run_id: &str,
    on_event: impl FnMut(CloudEvent),
) -> Result<(), String> {
    let url = format!("{api_base}/v1/agents/{agent_id}/runs/{run_id}/stream");
    if url.starts_with("https://") {
        return follow_via_curl(&url, api_key, on_event);
    }
    let body = request("GET", &url, api_key, None)?;
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

fn decode_event(event: &str, data: &str) -> Option<CloudEvent> {
    let value: Value = serde_json::from_str(data).unwrap_or(Value::Null);
    match event {
        "assistant" => value
            .get("text")
            .and_then(Value::as_str)
            .map(|text| CloudEvent::Assistant(text.to_owned())),
        "tool_call" => {
            if value.get("status").and_then(Value::as_str) == Some("completed") {
                Some(CloudEvent::ToolDone {
                    name: value
                        .get("name")
                        .and_then(Value::as_str)
                        .unwrap_or("tool")
                        .to_owned(),
                })
            } else {
                None
            }
        }
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
    api_key: &str,
    mut on_event: impl FnMut(CloudEvent),
) -> Result<(), String> {
    let mut child = std::process::Command::new("curl")
        .args([
            "-N",
            "-sS",
            "-f",
            "-X",
            "GET",
            "-u",
            &format!("{api_key}:"),
            "-H",
            "Accept: text/event-stream",
            url,
        ])
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
    api_key: &str,
    body: Option<&str>,
) -> Result<String, String> {
    if url.starts_with("https://") {
        return request_via_curl(method, url, api_key, body);
    }
    request_via_tcp(method, url, api_key, body)
}

fn request_via_curl(
    method: &str,
    url: &str,
    api_key: &str,
    body: Option<&str>,
) -> Result<String, String> {
    let mut cmd = std::process::Command::new("curl");
    cmd.args([
        "-sS",
        "-X",
        method,
        "-u",
        &format!("{api_key}:"),
        "-H",
        "Accept: application/json",
        "-w",
        "\n%{http_code}",
        url,
    ]);
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
    api_key: &str,
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
    let token = basic_token(api_key);
    let body = body.unwrap_or("");
    let request = format!(
        "{method} {path} HTTP/1.1\r\nHost: {host_port}\r\nAuthorization: Basic {token}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
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
    use base64::Engine;
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
        let created = dispatch(&base, "test-key", None, "refactor the auth module").unwrap();
        assert_eq!(created.id, "bc-1");
        assert_eq!(
            created.url.as_deref(),
            Some("https://cursor.com/agents/bc-1")
        );
        assert_eq!(created.run_status.as_deref(), Some("FINISHED"));
        assert_eq!(created.result.as_deref(), Some("Refactored auth."));
        let follow = dispatch(&base, "test-key", Some("bc-1"), "add tests").unwrap();
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
        let agent = refresh_existing(&base, "test-key", "bc-1").unwrap();
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
        let created = dispatch(&base, "test-key", None, "update the readme").unwrap();
        assert_eq!(created.run_status.as_deref(), Some("RUNNING"));
        assert_eq!(created.result, None);
        assert!(!run_is_terminal(created.run_status.as_deref()));
        let mut events = Vec::new();
        follow_run(&base, "test-key", "bc-1", "run-1", |event| events.push(event)).unwrap();
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
        let agent = cancel_existing(&base, "test-key", "bc-1").unwrap();
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
        let agent = cancel_existing(&base, "test-key", "bc-1").unwrap();
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
        let agent = dispatch(&base, "test-key", Some("bc-1"), "add tests").unwrap();
        assert_eq!(agent.id, "bc-1");
        assert_eq!(agent.run_status.as_deref(), Some("RUNNING"));
        assert_eq!(
            agent.result.as_deref(),
            Some(
                "Still running, so this task was not sent. Send & status to read it, or & cancel to stop it."
            )
        );
    }
}
