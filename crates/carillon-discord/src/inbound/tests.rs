use super::submit_event_with_retry;
use crate::{
    client::test_client as core_client,
    config::{DiscordActivation, DiscordConversation},
    conversations::{IncomingMessage, Participation, tests::discord_config},
};
use anyhow::{Context as _, anyhow};
use axum::{
    Json, Router,
    http::{Method, StatusCode, Uri},
    response::IntoResponse as _,
    routing::post,
};
use carillon_protocol::model::{EventReceipt, ExternalEvent, TurnState};
use serde_json::Value;
use std::{
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

/// Message preparation with staging under `directory`.
pub(crate) fn ingress(
    core: crate::CoreClient,
    config: crate::config::DiscordConfig,
    participation: Arc<Participation>,
    http: Arc<twilight_http::Client>,
    directory: &std::path::Path,
) -> anyhow::Result<Arc<super::Ingress>> {
    Ok(Arc::new(super::Ingress {
        downloads: Arc::new(crate::attachments::Downloads::open(
            directory.join("incoming"),
            config.attachments,
        )?),
        core,
        reactions: crate::delivery::Reactions::new(http.clone(), config.clone()),
        config,
        participation,
        http,
        stopping: tokio_util::sync::CancellationToken::new(),
    }))
}

#[tokio::test]
async fn discord_retries_ambiguous_submission_with_the_same_event_id() -> anyhow::Result<()> {
    let attempts = Arc::new(AtomicUsize::new(0));
    let event_ids = Arc::new(tokio::sync::Mutex::new(Vec::new()));
    let server_attempts = attempts.clone();
    let server_event_ids = event_ids.clone();
    let app = Router::new().route(
        "/v1/events",
        post(move |Json(event): Json<ExternalEvent>| {
            let attempts = server_attempts.clone();
            let event_ids = server_event_ids.clone();
            async move {
                event_ids.lock().await.push(event.event_id.clone());
                if attempts.fetch_add(1, Ordering::SeqCst) == 0 {
                    StatusCode::INTERNAL_SERVER_ERROR.into_response()
                } else {
                    Json(EventReceipt {
                        event_id: event.event_id,
                        turn_id: "turn".to_owned(),
                        state: TurnState::Queued,
                        duplicate: false,
                    })
                    .into_response()
                }
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let address = listener.local_addr()?;
    let server = tokio::spawn(async move { axum::serve(listener, app).await });

    let core = core_client(format!("http://{address}"));
    let event = ExternalEvent {
        event_id: "event".to_owned(),
        agent_id: "default".to_owned(),
        user_id: "user".to_owned(),
        audience_id: "audience".to_owned(),
        delivery_target_id: "target".to_owned(),
        feedback_target_id: "feedback".to_owned(),
        content: "hello".to_owned(),
        native: Value::Null,
        attachments: None,
    };

    let receipt = tokio::time::timeout(
        Duration::from_secs(2),
        submit_event_with_retry(&core, &event),
    )
    .await?
    .map_err(|failure| anyhow!(failure.message))?;
    server.abort();

    assert_eq!(receipt.event_id, event.event_id);
    assert_eq!(attempts.load(Ordering::SeqCst), 2);
    assert_eq!(*event_ids.lock().await, vec!["event", "event"]);
    Ok(())
}

#[tokio::test]
async fn interrupted_success_receipt_recovers_the_committed_turn() -> anyhow::Result<()> {
    use tokio::io::{AsyncBufReadExt as _, AsyncReadExt as _, AsyncWriteExt as _, BufReader};

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let core = core_client(format!("http://{}", listener.local_addr()?));
    let event = ExternalEvent {
        event_id: "lost-receipt".to_owned(),
        agent_id: "agent".to_owned(),
        user_id: "user".to_owned(),
        audience_id: "conversation".to_owned(),
        delivery_target_id: "target".to_owned(),
        feedback_target_id: "feedback".to_owned(),
        content: "hello".to_owned(),
        native: Value::Null,
        attachments: None,
    };
    let server = tokio::spawn(async move {
        for attempt in 0..2 {
            let (socket, _) = listener.accept().await?;
            let mut stream = BufReader::new(socket);
            let mut length = None;
            loop {
                let mut line = String::new();
                anyhow::ensure!(
                    stream.read_line(&mut line).await? > 0,
                    "missing request headers"
                );
                if line == "\r\n" {
                    break;
                }
                if let Some((name, value)) = line.split_once(':')
                    && name.eq_ignore_ascii_case("content-length")
                {
                    length = Some(value.trim().parse::<usize>()?);
                }
            }
            let mut body = vec![0; length.context("missing request length")?];
            stream.read_exact(&mut body).await?;
            let submitted: ExternalEvent = serde_json::from_slice(&body)?;
            assert_eq!(submitted.event_id, "lost-receipt");
            // Core commits the turn on the first attempt and recognises
            // the retry as the same event.
            let receipt = EventReceipt {
                event_id: submitted.event_id,
                turn_id: "committed".to_owned(),
                state: TurnState::Queued,
                duplicate: attempt == 1,
            };
            let body = serde_json::to_vec(&receipt)?;
            stream.write_all(format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len(),
                ).as_bytes()).await?;
            // Core committed. Deliver headers and an incomplete body, then close TCP.
            stream
                .write_all(if attempt == 0 { &body[..1] } else { &body })
                .await?;
            stream.shutdown().await?;
        }
        Ok::<_, anyhow::Error>(())
    });
    let outcome = tokio::time::timeout(
        Duration::from_secs(3),
        submit_event_with_retry(&core, &event),
    )
    .await;
    if !matches!(&outcome, Ok(Ok(_))) {
        server.abort();
    }
    let receipt = outcome?.map_err(|failure| anyhow!(failure.message))?;
    tokio::time::timeout(Duration::from_secs(3), server).await???;
    assert!(receipt.duplicate);
    assert_eq!(receipt.turn_id, "committed");
    assert_eq!(receipt.state, TurnState::Queued);
    Ok(())
}

// Keep the rejection, feedback and next admission trace together.
#[tokio::test]
#[expect(
    clippy::too_many_lines,
    reason = "one observable rejection/feedback/admission trace"
)]
async fn configuration_rejection_does_not_block_the_next_message_in_its_conversation()
-> anyhow::Result<()> {
    let (trace, mut observed) = tokio::sync::mpsc::unbounded_channel();
    let core_trace = trace.clone();
    let app = Router::new()
        .route(
            "/v1/events",
            post(move |Json(event): Json<ExternalEvent>| {
                let trace = core_trace.clone();
                async move {
                    let _ = trace.send(format!("submit {}", event.event_id));
                    if event.event_id == "400" {
                        (
                            StatusCode::CONFLICT,
                            "conversation runtime provider is disabled",
                        )
                            .into_response()
                    } else {
                        Json(EventReceipt {
                            event_id: event.event_id,
                            turn_id: "accepted".to_owned(),
                            duplicate: false,
                            state: TurnState::Queued,
                        })
                        .into_response()
                    }
                }
            }),
        )
        .fallback(move |method: Method, uri: Uri| {
            let trace = trace.clone();
            async move {
                let _ = trace.send(format!("{method} {uri}"));
                StatusCode::NO_CONTENT
            }
        });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let address = listener.local_addr()?;
    let http = Arc::new(
        twilight_http::Client::builder()
            .token("test-only".to_owned())
            .proxy(address.to_string(), true)
            .ratelimiter(None)
            .build(),
    );
    let directory = tempfile::tempdir()?;
    let core = core_client(format!("http://{address}"));
    let participation = Arc::new(Participation::load(
        directory.path().join("participation.json"),
    )?);
    let mut config = discord_config(DiscordActivation::Always, DiscordConversation::Channel);
    config.typing = false;
    config.failed_emoji = "❌".to_owned();
    let mut services = tokio::task::JoinSet::new();
    services.spawn(async move { axum::serve(listener, app).await });
    let (send, receive) = tokio::sync::mpsc::channel(2);
    // One conversation: the second message waits for the first to settle.
    for (event, channel) in [("400", "300"), ("401", "300")] {
        send.send(super::QueuedMessage {
            request: IncomingMessage {
                event_id: event.to_owned(),
                guild_id: Some("100".to_owned()),
                user_id: "200".to_owned(),
                channel_id: channel.to_owned(),
                parent_channel_id: None,
                content: "hello".to_owned(),
                attachments: Vec::new(),
            },
            native: Value::Null,
        })
        .await?;
    }
    drop(send);
    let ingress = ingress(core, config, participation, http, directory.path())?;
    tokio::time::timeout(
        Duration::from_secs(3),
        super::lanes::dispatch(ingress, receive),
    )
    .await?;
    let mut requests = Vec::new();
    while let Ok(request) = observed.try_recv() {
        requests.push(request);
    }
    assert_eq!(
        requests
            .iter()
            .filter(|request| request.starts_with("submit "))
            .map(String::as_str)
            .collect::<Vec<_>>(),
        ["submit 400", "submit 401"]
    );
    // Observe actual failure feedback before the next message's admission.
    let failure = requests
        .iter()
        .position(|request| {
            request.starts_with("PUT /api/v10/channels/300/messages/400/reactions/")
                && request.contains("%E2%9D%8C")
        })
        .context("rejected message had no failure feedback")?;
    let next = requests
        .iter()
        .position(|request| request == "submit 401")
        .context("next message not submitted")?;
    assert!(failure < next);
    Ok(())
}

#[tokio::test]
async fn complete_invalid_receipt_is_not_retried_forever() -> anyhow::Result<()> {
    let attempts = Arc::new(AtomicUsize::new(0));
    let server_attempts = attempts.clone();
    let app = Router::new().route(
        "/v1/events",
        post(move || {
            server_attempts.fetch_add(1, Ordering::SeqCst);
            async { (StatusCode::OK, "{invalid json}") }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let core = core_client(format!("http://{}", listener.local_addr()?));
    let server = tokio::spawn(async move { axum::serve(listener, app).await });
    let event = ExternalEvent {
        event_id: "invalid-receipt".to_owned(),
        agent_id: "agent".to_owned(),
        user_id: "user".to_owned(),
        audience_id: "conversation".to_owned(),
        delivery_target_id: "target".to_owned(),
        feedback_target_id: "feedback".to_owned(),
        content: "hello".to_owned(),
        native: Value::Null,
        attachments: None,
    };
    let outcome = tokio::time::timeout(
        Duration::from_secs(2),
        submit_event_with_retry(&core, &event),
    )
    .await;
    server.abort();
    let failure = outcome?.err().context("invalid receipt was accepted")?;
    assert!(!failure.is_transient());
    assert_eq!(attempts.load(Ordering::SeqCst), 1);
    Ok(())
}

mod files;

#[tokio::test]
async fn slow_feedback_never_delays_a_messages_admission() -> anyhow::Result<()> {
    let (trace, mut observed) = tokio::sync::mpsc::unbounded_channel();
    let core_trace = trace.clone();
    let app = Router::new()
        .route(
            "/v1/events",
            post(move |Json(event): Json<ExternalEvent>| {
                let trace = core_trace.clone();
                async move {
                    let _ = trace.send(format!("submit {}", event.event_id));
                    Json(EventReceipt {
                        event_id: event.event_id,
                        turn_id: "accepted".to_owned(),
                        duplicate: false,
                        state: TurnState::Queued,
                    })
                }
            }),
        )
        // Reactions and typing take Discord far longer than admission.
        .fallback(move |method: Method, uri: Uri| {
            let trace = trace.clone();
            async move {
                let _ = trace.send(format!("{method} {uri}"));
                tokio::time::sleep(Duration::from_secs(30)).await;
                StatusCode::NO_CONTENT
            }
        });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let address = listener.local_addr()?;
    let http = Arc::new(
        twilight_http::Client::builder()
            .token("test-only".to_owned())
            .proxy(address.to_string(), true)
            .ratelimiter(None)
            .build(),
    );
    let directory = tempfile::tempdir()?;
    let participation = Arc::new(Participation::load(
        directory.path().join("participation.json"),
    )?);
    let mut config = discord_config(DiscordActivation::Always, DiscordConversation::Channel);
    config.typing = true;
    let mut services = tokio::task::JoinSet::new();
    services.spawn(async move { axum::serve(listener, app).await });
    let (send, receive) = tokio::sync::mpsc::channel(2);
    send.send(super::QueuedMessage {
        request: IncomingMessage {
            event_id: "400".to_owned(),
            guild_id: Some("100".to_owned()),
            user_id: "200".to_owned(),
            channel_id: "300".to_owned(),
            parent_channel_id: None,
            content: "hello".to_owned(),
            attachments: Vec::new(),
        },
        native: Value::Null,
    })
    .await?;
    drop(send);
    let ingress = ingress(
        core_client(format!("http://{address}")),
        config,
        participation,
        http,
        directory.path(),
    )?;
    let started = std::time::Instant::now();
    tokio::time::timeout(
        crate::delivery::PROMPT_FEEDBACK + Duration::from_secs(2),
        super::lanes::dispatch(ingress, receive),
    )
    .await
    .context("admission waited for feedback")?;
    assert!(started.elapsed() >= crate::delivery::PROMPT_FEEDBACK);
    let requests = std::iter::from_fn(|| observed.try_recv().ok()).collect::<Vec<_>>();
    // The reaction and typing were asked for first, then the message admitted.
    assert_eq!(requests.last().map(String::as_str), Some("submit 400"));
    assert!(
        requests
            .iter()
            .any(|request| request.contains("/messages/400/reactions/"))
    );
    assert!(
        requests
            .iter()
            .any(|request| request.ends_with("/channels/300/typing"))
    );
    Ok(())
}

#[tokio::test]
async fn a_duplicate_never_lets_the_processing_reaction_land_after_the_outcome()
-> anyhow::Result<()> {
    // Discord applies the first processing reaction four seconds after
    // receiving it, after admission stopped waiting, and any later one at
    // once; core reports the message's second arrival as a duplicate.
    let applied = Arc::new(tokio::sync::Mutex::new(Vec::new()));
    let submissions = Arc::new(AtomicUsize::new(0));
    let submitted = submissions.clone();
    let app = Router::new()
        .route(
            "/v1/events",
            post(move |Json(event): Json<ExternalEvent>| {
                let duplicate = submitted.fetch_add(1, Ordering::SeqCst) > 0;
                async move {
                    Json(EventReceipt {
                        event_id: event.event_id,
                        turn_id: "accepted".to_owned(),
                        duplicate,
                        state: TurnState::Queued,
                    })
                }
            }),
        )
        .merge(crate::delivery::tests::late_processing(
            Duration::from_secs(4),
            applied.clone(),
        ));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let address = listener.local_addr()?;
    let server = tokio::spawn(async move { axum::serve(listener, app).await });
    let http = Arc::new(
        twilight_http::Client::builder()
            .token("test-only".to_owned())
            .proxy(address.to_string(), true)
            .ratelimiter(None)
            .build(),
    );
    let directory = tempfile::tempdir()?;
    let participation = Arc::new(Participation::load(
        directory.path().join("participation.json"),
    )?);
    let mut config = discord_config(DiscordActivation::Always, DiscordConversation::Channel);
    config.typing = false;
    let (processing, delivered) = (
        config.processing_emoji.clone(),
        config.delivered_emoji.clone(),
    );
    // The same message arrives twice, as a replayed gateway event does.
    let (send, receive) = tokio::sync::mpsc::channel(2);
    for _ in 0..2 {
        send.send(super::QueuedMessage {
            request: IncomingMessage {
                event_id: "400".to_owned(),
                guild_id: Some("100".to_owned()),
                user_id: "200".to_owned(),
                channel_id: "300".to_owned(),
                parent_channel_id: None,
                content: "hello".to_owned(),
                attachments: Vec::new(),
            },
            native: Value::Null,
        })
        .await?;
    }
    drop(send);
    let ingress = ingress(
        core_client(format!("http://{address}")),
        config,
        participation,
        http,
        directory.path(),
    )?;
    tokio::time::timeout(
        Duration::from_secs(10),
        super::lanes::dispatch(ingress.clone(), receive),
    )
    .await?;
    // The answer is delivered and its outcome shown, as the delivery loop does.
    ingress
        .reactions
        .show_outcome("300:400".to_owned(), crate::delivery::Feedback::Delivered);
    assert_eq!(ingress.reactions.stop(Duration::from_secs(10)).await, 0);
    server.abort();
    assert_eq!(submissions.load(Ordering::SeqCst), 2);
    // One processing reaction, removed by the duplicate and again by the
    // outcome, before the delivered one.
    assert_eq!(
        *applied.lock().await,
        [
            format!("PUT {processing}"),
            format!("DELETE {processing}"),
            format!("DELETE {processing}"),
            format!("PUT {delivered}"),
        ]
    );
    Ok(())
}
