use std::io::Read;
use std::path::{Component, Path, PathBuf};
use std::time::{Duration, SystemTime};

use serde_json::Value;

mod db;
mod files;

use super::{
    ApprovedRoot, ForeignSessionSource, ForeignSessionSummary, RecentCandidate, RecentProbe,
    approved_root_for_recent, finish_tool_scan, normalize_title,
};

// Codex is currently on single-digit state generations
// Probe a generous, deterministic supported range without enumerating unrelated CODEX_HOME files
const MAX_STATE_DB_GENERATION: u32 = 128;

pub(super) fn scan(cwd: &Path, now: SystemTime) -> Vec<ForeignSessionSummary> {
    let Some(codex_home) = std::env::var_os("CODEX_HOME")
        .map(PathBuf::from)
        .or_else(|| xai_dirs::home_dir().map(|home| home.join(".codex")))
    else {
        return Vec::new();
    };
    scan_in_home(&codex_home, cwd, now)
}

pub(super) fn most_recent(
    cwd: &Path,
    now: SystemTime,
    within: Duration,
) -> RecentProbe<RecentCandidate> {
    let Some(codex_home) = std::env::var_os("CODEX_HOME")
        .map(PathBuf::from)
        .or_else(|| xai_dirs::home_dir().map(|home| home.join(".codex")))
    else {
        return RecentProbe::Complete(None);
    };
    most_recent_in_home(&codex_home, cwd, now, within)
}

fn scan_in_home(codex_home: &Path, cwd: &Path, now: SystemTime) -> Vec<ForeignSessionSummary> {
    let Some(root) = ApprovedRoot::new(codex_home) else {
        return Vec::new();
    };
    let sessions = state_databases(&root)
        .find_map(|path| {
            db::scan_database(&root, &path, cwd, now).filter(|sessions| !sessions.is_empty())
        })
        .unwrap_or_else(|| files::scan_rollouts(&root, cwd, now));
    finish_tool_scan(sessions)
}

fn most_recent_in_home(
    codex_home: &Path,
    cwd: &Path,
    now: SystemTime,
    within: Duration,
) -> RecentProbe<RecentCandidate> {
    let root = match approved_root_for_recent(codex_home) {
        Ok(Some(root)) => root,
        Ok(None) => return RecentProbe::Complete(None),
        Err(()) => return RecentProbe::Incomplete,
    };
    let Some(path) = highest_named_state_database(&root) else {
        return files::most_recent_rollout(&root, cwd, now, within);
    };
    match db::most_recent_database(&root, &path, cwd, now, within) {
        db::RecentDatabaseResult::Usable(candidate) => RecentProbe::Complete(candidate),
        db::RecentDatabaseResult::Incomplete => RecentProbe::Incomplete,
        db::RecentDatabaseResult::Unusable => files::most_recent_rollout(&root, cwd, now, within),
    }
}

fn highest_named_state_database(root: &ApprovedRoot) -> Option<PathBuf> {
    (0..=MAX_STATE_DB_GENERATION).rev().find_map(|generation| {
        let path = root.join(format!("state_{generation}.sqlite"));
        match std::fs::symlink_metadata(&path) {
            Ok(_) => Some(path),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(_) => Some(path),
        }
    })
}

fn state_databases(root: &ApprovedRoot) -> impl Iterator<Item = PathBuf> + '_ {
    (0..=MAX_STATE_DB_GENERATION)
        .rev()
        .filter_map(move |generation| {
            let path = root.join(format!("state_{generation}.sqlite"));
            root.resolve_regular_file(&path).map(|(path, _)| path)
        })
}

fn source_from_str(source: &str) -> Option<ForeignSessionSource> {
    match source {
        "cli" => Some(ForeignSessionSource::CodexCli),
        "vscode" => Some(ForeignSessionSource::CodexVsCode),
        _ => None,
    }
}

fn source_from_persisted(source: &str) -> Option<ForeignSessionSource> {
    source_from_str(source).or_else(|| {
        let value = serde_json::from_str::<Value>(source).ok()?;
        source_from_value(&value)
    })
}

fn source_from_value(source: &Value) -> Option<ForeignSessionSource> {
    source
        .as_str()
        .and_then(source_from_str)
        .or_else(|| match source.get("custom")?.as_str()? {
            "atlas" => Some(ForeignSessionSource::CodexAtlas),
            "chatgpt" => Some(ForeignSessionSource::CodexChatGpt),
            _ => None,
        })
}

fn existing_rollout_path(root: &ApprovedRoot, value: &str, expected_id: &str) -> Option<PathBuf> {
    let path = PathBuf::from(value);
    if path.components().any(|part| part == Component::ParentDir) {
        return None;
    }
    let path = if path.is_absolute() {
        path
    } else {
        root.join(path)
    };
    let mut compressed = path.clone().into_os_string();
    compressed.push(".zst");
    let approved_prefixes = ["sessions", "archived_sessions"]
        .into_iter()
        .filter_map(|name| dunce::canonicalize(root.join(name)).ok())
        .filter(|path| path.starts_with(root.path()))
        .collect::<Vec<_>>();
    [path, PathBuf::from(compressed)]
        .into_iter()
        .filter_map(|candidate| root.open_regular_file(&candidate).map(|opened| opened.path))
        .find(|candidate| {
            rollout_id(candidate).as_deref() == Some(expected_id)
                && approved_prefixes
                    .iter()
                    .any(|prefix| candidate.starts_with(prefix))
        })
}

fn rollout_id(path: &Path) -> Option<String> {
    let name = path.file_name()?.to_str()?;
    let stem = name
        .strip_suffix(".jsonl.zst")
        .or_else(|| name.strip_suffix(".jsonl"))?;
    let value = stem.strip_prefix("rollout-")?;
    let id_start = value.len().checked_sub(36)?;
    if id_start == 0 || value.as_bytes().get(id_start - 1) != Some(&b'-') {
        return None;
    }
    let timestamp = value.get(..id_start.checked_sub(1)?)?;
    if timestamp.len() != 19 {
        return None;
    }
    chrono::NaiveDateTime::parse_from_str(timestamp, "%Y-%m-%dT%H-%M-%S").ok()?;
    let id = value.get(id_start..)?;
    uuid::Uuid::try_parse(id).ok()?;
    Some(id.to_owned())
}

pub(super) fn read_transcript(
    _cwd: &Path,
    session_id: &str,
) -> Vec<super::cursor::CursorTranscriptTurn> {
    if uuid::Uuid::try_parse(session_id).is_err() {
        return Vec::new();
    }
    let Some(codex_home) = std::env::var_os("CODEX_HOME")
        .map(PathBuf::from)
        .or_else(|| xai_dirs::home_dir().map(|home| home.join(".codex")))
    else {
        return Vec::new();
    };
    let Some(root) = ApprovedRoot::new(&codex_home) else {
        return Vec::new();
    };
    let Some(path) = find_rollout(&root, session_id) else {
        return Vec::new();
    };
    let Some(mut opened) = root.open_regular_file(&path) else {
        return Vec::new();
    };
    let mut bytes = Vec::new();
    let _ = opened.file.take(1024 * 1024).read_to_end(&mut bytes);
    codex_turns(&String::from_utf8_lossy(&bytes))
}

fn find_rollout(root: &ApprovedRoot, session_id: &str) -> Option<PathBuf> {
    let sessions = root.subroot(&root.join("sessions"))?;
    let mut years = Vec::new();
    sessions.for_each_entry_bounded(8, |name| {
        years.push(name.to_string_lossy().into_owned());
    });
    years.sort();
    years.reverse();
    let mut examined = 0usize;
    for year in years {
        let Some(year_root) = sessions.subroot(&sessions.join(&year)) else {
            continue;
        };
        let mut months = Vec::new();
        year_root.for_each_entry_bounded(12, |name| {
            months.push(name.to_string_lossy().into_owned());
        });
        months.sort();
        months.reverse();
        for month in months {
            let month_root = match year_root.subroot(&year_root.join(&month)) {
                Some(root) => root,
                None => continue,
            };
            let mut days = Vec::new();
            month_root.for_each_entry_bounded(31, |name| {
                days.push(name.to_string_lossy().into_owned());
            });
            for day in days {
                let day_root = match month_root.subroot(&month_root.join(&day)) {
                    Some(root) => root,
                    None => continue,
                };
                let mut files = Vec::new();
                day_root.for_each_entry_bounded(64, |name| {
                    files.push(name.to_string_lossy().into_owned());
                });
                for file in files {
                    examined += 1;
                    if examined > 256 {
                        return None;
                    }
                    if file.contains(session_id) && file.ends_with(".jsonl") {
                        return Some(day_root.join(file));
                    }
                }
            }
        }
    }
    None
}

fn codex_turns(text: &str) -> Vec<super::cursor::CursorTranscriptTurn> {
    let mut turns = Vec::new();
    for line in text.lines() {
        if turns.len() == 120 {
            break;
        }
        let Ok(entry) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        let Some((user, body)) = codex_turn(&entry) else {
            continue;
        };
        let body = body.trim();
        if body.is_empty() {
            continue;
        }
        let body = body.chars().take(8_000).collect();
        turns.push(super::cursor::CursorTranscriptTurn { user, text: body });
    }
    turns
}

fn codex_turn(entry: &Value) -> Option<(bool, String)> {
    let payload = entry.get("payload").unwrap_or(entry);
    if let Some(message) = payload.get("message").and_then(Value::as_str) {
        let user = payload.get("type").and_then(Value::as_str) != Some("agent_message");
        return Some((user, message.to_owned()));
    }
    let role = payload.get("role").and_then(Value::as_str)?;
    let user = role == "user";
    if role != "user" && role != "assistant" {
        return None;
    }
    let content = payload.get("content")?;
    let body = match content {
        Value::String(text) => text.clone(),
        Value::Array(blocks) => blocks
            .iter()
            .filter_map(|block| block.get("text").and_then(Value::as_str).map(str::to_owned))
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    };
    Some((user, body))
}

fn title(primary: &str, fallback: &str) -> Option<String> {
    normalize_title(primary).or_else(|| normalize_title(fallback))
}

#[cfg(test)]
mod fixed_path_tests {
    use super::*;

    #[test]
    fn fixed_rollout_qualification_does_not_require_enumeration() {
        let root = tempfile::tempdir().unwrap();
        let sessions = root.path().join("sessions/2027/01/15");
        std::fs::create_dir_all(&sessions).unwrap();
        let id = uuid::Uuid::from_u128(9_000);
        let rollout = sessions.join(format!("rollout-2027-01-15T12-00-00-{id}.jsonl"));
        std::fs::write(&rollout, "").unwrap();
        let approved = ApprovedRoot::new(root.path()).unwrap();
        assert_eq!(
            existing_rollout_path(&approved, &rollout.to_string_lossy(), &id.to_string()),
            Some(dunce::canonicalize(&rollout).unwrap())
        );
    }
}

#[cfg(all(test, unix))]
mod tests;
