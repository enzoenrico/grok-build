//! `/bot` against a mock Grok Bot gateway. The mock answers only requests that
//! carry the bearer token, so a missing or wrong header fails every mock test.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{Value, json};
use wiremock::matchers::{header, method};
use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

use super::gateway::{GatewayBotHub, GatewayCredentials, MISSING_CREDENTIALS, WaitTiming};
use super::hub::HubError;
use super::provider::{BotProvider, resolve_provider};
use super::session::{SubmitOutcome, format_bot_reply};
use super::{BotInput, BotSession};

const TOKEN: &str = "gw-test-token";

#[derive(Default)]
struct GatewayState {
    /// Bodies of every call, keyed by the `/api/<method>` name.
    calls: Vec<(String, Value)>,
    /// `clientNonce` of the last `sendPrompt`.
    nonce: Option<String>,
    /// Whether the transcript tail includes Ada's reply yet.
    replied: bool,
    /// Ada's turn ends on an approval card with no text.
    approval_only: bool,
    /// `sendPrompt` answers 200 with a body that is not JSON.
    send_plain_text: bool,
    /// When set, the next call to this method fails with this status and body.
    fail: Option<(String, u16, Value)>,
}

#[derive(Clone, Default)]
struct FakeGateway(Arc<Mutex<GatewayState>>);

impl FakeGateway {
    fn state(&self) -> std::sync::MutexGuard<'_, GatewayState> {
        self.0.lock().expect("gateway state")
    }

    fn calls(&self, name: &str) -> Vec<Value> {
        self.state()
            .calls
            .iter()
            .filter(|(method, _)| method == name)
            .map(|(_, body)| body.clone())
            .collect()
    }
}

impl Respond for FakeGateway {
    fn respond(&self, request: &Request) -> ResponseTemplate {
        let name = request
            .url
            .path()
            .strip_prefix("/api/")
            .unwrap_or("")
            .to_string();
        let body: Value = request.body_json().unwrap_or(Value::Null);
        let mut state = self.state();
        state.calls.push((name.clone(), body.clone()));
        if let Some((failing, status, payload)) = state.fail.clone()
            && failing == name
        {
            state.fail = None;
            return ResponseTemplate::new(status).set_body_json(payload);
        }
        match name.as_str() {
            "listAgents" => ResponseTemplate::new(200).set_body_json(json!({
                "agents": [
                    { "id": "agt-ada", "name": "Ada", "description": "writes rust" },
                    { "id": "agt-crew", "name": "Crew", "isGroup": true, "memberIds": ["agt-ada"] },
                ]
            })),
            "createAgent" => ResponseTemplate::new(200).set_body_json(json!({
                "agent": { "id": "agt-new", "name": body["name"] }
            })),
            "sendPrompt" => {
                state.nonce = body["clientNonce"].as_str().map(str::to_string);
                if state.send_plain_text {
                    return ResponseTemplate::new(200).set_body_string("queued");
                }
                ResponseTemplate::new(200).set_body_json(json!({ "messageId": "msg-1" }))
            }
            "getAgentTranscriptTail" => {
                let nonce = state.nonce.clone().unwrap_or_default();
                let mut entries = vec![
                    json!({ "id": "e0", "kind": "send-message", "requestId": "req-old",
                            "message": { "type": "text", "content": "an older answer" } }),
                    json!({ "id": "e1", "kind": "message", "role": "user", "clientNonce": nonce,
                            "requestId": "req-new", "prompt": "hi" }),
                    json!({ "id": "e2", "kind": "message", "role": "user", "clientNonce": "another-client",
                            "requestId": "req-other", "prompt": "from the desktop app" }),
                    json!({ "id": "e3", "kind": "send-message", "requestId": "req-other",
                            "message": { "type": "text", "content": "not your reply" } }),
                ];
                if state.replied {
                    entries.push(json!({ "id": "e4", "kind": "send-message", "requestId": "req-new",
                                         "message": { "type": "text", "content": "hello from Ada" } }));
                }
                if state.approval_only {
                    entries.push(json!({ "id": "e5", "kind": "send-message", "requestId": "req-new",
                                         "message": { "type": "local-tool-permission",
                                                      "ask": { "status": "pending", "requestId": "ask-1" } } }));
                }
                ResponseTemplate::new(200).set_body_json(json!({ "entries": entries }))
            }
            _ => ResponseTemplate::new(404).set_body_json(json!({ "error": "unknown method" })),
        }
    }
}

async fn gateway() -> (MockServer, FakeGateway, GatewayBotHub) {
    gateway_with_budget(Duration::from_secs(5)).await
}

/// A gateway whose waits give up after `budget`. Long enough by default that a slow runner does not time out.
async fn gateway_with_budget(budget: Duration) -> (MockServer, FakeGateway, GatewayBotHub) {
    let server = MockServer::start().await;
    let fake = FakeGateway::default();
    Mock::given(method("POST"))
        .and(header("authorization", format!("Bearer {TOKEN}").as_str()))
        .respond_with(fake.clone())
        .mount(&server)
        .await;
    let credentials =
        GatewayCredentials::parse(Some(server.uri().as_str()), Some(TOKEN), true).expect("creds");
    let timing = WaitTiming {
        poll_every: Duration::from_millis(5),
        settle: Duration::from_millis(20),
        budget,
    };
    let hub = GatewayBotHub::with_timing(credentials, timing).expect("hub");
    (server, fake, hub)
}

fn shown(outcome: SubmitOutcome) -> String {
    match outcome {
        SubmitOutcome::Shown(text) => text,
        other => panic!("expected Shown, got {other:?}"),
    }
}

#[tokio::test]
async fn roster_lists_gateway_bots_with_groups_marked() {
    let (_server, _fake, hub) = gateway().await;
    let mut session = BotSession::default();
    let text = shown(session.submit(BotInput::Command(String::new()), &hub).await);
    assert_eq!(text, "Ada — writes rust — unknown\nCrew — — — group");
}

#[tokio::test]
async fn routed_prompt_returns_only_the_reply_to_that_prompt() {
    let (_server, fake, hub) = gateway().await;
    fake.state().replied = true;
    let mut session = BotSession::default();
    session
        .submit(BotInput::Command("Ada".to_string()), &hub)
        .await;
    let outcome = session
        .submit(BotInput::Plain("hi".to_string()), &hub)
        .await;

    assert_eq!(
        outcome,
        SubmitOutcome::Reply(format_bot_reply("Ada", "hello from Ada"))
    );
    let sends = fake.calls("sendPrompt");
    assert_eq!(sends.len(), 1);
    assert_eq!(sends[0]["agentId"], "agt-ada");
    assert_eq!(sends[0]["prompt"], "hi");
    assert!(
        sends[0]["clientNonce"]
            .as_str()
            .is_some_and(|nonce| !nonce.is_empty())
    );
    assert_eq!(
        fake.calls("getAgentTranscriptTail")[0],
        json!({ "id": "agt-ada", "limit": 200 })
    );
}

#[tokio::test]
async fn slow_turn_keeps_the_handle_and_the_next_line_resumes_it() {
    let (_server, fake, hub) = gateway_with_budget(Duration::from_millis(100)).await;
    let mut session = BotSession::default();
    session
        .submit(BotInput::Command("Ada".to_string()), &hub)
        .await;

    let first = session
        .submit(BotInput::Plain("hi".to_string()), &hub)
        .await;
    assert_eq!(
        shown(first),
        "Ada is still working. The next message waits for that turn."
    );

    fake.state().replied = true;
    let second = session
        .submit(BotInput::Plain("are you done?".to_string()), &hub)
        .await;
    assert_eq!(
        second,
        SubmitOutcome::Reply(format_bot_reply("Ada", "hello from Ada"))
    );
    assert_eq!(fake.calls("sendPrompt").len(), 1, "resume must not resend");
}

#[tokio::test]
async fn failed_poll_keeps_the_prompt_pending_instead_of_dropping_it() {
    let (_server, fake, hub) = gateway().await;
    let mut session = BotSession::default();
    session
        .submit(BotInput::Command("Ada".to_string()), &hub)
        .await;
    fake.state().fail = Some((
        "getAgentTranscriptTail".to_string(),
        502,
        json!({ "error": "bad gateway" }),
    ));

    let first = session
        .submit(BotInput::Plain("hi".to_string()), &hub)
        .await;
    assert_eq!(
        shown(first),
        "Ada is still working. The next message waits for that turn."
    );

    fake.state().replied = true;
    let second = session
        .submit(BotInput::Plain("again".to_string()), &hub)
        .await;
    assert_eq!(
        second,
        SubmitOutcome::Reply(format_bot_reply("Ada", "hello from Ada"))
    );
    assert_eq!(fake.calls("sendPrompt").len(), 1, "resume must not resend");
}

#[tokio::test]
async fn turn_that_ends_on_an_approval_card_still_finishes() {
    let (_server, fake, hub) = gateway().await;
    fake.state().approval_only = true;
    let mut session = BotSession::default();
    session
        .submit(BotInput::Command("Ada".to_string()), &hub)
        .await;
    let outcome = session
        .submit(BotInput::Plain("hi".to_string()), &hub)
        .await;
    assert_eq!(
        outcome,
        SubmitOutcome::Reply(format_bot_reply(
            "Ada",
            "Waiting for your approval. Answer it in Grok Bot."
        ))
    );
}

#[tokio::test]
async fn send_accepted_with_a_plain_text_body_still_waits_for_the_reply() {
    let (_server, fake, hub) = gateway().await;
    {
        let mut state = fake.state();
        state.send_plain_text = true;
        state.replied = true;
    }
    let mut session = BotSession::default();
    session
        .submit(BotInput::Command("Ada".to_string()), &hub)
        .await;
    let outcome = session
        .submit(BotInput::Plain("hi".to_string()), &hub)
        .await;
    assert_eq!(
        outcome,
        SubmitOutcome::Reply(format_bot_reply("Ada", "hello from Ada"))
    );
}

#[tokio::test]
async fn lowercase_name_routes_by_exact_name_not_description() {
    let (_server, _fake, hub) = gateway().await;
    let mut session = BotSession::default();
    let text = shown(
        session
            .submit(BotInput::Command("ada".to_string()), &hub)
            .await,
    );
    assert_eq!(
        text,
        "Routed to Ada. Later messages go to that bot until /bot off."
    );
    let missing = shown(
        session
            .submit(BotInput::Command("rust".to_string()), &hub)
            .await,
    );
    assert_eq!(missing, "No bot matches \"rust\". The route is unchanged.");
}

#[tokio::test]
async fn gateway_error_is_shown_verbatim_without_the_token() {
    let (_server, fake, hub) = gateway().await;
    fake.state().fail = Some((
        "listAgents".to_string(),
        401,
        json!({ "message": format!("invalid gateway token {TOKEN}") }),
    ));
    let mut session = BotSession::default();
    let text = shown(session.submit(BotInput::Command(String::new()), &hub).await);
    assert_eq!(
        text,
        "listAgents failed: 401 invalid gateway token [redacted]"
    );
}

#[tokio::test]
async fn send_rejected_by_the_server_is_not_called_unknown() {
    let (_server, fake, hub) = gateway().await;
    let mut session = BotSession::default();
    session
        .submit(BotInput::Command("Ada".to_string()), &hub)
        .await;

    fake.state().fail = Some((
        "sendPrompt".to_string(),
        400,
        json!({ "error": "prompt is empty" }),
    ));
    let rejected = shown(session.submit(BotInput::Plain("x".to_string()), &hub).await);
    assert_eq!(rejected, "sendPrompt failed: 400 prompt is empty");

    fake.state().fail = Some((
        "sendPrompt".to_string(),
        503,
        json!({ "error": "box is restarting" }),
    ));
    let unknown = shown(session.submit(BotInput::Plain("x".to_string()), &hub).await);
    assert_eq!(
        unknown,
        "sendPrompt failed: 503 box is restarting (delivery unknown; check the thread before resending)"
    );
}

#[tokio::test]
async fn bot_new_creates_through_the_gateway_and_routes_to_it() {
    let (_server, fake, hub) = gateway().await;
    let mut session = BotSession::default();
    let text = shown(
        session
            .submit(BotInput::Command("new Scout".to_string()), &hub)
            .await,
    );
    assert_eq!(text, "Routed to Scout. It greets you itself.");
    assert_eq!(session.routed_id(), Some("agt-new"));
    assert_eq!(fake.calls("createAgent")[0]["name"], "Scout");
}

#[test]
fn credentials_follow_the_grok_bot_cli_host_policy() {
    let parse = |url: &str, allow_local: bool| {
        GatewayCredentials::parse(Some(url), Some(TOKEN), allow_local).map(|_| ())
    };
    let message = |text: &str| Err(HubError::Message(text.to_string()));

    assert_eq!(parse("https://box-1.us12.cursorvm.com", false), Ok(()));
    assert_eq!(parse("https://api2.cursor.sh/", false), Ok(()));
    assert_eq!(parse("http://127.0.0.1:8080", true), Ok(()));
    assert_eq!(
        parse("http://box-1.us12.cursorvm.com", false),
        message("Rejected gateway URL: only https is allowed (got http:).")
    );
    assert_eq!(
        parse("https://cursor.sh.example.com", false),
        message(
            "Rejected gateway URL host \"cursor.sh.example.com\". Expected *.cursor.sh / *.cursor.com / *.cursorvm.com."
        )
    );
    assert_eq!(
        parse("http://localhost:8080", false),
        message(
            "Rejected gateway URL host \"localhost\". Set GROK_BOT_ALLOW_LOCAL_GATEWAY=1 to permit localhost/127.0.0.1 gateways."
        )
    );
    assert_eq!(
        parse("https://user:pw@api2.cursor.sh", false),
        message("Rejected gateway URL: userinfo is not allowed.")
    );
    assert_eq!(
        GatewayCredentials::parse(Some("https://api2.cursor.sh"), Some("  "), false).map(|_| ()),
        message(MISSING_CREDENTIALS)
    );
    assert_eq!(
        GatewayCredentials::parse(None, Some(TOKEN), false).map(|_| ()),
        message(MISSING_CREDENTIALS)
    );
}

#[test]
fn credentials_debug_never_prints_the_token() {
    let credentials =
        GatewayCredentials::parse(Some("https://api2.cursor.sh"), Some(TOKEN), false).unwrap();
    assert!(!format!("{credentials:?}").contains(TOKEN));
}

#[test]
fn provider_env_beats_config_and_hub_is_the_default() {
    let config = |text: &str| -> toml::Value { toml::from_str(text).unwrap() };
    let gateway = config("[bot]\nprovider = \"gateway\"");
    assert_eq!(resolve_provider(None, &gateway), BotProvider::Gateway);
    assert_eq!(resolve_provider(Some("hub"), &gateway), BotProvider::Hub);
    assert_eq!(
        resolve_provider(Some("nope"), &gateway),
        BotProvider::Gateway
    );
    assert_eq!(
        resolve_provider(Some(" Gateway "), &config("")),
        BotProvider::Gateway
    );
    assert_eq!(resolve_provider(None, &config("")), BotProvider::Hub);
}
