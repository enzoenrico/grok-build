//! Which service answers `/bot`. Selected with `GROK_BOT_PROVIDER` or `[bot] provider`.

use super::gateway::{GatewayBotHub, GatewayCredentials};
use super::hub::{BotHub, HubError};
use super::sdk::SdkBotHub;
use super::session::LOGIN_HINT;
use super::{BotInput, BotSession, HubRun, SubmitOutcome};

pub const BOT_PROVIDER_ENV: &str = "GROK_BOT_PROVIDER";

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum BotProvider {
    /// The computer hub, with the grok.com login.
    #[default]
    Hub,
    /// The Grok Bot HTTP gateway, with `GROK_BOT_GATEWAY_URL` and `GROK_BOT_GATEWAY_TOKEN`.
    Gateway,
}

impl BotProvider {
    fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "hub" => Some(Self::Hub),
            "gateway" => Some(Self::Gateway),
            _ => None,
        }
    }
}

/// Precedence: `GROK_BOT_PROVIDER`, then `[bot] provider`, then the hub.
pub fn resolve_provider(env: Option<&str>, raw_config: &toml::Value) -> BotProvider {
    env.and_then(BotProvider::parse)
        .or_else(|| {
            raw_config
                .get("bot")
                .and_then(|bot| bot.get("provider"))
                .and_then(toml::Value::as_str)
                .and_then(BotProvider::parse)
        })
        .unwrap_or_default()
}

/// Connect the configured provider once and run one `/bot` line or one plain prompt.
pub async fn run(mut session: BotSession, input: BotInput) -> HubRun {
    let outcome = match connect().await {
        Ok(hub) => session.submit(input, hub.as_ref()).await,
        Err(HubError::NeedsLogin) => SubmitOutcome::Shown(LOGIN_HINT.to_string()),
        Err(error) => SubmitOutcome::Shown(error.to_string()),
    };
    HubRun { session, outcome }
}

/// A config that fails to load picks the hub, which reports that failure itself.
async fn connect() -> Result<Box<dyn BotHub>, HubError> {
    let raw = xai_grok_shell::config::load_effective_config()
        .unwrap_or_else(|_| toml::Value::Table(toml::Table::new()));
    let env = std::env::var(BOT_PROVIDER_ENV).ok();
    match resolve_provider(env.as_deref(), &raw) {
        BotProvider::Hub => Ok(Box::new(SdkBotHub::connect().await?)),
        BotProvider::Gateway => Ok(Box::new(GatewayBotHub::new(
            GatewayCredentials::from_env()?
        )?)),
    }
}
