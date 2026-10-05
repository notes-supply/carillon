use super::{Feedback, OUTCOMES, Reactions, delivery_loop, discord_chunks, send_discord_message};
use crate::{
    client::test_client as core_client,
    config::{DiscordActivation, DiscordConversation},
    conversations::tests::discord_config,
};
use anyhow::Context as _;
use axum::{
    Json, Router,
    extract::Path,
    http::{Method, StatusCode, Uri},
    response::IntoResponse as _,
    routing::{any, get, post},
};
use carillon_protocol::model::{
    Delivery, DeliveryOutcome, DeliveryReconciliation, ExternalEvent, TurnState,
};
use serde_json::json;
use std::{
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

/// Message `message` in channel 300, as admission hands it to core.
fn event(message: &str) -> ExternalEvent {
    ExternalEvent {
        event_id: message.to_owned(),
        agent_id: "default".to_owned(),
        user_id: "200".to_owned(),
        audience_id: "discord-100-300".to_owned(),
        delivery_target_id: "300".to_owned(),
        feedback_target_id: format!("300:{message}"),
        content: "hello".to_owned(),
        native: serde_json::Value::Null,
        attachments: None,
    }
}

/// Discord's client, sending every request to the fake at `address`.
fn discord_client(address: std::net::SocketAddr) -> Arc<twilight_http::Client> {
    Arc::new(
        twilight_http::Client::builder()
            .proxy(address.to_string(), true)
            .ratelimiter(None)
            .build(),
    )
}

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
    let http = discord_client(address);
    let (outcomes, _) = tokio::sync::mpsc::unbounded_channel();
    let outcome = tokio::time::timeout(
        Duration::from_secs(5),
        delivery_loop(core, Reactions::new(http.clone(), config), http, outcomes),
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

    // Exactly at the limit is one message; a whitespace break is preferred
    // to a cut, and the whitespace around a split is dropped.
    assert_eq!(discord_chunks(&"é".repeat(2_000)).len(), 1);
    let spaced = format!("  {} {}  ", "x".repeat(1_500), "y".repeat(1_000));
    assert_eq!(
        discord_chunks(&spaced),
        ["x".repeat(1_500), "y".repeat(1_000)]
    );
}

#[test]
fn a_long_answer_is_split_in_one_pass() {
    // 12,000 chunks of an answer with nowhere to break. Counting the whole
    // remaining answer for every chunk scanned it 6,000 times over, which
    // took seconds; one bounded scan per chunk reads it about once.
    let answer = "a".repeat(24_000_000);
    let started = std::time::Instant::now();
    let chunks = discord_chunks(&answer);
    let elapsed = started.elapsed();
    assert_eq!(chunks.len(), 12_000);
    assert!(chunks.iter().all(|chunk| chunk.len() == 2_000));
    assert!(
        elapsed < Duration::from_secs(2),
        "splitting took {elapsed:?}"
    );
}

/// Core serving the given deliveries in order, then rejecting further claims
/// to end the delivery loop, beside a Discord API that records created
/// messages and takes `feedback` to answer anything else, such as reactions,
/// counting those in `received` as they arrive.
fn delivering_core(
    deliveries: Vec<Delivery>,
    messages: Arc<tokio::sync::Mutex<Vec<serde_json::Value>>>,
    acks: Arc<tokio::sync::Mutex<Vec<serde_json::Value>>>,
    feedback: Duration,
    received: Arc<AtomicUsize>,
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
                Json(json!({
                    "id": (900 + messages.len()).to_string(),
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
                }))
            }),
        )
        .fallback(move || async move {
            received.fetch_add(1, Ordering::SeqCst);
            tokio::time::sleep(feedback).await;
            StatusCode::NO_CONTENT
        })
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
        Duration::ZERO,
        Arc::default(),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let address = listener.local_addr()?;
    let server = tokio::spawn(async move { axum::serve(listener, app).await });
    let http = discord_client(address);
    let (outcomes, mut observed) = tokio::sync::mpsc::unbounded_channel();
    let outcome = tokio::time::timeout(
        Duration::from_secs(5),
        delivery_loop(
            core_client(format!("http://{address}")),
            Reactions::new(
                http.clone(),
                discord_config(DiscordActivation::Always, DiscordConversation::Channel),
            ),
            http,
            outcomes,
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

#[tokio::test]
async fn slow_reactions_never_hold_up_deliveries_and_stay_bounded() -> anyhow::Result<()> {
    let delivery = |id: i64| Delivery {
        turn_state: TurnState::Completed,
        id,
        turn_id: format!("turn-{id}"),
        target_id: "300".to_owned(),
        feedback_target_id: format!("300:{}", 1_000 + id),
        content: format!("answer {id}"),
    };
    let burst = OUTCOMES + 2;
    let messages = Arc::new(tokio::sync::Mutex::new(Vec::new()));
    let acks = Arc::new(tokio::sync::Mutex::new(Vec::new()));
    let received = Arc::new(AtomicUsize::new(0));
    // Each reaction takes Discord far longer than delivering an answer.
    let app = delivering_core(
        (1..).take(burst).map(delivery).collect(),
        messages.clone(),
        acks.clone(),
        Duration::from_secs(20),
        received.clone(),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let address = listener.local_addr()?;
    let server = tokio::spawn(async move { axum::serve(listener, app).await });
    let http = discord_client(address);
    let reactions = Reactions::new(
        http.clone(),
        discord_config(DiscordActivation::Always, DiscordConversation::Channel),
    );
    let (outcomes, _observed) = tokio::sync::mpsc::unbounded_channel();
    let outcome = tokio::time::timeout(
        Duration::from_secs(5),
        delivery_loop(
            core_client(format!("http://{address}")),
            reactions.clone(),
            http,
            outcomes,
        ),
    )
    .await;
    // Every answer was delivered and acknowledged while the reactions waited.
    assert!(
        outcome
            .context("deliveries waited for reactions")?
            .is_err_and(|failure| failure.to_string().contains("delivery claim rejected"))
    );
    assert_eq!(messages.lock().await.len(), burst);
    assert_eq!(acks.lock().await.len(), burst);
    // Only the first OUTCOMES outcomes are under way; the rest were not
    // shown rather than queued.
    tokio::time::timeout(Duration::from_secs(5), async {
        while received.load(Ordering::SeqCst) < OUTCOMES {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .context("outcome reactions were not sent")?;
    let started = std::time::Instant::now();
    assert_eq!(reactions.stop(Duration::from_millis(200)).await, OUTCOMES);
    assert!(started.elapsed() < Duration::from_secs(2));
    tokio::time::sleep(Duration::from_millis(200)).await;
    server.abort();
    assert_eq!(received.load(Ordering::SeqCst), OUTCOMES);
    Ok(())
}

/// Discord's reaction endpoint, which receives the first processing reaction
/// at once but applies it only `delay` later, in work of its own that goes on
/// whatever its client does, as an accepted request can; later reactions
/// apply at once. `applied` records the order in which reactions take effect.
pub(crate) fn late_processing(
    delay: Duration,
    applied: Arc<tokio::sync::Mutex<Vec<String>>>,
) -> Router {
    let processing =
        discord_config(DiscordActivation::Always, DiscordConversation::Channel).processing_emoji;
    let first = Arc::new(std::sync::atomic::AtomicBool::new(true));
    Router::new().route(
        "/api/v10/channels/{channel}/messages/{message}/reactions/{emoji}/@me",
        any(
            move |method: Method, Path((_, _, emoji)): Path<(String, String, String)>| {
                let effect = format!("{method} {emoji}");
                let late = method == Method::PUT
                    && emoji == processing
                    && first.swap(false, Ordering::SeqCst);
                let applied = applied.clone();
                async move {
                    let applying = tokio::spawn(async move {
                        if late {
                            tokio::time::sleep(delay).await;
                        }
                        applied.lock().await.push(effect);
                    });
                    let _ = applying.await;
                    StatusCode::NO_CONTENT
                }
            },
        ),
    )
}

#[tokio::test]
async fn a_message_admitted_again_shares_its_processing_request() -> anyhow::Result<()> {
    // Discord applies the processing reaction four seconds late, and the
    // message is admitted again meanwhile.
    let applied = Arc::new(tokio::sync::Mutex::new(Vec::new()));
    let app = late_processing(Duration::from_secs(4), applied.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let address = listener.local_addr()?;
    let server = tokio::spawn(async move { axum::serve(listener, app).await });
    let mut config = discord_config(DiscordActivation::Always, DiscordConversation::Channel);
    config.typing = false;
    let (processing, delivered) = (
        config.processing_emoji.clone(),
        config.delivered_emoji.clone(),
    );
    let reactions = Reactions::new(discord_client(address), config);
    reactions.admission(&event("400")).await;
    reactions.admission(&event("400")).await;
    reactions.show_outcome("300:400".to_owned(), Feedback::Delivered);
    assert_eq!(reactions.stop(Duration::from_secs(10)).await, 0);
    server.abort();
    // One processing reaction, which the outcome waited for.
    assert_eq!(
        *applied.lock().await,
        [
            format!("PUT {processing}"),
            format!("DELETE {processing}"),
            format!("PUT {delivered}"),
        ]
    );
    Ok(())
}

#[tokio::test]
async fn a_stopping_provider_ends_its_reactions_within_the_grace() -> anyhow::Result<()> {
    // Discord answers the reactions of messages 401 and 406 only after 30
    // seconds, and those of message 402 after a moment.
    let received = Arc::new(tokio::sync::Mutex::new(Vec::new()));
    let (trace, other) = (received.clone(), received.clone());
    let app = Router::new()
        .route(
            "/api/v10/channels/{channel}/messages/{message}/reactions/{emoji}/@me",
            any(
                move |method: Method, Path((_, message, emoji)): Path<(String, String, String)>| {
                    let trace = trace.clone();
                    async move {
                        trace
                            .lock()
                            .await
                            .push(format!("{method} {message} {emoji}"));
                        let answer = if matches!(message.as_str(), "401" | "406") {
                            30_000
                        } else {
                            100
                        };
                        tokio::time::sleep(Duration::from_millis(answer)).await;
                        StatusCode::NO_CONTENT
                    }
                },
            ),
        )
        // Anything else, such as typing.
        .fallback(move |method: Method, uri: Uri| {
            let other = other.clone();
            async move {
                other.lock().await.push(format!("{method} {}", uri.path()));
                StatusCode::NO_CONTENT
            }
        });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let address = listener.local_addr()?;
    let server = tokio::spawn(async move { axum::serve(listener, app).await });
    // Typing is on, so an admission after the stop could show it.
    let config = discord_config(DiscordActivation::Always, DiscordConversation::Channel);
    let (processing, delivered) = (
        config.processing_emoji.clone(),
        config.delivered_emoji.clone(),
    );
    let reactions = Reactions::new(discord_client(address), config);
    reactions.show_outcome("300:401".to_owned(), Feedback::Delivered);
    reactions.show_outcome("300:402".to_owned(), Feedback::Delivered);
    // Work of its own awaits message 406's outcome.
    let awaited = tokio::spawn({
        let reactions = reactions.clone();
        async move { reactions.outcome("300:406", Feedback::Delivered).await }
    });
    let started = std::time::Instant::now();
    // The provider stops: what ends within the grace ends, the rest is
    // cancelled rather than awaited for thirty seconds, the awaited outcome
    // included.
    assert_eq!(reactions.stop(Duration::from_secs(1)).await, 1);
    let stopping = started.elapsed();
    assert!(
        stopping >= Duration::from_secs(1) && stopping < Duration::from_secs(2),
        "stopping took {stopping:?}"
    );
    tokio::time::timeout(Duration::from_millis(500), awaited)
        .await
        .context("an awaited outcome outlived the stop")??;
    // Nothing starts after the stop: an outcome shown or awaited, or an
    // admission.
    reactions.show_outcome("300:403".to_owned(), Feedback::Delivered);
    reactions.outcome("300:405", Feedback::Failed).await;
    let admitting = std::time::Instant::now();
    reactions.admission(&event("404")).await;
    assert!(admitting.elapsed() < Duration::from_millis(500));
    tokio::time::sleep(Duration::from_millis(300)).await;
    server.abort();
    // 402's outcome was shown; 401's and 406's were cut while removing the
    // processing reaction, so their outcome reactions never followed.
    let mut received = received.lock().await.clone();
    received.sort();
    assert_eq!(
        received,
        [
            format!("DELETE 401 {processing}"),
            format!("DELETE 402 {processing}"),
            format!("DELETE 406 {processing}"),
            format!("PUT 402 {delivered}"),
        ]
    );
    Ok(())
}
