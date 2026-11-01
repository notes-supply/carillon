//! Native admission and the `/settings` card, without a gateway or model
//! account.
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
    contract::{HostReply, HostRequest, InteractionKind, Invocation, Outcome, Reply},
    handle,
};
use carillon_protocol::settings::Page;
use serde_json::{Value, json};

/// The page the provider would compose for channel 300: its route lines and
/// a runtime's, inheriting the agent's model. It changes nothing.
struct PageFixture(Page);

impl HostCalls for PageFixture {
    fn call(&mut self, request: HostRequest) -> Result<HostReply, String> {
        match request {
            HostRequest::Settings => Ok(HostReply::Page {
                page: self.0.clone(),
                notice: None,
            }),
            HostRequest::ChangeSetting(_) | HostRequest::Commands | HostRequest::Submit { .. } => {
                Err("This read-only example changes and sends nothing.".to_owned())
            }
        }
    }
}

fn page() -> Result<Page> {
    let route_line = |id: &str, name: &str, value: &str, shown: &str, revision: char| {
        json!({
            "id": id, "name": name,
            "choices": [{"id": value, "name": shown}],
            "value": {"kind": "one", "id": value}, "inherited": false,
            "revision": revision.to_string().repeat(64),
        })
    };
    let models = [
        ("openai-codex/gpt-6.1-sol", "GPT-6.1 Sol"),
        ("openai-codex/gpt-6.1-mini", "GPT-6.1 Mini"),
    ]
    .map(|(id, name)| json!({"id": id, "name": name, "group": "OpenAI Codex"}));
    let thinking = [("low", "Low"), ("medium", "Medium"), ("high", "High")]
        .map(|(id, name)| json!({"id": id, "name": name}));
    Ok(serde_json::from_value(json!({
        "title": "<#300>",
        "subject": {"kind": "channel", "interface_id": "discord", "channel_id": "300", "agent_id": "assistant"},
        "interface": [
            route_line("agent", "Agent", "assistant", "assistant", '1'),
            route_line("listens", "Listens", "always", "always", '2'),
            route_line("replies", "Replies", "thread", "threads", '3'),
        ],
        "runtime": {"state": "settings", "settings": [
            {
                "id": "model", "name": "Model", "choices": models,
                "value": {"kind": "one", "id": "openai-codex/gpt-6.1-sol"}, "inherited": true,
                "revision": "a".repeat(64),
            },
            {
                "id": "model.thinking", "name": "Thinking", "choices": thinking,
                "value": {"kind": "one", "id": "medium"}, "inherited": true,
                "revision": "b".repeat(64),
            },
        ]},
    }))?)
}

async fn example() -> Result<Reply> {
    let directory = tempfile::tempdir()?;
    let participation = Participation::load(directory.path().join("participation.json"))?;
    // Brand emotes are the operator's, set in the provider's settings.
    let mut settings = serde_json::to_value(DiscordConfig::with_defaults(
        "100".to_owned(),
        "900".to_owned(),
        vec!["200".to_owned()],
        vec![DiscordRoute {
            channel_id: "300".to_owned(),
            agent: "assistant".to_owned(),
            activation: DiscordActivation::Always,
            conversation: DiscordConversation::Thread,
        }],
    ))?;
    settings["activity_markers"]["models"] = json!({ "gpt-": "<:openai:1300000000000000002>" });
    let config: DiscordConfig = serde_json::from_value(settings)?;
    let message = IncomingMessage {
        event_id: "400".to_owned(),
        guild_id: Some("100".to_owned()),
        user_id: "200".to_owned(),
        channel_id: "401".to_owned(),
        parent_channel_id: Some("300".to_owned()),
        content: "Hello".to_owned(),
        attachments: Vec::new(),
    };
    let admitted = admit_message(&config, &participation, message.clone(), Value::Null)?
        .context("configured message was not admitted")?;
    let http = twilight_http::Client::new(String::new());
    let resolved = prepare_conversation(&participation, &http, admitted).await?;
    assert_eq!(resolved.conversation_id, "discord-100-401");
    assert_eq!(
        resolved.event.channel_id, "300",
        "a thread's conversation belongs to its routed channel"
    );
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

    let interaction = admit_interaction(
        &config,
        InteractionSource {
            guild_id: Some("100".to_owned()),
            user_id: "200".to_owned(),
            channel_id: "401".to_owned(),
            parent_channel_id: Some("300".to_owned()),
            kind: InteractionKind::Command,
            name: "settings".to_owned(),
            values: Vec::new(),
            options: Value::Null,
        },
    )?;
    let Outcome::Reply(reply) = handle(
        &mut PageFixture(page()?),
        Invocation::Interaction(Box::new(interaction.interaction)),
    ) else {
        bail!("/settings did not return a reply");
    };
    assert!(
        reply
            .content
            .starts_with("<#300>\n```\nAgent     assistant\n")
    );
    assert!(!reply.components.is_empty());
    Ok(reply)
}

#[tokio::main]
async fn main() -> Result<()> {
    let reply = example().await?;
    println!("{}", reply.content);
    println!("{}", serde_json::to_string_pretty(&reply.components)?);
    Ok(())
}

#[tokio::test]
async fn example_uses_admission_and_the_settings_card() -> Result<()> {
    example().await?;
    Ok(())
}
