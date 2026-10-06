use super::{delivery_loop, discord_chunks, send_discord_message};
use crate::{
    client::test_client as core_client,
    config::{DiscordActivation, DiscordConversation},
    conversations::tests::discord_config,
};
use anyhow::Context as _;
use axum::{
    Json, Router,
    http::{Method, StatusCode, Uri},
    response::IntoResponse as _,
    routing::{get, post},
};
use carillon_protocol::model::{Delivery, DeliveryOutcome, DeliveryReconciliation, TurnState};
use serde_json::json;
use std::{
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

fn reconciling_core(trace: Arc<tokio::sync::Mutex<Vec<String>>>) -> Router {
    let polls = Arc::new(AtomicUsize::new(0));
    let reconcile_trace = trace.clone();
    let poll_trace = trace.clone();
    let discord_trace = trace.clone();
    Router::new()
        .route(
            "/v1/deliveries/reconcile",
            post(move || {
                let trace = reconcile_trace.clone();
                async move {
                    trace.lock().await.push("reconcile".to_owned());
                    Json(DeliveryReconciliation {
                        feedback_target_ids: vec!["300:400".to_owned()],
                    })
                }
            }),
        )
        .route(
            "/v1/deliveries",
            get(move || {
                let trace = poll_trace.clone();
                let attempt = polls.fetch_add(1, Ordering::SeqCst);
                async move {
                    trace.lock().await.push("poll".to_owned());
                    if attempt == 0 {
                        // The claim may have committed before core failed.
                        StatusCode::INTERNAL_SERVER_ERROR
                    } else {
                        // Terminate the infinite production loop after observing resumed polling.
                        StatusCode::FORBIDDEN
                    }
                }
            }),
        )
        .route(
            "/api/v10/channels/{channel}/messages/{message}/reactions/{emoji}/@me",
            axum::routing::any(
                move |method: Method,
                      axum::extract::Path((channel, message, emoji)): axum::extract::Path<(
                    String,
                    String,
                    String,
                )>| {
                    let trace = discord_trace.clone();
                    async move {
                        trace
                            .lock()
                            .await
                            .push(format!("{method} {channel}:{message} {emoji}"));
                        StatusCode::NO_CONTENT
                    }
                },
            ),
        )
        .fallback(move |method: Method, uri: Uri| {
            let trace = trace.clone();
            async move {
                trace
                    .lock()
                    .await
                    .push(format!("unexpected {method} {uri}"));
                StatusCode::NOT_FOUND
            }
        })
}

#[tokio::test]
async fn ambiguous_claim_failures_reconcile_before_polling_again() -> anyhow::Result<()> {
    let trace = Arc::new(tokio::sync::Mutex::new(Vec::<String>::new()));
    let app = reconciling_core(trace.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let address = listener.local_addr()?;
    let server = tokio::spawn(async move { axum::serve(listener, app).await });
    let core = core_client(format!("http://{address}"));
    let config = discord_config(DiscordActivation::Always, DiscordConversation::Channel);
    let expected_uncertain = config.uncertain_emoji.clone();
    let http = Arc::new(
        twilight_http::Client::builder()
            .proxy(address.to_string(), true)
            .ratelimiter(None)
            .build(),
    );
    let (outcomes, _) = tokio::sync::mpsc::unbounded_channel();
    let outcome = tokio::time::timeout(
        Duration::from_secs(5),
        delivery_loop(
            core,
            config,
            http,
            outcomes,
            tokio_util::sync::CancellationToken::new(),
        ),
    )
    .await;
    server.abort();
    assert!(
        outcome?
            .err()
            .context("delivery loop unexpectedly succeeded")?
            .to_string()
            .contains("delivery claim rejected")
    );
    let requests = trace.lock().await;
    // Reconciliation and uncertainty feedback precede resumed polling.
    // Processing-reaction cleanup is not part of this recovery contract.
    let required = [
        "poll".to_owned(),
        "reconcile".to_owned(),
        format!("PUT 300:400 {expected_uncertain}"),
        "poll".to_owned(),
    ];
    let mut remaining = requests.iter();
    for event in &required {
        assert!(
            remaining.any(|request| request == event),
            "missing {event} in order: {requests:?}"
        );
    }
    assert_eq!(
        requests.iter().filter(|request| *request == "poll").count(),
        2
    );
    assert!(
        requests
            .iter()
            .all(|request| !request.starts_with("unexpected"))
    );
    Ok(())
}

#[tokio::test]
async fn empty_discord_delivery_is_not_successful() -> anyhow::Result<()> {
    let failure = send_discord_message(
        &twilight_http::Client::new("unused".to_owned()),
        twilight_model::id::Id::new(1),
        " \n ",
    )
    .await
    .err()
    .context("empty content was accepted for delivery")?;
    assert_eq!(failure.to_string(), "Discord delivery content is empty");
    Ok(())
}

#[test]
fn discord_messages_split_cleanly_at_the_platform_limit() {
    let content = format!("{}\n\n{}", "a".repeat(1_990), "b".repeat(30));
    let chunks = discord_chunks(&content);
    assert_eq!(chunks.len(), 2);
    assert_eq!(chunks.join("\n\n"), content);
    assert!(chunks.iter().all(|chunk| chunk.chars().count() <= 2_000));

    let content = "🦀".repeat(2_001);
    let unbroken = discord_chunks(&content);
    assert!(unbroken.len() > 1);
    assert!(
        unbroken
            .iter()
            .all(|chunk| !chunk.is_empty() && chunk.chars().count() <= 2_000)
    );
    assert_eq!(unbroken.concat(), content);
}

/// Core serving the given deliveries in order, then rejecting further claims
/// to end the delivery loop, beside a Discord API that records created messages.
fn delivering_core(
    deliveries: Vec<Delivery>,
    messages: Arc<tokio::sync::Mutex<Vec<serde_json::Value>>>,
    acks: Arc<tokio::sync::Mutex<Vec<serde_json::Value>>>,
) -> Router {
    let pending = Arc::new(std::sync::Mutex::new(deliveries.into_iter()));
    Router::new()
        .route(
            "/v1/deliveries",
            get(move || {
                let next = pending.lock().ok().and_then(|mut pending| pending.next());
                async move {
                    match next {
                        Some(delivery) => Json(Some(delivery)).into_response(),
                        None => StatusCode::FORBIDDEN.into_response(),
                    }
                }
            }),
        )
        .route(
            "/v1/deliveries/{id}/ack",
            post(move |Json(ack): Json<serde_json::Value>| async move {
                acks.lock().await.push(ack);
                StatusCode::NO_CONTENT
            }),
        )
        .route(
            "/api/v10/channels/{channel}/messages",
            post(move |Json(body): Json<serde_json::Value>| async move {
                let mut messages = messages.lock().await;
                messages.push(body.clone());
                Json(created(900 + messages.len(), &body))
            }),
        )
        .fallback(|| async { StatusCode::NO_CONTENT })
}

/// The message Discord reports creating, as message `id`, from `body`.
fn created(id: usize, body: &serde_json::Value) -> serde_json::Value {
    json!({
        "id": id.to_string(),
        "channel_id": "300",
        "author": {"id": "201", "username": "carillon", "discriminator": "0"},
        "content": body["content"],
        "attachments": [],
        "embeds": [],
        "mention_everyone": false,
        "mention_roles": [],
        "mentions": [],
        "pinned": false,
        "timestamp": "2026-09-24T00:00:00.000000+00:00",
        "tts": false,
        "type": 0,
    })
}

#[tokio::test]
async fn stopping_finishes_a_claimed_delivery_and_claims_no_more() -> anyhow::Result<()> {
    // Core offers one delivery, then none; Discord holds the message it is
    // sent until released.
    let claims = Arc::new(AtomicUsize::new(0));
    let acks = Arc::new(tokio::sync::Mutex::new(Vec::new()));
    let sending = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Notify::new());
    let app = Router::new()
        .route(
            "/v1/deliveries",
            get({
                let claims = claims.clone();
                move || {
                    let first = claims.fetch_add(1, Ordering::SeqCst) == 0;
                    async move {
                        Json(first.then(|| Delivery {
                            turn_state: TurnState::Completed,
                            id: 1,
                            turn_id: "turn-1".to_owned(),
                            target_id: "300".to_owned(),
                            feedback_target_id: "300:400".to_owned(),
                            content: "answer".to_owned(),
                        }))
                    }
                }
            }),
        )
        .route(
            "/v1/deliveries/{id}/ack",
            post({
                let acks = acks.clone();
                move |Json(ack): Json<serde_json::Value>| async move {
                    acks.lock().await.push(ack);
                    StatusCode::NO_CONTENT
                }
            }),
        )
        .route(
            "/api/v10/channels/{channel}/messages",
            post({
                let (sending, release) = (sending.clone(), release.clone());
                move |Json(body): Json<serde_json::Value>| async move {
                    sending.notify_one();
                    release.notified().await;
                    Json(created(901, &body))
                }
            }),
        )
        .fallback(|| async { StatusCode::NO_CONTENT });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let address = listener.local_addr()?;
    let server = tokio::spawn(async move { axum::serve(listener, app).await });
    let http = Arc::new(
        twilight_http::Client::builder()
            .proxy(address.to_string(), true)
            .ratelimiter(None)
            .build(),
    );
    let run = |stopping: &tokio_util::sync::CancellationToken| {
        let (outcomes, _) = tokio::sync::mpsc::unbounded_channel();
        tokio::spawn(delivery_loop(
            core_client(format!("http://{address}")),
            discord_config(DiscordActivation::Always, DiscordConversation::Channel),
            http.clone(),
            outcomes,
            stopping.clone(),
        ))
    };
    // The provider stops while Discord is still answering the send.
    let stopping = tokio_util::sync::CancellationToken::new();
    let delivering = run(&stopping);
    tokio::time::timeout(Duration::from_secs(5), sending.notified()).await?;
    stopping.cancel();
    release.notify_one();
    tokio::time::timeout(Duration::from_secs(5), delivering).await???;
    {
        let acks = acks.lock().await;
        assert_eq!(acks.len(), 1);
        assert_eq!(acks[0]["outcome"], "delivered");
        assert_eq!(acks[0]["native_ref"], "901");
    }
    assert_eq!(claims.load(Ordering::SeqCst), 1);
    // An idle worker ends as soon as the provider stops, without waiting
    // out its pause between claims.
    let stopping = tokio_util::sync::CancellationToken::new();
    let idle = run(&stopping);
    tokio::time::timeout(Duration::from_secs(5), async {
        while claims.load(Ordering::SeqCst) < 2 {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await?;
    stopping.cancel();
    tokio::time::timeout(Duration::from_millis(400), idle).await???;
    server.abort();
    Ok(())
}

#[tokio::test]
async fn deliveries_reach_discord_verbatim_without_notifying_anyone() -> anyhow::Result<()> {
    // Mention syntax a provider explanation or model output might contain.
    let notice = "That turn was interrupted: Codex error: @everyone <@200> <@&7> The usage limit has been reached\nSend another message to continue in this conversation. Actions already taken may have had effects.";
    let answer = format!("{}\n\n@here {}", "a".repeat(1_990), "b".repeat(30));
    let delivery = |id: i64, turn_state, content: &str| Delivery {
        turn_state,
        id,
        turn_id: format!("turn-{id}"),
        target_id: "300".to_owned(),
        feedback_target_id: "300:400".to_owned(),
        content: content.to_owned(),
    };
    let messages = Arc::new(tokio::sync::Mutex::new(Vec::new()));
    let acks = Arc::new(tokio::sync::Mutex::new(Vec::new()));
    let app = delivering_core(
        vec![
            delivery(1, TurnState::Interrupted, notice),
            delivery(2, TurnState::Completed, &answer),
        ],
        messages.clone(),
        acks.clone(),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let address = listener.local_addr()?;
    let server = tokio::spawn(async move { axum::serve(listener, app).await });
    let http = Arc::new(
        twilight_http::Client::builder()
            .proxy(address.to_string(), true)
            .ratelimiter(None)
            .build(),
    );
    let (outcomes, mut observed) = tokio::sync::mpsc::unbounded_channel();
    let outcome = tokio::time::timeout(
        Duration::from_secs(5),
        delivery_loop(
            core_client(format!("http://{address}")),
            discord_config(DiscordActivation::Always, DiscordConversation::Channel),
            http,
            outcomes,
            tokio_util::sync::CancellationToken::new(),
        ),
    )
    .await;
    server.abort();
    assert!(
        outcome?
            .err()
            .context("delivery loop unexpectedly succeeded")?
            .to_string()
            .contains("delivery claim rejected")
    );
    let messages = messages.lock().await;
    let contents = messages
        .iter()
        .map(|message| message["content"].as_str().unwrap_or_default())
        .collect::<Vec<_>>();
    // The notice is one message, exactly as core composed it; the long answer
    // is split at the platform limit.
    assert_eq!(contents[0], notice);
    assert_eq!(contents[1..].join("\n\n"), answer);
    assert_eq!(contents.len(), 3);
    assert!(
        contents
            .iter()
            .all(|content| content.chars().count() <= 2_000)
    );
    for message in messages.iter() {
        assert_eq!(message["allowed_mentions"], json!({"parse": []}));
    }
    let acks = acks.lock().await;
    assert_eq!(acks.len(), 2);
    assert!(acks.iter().all(|ack| ack["outcome"] == "delivered"));
    assert_eq!(acks[0]["native_ref"], "901");
    // Each outcome is reported after its acknowledgement, for activity cards.
    let reported = std::iter::from_fn(|| observed.try_recv().ok())
        .map(|outcome| (outcome.turn_id, outcome.turn_state, outcome.delivery))
        .collect::<Vec<_>>();
    assert!(matches!(
        reported.as_slice(),
        [
            (first, TurnState::Interrupted, DeliveryOutcome::Delivered),
            (second, TurnState::Completed, DeliveryOutcome::Delivered),
        ] if first == "turn-1" && second == "turn-2"
    ));
    Ok(())
}
