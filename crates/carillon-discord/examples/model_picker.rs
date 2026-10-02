//! Native admission and command presentation, without a gateway or model account.
use anyhow::{Context, Result, bail};
use carillon_discord::{
    config::{DiscordActivation, DiscordConfig, DiscordConversation, DiscordRoute},
    conversations::{
        IncomingMessage, InteractionSource, Participation, admit_interaction, admit_message,
        prepare_conversation,
    },
};
use carillon_discord_commands::{
    HostCalls,
    contract::{HostRequest, InteractionKind, Invocation, Outcome},
    handle,
};
use carillon_protocol::models::ModelState;
use serde_json::{Value, json};

// A deliberately controlled model-state collaborator, not a second command implementation.
struct ModelFixture {
    state: ModelState,
}
impl HostCalls for ModelFixture {
    fn call(&mut self, request: HostRequest) -> Result<ModelState, String> {
        match request {
            HostRequest::ModelState => Ok(self.state.clone()),
            HostRequest::SetModel { .. } => {
                Err("This read-only example does not change a model.".to_owned())
            }
        }
    }
}

async fn example() -> Result<Value> {
    let directory = tempfile::tempdir()?;
    let participation = Participation::load(directory.path().join("participation.json"))?;
    let config = DiscordConfig::with_defaults(
        "default".to_owned(),
        "100".to_owned(),
        "900".to_owned(),
        vec!["200".to_owned()],
        vec![DiscordRoute {
            channel_id: "300".to_owned(),
            activation: DiscordActivation::Always,
            conversation: DiscordConversation::Channel,
        }],
    );
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
    assert!(
        admit_message(
            &config,
            &participation,
            IncomingMessage {
                user_id: "999".to_owned(),
                ..message
            },
            Value::Null
        )
        .is_err()
    );

    let interaction = admit_interaction(
        &config,
        InteractionSource {
            guild_id: Some("100".to_owned()),
            user_id: "200".to_owned(),
            channel_id: "300".to_owned(),
            parent_channel_id: None,
            kind: InteractionKind::Command,
            name: "model".to_owned(),
            values: Vec::new(),
            options: Value::Null,
        },
    )?;
    assert_eq!(
        interaction.audience_id.as_deref(),
        Some(resolved.conversation_id.as_str())
    );
    let mut host = ModelFixture {
        state: serde_json::from_value(json!({
            "available": [{ "provider": "example", "model": "small", "name": "Small", "options": [] }],
            "agent": { "selection": { "provider": "example", "model": "small", "options": {} }, "overridden": false },
            "conversation": { "selection": { "provider": "example", "model": "small", "options": {} }, "overridden": false }
        }))?,
    };
    let Outcome::Reply(reply) = handle(&mut host, Invocation::Interaction(interaction.interaction))
    else {
        bail!("the model command did not return a reply");
    };
    assert!(reply.content.contains("example"));
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
