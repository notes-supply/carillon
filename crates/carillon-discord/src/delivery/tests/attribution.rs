//! Who answered: the agent behind each message the bot delivered.

use super::*;
use crate::conversations::{Author, IncomingMessage, Reference, ReferenceKind, admit_message};
use twilight_model::id::Id;

#[tokio::test]
async fn a_reply_to_a_delivered_answer_names_the_agent_that_gave_it() -> anyhow::Result<()> {
    let messages = Arc::new(tokio::sync::Mutex::new(Vec::new()));
    let acks = Arc::new(tokio::sync::Mutex::new(Vec::new()));
    let app = delivering_core(
        vec![Delivery {
            content: format!("{}\n\n{}", "a".repeat(1_990), "b".repeat(30)),
            ..answer(1, TurnState::Completed)
        }],
        messages.clone(),
        acks.clone(),
        Duration::ZERO,
        Arc::default(),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let address = listener.local_addr()?;
    let server = tokio::spawn(async move { axum::serve(listener, app).await });
    let http = discord_client(address);
    let directory = tempfile::tempdir()?;
    let (outcomes, _observed) = tokio::sync::mpsc::unbounded_channel();
    let config = discord_config(DiscordActivation::Always, DiscordConversation::Channel);
    tokio::time::timeout(
        Duration::from_secs(5),
        delivery_loop(
            core_client(format!("http://{address}"))?,
            Reactions::new(http.clone(), crate::config::Settings::new(config.clone())),
            participation(directory.path())?,
            std::path::PathBuf::new(),
            outcomes,
            tokio_util::sync::CancellationToken::new(),
        ),
    )
    .await?
    .err()
    .context("the loop ends only when core refuses a claim")?;
    server.abort();
    assert_eq!(acks.lock().await[0]["native_ref"], "901,902");

    let restarted = participation(directory.path())?;
    let reply = |to: u64, agent: &str| -> anyhow::Result<String> {
        let mut config = config.clone();
        agent.clone_into(&mut config.routes[0].agent);
        let message = IncomingMessage {
            event_id: "903".to_owned(),
            guild_id: Some("100".to_owned()),
            user_id: "200".to_owned(),
            channel_id: "300".to_owned(),
            parent_channel_id: None,
            content: "thanks".to_owned(),
            attachments: Vec::new(),
            reference: Some(Reference {
                kind: ReferenceKind::Reply,
                author: Some(Author::Bot {
                    message_id: Id::new(to),
                }),
                sent_at: "2026-11-05T14:03:00+00:00".to_owned(),
                forwarding: false,
                content: "b".to_owned(),
                files: Vec::new(),
            }),
        };
        let plan = admit_message(&config, &restarted, message, serde_json::Value::Null)
            .map_err(|failure| anyhow::anyhow!(failure.message))?
            .context("the reply was ignored")?;
        Ok(plan.quotes.concat())
    };
    for to in [901, 902] {
        assert_eq!(
            reply(to, "default")?,
            "In reply to your message, sent 2026-11-05T14:03:00+00:00:\n> b"
        );
        assert_eq!(
            reply(to, "code")?,
            "In reply to a message from agent \"default\", sent 2026-11-05T14:03:00+00:00:\n> b"
        );
    }
    Ok(())
}
