use super::submit_event_with_retry;
use crate::{
    client::test_client as core_client,
    config::{DiscordActivation, DiscordConversation},
    conversations::{IncomingMessage, Participation, tests::discord_config},
};
use anyhow::{Context as _, anyhow};
use axum::{
    Json, Router,
    extract::Path,
    http::{Method, StatusCode, Uri},
    response::IntoResponse as _,
    routing::{any, get, post},
};
use carillon_protocol::model::{EventReceipt, EventStatus, ExternalEvent, TurnState};
use serde_json::Value;
use std::{
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};
use tokio::sync::Notify;

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

/// Core's answer to whether it holds an event: accepted once `accepted`
/// counts a submission, otherwise unknown.
fn event_status(accepted: Arc<AtomicUsize>) -> Router {
    Router::new().route(
        "/v1/events/{event_id}",
        get(move |Path(event_id): Path<String>| {
            let held = accepted.load(Ordering::SeqCst) > 0;
            async move {
                Json(if held {
                    EventStatus::Accepted(EventReceipt {
                        event_id,
                        turn_id: "accepted".to_owned(),
                        duplicate: false,
                        state: TurnState::Queued,
                    })
                } else {
                    EventStatus::Unknown
                })
            }
        }),
    )
}

/// Discord's client, sending every request to the fake at `address`.
fn discord_client(address: std::net::SocketAddr) -> Arc<twilight_http::Client> {
    Arc::new(
        twilight_http::Client::builder()
            .token("test-only".to_owned())
            .proxy(address.to_string(), true)
            .ratelimiter(None)
            .build(),
    )
}

/// Message 400 in channel 300, from the allowlisted person.
pub(crate) fn hello() -> super::QueuedMessage {
    super::QueuedMessage {
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
    }
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

#[tokio::test]
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
        .merge(event_status(Arc::default()))
        .fallback(move |method: Method, uri: Uri| {
            let trace = trace.clone();
            async move {
                let _ = trace.send(format!("{method} {uri}"));
                StatusCode::NO_CONTENT
            }
        });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let address = listener.local_addr()?;
    let http = discord_client(address);
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
        send.send(
            super::QueuedMessage {
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
            }
            .into(),
        )
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
        .merge(event_status(Arc::default()))
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
    let http = discord_client(address);
    let directory = tempfile::tempdir()?;
    let participation = Arc::new(Participation::load(
        directory.path().join("participation.json"),
    )?);
    let mut config = discord_config(DiscordActivation::Always, DiscordConversation::Channel);
    config.typing = true;
    let mut services = tokio::task::JoinSet::new();
    services.spawn(async move { axum::serve(listener, app).await });
    let (send, receive) = tokio::sync::mpsc::channel(2);
    send.send(hello().into()).await?;
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
    // The gateway replays a message core accepted while its outcome is
    // removing the processing reaction. Discord holds that removal until a
    // second processing reaction arrives or the test lets it go, and
    // applies a second processing reaction only four seconds late.
    let applied = Arc::new(tokio::sync::Mutex::new(Vec::new()));
    let accepted = Arc::new(AtomicUsize::new(0));
    let (removing, release) = (Arc::new(Notify::new()), Arc::new(Notify::new()));
    let mut config = discord_config(DiscordActivation::Always, DiscordConversation::Channel);
    config.typing = false;
    let (processing, delivered) = (
        config.processing_emoji.clone(),
        config.delivered_emoji.clone(),
    );
    let submitted = accepted.clone();
    let app = {
        let (applied, removing, release) = (applied.clone(), removing.clone(), release.clone());
        let (puts, deletes) = (Arc::new(AtomicUsize::new(0)), Arc::new(AtomicUsize::new(0)));
        let processing = processing.clone();
        Router::new()
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
            .merge(event_status(accepted.clone()))
            .route(
                "/api/v10/channels/{channel}/messages/{message}/reactions/{emoji}/@me",
                any(
                    move |method: Method, Path((_, _, emoji)): Path<(String, String, String)>| {
                        let effect = format!("{method} {emoji}");
                        let again = method == Method::PUT
                            && emoji == processing
                            && puts.fetch_add(1, Ordering::SeqCst) > 0;
                        let first_removal =
                            method == Method::DELETE && deletes.fetch_add(1, Ordering::SeqCst) == 0;
                        let (applied, removing, release) =
                            (applied.clone(), removing.clone(), release.clone());
                        async move {
                            if first_removal {
                                removing.notify_one();
                                release.notified().await;
                            }
                            if again {
                                release.notify_one();
                            }
                            let applying = tokio::spawn(async move {
                                if again {
                                    tokio::time::sleep(Duration::from_secs(4)).await;
                                }
                                applied.lock().await.push(effect);
                            });
                            let _ = applying.await;
                            StatusCode::NO_CONTENT
                        }
                    },
                ),
            )
    };
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let address = listener.local_addr()?;
    let server = tokio::spawn(async move { axum::serve(listener, app).await });
    let directory = tempfile::tempdir()?;
    let participation = Arc::new(Participation::load(
        directory.path().join("participation.json"),
    )?);
    let ingress = ingress(
        core_client(format!("http://{address}")),
        config,
        participation,
        discord_client(address),
        directory.path(),
    )?;
    super::prepare(&ingress, hello()).await;
    // The answer is delivered and its outcome shown, as the delivery loop does.
    ingress
        .reactions
        .show_outcome("300:400".to_owned(), crate::delivery::Feedback::Delivered);
    tokio::time::timeout(Duration::from_secs(2), removing.notified())
        .await
        .context("the outcome did not remove the processing reaction")?;
    // The replay is prepared like any message while that removal is under way.
    tokio::time::timeout(Duration::from_secs(10), super::prepare(&ingress, hello())).await?;
    release.notify_one();
    assert_eq!(ingress.reactions.stop(Duration::from_secs(10)).await, 0);
    server.abort();
    // Core already held the replay, so it was neither shown nor submitted.
    assert_eq!(
        *applied.lock().await,
        [
            format!("PUT {processing}"),
            format!("DELETE {processing}"),
            format!("PUT {delivered}"),
        ]
    );
    assert_eq!(accepted.load(Ordering::SeqCst), 1);
    Ok(())
}

#[tokio::test]
async fn an_accepted_message_whose_answer_was_lost_keeps_its_processing_reaction()
-> anyhow::Result<()> {
    // Core accepts the message, but its answer to the first attempt is
    // lost; the retry, with the same event, is that accepted attempt's
    // duplicate. The message still waits for its own answer.
    let (trace, mut observed) = tokio::sync::mpsc::unbounded_channel();
    let accepted = Arc::new(AtomicUsize::new(0));
    let (core_trace, submitted) = (trace.clone(), accepted.clone());
    let app = Router::new()
        .route(
            "/v1/events",
            post(move |Json(event): Json<ExternalEvent>| {
                let attempt = submitted.fetch_add(1, Ordering::SeqCst);
                let _ = core_trace.send(format!("submit {attempt}"));
                async move {
                    if attempt == 0 {
                        StatusCode::INTERNAL_SERVER_ERROR.into_response()
                    } else {
                        Json(EventReceipt {
                            event_id: event.event_id,
                            turn_id: "accepted".to_owned(),
                            duplicate: true,
                            state: TurnState::Queued,
                        })
                        .into_response()
                    }
                }
            }),
        )
        .merge(event_status(accepted))
        .fallback(move |method: Method, uri: Uri| {
            let trace = trace.clone();
            async move {
                let _ = trace.send(format!("{method} {}", uri.path()));
                StatusCode::NO_CONTENT
            }
        });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let address = listener.local_addr()?;
    let server = tokio::spawn(async move { axum::serve(listener, app).await });
    let directory = tempfile::tempdir()?;
    let participation = Arc::new(Participation::load(
        directory.path().join("participation.json"),
    )?);
    let mut config = discord_config(DiscordActivation::Always, DiscordConversation::Channel);
    config.typing = false;
    let ingress = ingress(
        core_client(format!("http://{address}")),
        config,
        participation,
        discord_client(address),
        directory.path(),
    )?;
    let reaction =
        |emoji: &str| format!("/api/v10/channels/300/messages/400/reactions/{emoji}/@me");
    let (processing, delivered) = (reaction("%F0%9F%91%80"), reaction("%E2%9C%85"));
    super::prepare(&ingress, hello()).await;
    // Accepted, the message keeps its processing reaction until its answer.
    let admitted = std::iter::from_fn(|| observed.try_recv().ok()).collect::<Vec<_>>();
    assert_eq!(
        admitted,
        [
            format!("PUT {processing}"),
            "submit 0".to_owned(),
            "submit 1".to_owned(),
        ]
    );
    // Its answer is delivered and its outcome shown, as the delivery loop does.
    ingress
        .reactions
        .show_outcome("300:400".to_owned(), crate::delivery::Feedback::Delivered);
    assert_eq!(ingress.reactions.stop(Duration::from_secs(10)).await, 0);
    server.abort();
    let shown = std::iter::from_fn(|| observed.try_recv().ok()).collect::<Vec<_>>();
    assert_eq!(
        shown,
        [format!("DELETE {processing}"), format!("PUT {delivered}")]
    );
    Ok(())
}

#[tokio::test]
async fn a_message_stopped_before_core_says_whether_it_holds_it_gets_no_feedback()
-> anyhow::Result<()> {
    // Core is slow to say whether it holds the message, and the provider
    // stops meanwhile.
    let (trace, mut observed) = tokio::sync::mpsc::unbounded_channel();
    let asked = Arc::new(Notify::new());
    let app = {
        let (status_trace, asked) = (trace.clone(), asked.clone());
        Router::new()
            .route(
                "/v1/events/{event_id}",
                get(move |uri: Uri| async move {
                    let _ = status_trace.send(format!("GET {uri}"));
                    asked.notify_one();
                    tokio::time::sleep(Duration::from_secs(30)).await;
                    Json(EventStatus::Unknown)
                }),
            )
            .fallback(move |method: Method, uri: Uri| {
                let trace = trace.clone();
                async move {
                    let _ = trace.send(format!("{method} {uri}"));
                    StatusCode::NO_CONTENT
                }
            })
    };
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let address = listener.local_addr()?;
    let server = tokio::spawn(async move { axum::serve(listener, app).await });
    let directory = tempfile::tempdir()?;
    let participation = Arc::new(Participation::load(
        directory.path().join("participation.json"),
    )?);
    let ingress = ingress(
        core_client(format!("http://{address}")),
        discord_config(DiscordActivation::Always, DiscordConversation::Channel),
        participation,
        discord_client(address),
        directory.path(),
    )?;
    let preparing = tokio::spawn({
        let ingress = ingress.clone();
        async move { super::prepare(&ingress, hello()).await }
    });
    tokio::time::timeout(Duration::from_secs(2), asked.notified()).await?;
    ingress.stopping.cancel();
    tokio::time::timeout(Duration::from_secs(1), preparing)
        .await
        .context("preparation waited out core after the stop")??;
    assert_eq!(ingress.reactions.stop(Duration::from_secs(1)).await, 0);
    server.abort();
    // Neither feedback nor a submission: the message was never admitted.
    let requests = std::iter::from_fn(|| observed.try_recv().ok()).collect::<Vec<_>>();
    assert_eq!(requests, ["GET /v1/events/400"]);
    Ok(())
}

#[tokio::test]
async fn an_overload_refusal_ends_with_the_reactions() -> anyhow::Result<()> {
    // Discord receives the refusal's explanation but answers it only after
    // the provider has stopped.
    let trace = Arc::new(tokio::sync::Mutex::new(Vec::new()));
    let (explaining, release) = (Arc::new(Notify::new()), Arc::new(Notify::new()));
    let app = {
        let (trace, explaining, release) = (trace.clone(), explaining.clone(), release.clone());
        Router::new().fallback(move |method: Method, uri: Uri| {
            let (trace, explaining, release) = (trace.clone(), explaining.clone(), release.clone());
            async move {
                trace.lock().await.push(format!("{method} {}", uri.path()));
                if method == Method::POST {
                    explaining.notify_one();
                    release.notified().await;
                }
                StatusCode::NO_CONTENT
            }
        })
    };
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let address = listener.local_addr()?;
    let server = tokio::spawn(async move { axum::serve(listener, app).await });
    let directory = tempfile::tempdir()?;
    let participation = Arc::new(Participation::load(
        directory.path().join("participation.json"),
    )?);
    let mut config = discord_config(DiscordActivation::Always, DiscordConversation::Channel);
    config.typing = false;
    let ingress = ingress(
        core_client(format!("http://{address}")),
        config,
        participation,
        discord_client(address),
        directory.path(),
    )?;
    let refusals = Arc::new(tokio::sync::Semaphore::new(1));
    super::lanes::refuse_overloaded(&ingress, &refusals, hello().request);
    tokio::time::timeout(Duration::from_secs(2), explaining.notified()).await?;
    ingress.stopping.cancel();
    // The refusal is the reactions' own: given the grace, then cancelled,
    // its slot returned at once.
    assert_eq!(ingress.reactions.stop(Duration::from_millis(200)).await, 1);
    assert_eq!(refusals.available_permits(), 1);
    release.notify_one();
    tokio::time::sleep(Duration::from_millis(300)).await;
    server.abort();
    // The explanation Discord already received is all it got: no reaction
    // followed after the stop.
    assert_eq!(*trace.lock().await, ["POST /api/v10/channels/300/messages"]);
    Ok(())
}
