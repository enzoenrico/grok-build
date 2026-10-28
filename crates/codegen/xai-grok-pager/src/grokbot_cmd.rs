//! `grok grokbot` calls a GrokBot host gateway over HTTP.
//!
//! `/bot` stays on the computer hub. This command does not read that route.

use anyhow::Result;
use clap::Subcommand;

use crate::grokbot_http::{
    GatewayClient, GatewayConfig, GatewayHealth, GrokBotAgent, GrokBotError,
};

#[derive(Debug, clap::Args, Clone)]
pub struct GrokbotArgs {
    #[command(subcommand)]
    command: GrokbotCommand,
}

#[derive(Debug, Subcommand, Clone)]
enum GrokbotCommand {
    /// Read GET /health. This call does not send SAND_GATEWAY_TOKEN.
    Health {
        /// Emit the health JSON.
        #[arg(long)]
        json: bool,
    },
    /// List agents with POST /api/listAgents.
    Agents {
        /// Emit the agent JSON.
        #[arg(long)]
        json: bool,
    },
    /// Send one prompt with POST /api/sendPrompt.
    Send {
        /// Agent id from `grok grokbot agents`.
        agent_id: String,
        /// Prompt text. Quote it when it contains spaces.
        prompt: String,
    },
}

pub async fn run(args: GrokbotArgs) -> Result<()> {
    let client = GatewayClient::new(GatewayConfig::from_env());
    match args.command {
        GrokbotCommand::Health { json } => {
            let health = client.health().await.map_err(anyhow_error)?;
            print_health(json, &health)?;
        }
        GrokbotCommand::Agents { json } => {
            let agents = client.list_agents().await.map_err(anyhow_error)?;
            print_agents(json, &agents)?;
        }
        GrokbotCommand::Send { agent_id, prompt } => {
            client
                .send_prompt(&agent_id, &prompt)
                .await
                .map_err(anyhow_error)?;
            println!("accepted");
        }
    }
    Ok(())
}

fn anyhow_error(err: GrokBotError) -> anyhow::Error {
    anyhow::anyhow!("{err}")
}

fn print_health(json: bool, health: &GatewayHealth) -> Result<()> {
    if json {
        println!("{}", serde_json::to_string(health)?);
        return Ok(());
    }
    if health.ok {
        match health.pid {
            Some(pid) => println!("ok pid={pid}"),
            None => println!("ok"),
        }
    } else {
        println!("not ok");
    }
    Ok(())
}

fn print_agents(json: bool, agents: &[GrokBotAgent]) -> Result<()> {
    if json {
        println!("{}", serde_json::to_string(agents)?);
        return Ok(());
    }
    if agents.is_empty() {
        println!("no agents");
        return Ok(());
    }
    for agent in agents {
        let state = if agent.is_running { "running" } else { "idle" };
        if agent.description.is_empty() {
            println!("{name}  {id}  {state}", name = agent.name, id = agent.id);
        } else {
            println!(
                "{name}  {id}  {state}  {description}",
                name = agent.name,
                id = agent.id,
                description = agent.description
            );
        }
    }
    Ok(())
}
