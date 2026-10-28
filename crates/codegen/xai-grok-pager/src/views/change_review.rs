//! `/changes`: review the files the agent changed this session, one file at a time.
//!
//! The source of truth is the edit/write tool-call diffs already in the scrollback; nothing is fetched.
//! Files appear in first-edit order and every successful edit to a file is grouped under it.

use std::path::{Path, PathBuf};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use similar::ChangeTag;
use xai_grok_pager_diff::DiffHunk;

use crate::render::tool_paths::{ToolPathSurface, path_for_tool_surface};
use crate::scrollback::block::RenderBlock;
use crate::scrollback::blocks::{DiffRenderConfig, ToolCallBlock, render_diff_hunks_highlighted};
use crate::theme::{Theme, ThemeKind};
use crate::views::modal_window::{
    self as mw, ModalContentArea, ModalSizing, ModalWindowConfig, ModalWindowState, Shortcut,
};

pub const EMPTY_MESSAGE: &str = "No files changed in this session yet.";

/// Every successful edit to one file, in the order the agent made them.
/// Edits stay separate because each edit's hunk line numbers are relative to the file as it was at that moment.
#[derive(Debug, Clone, PartialEq)]
pub struct FileChanges {
    /// Display path: cwd-relative when the file is inside the session cwd.
    pub path: String,
    pub edits: Vec<Vec<DiffHunk>>,
}

impl FileChanges {
    fn change_counts(&self) -> (usize, usize) {
        let lines = self.edits.iter().flatten().flatten();
        lines.fold((0, 0), |(ins, del), line| match line.tag {
            ChangeTag::Insert => (ins + 1, del),
            ChangeTag::Delete => (ins, del + 1),
            ChangeTag::Equal => (ins, del),
        })
    }
}

/// What a key press asks the caller to do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChangeReviewInput {
    Changed,
    Unchanged,
    Close,
    /// Close the review and seed the composer with this follow-up stub.
    FollowUp(String),
}

pub struct ChangeReview {
    pub files: Vec<FileChanges>,
    pub selected: usize,
    pub scroll: u16,
    body: Option<BodyCache>,
}

struct BodyCache {
    selected: usize,
    width: u16,
    theme: ThemeKind,
    rows: Vec<BodyRow>,
}

struct BodyRow {
    line: Line<'static>,
    /// Background color and the column it starts at, for inserted/deleted lines.
    background: Option<(Color, u16)>,
}

impl ChangeReview {
    pub fn from_blocks<'a>(
        blocks: impl IntoIterator<Item = &'a RenderBlock>,
        cwd: Option<&Path>,
    ) -> Self {
        let mut files: Vec<FileChanges> = Vec::new();
        for block in blocks {
            let RenderBlock::ToolCall(ToolCallBlock::Edit(edit)) = block else {
                continue;
            };
            // Failed edits changed nothing, memory notes are not project files, and an empty
            // hunk list is an edit still streaming in.
            if !edit.is_success() || edit.is_memory_activity || edit.hunks.is_empty() {
                continue;
            }
            let path = path_for_tool_surface(&edit.path, ToolPathSurface::Expanded, cwd, None, 0);
            match files.iter_mut().find(|file| file.path == path) {
                Some(file) => file.edits.push(edit.hunks.clone()),
                None => files.push(FileChanges {
                    path,
                    edits: vec![edit.hunks.clone()],
                }),
            }
        }
        Self {
            files,
            selected: 0,
            scroll: 0,
            body: None,
        }
    }

    pub fn current(&self) -> Option<&FileChanges> {
        self.files.get(self.selected)
    }

    /// File switching wraps around at both ends.
    pub fn handle_key(&mut self, key: &KeyEvent) -> ChangeReviewInput {
        if key
            .modifiers
            .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT | KeyModifiers::SUPER)
        {
            return ChangeReviewInput::Unchanged;
        }
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') => ChangeReviewInput::Close,
            KeyCode::Left | KeyCode::Char('h') => self.step_file(-1),
            KeyCode::Right | KeyCode::Char('l') => self.step_file(1),
            KeyCode::Char('i') => match self.current() {
                Some(file) => ChangeReviewInput::FollowUp(format!("About {}: ", file.path)),
                None => ChangeReviewInput::Unchanged,
            },
            code if crate::views::modal::apply_doc_scroll(code, &mut self.scroll) => {
                ChangeReviewInput::Changed
            }
            _ => ChangeReviewInput::Unchanged,
        }
    }

    fn step_file(&mut self, delta: isize) -> ChangeReviewInput {
        let len = self.files.len();
        if len < 2 {
            return ChangeReviewInput::Unchanged;
        }
        self.selected = (self.selected as isize + delta).rem_euclid(len as isize) as usize;
        self.scroll = 0;
        ChangeReviewInput::Changed
    }

    fn body_rows(&mut self, width: u16, theme: &Theme) -> &[BodyRow] {
        let theme_kind = Theme::current_kind();
        let stale = self.body.as_ref().is_none_or(|cache| {
            cache.selected != self.selected || cache.width != width || cache.theme != theme_kind
        });
        if stale {
            let rows = self
                .current()
                .map(|file| build_body_rows(file, width, theme))
                .unwrap_or_default();
            self.body = Some(BodyCache {
                selected: self.selected,
                width,
                theme: theme_kind,
                rows,
            });
        }
        self.body
            .as_ref()
            .map_or(&[], |cache| cache.rows.as_slice())
    }
}

fn build_body_rows(file: &FileChanges, width: u16, theme: &Theme) -> Vec<BodyRow> {
    let config = DiffRenderConfig::default();
    let path = PathBuf::from(&file.path);
    let total = file.edits.len();
    let mut rows = Vec::new();
    for (i, hunks) in file.edits.iter().enumerate() {
        if total > 1 {
            if i > 0 {
                rows.push(BodyRow {
                    line: Line::default(),
                    background: None,
                });
            }
            rows.push(BodyRow {
                line: Line::styled(
                    format!("Edit {} of {total}", i + 1),
                    theme.muted().add_modifier(Modifier::BOLD),
                ),
                background: None,
            });
        }
        rows.extend(
            render_diff_hunks_highlighted(hunks, &path, theme, width, &config)
                .into_iter()
                .map(|out| BodyRow {
                    background: out.background.map(|bg| (bg, out.content_start_col)),
                    line: out.line,
                }),
        );
    }
    rows
}

fn header_line(review: &ChangeReview, file: &FileChanges, theme: &Theme) -> Line<'static> {
    let (ins, del) = file.change_counts();
    Line::from(vec![
        Span::styled(
            format!(
                "\u{2039} {}/{} \u{203a} ",
                review.selected + 1,
                review.files.len()
            ),
            theme.muted(),
        ),
        Span::styled(
            file.path.clone(),
            Style::default().fg(theme.path).add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!("  +{ins}"),
            Style::default().fg(theme.diff_insert_fg),
        ),
        Span::styled("/", theme.muted()),
        Span::styled(format!("-{del}"), Style::default().fg(theme.diff_delete_fg)),
    ])
}

pub fn render_change_review(
    buf: &mut Buffer,
    area: Rect,
    window: &mut ModalWindowState,
    review: &mut ChangeReview,
    compact: bool,
    theme: &Theme,
) {
    let shortcuts = [
        Shortcut {
            label: "\u{2190}/\u{2192} file",
            clickable: false,
            id: 0,
        },
        Shortcut {
            label: "\u{2191}/\u{2193} scroll",
            clickable: false,
            id: 0,
        },
        Shortcut {
            label: "i follow up",
            clickable: false,
            id: 0,
        },
        Shortcut {
            label: "Esc close",
            clickable: false,
            id: 0,
        },
    ];
    let config = ModalWindowConfig {
        title: "Changes",
        tabs: None,
        shortcuts: &shortcuts,
        sizing: ModalSizing {
            width_pct: 0.90,
            max_width: 160,
            min_width: 44,
            v_margin: 2,
            h_pad: 2,
            v_pad: 1,
            footer_lines: 2,
        }
        .with_compact(compact),
        fold_info: None,
    };
    let Some(ModalContentArea { content, .. }) =
        mw::render_modal_window(buf, area, window, &config, theme)
    else {
        return;
    };
    if content.height == 0 {
        return;
    }
    let Some(file) = review.current() else {
        buf.set_stringn(
            content.x,
            content.y,
            EMPTY_MESSAGE,
            content.width as usize,
            theme.muted(),
        );
        return;
    };
    let header = header_line(review, file, theme);
    buf.set_line(content.x, content.y, &header, content.width);

    let body = Rect {
        y: content.y.saturating_add(2),
        height: content.height.saturating_sub(2),
        ..content
    };
    let rows_len = review.body_rows(body.width, theme).len();
    let max_scroll = rows_len.saturating_sub(body.height as usize);
    review.scroll = (review.scroll as usize).min(max_scroll) as u16;
    let start = review.scroll as usize;
    let rows = review.body_rows(body.width, theme);
    for (row, y) in rows
        .iter()
        .skip(start)
        .zip(body.y..body.y.saturating_add(body.height))
    {
        if let Some((bg, start_col)) = row.background {
            let x = body.x.saturating_add(start_col);
            let fill = Rect {
                x,
                y,
                width: body.right().saturating_sub(x),
                height: 1,
            };
            buf.set_style(fill, Style::default().bg(bg));
        }
        buf.set_line(body.x, y, &row.line, body.width);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use xai_grok_pager_diff::DiffLine;

    fn hunk(insert: &str) -> Vec<DiffHunk> {
        vec![vec![DiffLine {
            text: insert.to_string(),
            lo: 0,
            ln: 1,
            tag: ChangeTag::Insert,
        }]]
    }

    fn edit(path: &str, insert: &str) -> RenderBlock {
        RenderBlock::edit_with_hunks(path, hunk(insert))
    }

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn review_of(paths: &[&str]) -> ChangeReview {
        let blocks: Vec<RenderBlock> = paths.iter().map(|p| edit(p, "x")).collect();
        ChangeReview::from_blocks(&blocks, None)
    }

    fn paths(review: &ChangeReview) -> Vec<&str> {
        review.files.iter().map(|f| f.path.as_str()).collect()
    }

    fn buffer_text(buf: &Buffer) -> String {
        (buf.area.top()..buf.area.bottom())
            .map(|y| {
                (buf.area.left()..buf.area.right())
                    .filter_map(|x| buf.cell((x, y)))
                    .map(|cell| cell.symbol())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn groups_edits_per_file_in_first_edit_order() {
        let blocks = vec![
            edit("src/b.rs", "b1"),
            RenderBlock::user_prompt("again"),
            edit("src/a.rs", "a1"),
            edit("src/b.rs", "b2"),
        ];
        let review = ChangeReview::from_blocks(&blocks, None);
        assert_eq!(paths(&review), ["src/b.rs", "src/a.rs"]);
        assert_eq!(
            review.files.first().map(|f| f.edits.clone()),
            Some(vec![hunk("b1"), hunk("b2")]),
            "both b.rs edits are kept, in order"
        );
    }

    #[test]
    fn groups_absolute_and_relative_paths_to_the_same_file() {
        let blocks = vec![edit("/repo/src/a.rs", "1"), edit("src/a.rs", "2")];
        let review = ChangeReview::from_blocks(&blocks, Some(Path::new("/repo")));
        assert_eq!(paths(&review), ["src/a.rs"]);
        assert_eq!(review.files.first().map(|f| f.edits.len()), Some(2));
    }

    #[test]
    fn skips_failed_memory_and_pending_edits() {
        let failed = match edit("src/failed.rs", "x") {
            RenderBlock::ToolCall(ToolCallBlock::Edit(b)) => {
                RenderBlock::ToolCall(ToolCallBlock::Edit(b.with_error("boom")))
            }
            other => other,
        };
        let memory = match edit("memory/note.md", "x") {
            RenderBlock::ToolCall(ToolCallBlock::Edit(b)) => {
                RenderBlock::ToolCall(ToolCallBlock::Edit(b.with_memory_activity()))
            }
            other => other,
        };
        let pending = RenderBlock::edit_with_hunks("src/pending.rs", vec![]);
        let blocks = vec![failed, memory, pending, edit("src/ok.rs", "x")];
        assert_eq!(
            paths(&ChangeReview::from_blocks(&blocks, None)),
            ["src/ok.rs"]
        );
    }

    #[test]
    fn file_switch_wraps_and_resets_scroll() {
        let mut review = review_of(&["a.rs", "b.rs", "c.rs"]);
        review.scroll = 7;
        assert_eq!(
            review.handle_key(&key(KeyCode::Left)),
            ChangeReviewInput::Changed
        );
        assert_eq!(review.current().map(|f| f.path.as_str()), Some("c.rs"));
        assert_eq!(review.scroll, 0, "switching files starts at the top");
        review.handle_key(&key(KeyCode::Char('l')));
        assert_eq!(review.current().map(|f| f.path.as_str()), Some("a.rs"));
        review.handle_key(&key(KeyCode::Right));
        review.handle_key(&key(KeyCode::Char('l')));
        assert_eq!(review.current().map(|f| f.path.as_str()), Some("c.rs"));
        review.handle_key(&key(KeyCode::Char('h')));
        assert_eq!(review.current().map(|f| f.path.as_str()), Some("b.rs"));
    }

    #[test]
    fn single_file_switch_is_a_no_op() {
        let mut review = review_of(&["a.rs"]);
        assert_eq!(
            review.handle_key(&key(KeyCode::Right)),
            ChangeReviewInput::Unchanged
        );
        assert_eq!(review.selected, 0);
    }

    #[test]
    fn up_down_scroll_the_body() {
        let mut review = review_of(&["a.rs"]);
        assert_eq!(
            review.handle_key(&key(KeyCode::Down)),
            ChangeReviewInput::Changed
        );
        assert!(review.scroll > 0);
        review.handle_key(&key(KeyCode::PageUp));
        assert_eq!(review.scroll, 0);
    }

    #[test]
    fn i_follows_up_on_the_current_file() {
        let mut review = review_of(&["a.rs", "src/b.rs"]);
        review.handle_key(&key(KeyCode::Right));
        assert_eq!(
            review.handle_key(&key(KeyCode::Char('i'))),
            ChangeReviewInput::FollowUp("About src/b.rs: ".to_string())
        );
    }

    #[test]
    fn i_without_changes_does_nothing() {
        let mut review = review_of(&[]);
        assert_eq!(
            review.handle_key(&key(KeyCode::Char('i'))),
            ChangeReviewInput::Unchanged
        );
    }

    #[test]
    fn esc_and_q_close() {
        let mut review = review_of(&["a.rs"]);
        assert_eq!(
            review.handle_key(&key(KeyCode::Esc)),
            ChangeReviewInput::Close
        );
        assert_eq!(
            review.handle_key(&key(KeyCode::Char('q'))),
            ChangeReviewInput::Close
        );
    }

    #[test]
    fn renders_current_file_diff() {
        let theme = Theme::current();
        let mut review = review_of(&["src/a.rs", "src/b.rs"]);
        review.handle_key(&key(KeyCode::Right));
        let area = Rect::new(0, 0, 100, 20);
        let mut buf = Buffer::empty(area);
        render_change_review(
            &mut buf,
            area,
            &mut ModalWindowState::new(),
            &mut review,
            false,
            &theme,
        );
        let text = buffer_text(&buf);
        assert!(text.contains("2/2"), "position indicator:\n{text}");
        assert!(text.contains("src/b.rs"), "current path:\n{text}");
        assert!(!text.contains("src/a.rs"), "only the current file:\n{text}");
    }

    #[test]
    fn renders_empty_state() {
        let theme = Theme::current();
        let mut review = review_of(&[]);
        let area = Rect::new(0, 0, 100, 20);
        let mut buf = Buffer::empty(area);
        render_change_review(
            &mut buf,
            area,
            &mut ModalWindowState::new(),
            &mut review,
            false,
            &theme,
        );
        assert!(buffer_text(&buf).contains(EMPTY_MESSAGE));
    }
}
