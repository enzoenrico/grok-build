//! `/bot <name>` through pager dispatch, using the computer-hub wire shape.
//!
//! The unit-test fake hub already returns parsed bots and a `reply` string.
//! The harness does not. A roster row is camelCase `agentId` inside an MCP
//! text block, and `bot_await_turn` reports the reply as `preview`.

use std::sync::Mutex;

use async_trait::async_trait;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use serde_json::{Value, json};

use super::*;
use crate::bot::{
    AwaitedTurn, BotHub, BotInput, HubBot, HubError, OnBusy, SendAccepted,
    awaited_turn_from_tool_result, bots_from_tool_result,
};
use crate::scrollback::ScrollbackPane;
use crate::scrollback::render::ScratchBuffer;

const ROUTE_LINE: &str = "Routed to Ada. Later messages go to that bot until /bot off.";
const REPLY_LINE: &str = "Ada here. The docs are started.";

struct WireHub {
    list: Value,
    await_body: Value,
    sends: Mutex<Vec<String>>,
}

impl WireHub {
    fn computer_hub() -> Self {
        let roster = json!({
            "agents": [{
                "agentId": "agt_ada",
                "name": "Ada",
                "status": "idle",
                "viewerIsOwner": true
            }]
        });
        let awaited = json!({
            "finished": true,
            "preview": REPLY_LINE
        });
        Self {
            list: mcp_text(&roster.to_string()),
            await_body: mcp_text(&awaited.to_string()),
            sends: Mutex::new(Vec::new()),
        }
    }
}

fn mcp_text(text: &str) -> Value {
    json!({
        "blocks": [{ "type": "text", "text": text }]
    })
}

#[async_trait]
impl BotHub for WireHub {
    async fn list_agents(&self) -> Result<Vec<HubBot>, HubError> {
        bots_from_tool_result(&self.list)
    }

    async fn search_agents(&self, _query: &str) -> Result<Vec<HubBot>, HubError> {
        bots_from_tool_result(&self.list)
    }

    async fn create_agent(&self, _name: &str) -> Result<HubBot, HubError> {
        Err(HubError::Message("create is not used here".to_string()))
    }

    async fn send_prompt(
        &self,
        _agent_id: &str,
        prompt: &str,
        _on_busy: OnBusy,
    ) -> Result<SendAccepted, HubError> {
        self.sends.lock().expect("sends").push(prompt.to_string());
        Ok(SendAccepted {
            handle: json!({ "turnId": "turn_7" }),
        })
    }

    async fn await_turn(
        &self,
        _agent_id: &str,
        _handle: Option<&Value>,
    ) -> Result<AwaitedTurn, HubError> {
        Ok(awaited_turn_from_tool_result(&self.await_body))
    }
}

fn take_bot_submit(effects: Vec<Effect>) -> (AgentId, BotInput, crate::bot::BotSession) {
    let mut found = None;
    for effect in effects {
        if let Effect::BotSubmit {
            agent_id,
            input,
            session,
        } = effect
        {
            found = Some((agent_id, input, session));
        }
    }
    found.expect("pager dispatch must schedule the bot submit")
}

fn paint(app: &mut AppView, agent_id: AgentId) -> String {
    let agent = test_agent_mut(app, agent_id);
    let area = Rect::new(0, 0, 120, 24);
    agent.scrollback.prepare_layout(area.width, area.height);
    let mut buffer = Buffer::empty(area);
    let mut scratch = ScratchBuffer::default();
    let _ = ScrollbackPane::new()
        .active(true)
        .render_with_scratch_and_selection_boundaries(
            area,
            &mut buffer,
            &agent.scrollback,
            &mut scratch,
        );
    let mut lines = Vec::with_capacity(area.height as usize);
    for y in 0..area.height {
        let mut line = String::new();
        for x in 0..area.width {
            line.push_str(buffer.cell((x, y)).expect("cell").symbol());
        }
        lines.push(line);
    }
    lines.join("\n")
}

fn collapsed(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[tokio::test]
async fn bot_name_route_and_reply_reach_the_screen() {
    let mut app = test_app_with_agent();
    let hub = WireHub::computer_hub();

    let effects = dispatch(Action::SendPrompt("/bot Ada".into()), &mut app);
    let (agent_id, input, mut session) = take_bot_submit(effects);
    assert!(
        matches!(input, BotInput::Command(ref args) if args == "Ada"),
        "the slash line is the bot command"
    );
    let outcome = session.submit(input, &hub).await;
    dispatch(
        Action::TaskComplete(TaskResult::BotSubmit {
            agent_id,
            session,
            outcome,
        }),
        &mut app,
    );

    let screen = collapsed(&paint(&mut app, agent_id));
    assert!(
        screen.contains(ROUTE_LINE),
        "/bot Ada must show the route on screen, got {screen:?}"
    );
    assert_eq!(test_agent(&app, agent_id).bot.routed_id(), Some("agt_ada"));

    let effects = dispatch(Action::SendPrompt("hello from the pager".into()), &mut app);
    let (agent_id, input, mut session) = take_bot_submit(effects);
    assert!(matches!(input, BotInput::Plain(_)));
    let outcome = session.submit(input, &hub).await;
    dispatch(
        Action::TaskComplete(TaskResult::BotSubmit {
            agent_id,
            session,
            outcome,
        }),
        &mut app,
    );

    let screen = collapsed(&paint(&mut app, agent_id));
    assert!(
        screen.contains(REPLY_LINE),
        "the bot reply must reach the screen, got {screen:?}"
    );
    assert!(
        !screen.contains("finished with no text"),
        "a preview reply is text, got {screen:?}"
    );
    assert_eq!(
        hub.sends.lock().expect("sends").as_slice(),
        ["hello from the pager"]
    );
}
