//! External callers use native preparation, not private plans or simulation shortcuts.
use anyhow::{Context, Result};
use axum::{
    Json, Router,
    http::StatusCode,
    response::IntoResponse,
    routing::{get, post},
};
use carillon_discord::{
    config::{DiscordActivation, DiscordConfig, DiscordConversation, DiscordRoute},
    conversations::{IncomingMessage, Participation, admit_message, prepare_conversation},
};
use serde_json::{Value, json};
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

fn config() -> DiscordConfig {
    DiscordConfig::with_defaults(
        "agent".into(),
        "1".into(),
        "9".into(),
        vec!["2".into()],
        vec![DiscordRoute {
            channel_id: "100".into(),
            activation: DiscordActivation::Mention,
            conversation: DiscordConversation::Thread,
        }],
    )
}
fn message(id: &str, channel: &str, parent: Option<&str>, content: &str) -> IncomingMessage {
    IncomingMessage {
        event_id: id.into(),
        guild_id: Some("1".into()),
        user_id: "2".into(),
        channel_id: channel.into(),
        parent_channel_id: parent.map(str::to_owned),
        content: content.into(),
        attachments: Vec::new(),
    }
}

#[tokio::test]
async fn native_preparation_creates_once_and_persists_activation() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("participation.json");
    let participation = Participation::load(path.clone())?;
    let creates = Arc::new(AtomicUsize::new(0));
    let get_creates = creates.clone();
    let post_creates = creates.clone();
    let app = Router::new()
        .route(
            "/api/v10/channels/200",
            get(move || {
                let exists = get_creates.load(Ordering::SeqCst) != 0;
                async move {
                    if exists {
                        Json(json!({"id":"200","type":11,"parent_id":"100"})).into_response()
                    } else {
                        (
                            StatusCode::NOT_FOUND,
                            Json(json!({"code":10003,"message":"Unknown Channel"})),
                        )
                            .into_response()
                    }
                }
            }),
        )
        .route(
            "/api/v10/channels/100/messages/200/threads",
            post(move || {
                post_creates.fetch_add(1, Ordering::SeqCst);
                // The native side effect happened, but the response was lost/malformed.
                async { (StatusCode::CREATED, "{broken") }
            }),
        );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let http = twilight_http::Client::builder()
        .token("test-only".into())
        .proxy(listener.local_addr()?.to_string(), true)
        .ratelimiter(None)
        .build();
    let mut servers = tokio::task::JoinSet::new();
    servers.spawn(async move { axum::serve(listener, app).await });
    let config = config();
    let plan = admit_message(
        &config,
        &participation,
        message("200", "100", None, "<@9> hello"),
        Value::Null,
    )?
    .context("root message not admitted")?;
    assert!(
        prepare_conversation(&participation, &http, plan.clone())
            .await
            .is_err()
    );
    let resolved = prepare_conversation(&participation, &http, plan.clone()).await?;
    assert_eq!(
        resolved.event.native["origin"],
        json!({
            "guild_id":"1", "channel_id":"200", "source_channel_id":"100", "message_id":"200", "user_id":"2", "parent_channel_id":"100"
        })
    );
    assert_eq!(resolved.event.delivery_target_id, "200");
    assert_eq!(resolved.event.audience_id, "discord-1-200");
    let reopened = Participation::load(path)?;
    prepare_conversation(&reopened, &http, plan).await?;
    assert_eq!(creates.load(Ordering::SeqCst), 1);
    let followup = admit_message(
        &config,
        &reopened,
        message("201", "200", Some("100"), "no mention needed"),
        Value::Null,
    )?
    .context("created thread lost activation")?;
    assert_eq!(
        prepare_conversation(&reopened, &http, followup)
            .await?
            .conversation_id,
        resolved.conversation_id
    );
    servers.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn native_preparation_adopts_existing_threads_durably() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("participation.json");
    let participation = Participation::load(path.clone())?;
    let config = config();
    let http = twilight_http::Client::new(String::new());
    let unmentioned = message("401", "400", Some("100"), "hello");
    assert!(admit_message(&config, &participation, unmentioned.clone(), Value::Null)?.is_none());
    let plan = admit_message(
        &config,
        &participation,
        message("402", "400", Some("100"), "<@9> hello"),
        Value::Null,
    )?
    .context("mentioned thread not admitted")?;
    let resolved = prepare_conversation(&participation, &http, plan).await?;
    assert_eq!(resolved.conversation_id, "discord-1-400");
    let reopened = Participation::load(path)?;
    let plan = admit_message(&config, &reopened, unmentioned, Value::Null)?
        .context("adoption was not persisted")?;
    assert_eq!(
        prepare_conversation(&reopened, &http, plan)
            .await?
            .conversation_id,
        resolved.conversation_id
    );
    Ok(())
}
