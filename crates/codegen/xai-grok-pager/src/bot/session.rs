//! Session route for one pager agent. Hub calls go through [`BotHub`].

use serde_json::Value;

use super::BotInput;
use super::hub::{AwaitedTurn, BotHub, HubBot, HubError, OnBusy};

/// One login sentence. Missing grok.com auth prints this and nothing else.
pub const LOGIN_HINT: &str = "Log in at grok.com with /login to talk to your Grok bots.";

/// What the pager should do with the line after the session has looked at it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SubmitOutcome {
    /// Not a bot turn. The caller sends the text to the model (or runs a slash command).
    Model,
    /// Scrollback text that is not a bot reply (lists, route changes, errors).
    Shown(String),
    /// The bot's reply, already formatted.
    Reply(String),
}

/// The routed bot and, after a timed-out wait, the handle to resume with.
#[derive(Clone, Debug, PartialEq)]
struct BotRoute {
    id: String,
    name: String,
    pending_handle: Option<Value>,
}

/// Pager-owned route. Cloud jobs are a different list and are not stored here.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct BotSession {
    route: Option<BotRoute>,
}

impl BotSession {
    pub fn is_routed(&self) -> bool {
        self.route.is_some()
    }

    pub fn routed_id(&self) -> Option<&str> {
        self.route.as_ref().map(|route| route.id.as_str())
    }

    pub fn routed_name(&self) -> Option<&str> {
        self.route.as_ref().map(|route| route.name.as_str())
    }

    pub async fn submit(&mut self, input: BotInput, hub: &dyn BotHub) -> SubmitOutcome {
        match input {
            BotInput::Command(args) => self.command(args.as_str(), hub).await,
            BotInput::Plain(text) => self.plain(text.as_str(), hub).await,
        }
    }

    async fn command(&mut self, args: &str, hub: &dyn BotHub) -> SubmitOutcome {
        match parse_command(args) {
            ParsedCommand::List => self.list(hub).await,
            ParsedCommand::Off => {
                self.route = None;
                SubmitOutcome::Shown("Bot route cleared.".to_string())
            }
            ParsedCommand::New { name } => self.create(name.as_str(), hub).await,
            ParsedCommand::Route {
                name,
                text,
                replace,
            } => match self.resolve(name.as_str(), hub).await {
                Ok(Resolved::One(bot)) => {
                    self.route = Some(BotRoute {
                        id: bot.id.clone(),
                        name: bot.name.clone(),
                        pending_handle: None,
                    });
                    if let Some(text) = text {
                        self.send_or_wait(text.as_str(), replace, hub).await
                    } else {
                        SubmitOutcome::Shown(format!(
                            "Routed to {}. Later messages go to that bot until /bot off.",
                            bot.name
                        ))
                    }
                }
                Ok(Resolved::Ambiguous(candidates)) => {
                    SubmitOutcome::Shown(ambiguous_message(name.as_str(), &candidates))
                }
                Ok(Resolved::None) => SubmitOutcome::Shown(format!(
                    "No bot matches \"{name}\". The route is unchanged."
                )),
                Err(error) => shown_error(error),
            },
            ParsedCommand::Usage(message) => SubmitOutcome::Shown(message),
        }
    }

    async fn list(&mut self, hub: &dyn BotHub) -> SubmitOutcome {
        match hub.list_agents().await {
            Ok(bots) => {
                let current = self.routed_id().map(str::to_string);
                SubmitOutcome::Shown(format_bots(&bots, current.as_deref()))
            }
            Err(error) => shown_error(error),
        }
    }

    async fn create(&mut self, name: &str, hub: &dyn BotHub) -> SubmitOutcome {
        if name.is_empty() {
            return SubmitOutcome::Shown("Usage: /bot new <name>".to_string());
        }
        match hub.create_agent(name).await {
            Ok(bot) => {
                let label = bot.name.clone();
                self.route = Some(BotRoute {
                    id: bot.id,
                    name: bot.name,
                    pending_handle: None,
                });
                SubmitOutcome::Shown(format!("Routed to {label}. It greets you itself."))
            }
            Err(error) => shown_error(error),
        }
    }

    async fn resolve(&self, name: &str, hub: &dyn BotHub) -> Result<Resolved, HubError> {
        let listed = hub.list_agents().await?;
        let exact: Vec<HubBot> = listed.into_iter().filter(|bot| bot.name == name).collect();
        if exact.len() == 1 {
            return Ok(exact
                .into_iter()
                .next()
                .map(Resolved::One)
                .unwrap_or(Resolved::None));
        }
        if exact.len() > 1 {
            return Ok(Resolved::Ambiguous(exact));
        }
        let found = hub.search_agents(name).await?;
        match found.len() {
            0 => Ok(Resolved::None),
            1 => Ok(found
                .into_iter()
                .next()
                .map(Resolved::One)
                .unwrap_or(Resolved::None)),
            _ => Ok(Resolved::Ambiguous(found)),
        }
    }

    async fn plain(&mut self, text: &str, hub: &dyn BotHub) -> SubmitOutcome {
        if self.route.is_none() {
            return SubmitOutcome::Model;
        }
        let (prompt, replace) = split_trailing_replace(text);
        if prompt.is_empty() {
            return SubmitOutcome::Shown("Nothing to send.".to_string());
        }
        self.send_or_wait(prompt.as_str(), replace, hub).await
    }

    async fn send_or_wait(
        &mut self,
        prompt: &str,
        replace: bool,
        hub: &dyn BotHub,
    ) -> SubmitOutcome {
        let Some(route) = self.route.clone() else {
            return SubmitOutcome::Model;
        };
        if route.pending_handle.is_some() && !replace {
            return self.resume(route, hub).await;
        }
        let on_busy = if replace {
            OnBusy::Supersede
        } else {
            OnBusy::Reject
        };
        match hub.send_prompt(route.id.as_str(), prompt, on_busy).await {
            Ok(accepted) => self.finish_wait(route, Some(accepted.handle), hub).await,
            Err(HubError::Busy) => {
                let message = format!(
                    "{} already has a turn in flight. Waiting for it. Pass --replace to supersede.",
                    route.name
                );
                let waited = self.finish_wait(route, None, hub).await;
                match waited {
                    SubmitOutcome::Reply(reply) => {
                        SubmitOutcome::Shown(format!("{message}\n{reply}"))
                    }
                    SubmitOutcome::Shown(rest) => {
                        SubmitOutcome::Shown(format!("{message}\n{rest}"))
                    }
                    other => other,
                }
            }
            Err(error) => shown_error(error),
        }
    }

    async fn resume(&mut self, route: BotRoute, hub: &dyn BotHub) -> SubmitOutcome {
        let handle = route.pending_handle.clone();
        self.finish_wait(route, handle, hub).await
    }

    async fn finish_wait(
        &mut self,
        route: BotRoute,
        handle: Option<Value>,
        hub: &dyn BotHub,
    ) -> SubmitOutcome {
        let name = route.name.clone();
        let awaited = match hub.await_turn(route.id.as_str(), handle.as_ref()).await {
            Ok(turn) => turn,
            Err(HubError::TimedOut) => AwaitedTurn::Timeout {
                handle: handle.unwrap_or(Value::Null),
            },
            Err(error) => return shown_error(error),
        };
        match awaited {
            AwaitedTurn::Finished { reply } => {
                if let Some(current) = self.route.as_mut() {
                    current.pending_handle = None;
                }
                SubmitOutcome::Reply(format_bot_reply(name.as_str(), reply.as_str()))
            }
            AwaitedTurn::Timeout { handle } => {
                if let Some(current) = self.route.as_mut() {
                    current.pending_handle = Some(handle);
                }
                SubmitOutcome::Shown(format!(
                    "{name} is still working. The next message waits for that turn."
                ))
            }
        }
    }
}

/// Render a bot reply. The 100-line budget in the unit test times this, not the hub.
pub fn format_bot_reply(name: &str, reply: &str) -> String {
    if reply.is_empty() {
        format!("{name} finished with no text.")
    } else {
        format!("{name}\n{reply}")
    }
}

enum Resolved {
    One(HubBot),
    Ambiguous(Vec<HubBot>),
    None,
}

enum ParsedCommand {
    List,
    Off,
    New {
        name: String,
    },
    Route {
        name: String,
        text: Option<String>,
        replace: bool,
    },
    Usage(String),
}

fn parse_command(args: &str) -> ParsedCommand {
    let replace = args.split_whitespace().any(|token| token == "--replace");
    let tokens: Vec<&str> = args
        .split_whitespace()
        .filter(|token| *token != "--replace")
        .collect();
    let Some(head) = tokens.first().copied() else {
        return ParsedCommand::List;
    };
    if head == "off" {
        return ParsedCommand::Off;
    }
    if head == "new" {
        let name = tokens.iter().skip(1).copied().collect::<Vec<_>>().join(" ");
        return ParsedCommand::New { name };
    }
    let text = tokens.iter().skip(1).copied().collect::<Vec<_>>().join(" ");
    ParsedCommand::Route {
        name: head.to_string(),
        text: if text.is_empty() { None } else { Some(text) },
        replace,
    }
}

/// `--replace` only counts when it is the last token, matching a plain prompt that ends with it.
fn split_trailing_replace(text: &str) -> (String, bool) {
    let trimmed = text.trim_end();
    let Some(rest) = trimmed.strip_suffix("--replace") else {
        return (text.trim().to_string(), false);
    };
    if rest.is_empty() || rest.ends_with(char::is_whitespace) {
        return (rest.trim().to_string(), true);
    }
    (text.trim().to_string(), false)
}

fn shown_error(error: HubError) -> SubmitOutcome {
    let message = match error {
        HubError::NeedsLogin => LOGIN_HINT.to_string(),
        HubError::Busy => "That bot already has a turn in flight.".to_string(),
        HubError::TimedOut => "The bot timed out.".to_string(),
        HubError::Message(message) => message,
    };
    SubmitOutcome::Shown(message)
}

fn format_bots(bots: &[HubBot], current_id: Option<&str>) -> String {
    if bots.is_empty() {
        return "No Grok bots on this computer.".to_string();
    }
    let mut lines = Vec::with_capacity(bots.len());
    for bot in bots {
        let mark = if current_id == Some(bot.id.as_str()) {
            " *"
        } else {
            ""
        };
        let description = if bot.description.is_empty() {
            "—"
        } else {
            bot.description.as_str()
        };
        let status = if bot.status.is_empty() {
            "unknown"
        } else {
            bot.status.as_str()
        };
        lines.push(format!("{} — {description} — {status}{mark}", bot.name));
    }
    lines.join("\n")
}

fn ambiguous_message(name: &str, candidates: &[HubBot]) -> String {
    let mut lines = vec![format!(
        "\"{name}\" matches more than one bot. The route is unchanged."
    )];
    lines.push(format_bots(candidates, None));
    lines.join("\n")
}
