//! Native admission and command presentation, without a gateway or model account.
use anyhow::{Context, Result, bail};
use carillon_discord::{
    config::{DiscordActivation, DiscordConfig, DiscordConversation, DiscordRoute},
    conversations::{
        IncomingMessage, InteractionSource, Participation, admit_interaction, admit_message,
        prepare_conversation,
    },
};
use carillon_discord_module::{
    HostCalls,
    contract::{HostReply, HostRequest, Interaction, InteractionKind, Invocation, Outcome},
    handle,
};
use carillon_protocol::{model::CommandCatalog, models::ModelState};
use serde_json::{Value, json};

// A deliberately controlled model-state collaborator, not a second command implementation.
struct ModelFixture {
    state: ModelState,
    conversation: bool,
}
impl HostCalls for ModelFixture {
    fn call(&mut self, request: HostRequest) -> Result<HostReply, String> {
        match request {
            HostRequest::ModelState => {
                let mut state = self.state.clone();
                if !self.conversation {
                    state.conversation = None;
                }
                Ok(HostReply::ModelState(state))
            }
            HostRequest::SetModel { .. } | HostRequest::Submit { .. } => {
                Err("This read-only example changes and sends nothing.".to_owned())
            }
            HostRequest::Commands => Ok(HostReply::Commands(CommandCatalog::default())),
        }
    }
}

async fn example() -> Result<Value> {
    let directory = tempfile::tempdir()?;
    let participation = Participation::load(directory.path().join("participation.json"))?;
    let route = |channel: &str, conversation| DiscordRoute {
        channel_id: channel.to_owned(),
        agent: "default".to_owned(),
        activation: DiscordActivation::Always,
        conversation,
    };
    // Brand emotes are the operator's, set in the provider's settings.
    let mut settings = serde_json::to_value(DiscordConfig::with_defaults(
        "100".to_owned(),
        "900".to_owned(),
        vec!["200".to_owned()],
        vec![
            route("300", DiscordConversation::Channel),
            route("301", DiscordConversation::Thread),
        ],
    ))?;
    settings["activity_markers"]["models"] = json!({
        "claude-": "<:claude:1300000000000000001>",
        "gpt-": "<:openai:1300000000000000002>",
    });
    let config: DiscordConfig = serde_json::from_value(settings)?;
    let message = IncomingMessage {
        event_id: "400".to_owned(),
        guild_id: Some("100".to_owned()),
        user_id: "200".to_owned(),
        channel_id: "300".to_owned(),
        parent_channel_id: None,
        content: "Hello".to_owned(),
        attachments: Vec::new(),
    };
    let admitted = admit_message(&config, &participation, message.clone(), Value::Null)?
        .context("configured message was not admitted")?;
    let http = twilight_http::Client::new(String::new());
    let resolved = prepare_conversation(&participation, &http, admitted).await?;
    assert_eq!(resolved.conversation_id, "discord-100-300");
    assert_eq!(resolved.event.content, "Hello");
    let refused = admit_message(
        &config,
        &participation,
        IncomingMessage {
            user_id: "999".to_owned(),
            ..message
        },
        Value::Null,
    )
    .err()
    .context("a user outside the allowlist was admitted")?;
    assert_eq!(refused.to_string(), "user is not allowlisted");

    let source = |channel: &str| InteractionSource {
        guild_id: Some("100".to_owned()),
        user_id: "200".to_owned(),
        channel_id: channel.to_owned(),
        parent_channel_id: None,
        kind: InteractionKind::Command,
        name: "model".to_owned(),
        values: Vec::new(),
        options: Value::Null,
    };
    let interaction = admit_interaction(&config, source("300"))?;
    assert_eq!(
        interaction.audience_id.as_deref(),
        Some(resolved.conversation_id.as_str())
    );
    let conversation = picker(interaction.interaction, true)?;
    assert_eq!(
        conversation["embeds"][0]["fields"][0]["name"],
        "This conversation"
    );
    // A thread route's own channel is no conversation: only the default for
    // new conversations is offered there.
    let main = admit_interaction(&config, source("301"))?;
    assert_eq!(main.audience_id, None);
    let channel = picker(main.interaction, false)?;
    assert_eq!(
        channel["embeds"][0]["fields"][0]["name"],
        "Default for new conversations"
    );
    Ok(json!({ "conversation": conversation, "main_channel": channel }))
}

/// The `/model` reply to an admitted interaction, against a T3-like catalogue.
fn picker(interaction: Interaction, conversation: bool) -> Result<Value> {
    let choices = |names: &[(&str, &str)]| -> Value {
        names
            .iter()
            .map(|(id, name)| json!({ "id": id, "name": name }))
            .collect()
    };
    let reasoning = json!({
        "id": "reasoningEffort", "name": "Reasoning", "default": "medium",
        "choices": choices(&[("low", "Low"), ("medium", "Medium"), ("high", "High")]),
    });
    let mut host = ModelFixture {
        state: serde_json::from_value(json!({
            "providers": [
                { "id": "carillon_claude", "name": "Claude · Carillon" },
                { "id": "carillon_codex", "name": "Codex · Carillon" },
            ],
            "available": [
                { "provider": "carillon_claude", "model": "claude-opus-5-5", "name": "Claude Opus 5.5", "options": [
                    reasoning,
                    { "id": "fastMode", "name": "Fast Mode", "default": "off", "choices": choices(&[("off", "Off"), ("on", "On")]) },
                ] },
                { "provider": "carillon_codex", "model": "gpt-6-astra", "name": "GPT-6 Astra", "options": [
                    reasoning,
                    { "id": "serviceTier", "name": "Speed", "default": "default", "choices": choices(&[("default", "Standard"), ("priority", "Fast")]) },
                ] },
            ],
            "agent": {
                "selection": { "provider": "carillon_claude", "model": "claude-opus-5-5", "options": { "reasoningEffort": "medium", "fastMode": "off" } },
                "overridden": true,
            },
            "conversation": {
                "selection": { "provider": "carillon_codex", "model": "gpt-6-astra", "options": { "reasoningEffort": "high", "serviceTier": "priority" } },
                "overridden": true,
            },
        }))?,
        conversation,
    };
    let Outcome::Reply(reply) = handle(&mut host, Invocation::Interaction(Box::new(interaction)))
    else {
        bail!("the model command did not return a reply");
    };
    assert!(!reply.embeds.is_empty());
    assert!(!reply.components.is_empty());
    Ok(serde_json::to_value(reply)?)
}

#[tokio::main]
async fn main() -> Result<()> {
    println!("{}", serde_json::to_string_pretty(&example().await?)?);
    Ok(())
}

#[tokio::test]
async fn example_uses_admission_and_command_components() -> Result<()> {
    example().await?;
    Ok(())
}
