use super::{Feedback, OUTCOMES, Reactions, delivery_loop, discord_chunks};
use crate::{
    activity::Delivering,
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
    Delivery, DeliveryReconciliation, ExternalEvent, Origin, TurnState,
};
use serde_json::json;
use std::{
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

/// The answer of turn `turn-{id}`, a message turn of its own job, to channel 300.
fn answer(id: i64, turn_state: TurnState) -> Delivery {
    Delivery {
        turn_state,
        id,
        turn_id: format!("turn-{id}"),
        target_id: "300".to_owned(),
        feedback_target_id: "300:400".to_owned(),
        content: format!("answer {id}"),
        silence: None,
        origin: Origin {
            turn_id: format!("turn-{id}"),
            accepted_at: 1_790_000_000,
        },
    }
}

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
            any(
                move |method: Method,
                      Path((channel, message, emoji)): Path<(
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
    let core = core_client(format!("http://{address}"))?;
    let config = discord_config(DiscordActivation::Always, DiscordConversation::Channel);
    let expected_uncertain = config.uncertain_emoji.clone();
    let http = discord_client(address);
    let (outcomes, _) = tokio::sync::mpsc::unbounded_channel();
    let outcome = tokio::time::timeout(
        Duration::from_secs(5),
        delivery_loop(
            core,
            Reactions::new(http.clone(), crate::config::Settings::new(config)),
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
    let messages = Arc::new(tokio::sync::Mutex::new(Vec::new()));
    let acks = Arc::new(tokio::sync::Mutex::new(Vec::new()));
    let app = delivering_core(
        vec![Delivery {
            content: " \n ".to_owned(),
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
    let (outcomes, _observed) = tokio::sync::mpsc::unbounded_channel();
    tokio::time::timeout(
        Duration::from_secs(5),
        delivery_loop(
            core_client(format!("http://{address}"))?,
            Reactions::new(
                http.clone(),
                crate::config::Settings::new(discord_config(
                    DiscordActivation::Always,
                    DiscordConversation::Channel,
                )),
            ),
            http,
            outcomes,
            tokio_util::sync::CancellationToken::new(),
        ),
    )
    .await?
    .err()
    .context("delivery loop unexpectedly succeeded")?;
    server.abort();
    assert!(messages.lock().await.is_empty());
    let acks = acks.lock().await;
    assert_eq!(acks.len(), 1);
    assert_eq!(acks[0]["outcome"], "uncertain");
    assert_eq!(acks[0]["native_ref"], serde_json::Value::Null);
    Ok(())
}

#[test]
fn discord_messages_split_cleanly_at_the_platform_limit() {
    let (a, b, c) = ("a".repeat(1_000), "b".repeat(500), "c".repeat(1_000));
    assert_eq!(
        discord_chunks(&format!("{a}\n\n{b}\n{c}")),
        [a.clone(), format!("{b}\n{c}")],
        "a paragraph break is preferred to a line break"
    );
    assert_eq!(
        discord_chunks(&format!("{b} {b}\n{a}")),
        [format!("{b} {b}"), a],
        "a line break is preferred to a space"
    );
    assert_eq!(
        discord_chunks(&"🦀".repeat(2_001)),
        ["🦀".repeat(2_000), "🦀".to_owned()],
        "with nowhere to break, the limit counts characters, not bytes"
    );

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
    // A coarse guard against splitting in time quadratic in the answer, as
    // an earlier splitter did by counting the whole rest for every chunk:
    // eight times the answer takes about eight times as long, not 64.
    let split = |length: usize| {
        let answer = "a".repeat(length);
        let started = std::time::Instant::now();
        let chunks = discord_chunks(&answer);
        let elapsed = started.elapsed();
        assert_eq!(chunks.len(), length / 2_000);
        assert!(chunks.iter().all(|chunk| chunk.len() == 2_000));
        elapsed
    };
    let short = split(3_000_000).min(split(3_000_000));
    let long = split(24_000_000);
    assert!(
        long < short * 16,
        "eight times the answer took {:.1} times as long",
        long.as_secs_f64() / short.as_secs_f64()
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
                Json(created(900 + messages.len(), &body))
            }),
        )
        .fallback(move || async move {
            received.fetch_add(1, Ordering::SeqCst);
            tokio::time::sleep(feedback).await;
            StatusCode::NO_CONTENT
        })
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
                            content: "answer".to_owned(),
                            ..answer(1, TurnState::Completed)
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
    let http = discord_client(address);
    let core = core_client(format!("http://{address}"))?;
    let run = |stopping: &tokio_util::sync::CancellationToken| {
        let (outcomes, _) = tokio::sync::mpsc::unbounded_channel();
        tokio::spawn(delivery_loop(
            core.clone(),
            Reactions::new(
                http.clone(),
                crate::config::Settings::new(discord_config(
                    DiscordActivation::Always,
                    DiscordConversation::Channel,
                )),
            ),
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
    let long = format!("{}\n\n@here {}", "a".repeat(1_990), "b".repeat(30));
    let delivery = |id: i64, turn_state, content: &str| Delivery {
        content: content.to_owned(),
        ..answer(id, turn_state)
    };
    let messages = Arc::new(tokio::sync::Mutex::new(Vec::new()));
    let acks = Arc::new(tokio::sync::Mutex::new(Vec::new()));
    let app = delivering_core(
        vec![
            delivery(1, TurnState::Interrupted, notice),
            delivery(2, TurnState::Completed, &long),
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
            core_client(format!("http://{address}"))?,
            Reactions::new(
                http.clone(),
                crate::config::Settings::new(discord_config(
                    DiscordActivation::Always,
                    DiscordConversation::Channel,
                )),
            ),
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
    assert_eq!(contents[1..].join("\n\n"), long);
    assert_eq!(contents.len(), 3);
    assert!(
        contents
            .iter()
            .all(|content| content.chars().count() <= 2_000)
    );
    for message in messages.iter() {
        assert_eq!(message["allowed_mentions"], json!({"parse": []}));
        assert_eq!(message.get("embeds"), None, "answers are plain text");
    }
    let acks = acks.lock().await;
    assert_eq!(acks.len(), 2);
    assert!(acks.iter().all(|ack| ack["outcome"] == "delivered"));
    assert_eq!(acks[0]["native_ref"], "901");
    assert_eq!(acks[1]["native_ref"], "902,903");
    // The monitor hears of each answer's job as it is taken up, and of its
    // outcome once core recorded it.
    let heard = std::iter::from_fn(|| observed.try_recv().ok()).collect::<Vec<_>>();
    assert_eq!(
        heard,
        [
            Delivering::Claimed("turn-1".to_owned()),
            Delivering::Recorded,
            Delivering::Claimed("turn-2".to_owned()),
            Delivering::Recorded,
        ]
    );
    Ok(())
}

#[tokio::test]
async fn slow_reactions_never_hold_up_deliveries_and_stay_bounded() -> anyhow::Result<()> {
    let delivery = |id: i64| Delivery {
        feedback_target_id: format!("300:{}", 1_000 + id),
        content: format!("answer {id}"),
        ..answer(id, TurnState::Completed)
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
        crate::config::Settings::new(discord_config(
            DiscordActivation::Always,
            DiscordConversation::Channel,
        )),
    );
    let (outcomes, _observed) = tokio::sync::mpsc::unbounded_channel();
    let outcome = tokio::time::timeout(
        Duration::from_secs(5),
        delivery_loop(
            core_client(format!("http://{address}"))?,
            reactions.clone(),
            http,
            outcomes,
            tokio_util::sync::CancellationToken::new(),
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
    let reactions = Reactions::new(
        discord_client(address),
        crate::config::Settings::new(config),
    );
    reactions
        .admission(&event("400"), &reactions.settings().current())
        .await;
    reactions
        .admission(&event("400"), &reactions.settings().current())
        .await;
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

/// Discord's reaction endpoint, which applies the reactions of message 400
/// at once, recording them in `applied`, and holds every other one for 30
/// seconds.
fn reactions_on_400(applied: Arc<tokio::sync::Mutex<Vec<String>>>) -> Router {
    Router::new().route(
        "/api/v10/channels/{channel}/messages/{message}/reactions/{emoji}/@me",
        any(
            move |method: Method, Path((_, message, emoji)): Path<(String, String, String)>| {
                let applied = applied.clone();
                async move {
                    if message == "400" {
                        applied.lock().await.push(format!("{method} {emoji}"));
                    } else {
                        tokio::time::sleep(Duration::from_secs(30)).await;
                    }
                    StatusCode::NO_CONTENT
                }
            },
        ),
    )
}

/// Message 400's settings: the defaults, without typing, before any apply.
fn admitted_settings() -> crate::config::DiscordConfig {
    let mut config = discord_config(DiscordActivation::Always, DiscordConversation::Channel);
    config.typing = false;
    config
}

/// Settings whose reactions differ from [`admitted_settings`].
fn other_reactions() -> crate::config::DiscordConfig {
    let mut config = admitted_settings();
    "⏳".clone_into(&mut config.processing_emoji);
    "📬".clone_into(&mut config.delivered_emoji);
    config
}

#[tokio::test]
async fn an_outcome_replaces_the_processing_reaction_its_message_was_admitted_with()
-> anyhow::Result<()> {
    let applied = Arc::new(tokio::sync::Mutex::new(Vec::new()));
    let app = reactions_on_400(applied.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let address = listener.local_addr()?;
    let server = tokio::spawn(async move { axum::serve(listener, app).await });
    let settings = crate::config::Settings::new(admitted_settings());
    let reactions = Reactions::new(discord_client(address), settings.clone());
    reactions
        .admission(&event("400"), &settings.current())
        .await;
    // The operator serves other reactions while the message is answered.
    let changed = settings
        .apply(
            || Ok(other_reactions()),
            |_| std::future::ready(Ok(crate::module::tests::unchanged())),
        )
        .await
        .map_err(|refusal| anyhow::anyhow!("{refusal:?}"))?
        .changed;
    assert_eq!(changed, ["processing_emoji", "delivered_emoji"]);
    reactions.outcome("300:400", Feedback::Delivered).await;
    assert_eq!(reactions.stop(Duration::from_secs(10)).await, 0);
    server.abort();
    // The reaction removed is the one added, and the outcome is the one
    // the message was admitted with.
    assert_eq!(*applied.lock().await, ["PUT 👀", "DELETE 👀", "PUT ✅"]);
    Ok(())
}

#[tokio::test]
async fn a_skipped_outcome_leaves_a_replayed_message_to_the_settings_serving() -> anyhow::Result<()>
{
    let applied = Arc::new(tokio::sync::Mutex::new(Vec::new()));
    let app = reactions_on_400(applied.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let address = listener.local_addr()?;
    let server = tokio::spawn(async move { axum::serve(listener, app).await });
    let settings = crate::config::Settings::new(admitted_settings());
    let reactions = Reactions::new(discord_client(address), settings.clone());
    reactions
        .admission(&event("400"), &settings.current())
        .await;
    // Core refuses message 400 while every outcome slot is taken by
    // reactions Discord holds, so its outcome is not shown.
    for message in 1_000..1_000 + OUTCOMES {
        reactions.show_outcome(format!("300:{message}"), Feedback::Delivered);
    }
    reactions.show_outcome("300:400".to_owned(), Feedback::Failed);
    // The skipped outcome still frees the message's record of controls.
    assert!(super::lock(&reactions.0.controls).is_empty());
    // The operator then serves other reactions, and the gateway replays the
    // message, which is admitted again and answered.
    settings
        .apply(
            || Ok(other_reactions()),
            |_| std::future::ready(Ok(crate::module::tests::unchanged())),
        )
        .await
        .map_err(|refusal| anyhow::anyhow!("{refusal:?}"))?;
    reactions
        .admission(&event("400"), &settings.current())
        .await;
    reactions.outcome("300:400", Feedback::Delivered).await;
    assert_eq!(
        reactions.stop(Duration::from_millis(100)).await,
        OUTCOMES,
        "only the held outcomes were still under way"
    );
    server.abort();
    // The replay follows the settings serving when it was admitted, not
    // those of the admission whose outcome was skipped.
    assert_eq!(
        *applied.lock().await,
        ["PUT 👀", "PUT ⏳", "DELETE ⏳", "PUT 📬"]
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
    let reactions = Reactions::new(
        discord_client(address),
        crate::config::Settings::new(config),
    );
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
    reactions
        .admission(&event("404"), &reactions.settings().current())
        .await;
    assert!(admitting.elapsed() < Duration::from_millis(500));
    tokio::time::sleep(Duration::from_millis(300)).await;
    server.abort();
    // 402's outcome was shown; 401's and 406's were cut while removing the
    // controls an earlier process may have left, so their outcome reactions
    // never followed.
    let mut received = received.lock().await.clone();
    received.sort();
    assert_eq!(
        received,
        [
            "DELETE 401 ⏩".to_owned(),
            "DELETE 402 ⏩".to_owned(),
            "DELETE 402 ⏭️".to_owned(),
            format!("DELETE 402 {processing}"),
            "DELETE 402 🛑".to_owned(),
            "DELETE 406 ⏩".to_owned(),
            format!("PUT 402 {delivered}"),
        ]
    );
    Ok(())
}

mod silence;
