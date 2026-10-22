//! Checked-in RGBA icon painted in the composer gutter.
//!
//! The bytes are a PNG (color type 6, RGBA). Kitty and iTerm2 both take those
//! bytes as the inline-image payload. Terminals with neither protocol leave
//! the gutter empty: this module returns no escapes and the widget draws no
//! stand-in character.

use crate::terminal::image::GraphicsProtocol;
use crate::terminal::overlay::Escapes;

/// Repo-relative path of the icon. Tests and the pull request cite this path.
pub const BOT_ICON_ASSET: &str = "crates/codegen/xai-grok-pager/assets/bot-icon.png";

/// Raw file bytes. The composer escape payload is these bytes, not a transcode.
pub const BOT_ICON_PNG: &[u8] = include_bytes!("../../assets/bot-icon.png");

/// Stable overlay owner so consecutive frames keep one placement.
const BOT_ICON_OWNER: u64 = 0xB071_C0DE;

/// Cells the gutter reserves when an image protocol is available.
pub const GUTTER_COLS: u16 = 2;
const GUTTER_ROWS: u16 = 1;

/// Kitty or iTerm2 sequence for the gutter cell, using the preview protocol
/// (iTerm2 is allowed there). `None` when the terminal has neither protocol
/// or the asset is not a format that protocol can place.
pub fn composer_escapes(cell_x: u16, cell_y: u16) -> Option<Escapes> {
    if crate::terminal::image::prompt_preview_graphics_protocol() == GraphicsProtocol::None {
        return None;
    }
    crate::terminal::overlay::composer_gutter_image(
        BOT_ICON_PNG,
        GUTTER_COLS,
        GUTTER_ROWS,
        cell_x,
        cell_y,
        BOT_ICON_OWNER,
    )
}
