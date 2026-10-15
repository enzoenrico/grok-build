//! Cursor Cloud Agents HTTP API. ACP `session/prompt` does not create a cloud agent.
//! A message that starts with `&` is sent here instead.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::time::Duration;

use serde_json::{Value, json};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CloudAgent {
    pub id: String,
    pub url: Option<String>,
    pub run_status: Option<String>,
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
    let run_status = value
        .pointer("/run/status")
        .and_then(Value::as_str)
        .map(str::to_owned);
    Ok(CloudAgent {
        id,
        url,
        run_status,
    })
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
    let raw = post_json(&url, api_key, &body)?;
    parse_cloud_response(&raw)
}

pub(crate) fn post_json(url: &str, api_key: &str, body: &str) -> Result<String, String> {
    if url.starts_with("https://") {
        return post_via_curl(url, api_key, body);
    }
    post_via_tcp(url, api_key, body)
}

fn post_via_curl(url: &str, api_key: &str, body: &str) -> Result<String, String> {
    let output = std::process::Command::new("curl")
        .args([
            "-sS",
            "-u",
            &format!("{api_key}:"),
            "-H",
            "Content-Type: application/json",
            "-H",
            "Accept: application/json",
            "-d",
            body,
            "-w",
            "\n%{http_code}",
            url,
        ])
        .output()
        .map_err(|err| err.to_string())?;
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

fn post_via_tcp(url: &str, api_key: &str, body: &str) -> Result<String, String> {
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
    let request = format!(
        "POST {path} HTTP/1.1\r\nHost: {host_port}\r\nAuthorization: Basic {token}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
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
            for expected in ["/v1/agents", "/v1/agents/bc-1/runs"] {
                let (mut sock, _) = listener.accept().unwrap();
                let mut buf = [0_u8; 8192];
                let n = sock.read(&mut buf).unwrap();
                let req = String::from_utf8_lossy(&buf[..n]);
                assert!(
                    req.starts_with(&format!("POST {expected} ")),
                    "{req}"
                );
                assert!(req.contains("refactor") || req.contains("add tests"));
                let body = if expected.ends_with("/runs") {
                    r#"{"run":{"id":"run-2","agentId":"bc-1","status":"CREATING"}}"#
                } else {
                    r#"{"agent":{"id":"bc-1","url":"https://cursor.com/agents/bc-1","status":"ACTIVE"},"run":{"id":"run-1","status":"CREATING"}}"#
                };
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
        let follow = dispatch(&base, "test-key", Some("bc-1"), "add tests").unwrap();
        assert_eq!(follow.id, "bc-1");
        assert_eq!(follow.run_status.as_deref(), Some("CREATING"));
    }
}
