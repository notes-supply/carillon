use super::*;

/// A conversation in channel 300 whose message 400 is still being
/// prepared, core taking `status` to say whether it holds it. Beside it
/// [`preparing`] returns calls of a person in that conversation, which
/// take their place in the same lanes. `accepted` records the events
/// core accepts, `markers` the marker messages posted.
struct Preparing {
    ingress: Arc<crate::inbound::Ingress>,
    send: tokio::sync::mpsc::Sender<crate::inbound::lanes::Queued>,
    dispatcher: tokio::task::JoinHandle<()>,
    accepted: Arc<tokio::sync::Mutex<Vec<String>>>,
    markers: Arc<AtomicUsize>,
    server: tokio::task::JoinHandle<std::io::Result<()>>,
    _directory: tempfile::TempDir,
}

impl Preparing {
    /// Closes the lanes and waits, at most ten seconds, for the
    /// dispatcher to settle what it holds; the calls must be gone, as
    /// they hold the lanes too. Returns the events core accepted.
    async fn settled(self) -> anyhow::Result<Vec<String>> {
        drop(self.send);
        tokio::time::timeout(Duration::from_secs(10), self.dispatcher)
            .await
            .map_err(|_| anyhow::anyhow!("the lanes did not settle"))??;
        self.server.abort();
        Ok(self.accepted.lock().await.clone())
    }

    /// Stops the provider in the order its shutdown does: the lanes
    /// settle what they hold, at most ten seconds, then the reactions
    /// get `grace`. Returns the events core accepted and the reactions
    /// the stop cancelled.
    async fn stopped(self, grace: Duration) -> anyhow::Result<(Vec<String>, usize)> {
        self.ingress.stopping.cancel();
        drop(self.send);
        tokio::time::timeout(Duration::from_secs(10), self.dispatcher)
            .await
            .map_err(|_| anyhow::anyhow!("the lanes did not settle"))??;
        let cancelled = self.ingress.reactions.stop(grace).await;
        self.server.abort();
        Ok((self.accepted.lock().await.clone(), cancelled))
    }
}

/// Core and Discord for [`preparing`]: core is slow by `status` to say
/// whether it holds message 400, and records what it accepts and the
/// marker messages posted.
fn preparing_server(
    status: Duration,
    asked: Arc<tokio::sync::Notify>,
    accepted: Arc<tokio::sync::Mutex<Vec<String>>>,
    markers: Arc<AtomicUsize>,
) -> Router {
    use axum::{
        Json,
        extract::Path as Segment,
        routing::{get, post},
    };
    use carillon_protocol::model::{EventReceipt, EventStatus, ExternalEvent, TurnState};
    Router::new()
        .route(
            "/v1/events/{event_id}",
            get(move |Segment(event_id): Segment<String>| async move {
                if event_id == "400" {
                    asked.notify_one();
                    tokio::time::sleep(status).await;
                }
                Json(EventStatus::Unknown)
            }),
        )
        .route(
            "/v1/events",
            post(move |Json(event): Json<ExternalEvent>| {
                let accepted = accepted.clone();
                async move {
                    accepted.lock().await.push(event.event_id.clone());
                    Json(EventReceipt {
                        event_id: event.event_id,
                        turn_id: "turn".to_owned(),
                        duplicate: false,
                        state: TurnState::Queued,
                    })
                }
            }),
        )
        .route(
            "/v1/commands",
            get(|| async { Json(json!({"commands": [{"name": "compact"}]})) }),
        )
        .merge(marking(markers))
        .fallback(|| async { StatusCode::NO_CONTENT })
}

/// Discord posting every marker in channel 300 as `901`, counting them.
fn marking(markers: Arc<AtomicUsize>) -> Router {
    use axum::{Json, routing::post};
    Router::new().route(
        "/api/v10/channels/{channel}/messages",
        post(move || {
            markers.fetch_add(1, Ordering::SeqCst);
            async {
                Json(json!({
                    "id": "901",
                    "channel_id": "300",
                    "author": {"id": "201", "username": "carillon", "discriminator": "0"},
                    "content": "marker",
                    "attachments": [],
                    "embeds": [],
                    "mention_everyone": false,
                    "mention_roles": [],
                    "mentions": [],
                    "pinned": false,
                    "timestamp": "2026-10-05T00:00:00.000000+00:00",
                    "tts": false,
                    "type": 0,
                }))
            }
        }),
    )
}

/// Core whose acceptance of command 901 takes `admission`, in work of
/// its own that goes on whatever its caller does, as core's storage work
/// does: a request for an event already being accepted waits for that
/// and is answered as its duplicate, as core's transaction answers.
/// Other events are accepted at once. `accepted` records the events in
/// the order core commits them, and `posts` counts the submissions.
fn admitting_server(
    admission: Duration,
    accepted: Arc<tokio::sync::Mutex<Vec<String>>>,
    markers: Arc<AtomicUsize>,
    posts: Arc<AtomicUsize>,
) -> Router {
    use axum::{
        Json,
        extract::Path as Segment,
        routing::{get, post},
    };
    use carillon_protocol::model::{EventReceipt, EventStatus, ExternalEvent, TurnState};
    use std::collections::HashMap;
    let jobs = Arc::new(std::sync::Mutex::new(HashMap::<
        String,
        tokio::sync::watch::Receiver<bool>,
    >::new()));
    let committed = accepted.clone();
    Router::new()
        .route(
            "/v1/events/{event_id}",
            get(move |Segment(event_id): Segment<String>| {
                let committed = committed.clone();
                async move {
                    if committed.lock().await.contains(&event_id) {
                        Json(EventStatus::Accepted(EventReceipt {
                            event_id,
                            turn_id: "turn".to_owned(),
                            duplicate: false,
                            state: TurnState::Queued,
                        }))
                    } else {
                        Json(EventStatus::Unknown)
                    }
                }
            }),
        )
        .route(
            "/v1/events",
            post(move |Json(event): Json<ExternalEvent>| {
                posts.fetch_add(1, Ordering::SeqCst);
                let (mut done, first) = {
                    let mut jobs = jobs
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    if let Some(job) = jobs.get(&event.event_id) {
                        (job.clone(), false)
                    } else {
                        let (commit, done) = tokio::sync::watch::channel(false);
                        jobs.insert(event.event_id.clone(), done.clone());
                        let (accepted, id) = (accepted.clone(), event.event_id.clone());
                        let delay = if id == "901" {
                            admission
                        } else {
                            Duration::ZERO
                        };
                        tokio::spawn(async move {
                            tokio::time::sleep(delay).await;
                            accepted.lock().await.push(id);
                            let _ = commit.send(true);
                        });
                        (done, true)
                    }
                };
                async move {
                    let _ = done.wait_for(|committed| *committed).await;
                    Json(EventReceipt {
                        event_id: event.event_id,
                        turn_id: "turn".to_owned(),
                        duplicate: !first,
                        state: TurnState::Queued,
                    })
                }
            }),
        )
        .merge(marking(markers))
        .fallback(|| async { StatusCode::NO_CONTENT })
}

async fn preparing(
    status: Duration,
) -> anyhow::Result<(super::super::InteractionCalls, Preparing)> {
    let (accepted, markers) = (
        Arc::new(tokio::sync::Mutex::new(Vec::new())),
        Arc::new(AtomicUsize::new(0)),
    );
    let asked = Arc::new(tokio::sync::Notify::new());
    let router = preparing_server(status, asked.clone(), accepted.clone(), markers.clone());
    let (calls, preparing) = conversation(router, accepted, markers).await?;
    // Message 400 arrives first, through the gateway's lanes.
    preparing
        .send
        .send(crate::inbound::tests::hello(&preparing.ingress).into())
        .await?;
    tokio::time::timeout(Duration::from_secs(10), asked.notified())
        .await
        .context("message 400 was never prepared")?;
    Ok((calls, preparing))
}

/// Conversation 300's lanes, served by `router` as core and Discord, and
/// calls of a person in that conversation, which take their place in the
/// same lanes.
async fn conversation(
    router: Router,
    accepted: Arc<tokio::sync::Mutex<Vec<String>>>,
    markers: Arc<AtomicUsize>,
) -> anyhow::Result<(super::super::InteractionCalls, Preparing)> {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let address = listener.local_addr()?;
    let server = tokio::spawn(async move { axum::serve(listener, router).await });
    let mut config = crate::conversations::tests::discord_config(
        crate::config::DiscordActivation::Always,
        crate::config::DiscordConversation::Channel,
    );
    config.typing = false;
    let http = Arc::new(
        twilight_http::Client::builder()
            .token("test-only".to_owned())
            .proxy(address.to_string(), true)
            .ratelimiter(None)
            .build(),
    );
    let core = crate::client::test_client(format!("http://{address}"))?;
    let directory = tempfile::tempdir()?;
    let participation = Arc::new(crate::conversations::Participation::load(
        directory.path().join("participation.json"),
    )?);
    let ingress = crate::inbound::tests::ingress(
        core.clone(),
        config,
        participation,
        http.clone(),
        directory.path(),
    )?;
    let (send, receive) = tokio::sync::mpsc::channel(4);
    let dispatcher = tokio::spawn(crate::inbound::lanes::dispatch(ingress.clone(), receive));
    let calls =
        super::super::InteractionCalls::new(core, route(), Some("discord-100-300".to_owned()))
            .with_submissions(super::super::Submissions {
                http,
                reactions: ingress.reactions.clone(),
                lanes: crate::inbound::lanes::Lanes::new(send.clone()),
                origin: crate::conversations::InteractionOrigin {
                    guild_id: "100".to_owned(),
                    user_id: "200".to_owned(),
                    channel_id: "300".to_owned(),
                    route_channel_id: "300".to_owned(),
                },
                settings: ingress.reactions.settings().current(),
            });
    let preparing = Preparing {
        ingress,
        send,
        dispatcher,
        accepted,
        markers,
        server,
        _directory: directory,
    };
    Ok((calls, preparing))
}

#[tokio::test]
async fn a_command_is_accepted_after_the_messages_before_it_in_its_conversation()
-> anyhow::Result<()> {
    // Core takes two seconds to say whether it holds message 400 when a
    // Run click comes in the same conversation.
    let (calls, preparing) = preparing(Duration::from_secs(2)).await?;
    let directory = tempfile::tempdir()?;
    std::fs::write(directory.path().join("release.wasm"), running_module()?)?;
    let commands = Module::load(files(directory.path()), Source::Bundled).await?;
    assert_eq!(commands.invoke(calls, interaction()).await?.content, "ran");
    // Core accepted the message before the command that followed it.
    assert_eq!(preparing.settled().await?, ["400", "901"]);
    Ok(())
}

#[tokio::test]
async fn a_command_still_waiting_at_its_deadline_is_not_sent_and_its_conversation_goes_on()
-> anyhow::Result<()> {
    use carillon_discord_module::contract::HostReply;
    let (calls, preparing) = preparing(Duration::from_secs(3)).await?;
    let started = std::time::Instant::now();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(1);
    let reply = calls.answer(&submit_request()?, deadline).await;
    drop(calls);
    assert!(started.elapsed() < Duration::from_secs(2));
    assert_eq!(
            reply,
            HostReply::Error("Earlier messages in this conversation are still being prepared, so it was not sent; please run it again once they are answered.".to_owned())
        );
    let markers = preparing.markers.clone();
    // The message was accepted; nothing of the command was posted or sent.
    assert_eq!(preparing.settled().await?, ["400"]);
    assert_eq!(markers.load(Ordering::SeqCst), 0);
    Ok(())
}

/// Core accepting every event at once, and Discord that records each
/// marker it is asked to post, counts it, and never answers.
fn unanswered_marker_server(
    accepted: Arc<tokio::sync::Mutex<Vec<String>>>,
    markers: Arc<AtomicUsize>,
    posted: Arc<tokio::sync::Mutex<Vec<String>>>,
) -> Router {
    use axum::{
        Json,
        routing::{get, post},
    };
    use carillon_protocol::model::{EventReceipt, EventStatus, ExternalEvent, TurnState};
    Router::new()
        .route(
            "/v1/events/{event_id}",
            get(|| async { Json(EventStatus::Unknown) }),
        )
        .route(
            "/v1/events",
            post(move |Json(event): Json<ExternalEvent>| {
                let accepted = accepted.clone();
                async move {
                    accepted.lock().await.push(event.event_id.clone());
                    Json(EventReceipt {
                        event_id: event.event_id,
                        turn_id: "turn".to_owned(),
                        duplicate: false,
                        state: TurnState::Queued,
                    })
                }
            }),
        )
        .route(
            "/api/v10/channels/{channel}/messages",
            post(move |Json(body): Json<Value>| {
                let (markers, posted) = (markers.clone(), posted.clone());
                async move {
                    markers.fetch_add(1, Ordering::SeqCst);
                    posted
                        .lock()
                        .await
                        .push(body["content"].as_str().unwrap_or_default().to_owned());
                    std::future::pending::<StatusCode>().await
                }
            }),
        )
        .fallback(|| async { StatusCode::NO_CONTENT })
}

#[tokio::test]
async fn a_command_whose_marker_discord_does_not_confirm_is_not_sent_and_its_conversation_goes_on()
-> anyhow::Result<()> {
    use carillon_discord_module::contract::HostReply;
    let (accepted, markers, posted) = (
        Arc::new(tokio::sync::Mutex::new(Vec::new())),
        Arc::new(AtomicUsize::new(0)),
        Arc::new(tokio::sync::Mutex::new(Vec::new())),
    );
    let router = unanswered_marker_server(accepted.clone(), markers.clone(), posted.clone());
    let (calls, preparing) = conversation(router, accepted.clone(), markers).await?;
    let started = std::time::Instant::now();
    let submit = submit_request()?;
    let answering = tokio::spawn(async move { calls.answer(&submit, calls_until()).await });
    // The person's next message arrives while Discord holds the marker's
    // answer: it waits behind the command.
    tokio::time::timeout(Duration::from_secs(5), async {
        while preparing.markers.load(Ordering::SeqCst) == 0 {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .context("the marker was never posted")?;
    preparing
        .send
        .send(crate::inbound::tests::hello(&preparing.ingress).into())
        .await?;
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(accepted.lock().await.is_empty());
    let reply = answering.await?;
    let elapsed = started.elapsed();
    // Discord may have posted the marker, but without its answer the
    // command has no identity: nothing of it reaches core, and the reply
    // claims no more than that.
    assert_eq!(
        reply,
        HostReply::Error(
            "Discord did not confirm the marker message, so the command was not sent to Carillon."
                .to_owned()
        )
    );
    assert!(
        elapsed >= super::super::MARKER_LIMIT && elapsed < super::super::DEADLINE,
        "answered after {elapsed:?}"
    );
    // The marker asks; it never says the command ran.
    assert_eq!(
        *posted.lock().await,
        ["<@200> asked to run `/compact keep decisions`"]
    );
    // The conversation goes on with the message, and only the message.
    assert_eq!(preparing.settled().await?, ["400"]);
    Ok(())
}

#[tokio::test]
async fn a_command_waiting_when_the_provider_stops_is_not_sent() -> anyhow::Result<()> {
    use carillon_discord_module::contract::HostReply;
    let (calls, preparing) = preparing(Duration::from_secs(30)).await?;
    let submit = submit_request()?;
    let waiting = tokio::spawn(async move { calls.answer(&submit, calls_until()).await });
    tokio::time::sleep(Duration::from_millis(300)).await;
    preparing.ingress.stopping.cancel();
    let reply = tokio::time::timeout(Duration::from_secs(2), waiting).await??;
    assert_eq!(
        reply,
        HostReply::Error(
            "Carillon is too busy or restarting, so it was not sent; please run it again shortly."
                .to_owned()
        )
    );
    let markers = preparing.markers.clone();
    assert!(preparing.settled().await?.is_empty());
    assert_eq!(markers.load(Ordering::SeqCst), 0);
    Ok(())
}

#[tokio::test]
async fn listing_commands_never_waits_for_the_conversation() -> anyhow::Result<()> {
    use carillon_discord_module::contract::{HostReply, HostRequest};
    // The conversation's message is being prepared for half a minute.
    let (calls, preparing) = preparing(Duration::from_secs(30)).await?;
    let started = std::time::Instant::now();
    let reply = calls
        .answer(&serde_json::to_vec(&HostRequest::Commands)?, calls_until())
        .await;
    assert!(started.elapsed() < Duration::from_secs(1));
    assert!(matches!(reply, HostReply::Commands(catalog) if catalog.commands.len() == 1));
    preparing.server.abort();
    Ok(())
}

/// Calls of the conversation of [`admitting_server`], whose core takes
/// three seconds to accept command 901, the reply to its submission,
/// due one second in, and the count of core's submissions.
async fn admitting() -> anyhow::Result<(
    Preparing,
    Arc<tokio::sync::Mutex<Vec<String>>>,
    Arc<AtomicUsize>,
)> {
    let posts = Arc::new(AtomicUsize::new(0));
    let counted = posts.clone();
    let (preparing, accepted) = unconfirmed(|accepted, markers| {
        admitting_server(Duration::from_secs(3), accepted, markers, counted)
    })
    .await?;
    Ok((preparing, accepted, posts))
}

/// Calls of the conversation served by `serve` as core and Discord, and
/// the reply to a submission of command 901, due one second in, before
/// core has answered for it.
async fn unconfirmed(
    serve: impl FnOnce(Arc<tokio::sync::Mutex<Vec<String>>>, Arc<AtomicUsize>) -> Router,
) -> anyhow::Result<(Preparing, Arc<tokio::sync::Mutex<Vec<String>>>)> {
    use carillon_discord_module::contract::HostReply;
    let (accepted, markers) = (
        Arc::new(tokio::sync::Mutex::new(Vec::new())),
        Arc::new(AtomicUsize::new(0)),
    );
    let router = serve(accepted.clone(), markers.clone());
    let (calls, preparing) = conversation(router, accepted.clone(), markers).await?;
    let started = std::time::Instant::now();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(1);
    let reply = calls.answer(&submit_request()?, deadline).await;
    // The reply is due before core has answered: it says core may have
    // accepted the command, in time.
    assert!(started.elapsed() < Duration::from_secs(2));
    assert_eq!(
            reply,
            HostReply::Error("Carillon did not confirm that it accepted it; if it did, the answer follows in this conversation.".to_owned())
        );
    assert_eq!(preparing.markers.load(Ordering::SeqCst), 1);
    Ok((preparing, accepted))
}

#[tokio::test]
async fn a_command_core_is_still_accepting_keeps_its_conversation_until_core_answers()
-> anyhow::Result<()> {
    let (preparing, accepted, _) = admitting().await?;
    // The person's next message arrives while core is still accepting
    // the command.
    let mut next = crate::inbound::tests::hello(&preparing.ingress);
    next.request.event_id = "402".to_owned();
    preparing.send.send(next.into()).await?;
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(accepted.lock().await.is_empty());
    // Core accepts the command before the message that followed it.
    assert_eq!(preparing.settled().await?, ["901", "402"]);
    Ok(())
}

#[tokio::test]
async fn a_stopping_provider_waits_for_a_command_core_is_still_accepting() -> anyhow::Result<()> {
    let (preparing, accepted, posts) = admitting().await?;
    // The lane asks core again at once, and the provider stops while
    // that attempt waits for core to accept the command.
    tokio::time::timeout(Duration::from_secs(2), async {
        while posts.load(Ordering::SeqCst) < 2 {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await?;
    preparing.ingress.stopping.cancel();
    // The lanes settle only once core has answered that attempt, and no
    // other is made.
    let started = std::time::Instant::now();
    assert_eq!(preparing.settled().await?, ["901"]);
    assert!(started.elapsed() >= Duration::from_secs(1));
    assert_eq!(*accepted.lock().await, ["901"]);
    assert_eq!(posts.load(Ordering::SeqCst), 2);
    Ok(())
}

/// Core that cannot take command 901, answering every attempt as
/// unavailable after `answer`, and accepts other events at once, beside
/// a Discord that answers at once. `trace` records core's requests and
/// Discord's reactions.
fn unavailable_server(
    trace: Arc<tokio::sync::Mutex<Vec<String>>>,
    accepted: Arc<tokio::sync::Mutex<Vec<String>>>,
    markers: Arc<AtomicUsize>,
    answer: Duration,
) -> Router {
    use axum::{
        Json,
        extract::Path as Segment,
        http::{Method, Uri},
        response::IntoResponse,
        routing::{get, post},
    };
    use carillon_protocol::model::{EventReceipt, EventStatus, ExternalEvent, TurnState};
    let (asked, attempted, reacted) = (trace.clone(), trace.clone(), trace);
    Router::new()
        .route(
            "/v1/events/{event_id}",
            get(move |Segment(event_id): Segment<String>| async move {
                asked.lock().await.push(format!("status {event_id}"));
                Json(EventStatus::Unknown)
            }),
        )
        .route(
            "/v1/events",
            post(move |Json(event): Json<ExternalEvent>| async move {
                attempted
                    .lock()
                    .await
                    .push(format!("event {}", event.event_id));
                if event.event_id == "901" {
                    tokio::time::sleep(answer).await;
                    return StatusCode::SERVICE_UNAVAILABLE.into_response();
                }
                accepted.lock().await.push(event.event_id.clone());
                Json(EventReceipt {
                    event_id: event.event_id,
                    turn_id: "turn".to_owned(),
                    duplicate: false,
                    state: TurnState::Queued,
                })
                .into_response()
            }),
        )
        .merge(marking(markers))
        .fallback(move |method: Method, uri: Uri| async move {
            reacted
                .lock()
                .await
                .push(format!("{method} {}", uri.path()));
            StatusCode::NO_CONTENT
        })
}

#[tokio::test]
async fn a_stopping_provider_leaves_a_command_core_cannot_confirm_unconfirmed() -> anyhow::Result<()>
{
    let trace = Arc::new(tokio::sync::Mutex::new(Vec::new()));
    let served = trace.clone();
    let (preparing, _) = unconfirmed(|accepted, markers| {
        unavailable_server(served, accepted, markers, Duration::ZERO)
    })
    .await?;
    let attempts = async || {
        trace
            .lock()
            .await
            .iter()
            .filter(|request| *request == "event 901")
            .count()
    };
    // The person's next message waits while the lane goes on asking
    // core, which still cannot take the command.
    let mut next = crate::inbound::tests::hello(&preparing.ingress);
    next.request.event_id = "402".to_owned();
    preparing.send.send(next.into()).await?;
    let before = attempts().await;
    tokio::time::timeout(Duration::from_secs(5), async {
        while attempts().await <= before + 1 {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .context("the lane stopped asking core")?;
    // The provider stops: the lanes settle without core's answer, and
    // the message waiting behind the command is dropped unprepared.
    let started = std::time::Instant::now();
    let (accepted, cancelled) = preparing.stopped(crate::delivery::PROMPT_FEEDBACK).await?;
    assert!(started.elapsed() < Duration::from_secs(1));
    assert!(accepted.is_empty());
    assert_eq!(cancelled, 0);
    let trace = trace.lock().await;
    assert!(
        !trace.iter().any(|request| request.ends_with("402")),
        "{trace:?}"
    );
    // The command's marker says it is unconfirmed, not refused.
    let marker = "/api/v10/channels/300/messages/901/reactions";
    assert_eq!(
        trace
            .iter()
            .filter(|request| request.contains("/reactions/"))
            .collect::<Vec<_>>(),
        [
            &format!("PUT {marker}/%F0%9F%91%80/@me"),
            &format!("DELETE {marker}/%F0%9F%91%80/@me"),
            &format!("PUT {marker}/%E2%9A%A0%EF%B8%8F/@me"),
        ]
    );
    Ok(())
}

/// A Run click's command in conversation 300 while core cannot take it,
/// core taking `answer` to say so for each attempt. Once core has
/// received the command's first attempt, message 402 arrives behind it,
/// and the provider stops `after` that. Returns the reply and, in order, core's
/// requests and Discord's reactions.
async fn stopped_while_submitting(
    answer: Duration,
    after: Duration,
) -> anyhow::Result<(carillon_discord_module::contract::HostReply, Vec<String>)> {
    let trace = Arc::new(tokio::sync::Mutex::new(Vec::new()));
    let (accepted, markers) = (
        Arc::new(tokio::sync::Mutex::new(Vec::new())),
        Arc::new(AtomicUsize::new(0)),
    );
    let router = unavailable_server(trace.clone(), accepted.clone(), markers.clone(), answer);
    let (calls, preparing) = conversation(router, accepted, markers).await?;
    let submit = submit_request()?;
    let reply = tokio::spawn(async move { calls.answer(&submit, calls_until()).await });
    tokio::time::timeout(Duration::from_secs(2), async {
        while !trace.lock().await.contains(&"event 901".to_owned()) {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await?;
    // The command holds its conversation's turn; the message waits.
    let mut next = crate::inbound::tests::hello(&preparing.ingress);
    next.request.event_id = "402".to_owned();
    preparing.send.send(next.into()).await?;
    tokio::time::sleep(after).await;
    let started = std::time::Instant::now();
    let (accepted, cancelled) = preparing.stopped(crate::delivery::PROMPT_FEEDBACK).await?;
    assert!(
        started.elapsed() < answer + Duration::from_secs(1),
        "the stop took {:?}",
        started.elapsed()
    );
    assert!(accepted.is_empty());
    assert_eq!(cancelled, 0);
    let reply = tokio::time::timeout(Duration::from_secs(2), reply).await??;
    let trace = trace.lock().await.clone();
    Ok((reply, trace))
}

/// What a command core never confirmed shows once the provider stopped:
/// the reply says it may have been accepted, core got one attempt, the
/// message behind it was never prepared, and the marker shows the
/// uncertain reaction.
fn assert_unconfirmed(reply: &carillon_discord_module::contract::HostReply, trace: &[String]) {
    use carillon_discord_module::contract::HostReply;
    assert_eq!(
            *reply,
            HostReply::Error("Carillon did not confirm that it accepted it; if it did, the answer follows in this conversation.".to_owned())
        );
    assert_eq!(
        trace
            .iter()
            .filter(|request| *request == "event 901")
            .count(),
        1,
        "{trace:?}"
    );
    assert!(
        !trace.iter().any(|request| request.ends_with("402")),
        "{trace:?}"
    );
    let marker = "/api/v10/channels/300/messages/901/reactions";
    assert_eq!(
        trace
            .iter()
            .filter(|request| request.contains("/reactions/"))
            .collect::<Vec<_>>(),
        [
            &format!("PUT {marker}/%F0%9F%91%80/@me"),
            &format!("DELETE {marker}/%F0%9F%91%80/@me"),
            &format!("PUT {marker}/%E2%9A%A0%EF%B8%8F/@me"),
        ]
    );
}

#[tokio::test]
async fn a_command_stopped_while_waiting_to_try_again_makes_no_other_attempt() -> anyhow::Result<()>
{
    // Core says at once that it cannot take the command; the provider
    // stops while the submission waits to try again.
    let (reply, trace) =
        stopped_while_submitting(Duration::ZERO, Duration::from_millis(100)).await?;
    assert_unconfirmed(&reply, &trace);
    Ok(())
}

#[tokio::test]
async fn a_command_stopped_while_core_answers_its_attempt_makes_no_other() -> anyhow::Result<()> {
    // The provider stops while core takes half a second to say it
    // cannot take the command: that attempt ends, and none follows.
    let (reply, trace) =
        stopped_while_submitting(Duration::from_millis(500), Duration::from_millis(100)).await?;
    assert_unconfirmed(&reply, &trace);
    Ok(())
}

/// Core that does not answer the first submission of command 901 and
/// refuses it when asked again, accepting other events at once, beside
/// a Discord that holds every reaction for a minute.
fn refusing_server(
    accepted: Arc<tokio::sync::Mutex<Vec<String>>>,
    markers: Arc<AtomicUsize>,
) -> Router {
    use axum::{
        Json,
        extract::Path as Segment,
        http::Uri,
        response::IntoResponse,
        routing::{get, post},
    };
    use carillon_protocol::model::{EventReceipt, EventStatus, ExternalEvent, TurnState};
    let attempts = Arc::new(AtomicUsize::new(0));
    Router::new()
        .route(
            "/v1/events/{event_id}",
            get(|Segment(_): Segment<String>| async { Json(EventStatus::Unknown) }),
        )
        .route(
            "/v1/events",
            post(move |Json(event): Json<ExternalEvent>| async move {
                if event.event_id == "901" {
                    if attempts.fetch_add(1, Ordering::SeqCst) == 0 {
                        tokio::time::sleep(Duration::from_secs(30)).await;
                    }
                    return StatusCode::UNPROCESSABLE_ENTITY.into_response();
                }
                accepted.lock().await.push(event.event_id.clone());
                Json(EventReceipt {
                    event_id: event.event_id,
                    turn_id: "turn".to_owned(),
                    duplicate: false,
                    state: TurnState::Queued,
                })
                .into_response()
            }),
        )
        .merge(marking(markers))
        .fallback(|uri: Uri| async move {
            if uri.path().contains("/reactions/") {
                tokio::time::sleep(Duration::from_secs(60)).await;
            }
            StatusCode::NO_CONTENT
        })
}

#[tokio::test]
async fn a_refused_command_lets_its_conversation_go_on_beside_its_feedback() -> anyhow::Result<()> {
    let (preparing, accepted) = unconfirmed(refusing_server).await?;
    // Core refuses the command when the lane asks again, while Discord
    // still holds its processing reaction, which its failure waits for.
    let mut next = crate::inbound::tests::hello(&preparing.ingress);
    next.request.event_id = "402".to_owned();
    preparing.send.send(next.into()).await?;
    // The person's next message is accepted within its own wait for
    // feedback, not after the command's.
    tokio::time::timeout(Duration::from_secs(5), async {
        while accepted.lock().await.is_empty() {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .map_err(|_| anyhow::anyhow!("the conversation waited for the command's feedback"))?;
    // The reactions still own the command's failure and both processing
    // requests, and their stop ends them.
    let (accepted, cancelled) = preparing.stopped(Duration::from_millis(100)).await?;
    assert_eq!(accepted, ["402"]);
    assert_eq!(cancelled, 3);
    Ok(())
}

/// Core accepting every event, beside a Discord that posts the marker
/// and applies the first processing reaction `delay` after receiving it.
fn applying_server(delay: Duration, applied: Arc<tokio::sync::Mutex<Vec<String>>>) -> Router {
    use axum::{Json, routing::post};
    use carillon_protocol::model::{EventReceipt, ExternalEvent, TurnState};
    Router::new()
        .route(
            "/v1/events",
            post(|Json(event): Json<ExternalEvent>| async move {
                Json(EventReceipt {
                    event_id: event.event_id,
                    turn_id: "turn".to_owned(),
                    duplicate: false,
                    state: TurnState::Queued,
                })
            }),
        )
        .route(
            "/api/v10/channels/{channel}/messages",
            post(|Json(body): Json<Value>| async move {
                Json(json!({
                    "id": "901",
                    "channel_id": "400",
                    "author": {"id": "201", "username": "carillon", "discriminator": "0"},
                    "content": body["content"],
                    "attachments": [],
                    "embeds": [],
                    "mention_everyone": false,
                    "mention_roles": [],
                    "mentions": [],
                    "pinned": false,
                    "timestamp": "2026-10-05T00:00:00.000000+00:00",
                    "tts": false,
                    "type": 0,
                }))
            }),
        )
        .merge(crate::delivery::tests::late_processing(delay, applied))
        .fallback(|| async { StatusCode::NO_CONTENT })
}

#[tokio::test]
async fn a_late_processing_reaction_never_lands_after_the_outcome() -> anyhow::Result<()> {
    use carillon_discord_module::contract::HostReply;
    // The coordinator's fault: Discord applies the processing reaction
    // four seconds after receiving it, after admission stopped waiting.
    let applied = Arc::new(tokio::sync::Mutex::new(Vec::new()));
    let router = applying_server(Duration::from_secs(4), applied.clone());
    let (calls, reactions, server) = submitting(router, |config| config.typing = true).await?;
    let started = std::time::Instant::now();
    assert_eq!(
        calls.answer(&submit_request()?, calls_until()).await,
        HostReply::Submitted
    );
    // Admission did not wait for Discord to apply it.
    assert!(started.elapsed() < crate::delivery::PROMPT_FEEDBACK + Duration::from_secs(1));
    // The answer is delivered at once and its outcome shown, as the
    // delivery loop does.
    reactions.show_outcome("400:901".to_owned(), super::super::Feedback::Delivered);
    assert_eq!(reactions.stop(Duration::from_secs(10)).await, 0);
    server.abort();
    let config = crate::conversations::tests::discord_config(
        crate::config::DiscordActivation::Always,
        crate::config::DiscordConversation::Thread,
    );
    assert_eq!(
        *applied.lock().await,
        [
            format!("PUT {}", config.processing_emoji),
            format!("DELETE {}", config.processing_emoji),
            format!("PUT {}", config.delivered_emoji),
        ]
    );
    Ok(())
}

#[tokio::test]
async fn a_command_shows_the_reactions_of_the_settings_it_was_admitted_with() -> anyhow::Result<()>
{
    use carillon_discord_module::contract::HostReply;
    let applied = Arc::new(tokio::sync::Mutex::new(Vec::new()));
    let router = applying_server(Duration::ZERO, applied.clone());
    let (calls, reactions, server) = submitting(router, |config| config.typing = false).await?;
    let admitted = reactions.settings().current();
    // The operator serves other reactions after the interaction was
    // admitted, before the module submits its command.
    let mut other = crate::config::DiscordConfig::clone(&admitted);
    "⏳".clone_into(&mut other.processing_emoji);
    "📬".clone_into(&mut other.delivered_emoji);
    reactions
        .settings()
        .apply(|| Ok(other), |_| std::future::ready(Ok(unchanged())))
        .await
        .map_err(|refusal| anyhow::anyhow!("{refusal:?}"))?;
    assert_eq!(
        calls.answer(&submit_request()?, calls_until()).await,
        HostReply::Submitted
    );
    reactions.show_outcome("400:901".to_owned(), super::super::Feedback::Delivered);
    assert_eq!(reactions.stop(Duration::from_secs(10)).await, 0);
    server.abort();
    assert_eq!(
        *applied.lock().await,
        [
            format!("PUT {}", admitted.processing_emoji),
            format!("DELETE {}", admitted.processing_emoji),
            format!("PUT {}", admitted.delivered_emoji),
        ]
    );
    Ok(())
}

#[tokio::test]
async fn a_refused_settings_command_makes_no_core_call() -> anyhow::Result<()> {
    let asked = Arc::new(AtomicUsize::new(0));
    let counted = asked.clone();
    let router = Router::new().fallback(move || {
        counted.fetch_add(1, Ordering::SeqCst);
        async { StatusCode::SERVICE_UNAVAILABLE }
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let core = crate::client::test_client(format!("http://{}", listener.local_addr()?))?;
    let server = tokio::spawn(async move { axum::serve(listener, router).await });
    let config = crate::conversations::tests::discord_config(
        crate::config::DiscordActivation::Always,
        crate::config::DiscordConversation::Thread,
    );
    let module = release_module().await?;
    for (user, guild, reads) in [("999", "100", 0), ("200", "101", 0), ("200", "100", 1)] {
        let source = crate::conversations::InteractionSource {
            user_id: user.to_owned(),
            guild_id: Some(guild.to_owned()),
            ..crate::conversations::tests::interaction("300", None)
        };
        match crate::conversations::admit_interaction(&config, source) {
            Ok(admitted) => {
                let calls = super::super::InteractionCalls::new(
                    core.clone(),
                    admitted.route,
                    admitted.audience_id,
                );
                module.invoke(calls, admitted.interaction).await?;
            }
            Err(refusal) => assert_eq!(refusal.status, StatusCode::FORBIDDEN),
        }
        assert_eq!(
            asked.load(Ordering::SeqCst),
            reads,
            "{user} in guild {guild}"
        );
    }
    server.abort();
    Ok(())
}
