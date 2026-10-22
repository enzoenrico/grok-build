//! Fake-hub coverage for `/bot`. The composer assertion checks the escape
//! payload against the checked-in asset and rejects a stand-in character.

use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use base64::Engine as _;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use serde_json::{Value, json};

use super::hub::{AwaitedTurn, BotHub, HubBot, HubError, OnBusy, SendAccepted};
use super::image::BOT_ICON_PNG;
use super::session::{LOGIN_HINT, SubmitOutcome, format_bot_reply};
use super::{BotInput, BotSession};
use crate::views::prompt_widget::{PromptStyle, PromptWidget};

#[derive(Clone, Debug, PartialEq, Eq)]
struct SendRecord {
    agent_id: String,
    prompt: String,
    on_busy: OnBusy,
}

struct FakeHub {
    bots: Vec<HubBot>,
    /// When set, `bot_search_agents` returns this list instead of filtering.
    search_hits: Mutex<Option<Vec<HubBot>>>,
    sends: Mutex<Vec<SendRecord>>,
    awaits: Mutex<Vec<Option<Value>>>,
    lists: AtomicUsize,
    searches: AtomicUsize,
    logged_in: AtomicBool,
    /// The next `bot_await_turn` reports a timeout and keeps the handle.
    timeout_next_await: AtomicBool,
}

impl FakeHub {
    fn fixtures() -> Self {
        Self {
            bots: vec![
                HubBot {
                    id: "bot-ada".to_string(),
                    name: "Ada".to_string(),
                    description: "writes rust".to_string(),
                    status: "idle".to_string(),
                },
                HubBot {
                    id: "bot-bea".to_string(),
                    name: "Bea".to_string(),
                    description: "reviews diffs".to_string(),
                    status: "running".to_string(),
                },
            ],
            search_hits: Mutex::new(None),
            sends: Mutex::new(Vec::new()),
            awaits: Mutex::new(Vec::new()),
            lists: AtomicUsize::new(0),
            searches: AtomicUsize::new(0),
            logged_in: AtomicBool::new(true),
            timeout_next_await: AtomicBool::new(false),
        }
    }

    fn sends(&self) -> Vec<SendRecord> {
        self.sends.lock().expect("sends").clone()
    }

    fn await_count(&self) -> usize {
        self.awaits.lock().expect("awaits").len()
    }
}

#[async_trait]
impl BotHub for FakeHub {
    async fn list_agents(&self) -> Result<Vec<HubBot>, HubError> {
        self.gate()?;
        self.lists.fetch_add(1, Ordering::SeqCst);
        Ok(self.bots.clone())
    }

    async fn search_agents(&self, query: &str) -> Result<Vec<HubBot>, HubError> {
        self.gate()?;
        self.searches.fetch_add(1, Ordering::SeqCst);
        if let Some(hits) = self.search_hits.lock().expect("search").clone() {
            return Ok(hits);
        }
        let query = query.to_ascii_lowercase();
        Ok(self
            .bots
            .iter()
            .filter(|bot| {
                bot.name.to_ascii_lowercase().contains(query.as_str())
                    || bot
                        .description
                        .to_ascii_lowercase()
                        .contains(query.as_str())
            })
            .cloned()
            .collect())
    }

    async fn create_agent(&self, name: &str) -> Result<HubBot, HubError> {
        self.gate()?;
        Ok(HubBot {
            id: format!("bot-{name}"),
            name: name.to_string(),
            description: String::new(),
            status: "idle".to_string(),
        })
    }

    async fn send_prompt(
        &self,
        agent_id: &str,
        prompt: &str,
        on_busy: OnBusy,
    ) -> Result<SendAccepted, HubError> {
        self.gate()?;
        self.sends.lock().expect("sends").push(SendRecord {
            agent_id: agent_id.to_string(),
            prompt: prompt.to_string(),
            on_busy,
        });
        Ok(SendAccepted {
            handle: json!({ "turn": format!("h-{}", self.sends().len()) }),
        })
    }

    async fn await_turn(
        &self,
        agent_id: &str,
        handle: Option<&Value>,
    ) -> Result<AwaitedTurn, HubError> {
        self.gate()?;
        self.awaits.lock().expect("awaits").push(handle.cloned());
        if self.timeout_next_await.swap(false, Ordering::SeqCst) {
            return Ok(AwaitedTurn::Timeout {
                handle: handle.cloned().unwrap_or(Value::Null),
            });
        }
        Ok(AwaitedTurn::Finished {
            reply: format!("reply from {agent_id}"),
        })
    }
}

impl FakeHub {
    fn gate(&self) -> Result<(), HubError> {
        if self.logged_in.load(Ordering::SeqCst) {
            Ok(())
        } else {
            Err(HubError::NeedsLogin)
        }
    }
}

fn command(args: &str) -> BotInput {
    BotInput::Command(args.to_string())
}

fn plain(text: &str) -> BotInput {
    BotInput::Plain(text.to_string())
}

async fn submit(session: &mut BotSession, hub: &FakeHub, input: BotInput) -> SubmitOutcome {
    session.submit(input, hub).await
}

#[tokio::test]
async fn bot_lists_two_fixtures_and_route_sends_nothing() {
    let hub = FakeHub::fixtures();
    let mut session = BotSession::default();
    let listed = submit(&mut session, &hub, command("")).await;
    let SubmitOutcome::Shown(text) = listed else {
        panic!("expected a list, got {listed:?}");
    };
    assert!(text.contains("Ada"), "{text}");
    assert!(text.contains("writes rust"), "{text}");
    assert!(text.contains("idle"), "{text}");
    assert!(text.contains("Bea"), "{text}");
    assert!(text.contains("reviews diffs"), "{text}");
    assert!(text.contains("running"), "{text}");
    assert!(session.routed_id().is_none());

    let routed = submit(&mut session, &hub, command("Ada")).await;
    let SubmitOutcome::Shown(text) = routed else {
        panic!("expected a route notice, got {routed:?}");
    };
    assert!(text.contains("Routed to Ada"), "{text}");
    assert_eq!(session.routed_id(), Some("bot-ada"));
    let listed_again = submit(&mut session, &hub, command("")).await;
    let SubmitOutcome::Shown(marked) = listed_again else {
        panic!("expected a marked list, got {listed_again:?}");
    };
    assert!(
        marked.contains("Ada — writes rust — idle *"),
        "the routed bot is marked: {marked}"
    );
    assert!(
        !marked.contains("Bea — reviews diffs — running *"),
        "only the routed bot is marked: {marked}"
    );
    assert!(hub.sends().is_empty(), "naming a bot sends nothing");
    assert_eq!(hub.searches.load(Ordering::SeqCst), 0);
    assert_eq!(hub.await_count(), 0);
}

#[tokio::test]
async fn plain_prompts_use_the_roster_id_until_bot_off() {
    let hub = FakeHub::fixtures();
    let mut session = BotSession::default();
    let _ = submit(&mut session, &hub, command("Ada")).await;

    let first = submit(&mut session, &hub, plain("one")).await;
    let second = submit(&mut session, &hub, plain("two")).await;
    assert!(matches!(first, SubmitOutcome::Reply(_)));
    assert!(matches!(second, SubmitOutcome::Reply(_)));
    let sends = hub.sends();
    assert_eq!(sends.len(), 2);
    assert!(sends.iter().all(|send| send.agent_id == "bot-ada"));
    assert_eq!(sends.first().map(|send| send.prompt.as_str()), Some("one"));
    assert_eq!(sends.get(1).map(|send| send.prompt.as_str()), Some("two"));
    assert_eq!(hub.await_count(), 2);

    let cleared = submit(&mut session, &hub, command("off")).await;
    assert!(matches!(cleared, SubmitOutcome::Shown(_)));
    assert!(session.routed_id().is_none());
    let after = submit(&mut session, &hub, plain("back to the model")).await;
    assert_eq!(after, SubmitOutcome::Model);
    assert_eq!(
        hub.sends().len(),
        2,
        "the model prompt must not hit the hub"
    );
}

#[tokio::test]
async fn ambiguous_name_leaves_the_route_unchanged() {
    let hub = FakeHub::fixtures();
    *hub.search_hits.lock().expect("search") = Some(hub.bots.clone());
    let mut session = BotSession::default();
    let _ = submit(&mut session, &hub, command("Ada")).await;
    assert_eq!(session.routed_id(), Some("bot-ada"));

    let outcome = submit(&mut session, &hub, command("helper")).await;
    let SubmitOutcome::Shown(text) = outcome else {
        panic!("expected candidates, got {outcome:?}");
    };
    assert!(text.contains("route is unchanged"), "{text}");
    assert!(text.contains("Ada"), "{text}");
    assert!(text.contains("Bea"), "{text}");
    assert_eq!(session.routed_id(), Some("bot-ada"));
    assert!(hub.sends().is_empty());
}

#[tokio::test]
async fn timeout_then_a_plain_prompt_only_awaits() {
    let hub = FakeHub::fixtures();
    hub.timeout_next_await.store(true, Ordering::SeqCst);
    let mut session = BotSession::default();
    let _ = submit(&mut session, &hub, command("Ada")).await;

    let waiting = submit(&mut session, &hub, plain("first")).await;
    let SubmitOutcome::Shown(text) = waiting else {
        panic!("expected a timeout notice, got {waiting:?}");
    };
    assert!(text.contains("still working"), "{text}");
    assert_eq!(hub.sends().len(), 1);
    assert_eq!(hub.await_count(), 1);

    let resumed = submit(&mut session, &hub, plain("do not send this again")).await;
    assert!(matches!(resumed, SubmitOutcome::Reply(_)));
    assert_eq!(hub.sends().len(), 1, "resume must not call bot_send_prompt");
    assert_eq!(hub.await_count(), 2);
    let awaits = hub.awaits.lock().expect("awaits");
    assert!(
        awaits.get(1).is_some_and(Option::is_some),
        "the stored handle is passed to bot_await_turn"
    );
}

#[tokio::test]
async fn missing_grok_com_auth_prints_one_login_hint() {
    let hub = FakeHub::fixtures();
    hub.logged_in.store(false, Ordering::SeqCst);
    let mut session = BotSession::default();
    let outcome = submit(&mut session, &hub, command("")).await;
    let SubmitOutcome::Shown(text) = outcome else {
        panic!("expected the login hint, got {outcome:?}");
    };
    assert_eq!(text, LOGIN_HINT);
    assert_eq!(text.matches("login").count(), 1, "{text}");
    assert!(session.routed_id().is_none());
    assert!(hub.sends().is_empty());
    assert_eq!(hub.lists.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn rendering_a_hundred_line_reply_stays_under_the_budget() {
    let reply = "line\n".repeat(100);
    let started = Instant::now();
    let rendered = std::hint::black_box(format_bot_reply("Ada", reply.as_str()));
    let elapsed = started.elapsed();
    assert!(
        rendered.lines().count() >= 100,
        "{}",
        rendered.lines().count()
    );
    assert!(
        elapsed < Duration::from_millis(50),
        "reply render took {elapsed:?}"
    );
}

#[test]
fn composer_paints_the_asset_and_bot_off_drops_it() {
    let kitty = draw_composer(crate::terminal::image::GraphicsProtocol::Kitty);
    assert_eq!(
        payload_bytes(kitty.escape.as_str()),
        BOT_ICON_PNG,
        "kitty payload must be the asset bytes"
    );
    assert_eq!(kitty.text, "hello");
    assert!(
        !has_stand_in(kitty.text.as_str()),
        "composer text contains a stand-in character: {}",
        kitty.text
    );
    assert!(
        !kitty.cells_have_stand_in,
        "the composer must not substitute a glyph for the bot image"
    );
    assert!(
        !payload_bytes(kitty.after_off.as_str())
            .as_slice()
            .eq(BOT_ICON_PNG),
        "bot off must drop the image sequence"
    );
    assert!(!kitty.after_off.contains(&asset_b64()));

    let iterm = draw_composer(crate::terminal::image::GraphicsProtocol::ITerm2);
    assert_eq!(
        payload_bytes(iterm.escape.as_str()),
        BOT_ICON_PNG,
        "iterm2 payload must be the asset bytes"
    );
    assert_eq!(iterm.text, "hello");
    assert!(!has_stand_in(iterm.text.as_str()));

    let none = draw_composer(crate::terminal::image::GraphicsProtocol::None);
    assert!(
        none.escape.is_empty() || !none.escape.contains(&asset_b64()),
        "no image protocol leaves the gutter without a payload: {}",
        none.escape
    );
    assert_eq!(none.text, "hello");
    assert!(!has_stand_in(none.text.as_str()));
    assert!(
        !none.cells_have_stand_in,
        "terminals without an image protocol leave the gutter empty"
    );
}

struct ComposerFrame {
    text: String,
    escape: String,
    after_off: String,
    cells_have_stand_in: bool,
}

fn draw_composer(protocol: crate::terminal::image::GraphicsProtocol) -> ComposerFrame {
    let _guard = crate::terminal::image::set_protocol_for_test(protocol);
    crate::terminal::overlay::reset_owner();
    let mut widget = PromptWidget::new();
    widget.set_text("hello");
    widget.set_bot_icon(true);
    let area = Rect::new(0, 2, 60, 3);
    let mut buf = Buffer::empty(Rect::new(0, 0, 60, 6));
    let style = PromptStyle {
        focused: true,
        ..PromptStyle::default()
    };
    let drawn = widget.draw(&mut buf, area, None, &style, None, None);
    let escape = drawn
        .post_flush_escapes
        .map(|escapes| escapes.as_str().to_string())
        .unwrap_or_default();
    let cells_have_stand_in = buffer_has_stand_in(&buf);
    let text = widget.text().to_string();

    widget.set_bot_icon(false);
    let mut buf = Buffer::empty(Rect::new(0, 0, 60, 6));
    let cleared = widget.draw(&mut buf, area, None, &style, None, None);
    let after_off = cleared
        .post_flush_escapes
        .map(|escapes| escapes.as_str().to_string())
        .unwrap_or_default();
    ComposerFrame {
        text,
        escape,
        after_off,
        cells_have_stand_in,
    }
}

fn buffer_has_stand_in(buf: &Buffer) -> bool {
    let area = buf.area;
    for y in area.y..area.y.saturating_add(area.height) {
        for x in area.x..area.x.saturating_add(area.width) {
            if buf
                .cell((x, y))
                .is_some_and(|cell| cell.symbol().chars().any(is_stand_in_char))
            {
                return true;
            }
        }
    }
    false
}

fn asset_b64() -> String {
    base64::engine::general_purpose::STANDARD.encode(BOT_ICON_PNG)
}

fn payload_bytes(escape: &str) -> Vec<u8> {
    if let Some(bytes) = kitty_payload(escape) {
        return bytes;
    }
    iterm_payload(escape).unwrap_or_default()
}

fn kitty_payload(escape: &str) -> Option<Vec<u8>> {
    let marker = escape.find("a=T")?;
    let after = escape.get(marker..)?;
    let semi = after.find(';')?;
    let data = after.get(semi + 1..)?;
    let end = data.find('\u{1b}')?;
    let b64 = data.get(..end)?;
    base64::engine::general_purpose::STANDARD.decode(b64).ok()
}

fn iterm_payload(escape: &str) -> Option<Vec<u8>> {
    let marker = escape.find("]1337;File=")?;
    let after = escape.get(marker..)?;
    let colon = after.rfind(':')?;
    let data = after.get(colon + 1..)?;
    let end = data.find('\u{7}')?;
    let b64 = data.get(..end)?;
    base64::engine::general_purpose::STANDARD.decode(b64).ok()
}

fn has_stand_in(text: &str) -> bool {
    text.chars().any(is_stand_in_char)
}

fn is_stand_in_char(ch: char) -> bool {
    ('\u{1F000}'..='\u{1FAFF}').contains(&ch)
        || ('\u{E000}'..='\u{F8FF}').contains(&ch)
        || ch == '\u{FFFD}'
        || ch == '🤖'
}
