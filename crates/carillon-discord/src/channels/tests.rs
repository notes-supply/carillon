use std::{
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

use axum::{Router, routing::post};
use carillon_discord_module::contract::Notice;
use carillon_protocol::settings::{Change, Revision, RuntimePart, RuntimeSettings, Setting, Value};
use sha2::{Digest as _, Sha256};
use tokio::time::Instant;

use super::{change, page, settled};
use crate::{
    client::test_client,
    config::{DiscordActivation, DiscordConversation, DiscordRoute},
};

fn route(agent: &str) -> DiscordRoute {
    DiscordRoute {
        channel_id: "300".to_owned(),
        agent: agent.to_owned(),
        activation: DiscordActivation::Mention,
        conversation: DiscordConversation::Channel,
    }
}

/// Core at its address, answering every settings request after `delay`
/// with nothing, and counting the requests that carry a change.
async fn core(
    delay: Duration,
) -> anyhow::Result<(
    crate::client::CoreClient,
    Arc<AtomicUsize>,
    tokio::task::JoinHandle<std::io::Result<()>>,
)> {
    let changes = Arc::new(AtomicUsize::new(0));
    let counted = changes.clone();
    let router = Router::new().route(
        "/v1/settings",
        post(move |body: String| {
            let counted = counted.clone();
            async move {
                if body.contains("\"change\"") {
                    counted.fetch_add(1, Ordering::SeqCst);
                }
                tokio::time::sleep(delay).await;
                axum::Json(serde_json::json!({"settings": []}))
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let address = listener.local_addr()?;
    let server = tokio::spawn(async move { axum::serve(listener, router).await });
    Ok((test_client(format!("http://{address}"))?, changes, server))
}

fn until() -> Instant {
    Instant::now() + Duration::from_secs(5)
}

fn change_of(setting: &Setting, value: &str) -> Change {
    Change {
        setting: setting.id.clone(),
        value: Some(Value::One {
            id: value.to_owned(),
        }),
        expected: setting.revision.clone(),
    }
}

#[tokio::test]
async fn route_lines_are_named_and_revised_for_their_route() -> anyhow::Result<()> {
    let (core, _, server) = core(Duration::ZERO).await?;
    let drawn = page(&core, &route("assistant"), "<#300>".to_owned(), until()).await;
    let lines: Vec<_> = drawn
        .interface
        .iter()
        .map(|line| {
            (
                line.id.as_str(),
                line.name.as_str(),
                line.choices[0].name.as_str(),
            )
        })
        .collect();
    assert_eq!(
        lines,
        [
            ("agent", "Agent", "assistant"),
            ("listens", "Listens", "mentions"),
            ("replies", "Replies", "channel"),
        ]
    );
    assert_eq!(
        drawn.runtime,
        RuntimePart::Settings(RuntimeSettings::default())
    );
    let canonical = r#"["listens",["channel","discord","300","assistant"],"mention"]"#;
    assert_eq!(
        drawn.interface[1].revision,
        Revision::try_from(hex::encode(Sha256::digest(canonical))).map_err(anyhow::Error::msg)?
    );
    let rerouted = page(&core, &route("other"), "<#300>".to_owned(), until()).await;
    assert_ne!(
        rerouted.interface[1].revision, drawn.interface[1].revision,
        "a re-routed channel's lines are other lines"
    );
    server.abort();
    Ok(())
}

#[tokio::test]
async fn a_route_line_is_read_only_and_reaches_no_runtime() -> anyhow::Result<()> {
    let (core, changes, server) = core(Duration::ZERO).await?;
    let route = route("assistant");
    let drawn = page(&core, &route, "#300".to_owned(), until()).await;
    let refusal = change(
        &core,
        &route,
        "#300".to_owned(),
        change_of(&drawn.interface[0], "assistant"),
        until(),
    )
    .await
    .err()
    .ok_or_else(|| anyhow::anyhow!("a route line changed"))?;
    assert_eq!(refusal.status, axum::http::StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(refusal.message, "Agent is read-only.");
    let (shown, notice) = settled(
        &core,
        &route,
        "<#300>".to_owned(),
        change_of(&drawn.interface[2], "thread"),
        until(),
    )
    .await;
    assert_eq!(
        notice,
        Some(Notice::Refused("Replies is read-only.".to_owned()))
    );
    assert_eq!(shown.interface, drawn.interface);
    assert_eq!(changes.load(Ordering::SeqCst), 0);
    server.abort();
    Ok(())
}

/// A change still unanswered at the interaction's deadline is unconfirmed:
/// sent once, never again, and the page shows what could be read.
#[tokio::test]
async fn a_change_unanswered_by_the_deadline_is_unconfirmed_and_not_retried() -> anyhow::Result<()>
{
    let (core, changes, server) = core(Duration::from_secs(30)).await?;
    let route = route("assistant");
    let line = Setting {
        id: "model.thinking".to_owned(),
        name: "Thinking".to_owned(),
        choices: Vec::new(),
        value: None,
        inherited: true,
        revision: Revision::try_from("0".repeat(64)).map_err(anyhow::Error::msg)?,
    };
    let started = Instant::now();
    let (shown, notice) = settled(
        &core,
        &route,
        "<#300>".to_owned(),
        change_of(&line, "high"),
        started + Duration::from_millis(300),
    )
    .await;
    assert!(started.elapsed() < Duration::from_secs(2));
    assert_eq!(notice, Some(Notice::Unconfirmed));
    assert_eq!(changes.load(Ordering::SeqCst), 1);
    assert_eq!(
        shown.runtime,
        RuntimePart::Unavailable {
            reason: "The runtime did not answer in time.".to_owned()
        }
    );
    assert_eq!(shown.interface.len(), 3, "the route lines still draw");
    server.abort();
    Ok(())
}
