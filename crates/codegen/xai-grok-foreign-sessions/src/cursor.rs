//! Cursor Agent CLI and Agents-window sessions.
//!
//! Two stores, both under `${CURSOR_CONFIG_DIR:-~/.cursor}`:
//! - `projects/<slug>/agent-transcripts/<id>/<id>.jsonl` — transcript the CLI and the Agents window share
//! - `chats/<md5(cwd)>/<id>/meta.json` — CLI chat metadata (`agent --resume <id>`)
//!
//! IDE sidepane blobs in `state.vscdb` are not listed. The Cursor CLI cannot resume those ids.

use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use serde_json::Value;

use super::{
    ApprovedRoot, ForeignSessionSource, ForeignSessionSummary, ForeignSessionTool, MAX_SESSION_AGE,
    MAX_SESSIONS_PER_TOOL, RecentCandidate, RecentProbe, approved_root_for_recent, is_within,
    normalize_title,
};

const MAX_HASH_DIRS: usize = 32;
const MAX_CHATS_PER_HASH: usize = 64;
const TITLE_READ_BYTES: usize = 64 * 1024;
const TRANSCRIPT_READ_BYTES: usize = 1024 * 1024;
const MAX_TRANSCRIPT_TURNS: usize = 120;
const MAX_TURN_CHARS: usize = 8_000;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CursorTranscriptTurn {
    pub user: bool,
    pub text: String,
}

pub(super) fn scan(cwd: &Path, now: SystemTime) -> Vec<ForeignSessionSummary> {
    let Some(config_dir) = config_dir() else {
        return Vec::new();
    };
    scan_in(&config_dir, cwd, now)
}

pub(super) fn most_recent(
    cwd: &Path,
    now: SystemTime,
    within: Duration,
) -> RecentProbe<RecentCandidate> {
    let Some(config_dir) = config_dir() else {
        return RecentProbe::Complete(None);
    };
    match approved_root_for_recent(&config_dir) {
        Ok(None) => RecentProbe::Complete(None),
        Err(()) => RecentProbe::Incomplete,
        Ok(Some(_)) => {
            let winner = scan_in(&config_dir, cwd, now)
                .into_iter()
                .filter(|session| is_within(session.updated_at, now, within))
                .min_by(|left, right| {
                    right
                        .updated_at
                        .cmp(&left.updated_at)
                        .then_with(|| left.native_id.cmp(&right.native_id))
                });
            RecentProbe::Complete(winner.map(|session| RecentCandidate {
                tool: ForeignSessionTool::Cursor,
                source: session.source,
                native_id: session.native_id,
                updated_at: session.updated_at,
            }))
        }
    }
}

pub fn read_transcript(cwd: &Path, session_id: &str) -> Vec<CursorTranscriptTurn> {
    if uuid::Uuid::try_parse(session_id).is_err() {
        return Vec::new();
    }
    let Some(config_dir) = config_dir() else {
        return Vec::new();
    };
    let Some(root) = ApprovedRoot::new(&config_dir) else {
        return Vec::new();
    };
    let Some(path) = find_transcript(&root, cwd, session_id) else {
        return Vec::new();
    };
    let Some(mut opened) = root.open_regular_file(&path) else {
        return Vec::new();
    };
    read_turns(&mut opened.file)
}

pub(super) fn scan_in(
    config_dir: &Path,
    cwd: &Path,
    now: SystemTime,
) -> Vec<ForeignSessionSummary> {
    let Some(root) = ApprovedRoot::new(config_dir) else {
        return Vec::new();
    };
    let mut sessions = scan_transcripts(&root, cwd, now);
    sessions.extend(scan_chats(&root, cwd, now));
    sessions
}

fn config_dir() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os("CURSOR_CONFIG_DIR") {
        let path = PathBuf::from(dir);
        if !path.as_os_str().is_empty() {
            return Some(path);
        }
    }
    xai_dirs::home_dir().map(|home| home.join(".cursor"))
}

fn scan_transcripts(
    root: &ApprovedRoot,
    cwd: &Path,
    now: SystemTime,
) -> Vec<ForeignSessionSummary> {
    let mut sessions = Vec::new();
    for slug in slug_candidates(cwd) {
        let dir = root.join("projects").join(&slug).join("agent-transcripts");
        let Some(transcripts) = root.subroot(&dir) else {
            continue;
        };
        let mut names = Vec::new();
        transcripts.for_each_entry_bounded(MAX_SESSIONS_PER_TOOL, |name| {
            names.push(name.to_string_lossy().into_owned());
        });
        for name in names {
            if name == "subagents" || uuid::Uuid::try_parse(&name).is_err() {
                continue;
            }
            let nested = transcripts.join(&name).join(format!("{name}.jsonl"));
            let flat = transcripts.join(format!("{name}.jsonl"));
            let path = if transcripts.resolve_regular_file(&nested).is_some() {
                nested
            } else {
                flat
            };
            if let Some(summary) = transcript_summary(&transcripts, &path, &name, cwd, now) {
                sessions.push(summary);
            }
        }
    }
    sessions
}

fn scan_chats(root: &ApprovedRoot, cwd: &Path, now: SystemTime) -> Vec<ForeignSessionSummary> {
    let chats_path = root.join("chats");
    let Some(chats) = root.subroot(&chats_path) else {
        return Vec::new();
    };
    let mut names = Vec::new();
    chats.for_each_entry_bounded(MAX_HASH_DIRS, |name| {
        names.push(name.to_string_lossy().into_owned());
    });
    let wanted = hash_candidates(cwd);
    let focused: Vec<String> = names
        .iter()
        .filter(|name| wanted.iter().any(|hash| hash == *name))
        .cloned()
        .collect();
    let names = if focused.is_empty() { names } else { focused };
    let mut sessions = Vec::new();
    for name in names.into_iter().take(MAX_HASH_DIRS) {
        let dir = chats.join(&name);
        let Some(hash_root) = chats.subroot(&dir) else {
            continue;
        };
        let mut ids = Vec::new();
        hash_root.for_each_entry_bounded(MAX_CHATS_PER_HASH, |id| {
            ids.push(id.to_string_lossy().into_owned());
        });
        for id in ids {
            if uuid::Uuid::try_parse(&id).is_err() {
                continue;
            }
            if let Some(summary) = chat_summary(&hash_root, &id, cwd, now) {
                sessions.push(summary);
            }
        }
    }
    sessions
}

fn transcript_summary(
    root: &ApprovedRoot,
    path: &Path,
    id: &str,
    cwd: &Path,
    now: SystemTime,
) -> Option<ForeignSessionSummary> {
    let mut opened = root.open_regular_file(path)?;
    let modified = opened.metadata.modified().ok()?;
    if !is_within(modified, now, MAX_SESSION_AGE) {
        return None;
    }
    let title = first_user_text(&mut opened.file, TITLE_READ_BYTES)?;
    Some(ForeignSessionSummary {
        tool: ForeignSessionTool::Cursor,
        source: ForeignSessionSource::CursorCli,
        native_id: id.to_owned(),
        title,
        cwd: cwd.to_path_buf(),
        updated_at: modified,
        branch: None,
    })
}

fn chat_summary(
    hash_root: &ApprovedRoot,
    id: &str,
    cwd: &Path,
    now: SystemTime,
) -> Option<ForeignSessionSummary> {
    let chat_dir = hash_root.join(id);
    let meta_path = chat_dir.join("meta.json");
    let mut opened = hash_root.open_regular_file(&meta_path)?;
    let modified = opened.metadata.modified().ok()?;
    let mut buf = String::new();
    opened.file.read_to_string(&mut buf).ok()?;
    let value: Value = serde_json::from_str(&buf).ok()?;
    let recorded = value
        .get("cwd")
        .or_else(|| value.get("workspace"))
        .and_then(Value::as_str)?;
    if !cwd_matches(recorded, cwd) {
        return None;
    }
    let updated_at = timestamp_from_meta(&value).unwrap_or(modified);
    if !is_within(updated_at, now, MAX_SESSION_AGE) {
        return None;
    }
    let title = value
        .get("name")
        .or_else(|| value.get("title"))
        .or_else(|| value.get("summary"))
        .and_then(Value::as_str)
        .and_then(normalize_title)
        .or_else(|| sibling_title(hash_root, &chat_dir))?;
    Some(ForeignSessionSummary {
        tool: ForeignSessionTool::Cursor,
        source: ForeignSessionSource::CursorCli,
        native_id: id.to_owned(),
        title,
        cwd: cwd.to_path_buf(),
        updated_at,
        branch: value
            .get("branch")
            .and_then(Value::as_str)
            .map(str::to_owned),
    })
}

fn sibling_title(root: &ApprovedRoot, chat_dir: &Path) -> Option<String> {
    let mut names = Vec::new();
    root.subroot(chat_dir)?.for_each_entry_bounded(8, |name| {
        names.push(name.to_string_lossy().into_owned());
    });
    for name in names {
        if !name.ends_with(".jsonl") {
            continue;
        }
        let mut opened = root.open_regular_file(&chat_dir.join(&name))?;
        if let Some(title) = first_user_text(&mut opened.file, TITLE_READ_BYTES) {
            return Some(title);
        }
    }
    None
}

fn find_transcript(root: &ApprovedRoot, cwd: &Path, session_id: &str) -> Option<PathBuf> {
    for slug in slug_candidates(cwd) {
        let transcripts = root.join("projects").join(&slug).join("agent-transcripts");
        let nested = transcripts
            .join(session_id)
            .join(format!("{session_id}.jsonl"));
        if root.resolve_regular_file(&nested).is_some() {
            return Some(nested);
        }
        let flat = transcripts.join(format!("{session_id}.jsonl"));
        if root.resolve_regular_file(&flat).is_some() {
            return Some(flat);
        }
    }
    let chats_path = root.join("chats");
    let chats = root.subroot(&chats_path)?;
    let mut names = Vec::new();
    chats.for_each_entry_bounded(MAX_HASH_DIRS, |name| {
        names.push(name.to_string_lossy().into_owned());
    });
    for name in names {
        let Some(hash_root) = chats.subroot(&chats.join(&name)) else {
            continue;
        };
        let chat_dir = hash_root.join(session_id);
        let Some(chat_root) = hash_root.subroot(&chat_dir) else {
            continue;
        };
        let mut files = Vec::new();
        chat_root.for_each_entry_bounded(8, |file| {
            files.push(file.to_string_lossy().into_owned());
        });
        for file in files {
            if file.ends_with(".jsonl") {
                let path = chat_dir.join(file);
                if root.resolve_regular_file(&path).is_some() {
                    return Some(path);
                }
            }
        }
    }
    None
}

fn read_turns(file: &mut File) -> Vec<CursorTranscriptTurn> {
    let mut buf = vec![0u8; TRANSCRIPT_READ_BYTES];
    let Ok(n) = file.read(&mut buf) else {
        return Vec::new();
    };
    buf.truncate(n);
    let text = String::from_utf8_lossy(&buf);
    let mut turns = Vec::new();
    for line in text.lines() {
        if turns.len() == MAX_TRANSCRIPT_TURNS {
            break;
        }
        let Ok(value) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        let Some(user) = role_is_user(&value) else {
            continue;
        };
        let Some(body) = message_text(&value) else {
            continue;
        };
        let body = if user {
            strip_preamble(&body).to_owned()
        } else {
            body
        };
        let body = truncate_chars(body.trim(), MAX_TURN_CHARS);
        if body.is_empty() {
            continue;
        }
        turns.push(CursorTranscriptTurn { user, text: body });
    }
    turns
}

fn first_user_text(file: &mut File, limit: usize) -> Option<String> {
    let mut buf = vec![0u8; limit];
    let n = file.read(&mut buf).ok()?;
    buf.truncate(n);
    let text = String::from_utf8_lossy(&buf);
    for line in text.lines() {
        let Ok(value) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        if role_is_user(&value) != Some(true) {
            continue;
        }
        let body = message_text(&value)?;
        return title_from_user_text(&body);
    }
    None
}

fn title_from_user_text(text: &str) -> Option<String> {
    let text = strip_preamble(text);
    let line = text
        .lines()
        .find(|line| !line.trim().is_empty())
        .unwrap_or(text);
    normalize_title(line)
}

fn strip_preamble(text: &str) -> &str {
    let mut rest = text.trim();
    if let Some(end) = rest.find("</user_info>") {
        rest = rest[end + "</user_info>".len()..].trim();
    }
    if let Some(stripped) = rest.strip_prefix('`')
        && let Some(end) = stripped.find('`')
    {
        let inner = &stripped[..end];
        if inner.contains("UTC") || inner.contains("day") {
            rest = stripped[end + 1..].trim();
        }
    }
    rest
}

fn role_is_user(value: &Value) -> Option<bool> {
    let role = value
        .get("role")
        .or_else(|| value.get("message").and_then(|message| message.get("role")))
        .and_then(Value::as_str)?;
    match role {
        "user" => Some(true),
        "assistant" => Some(false),
        _ => None,
    }
}

fn message_text(value: &Value) -> Option<String> {
    let message = value.get("message").unwrap_or(value);
    let content = message.get("content").or_else(|| message.get("text"))?;
    match content {
        Value::String(text) => Some(text.clone()),
        Value::Array(parts) => {
            let mut out = String::new();
            for part in parts {
                let kind = part.get("type").and_then(Value::as_str);
                if kind.is_some_and(|kind| kind != "text") {
                    continue;
                }
                let Some(text) = part.get("text").and_then(Value::as_str) else {
                    continue;
                };
                if !out.is_empty() {
                    out.push('\n');
                }
                out.push_str(text);
            }
            (!out.is_empty()).then_some(out)
        }
        _ => None,
    }
}

fn timestamp_from_meta(value: &Value) -> Option<SystemTime> {
    for key in ["updatedAt", "updated_at", "createdAt", "created_at"] {
        let Some(raw) = value.get(key) else {
            continue;
        };
        if let Some(millis) = raw.as_i64() {
            return super::system_time_from_millis(millis);
        }
        if let Some(millis) = raw.as_u64().and_then(|millis| i64::try_from(millis).ok()) {
            return super::system_time_from_millis(millis);
        }
        if let Some(text) = raw.as_str()
            && let Ok(parsed) = chrono::DateTime::parse_from_rfc3339(text)
            && let Ok(secs) = u64::try_from(parsed.timestamp())
        {
            return std::time::UNIX_EPOCH.checked_add(Duration::from_secs(secs));
        }
    }
    None
}

fn cwd_matches(recorded: &str, cwd: &Path) -> bool {
    let recorded_path = Path::new(recorded);
    if recorded_path == cwd {
        return true;
    }
    match (dunce::canonicalize(recorded_path), dunce::canonicalize(cwd)) {
        (Ok(left), Ok(right)) => left == right,
        _ => false,
    }
}

fn hash_candidates(cwd: &Path) -> Vec<String> {
    let canonical = dunce::canonicalize(cwd).unwrap_or_else(|_| cwd.to_path_buf());
    let text = canonical.to_string_lossy();
    let with_slash = format!("{text}/");
    [text.as_ref(), with_slash.as_str()]
        .into_iter()
        .map(|value| format!("{:x}", md5::compute(value.as_bytes())))
        .collect()
}

fn slug_candidates(cwd: &Path) -> Vec<String> {
    let canonical = dunce::canonicalize(cwd).unwrap_or_else(|_| cwd.to_path_buf());
    let slash = slash_slug(&canonical);
    let alnum = alnum_slug(&canonical);
    let mut out = vec![slash];
    if out.first().is_some_and(|first| first != &alnum) {
        out.push(alnum);
    }
    out
}

fn slash_slug(path: &Path) -> String {
    let mut slug = String::new();
    for ch in path.to_string_lossy().chars() {
        if ch == '/' || ch == '\\' {
            if !slug.is_empty() && !slug.ends_with('-') {
                slug.push('-');
            }
        } else {
            slug.push(ch);
        }
    }
    slug.trim_matches('-').to_owned()
}

fn alnum_slug(path: &Path) -> String {
    path.to_string_lossy()
        .chars()
        .map(|ch| if ch.is_ascii_alphanumeric() { ch } else { '-' })
        .collect::<String>()
        .trim_matches('-')
        .to_owned()
}

fn truncate_chars(text: &str, max_chars: usize) -> String {
    let mut out: String = text.chars().take(max_chars).collect();
    if text.chars().nth(max_chars).is_some() {
        out.push('…');
    }
    out
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::time::UNIX_EPOCH;

    use filetime::FileTime;
    use serde_json::json;
    use tempfile::TempDir;

    use super::*;

    fn now() -> SystemTime {
        UNIX_EPOCH + Duration::from_secs(1_800_000_000)
    }

    fn set_mtime(path: &Path, time: SystemTime) {
        filetime::set_file_mtime(path, FileTime::from_system_time(time)).unwrap();
    }

    #[test]
    fn agent_transcripts_for_this_workspace_are_listed() {
        let tmp = TempDir::new().unwrap();
        let cwd = tmp.path().join("repo");
        fs::create_dir_all(&cwd).unwrap();
        let cursor = tmp.path().join("cursor");
        let canonical = dunce::canonicalize(&cwd).unwrap();
        let id = uuid::Uuid::from_u128(7);
        let dir = cursor
            .join("projects")
            .join(slash_slug(&canonical))
            .join("agent-transcripts")
            .join(id.to_string());
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join(format!("{id}.jsonl"));
        fs::write(
            &path,
            format!(
                "{}\n{}\n",
                json!({
                    "role": "user",
                    "message": {"content": [{"type": "text", "text": "Fix the session picker"}]}
                }),
                json!({
                    "role": "assistant",
                    "message": {"content": [{"type": "text", "text": "Done."}]}
                }),
            ),
        )
        .unwrap();
        set_mtime(&path, now());
        let other = cursor
            .join("projects")
            .join("somewhere-else")
            .join("agent-transcripts")
            .join("11111111-1111-1111-1111-111111111111");
        fs::create_dir_all(&other).unwrap();
        fs::write(
            other.join("11111111-1111-1111-1111-111111111111.jsonl"),
            json!({"role":"user","message":{"content":"other"}}).to_string(),
        )
        .unwrap();

        let found = scan_in(&cursor, &cwd, now());
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].native_id, id.to_string());
        assert_eq!(found[0].title, "Fix the session picker");
        assert_eq!(found[0].tool, ForeignSessionTool::Cursor);
        assert_eq!(found[0].source, ForeignSessionSource::CursorCli);

        let turns = {
            // read_transcript uses CURSOR_CONFIG_DIR
            let _guard = EnvGuard::set("CURSOR_CONFIG_DIR", &cursor);
            read_transcript(&cwd, &id.to_string())
        };
        assert_eq!(turns.len(), 2);
        assert!(turns[0].user);
        assert_eq!(turns[0].text, "Fix the session picker");
        assert!(!turns[1].user);
    }

    #[test]
    fn cli_chat_meta_matches_cwd_and_skips_other_workspaces() {
        let tmp = TempDir::new().unwrap();
        let cwd = tmp.path().join("repo");
        fs::create_dir_all(&cwd).unwrap();
        let cursor = tmp.path().join("cursor");
        let canonical = dunce::canonicalize(&cwd).unwrap();
        let hash = format!("{:x}", md5::compute(canonical.to_string_lossy().as_bytes()));
        let id = uuid::Uuid::from_u128(9);
        let chat = cursor.join("chats").join(&hash).join(id.to_string());
        fs::create_dir_all(&chat).unwrap();
        let meta = chat.join("meta.json");
        fs::write(
            &meta,
            json!({
                "cwd": canonical,
                "name": "Resume me",
                "updatedAt": 1_800_000_000_000i64
            })
            .to_string(),
        )
        .unwrap();
        set_mtime(&meta, now());
        let other = cursor
            .join("chats")
            .join("deadbeef")
            .join("22222222-2222-2222-2222-222222222222");
        fs::create_dir_all(&other).unwrap();
        fs::write(
            other.join("meta.json"),
            json!({"cwd": "/somewhere/else", "name": "nope", "updatedAt": 1_800_000_000_000i64})
                .to_string(),
        )
        .unwrap();

        let found = scan_in(&cursor, &cwd, now());
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].title, "Resume me");
        assert_eq!(found[0].native_id, id.to_string());
    }

    #[test]
    fn stale_transcripts_are_dropped() {
        let tmp = TempDir::new().unwrap();
        let cwd = tmp.path().join("repo");
        fs::create_dir_all(&cwd).unwrap();
        let cursor = tmp.path().join("cursor");
        let canonical = dunce::canonicalize(&cwd).unwrap();
        let id = uuid::Uuid::from_u128(3);
        let dir = cursor
            .join("projects")
            .join(slash_slug(&canonical))
            .join("agent-transcripts")
            .join(id.to_string());
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join(format!("{id}.jsonl"));
        fs::write(
            &path,
            json!({"role":"user","message":{"content":"old"}}).to_string(),
        )
        .unwrap();
        set_mtime(&path, now() - MAX_SESSION_AGE - Duration::from_secs(60));
        assert!(scan_in(&cursor, &cwd, now()).is_empty());
    }

    struct EnvGuard {
        key: &'static str,
        previous: Option<std::ffi::OsString>,
    }

    impl EnvGuard {
        fn set(key: &'static str, value: &Path) -> Self {
            let previous = std::env::var_os(key);
            // SAFETY: this test is the only caller, and it holds the guard until `read_transcript` returns.
            unsafe { std::env::set_var(key, value) };
            Self { key, previous }
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            // SAFETY: see `EnvGuard::set`.
            match self.previous.take() {
                Some(value) => unsafe { std::env::set_var(self.key, value) },
                None => unsafe { std::env::remove_var(self.key) },
            }
        }
    }
}
