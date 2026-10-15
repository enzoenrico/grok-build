//! Import a Cursor Agent transcript into a grok session directory so Ctrl-R can `session/load` it.

use std::io;

use agent_client_protocol as acp;

use super::info::Info;
use super::persistence::default_model_id;
use super::storage::{JsonlStorageAdapter, SessionUpdate, StorageAdapter};
use xai_grok_foreign_sessions::CursorTranscriptTurn;

/// Write `turns` under `session_id` when that grok session does not already exist.
/// A second pick reloads the same directory instead of appending the transcript again.
pub async fn materialize_cursor_session(
    session_id: &str,
    cwd: &str,
    title: &str,
    turns: &[CursorTranscriptTurn],
) -> io::Result<()> {
    if uuid::Uuid::try_parse(session_id).is_err() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "cursor session id must be a uuid",
        ));
    }
    if turns.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            "cursor session has no transcript to resume",
        ));
    }
    if super::resolve_local_session(session_id, cwd).is_some() {
        return Ok(());
    }
    let info = Info {
        id: acp::SessionId::new(session_id.to_owned()),
        cwd: cwd.to_owned(),
    };
    let adapter = JsonlStorageAdapter::new();
    adapter.init_session(&info, default_model_id()).await?;
    for turn in turns {
        let chunk = acp::ContentChunk::new(acp::ContentBlock::Text(acp::TextContent::new(
            turn.text.clone(),
        )));
        let update = if turn.user {
            acp::SessionUpdate::UserMessageChunk(chunk)
        } else {
            acp::SessionUpdate::AgentMessageChunk(chunk)
        };
        let notification = acp::SessionNotification::new(info.id.clone(), update);
        adapter
            .append_update(&info, &SessionUpdate::Acp(Box::new(notification)))
            .await?;
    }
    let title = if title.trim().is_empty() {
        turns
            .iter()
            .find(|turn| turn.user)
            .map(|turn| turn.text.clone())
            .unwrap_or_default()
    } else {
        title.to_owned()
    };
    if !title.trim().is_empty() {
        let _ = adapter.set_generated_title_if_absent(&info, title).await?;
    }
    Ok(())
}
