use super::*;

/// Core and Discord in one fake: lists `catalog` after `listing`, accepts
/// every event after `admission` and shows feedback after `feedback`,
/// posts every message as `901`, and traces the requests in order as they
/// arrive.
fn submission_server(
    catalog: carillon_protocol::model::CommandCatalog,
    trace: Arc<tokio::sync::Mutex<Vec<String>>>,
    submitted: Arc<tokio::sync::Mutex<Vec<carillon_protocol::model::ExternalEvent>>>,
    (listing, admission, feedback): (Duration, Duration, Duration),
) -> Router {
    use axum::{
        Json,
        extract::Query,
        http::{Method, Uri},
        routing::{get, post},
    };
    use carillon_protocol::model::{EventReceipt, ExternalEvent, TurnState};
    use std::collections::HashMap;
    let (messages, events) = (trace.clone(), trace.clone());
    Router::new()
        .route(
            "/v1/commands",
            get(move |Query(query): Query<HashMap<String, String>>| {
                let catalog = catalog.clone();
                async move {
                    assert_eq!(
                        query.get("audience_id").map(String::as_str),
                        Some("discord-100-400")
                    );
                    tokio::time::sleep(listing).await;
                    Json(catalog)
                }
            }),
        )
        .route(
            "/v1/events",
            post(move |Json(event): Json<ExternalEvent>| {
                let (events, submitted) = (events.clone(), submitted.clone());
                async move {
                    events
                        .lock()
                        .await
                        .push(format!("event {}", event.event_id));
                    tokio::time::sleep(admission).await;
                    submitted.lock().await.push(event.clone());
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
                let messages = messages.clone();
                async move {
                    messages.lock().await.push(format!(
                        "message {} mentions {}",
                        body["content"].as_str().unwrap_or_default(),
                        body["allowed_mentions"]
                    ));
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
                }
            }),
        )
        .fallback(move |method: Method, uri: Uri| {
            let trace = trace.clone();
            async move {
                trace.lock().await.push(format!("{method} {}", uri.path()));
                tokio::time::sleep(feedback).await;
                StatusCode::NO_CONTENT
            }
        })
}

#[tokio::test]
async fn commands_are_listed_and_submissions_anchored_in_the_conversation() -> anyhow::Result<()> {
    use carillon_discord_module::contract::{HostReply, HostRequest, Intent};
    use carillon_protocol::model::{CommandCatalog, RuntimeCommand};
    let catalog = CommandCatalog {
        commands: vec![RuntimeCommand {
            name: "compact".to_owned(),
            description: None,
            hint: None,
        }],
    };
    let trace = Arc::new(tokio::sync::Mutex::new(Vec::new()));
    let submitted = Arc::new(tokio::sync::Mutex::new(Vec::new()));
    let router = submission_server(
        catalog.clone(),
        trace.clone(),
        submitted.clone(),
        (Duration::ZERO, Duration::ZERO, Duration::ZERO),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let address = listener.local_addr()?;
    let server = tokio::spawn(async move { axum::serve(listener, router).await });
    let core = crate::client::test_client(format!("http://{address}"))?;
    let audience_id = Some("discord-100-400".to_owned());
    let submit = serde_json::to_vec(&HostRequest::Submit {
        text: "/compact keep `this`".to_owned(),
        intent: Intent::Command,
    })?;

    // Without a Discord connection there is nowhere to anchor it.
    let unanchored =
        super::super::InteractionCalls::new(core.clone(), route(), audience_id.clone());
    assert_eq!(
        unanchored
            .answer(&serde_json::to_vec(&HostRequest::Commands)?, calls_until())
            .await,
        HostReply::Commands(catalog)
    );
    assert_eq!(
        unanchored.answer(&submit, calls_until()).await,
        HostReply::Error("Sending to the conversation needs the Discord connection.".to_owned())
    );
    assert!(submitted.lock().await.is_empty());

    let mut config = crate::conversations::tests::discord_config(
        crate::config::DiscordActivation::Always,
        crate::config::DiscordConversation::Thread,
    );
    config.typing = false;
    let http = Arc::new(
        twilight_http::Client::builder()
            .token("test-only".to_owned())
            .proxy(address.to_string(), true)
            .ratelimiter(None)
            .build(),
    );
    let settings = crate::config::Settings::new(config);
    let calls = super::super::InteractionCalls::new(core.clone(), route(), audience_id)
        .with_submissions(super::super::Submissions {
            reactions: crate::delivery::Reactions::new(http.clone(), settings.clone()),
            lanes: serving_lanes(core, http.clone())?,
            http,
            origin: crate::conversations::InteractionOrigin {
                guild_id: "100".to_owned(),
                user_id: "200".to_owned(),
                channel_id: "400".to_owned(),
                route_channel_id: "300".to_owned(),
            },
            settings: settings.current(),
        });
    assert_eq!(
        calls.answer(&submit, calls_until()).await,
        HostReply::Submitted
    );
    server.abort();
    let trace = trace.lock().await;
    // The marker is posted silently first, then marked processing and
    // submitted.
    assert_eq!(
        trace[0],
        "message <@200> asked to run `/compact keep ˋthisˋ` mentions {\"parse\":[]}"
    );
    assert!(trace[1..3].iter().any(|request| {
        request.starts_with("PUT /api/v10/channels/400/messages/901/reactions/")
    }));
    assert!(trace[1..3].iter().any(|request| request == "event 901"));
    let event = submitted.lock().await.remove(0);
    assert_eq!(event.content, "/compact keep `this`");
    assert_eq!(event.feedback_target_id, "400:901");
    assert_eq!(event.delivery_target_id, "400");
    assert_eq!(event.audience_id, "discord-100-400");
    assert_eq!(event.native["intent"], "command");
    assert_eq!(event.native["route_channel_id"], "300");
    assert_eq!(event.native["origin"]["parent_channel_id"], "300");
    Ok(())
}

#[tokio::test]
async fn interactions_receive_the_module_reply_or_a_failure() -> anyhow::Result<()> {
    let directory = tempfile::tempdir()?;
    let button =
        json!({"type": 1, "components": [{"type": 2, "style": 2, "custom_id": "x", "label": "X"}]});
    write_module(
        &directory.path().join("release.wasm"),
        &describing(&[]),
        &replying("hello", std::slice::from_ref(&button)),
    )?;
    let commands = Module::load(files(directory.path()), Source::Bundled).await?;
    let reply = commands.invoke(calls()?, interaction()).await?;
    assert_eq!(reply.content, "hello");
    assert_eq!(reply.components, vec![button]);
    for (output, refused) in [
        (
            json!({"Failure": "no such command"}).to_string(),
            "Discord module failed: no such command",
        ),
        (
            describing(&[]),
            "Discord module answered an interaction with something else",
        ),
        ("{broken".to_owned(), "decode Discord module outcome: "),
    ] {
        std::fs::write(
            directory.path().join("custom.wasm"),
            module(&describing(&[]), &output)?,
        )?;
        commands
            .apply(custom(directory.path(), "custom.wasm"), None)
            .await?;
        let failure = commands
            .invoke(calls()?, interaction())
            .await
            .err()
            .with_context(|| format!("accepted {output}"))?;
        assert!(format!("{failure:#}").starts_with(refused), "{failure:#}");
    }
    Ok(())
}

#[tokio::test]
async fn slow_feedback_never_costs_a_command_its_admission() -> anyhow::Result<()> {
    use carillon_discord_module::contract::HostReply;
    // Discord takes longer than the module's whole deadline to show the
    // processing reaction and typing.
    let trace = Arc::new(tokio::sync::Mutex::new(Vec::new()));
    let submitted = Arc::new(tokio::sync::Mutex::new(Vec::new()));
    let router = submission_server(
        carillon_protocol::model::CommandCatalog {
            commands: Vec::new(),
        },
        trace.clone(),
        submitted.clone(),
        (Duration::ZERO, Duration::ZERO, Duration::from_secs(30)),
    );
    let (calls, _reactions, server) = submitting(router, |config| config.typing = true).await?;
    let started = std::time::Instant::now();
    let reply = calls.answer(&submit_request()?, calls_until()).await;
    let elapsed = started.elapsed();
    server.abort();
    assert_eq!(reply, HostReply::Submitted);
    assert!(
        elapsed < crate::delivery::PROMPT_FEEDBACK + Duration::from_secs(1),
        "admission waited {elapsed:?} for feedback"
    );
    assert_eq!(submitted.lock().await.len(), 1);
    // The marker, then the feedback that was asked for, with admission
    // beside it.
    let trace = trace.lock().await;
    assert!(trace[0].starts_with("message <@200> asked to run"));
    let after = &trace[1..4];
    assert!(after.iter().any(|request| request.contains("/reactions/")));
    assert!(after.iter().any(|request| request.ends_with("/typing")));
    assert!(after.iter().any(|request| request == "event 901"));
    Ok(())
}

#[tokio::test]
async fn a_command_core_does_not_answer_in_time_says_so_within_the_module_deadline()
-> anyhow::Result<()> {
    use carillon_discord_module::contract::HostReply;
    let trace = Arc::new(tokio::sync::Mutex::new(Vec::new()));
    let router = submission_server(
        carillon_protocol::model::CommandCatalog {
            commands: Vec::new(),
        },
        trace.clone(),
        Arc::new(tokio::sync::Mutex::new(Vec::new())),
        (Duration::ZERO, Duration::from_secs(30), Duration::ZERO),
    );
    let (calls, reactions, server) = submitting(router, |config| config.typing = false).await?;
    let started = std::time::Instant::now();
    let reply = calls.answer(&submit_request()?, calls_until()).await;
    let elapsed = started.elapsed();
    // Core may have accepted it: the answer says so, in time to be shown.
    assert_eq!(
            reply,
            HostReply::Error(
                "Carillon did not confirm that it accepted it; if it did, the answer follows in this conversation."
                    .to_owned()
            )
        );
    assert!(
        elapsed < super::super::DEADLINE,
        "answered after {elapsed:?}"
    );
    assert_eq!(reactions.stop(Duration::from_secs(10)).await, 0);
    server.abort();
    // Core received it, and its conversation's lane goes on settling it:
    // the marker keeps its processing reaction, with no outcome yet.
    let trace = trace.lock().await;
    assert!(trace.contains(&"event 901".to_owned()), "{trace:?}");
    assert_eq!(
        trace
            .iter()
            .filter(|request| request.contains("/reactions/"))
            .collect::<Vec<_>>(),
        ["PUT /api/v10/channels/400/messages/901/reactions/%F0%9F%91%80/@me"]
    );
    Ok(())
}

#[tokio::test]
async fn a_run_click_answers_within_the_module_deadline_however_slow_its_calls()
-> anyhow::Result<()> {
    // The runtime takes five seconds to list its commands, then core
    // does not answer the submission at all: separately bounded, the two
    // would outlast the module's deadline.
    let trace = Arc::new(tokio::sync::Mutex::new(Vec::new()));
    let router = submission_server(
        carillon_protocol::model::CommandCatalog {
            commands: vec![carillon_protocol::model::RuntimeCommand {
                name: "compact".to_owned(),
                description: None,
                hint: None,
            }],
        },
        trace.clone(),
        Arc::new(tokio::sync::Mutex::new(Vec::new())),
        (
            Duration::from_secs(5),
            Duration::from_secs(30),
            Duration::ZERO,
        ),
    );
    let (calls, reactions, server) = submitting(router, |config| config.typing = false).await?;
    let directory = tempfile::tempdir()?;
    std::fs::write(directory.path().join("release.wasm"), running_module()?)?;
    let commands = Module::load(files(directory.path()), Source::Bundled).await?;
    let started = std::time::Instant::now();
    let reply = commands.invoke(calls, interaction()).await?;
    let elapsed = started.elapsed();
    // The module answered in time, after both calls had ended.
    assert_eq!(reply.content, "ran");
    assert!(
        elapsed < super::super::DEADLINE,
        "answered after {elapsed:?}"
    );
    // Core received the command; its silence is left to the
    // conversation's lane to settle, so no outcome is shown yet.
    assert_eq!(reactions.stop(Duration::from_secs(10)).await, 0);
    server.abort();
    let trace = trace.lock().await;
    assert!(
        trace[0].starts_with("message <@200> asked to run `/compact keep decisions`"),
        "{trace:?}"
    );
    assert!(trace.contains(&"event 901".to_owned()), "{trace:?}");
    assert!(
        !trace
            .iter()
            .any(|request| request.contains("/reactions/%E2%9A%A0")),
        "{trace:?}"
    );
    Ok(())
}

#[tokio::test]
async fn a_run_click_hands_the_command_to_core_however_slow_its_feedback() -> anyhow::Result<()> {
    // The runtime lists its commands just inside its own limit, leaving
    // less than the feedback's wait; Discord then takes far longer to
    // show the processing reaction and typing, while core answers at once.
    let trace = Arc::new(tokio::sync::Mutex::new(Vec::new()));
    let submitted = Arc::new(tokio::sync::Mutex::new(Vec::new()));
    let router = submission_server(
        carillon_protocol::model::CommandCatalog {
            commands: vec![carillon_protocol::model::RuntimeCommand {
                name: "compact".to_owned(),
                description: None,
                hint: None,
            }],
        },
        trace.clone(),
        submitted.clone(),
        (
            Duration::from_millis(5_500),
            Duration::ZERO,
            Duration::from_secs(30),
        ),
    );
    let (calls, reactions, server) = submitting(router, |config| config.typing = true).await?;
    let directory = tempfile::tempdir()?;
    std::fs::write(directory.path().join("release.wasm"), running_module()?)?;
    let commands = Module::load(files(directory.path()), Source::Bundled).await?;
    let started = std::time::Instant::now();
    let reply = commands.invoke(calls, interaction()).await?;
    let elapsed = started.elapsed();
    assert_eq!(reply.content, "ran");
    assert!(
        elapsed < super::super::DEADLINE,
        "answered after {elapsed:?}"
    );
    // The optional feedback took none of core's time: it has the command,
    // once, and the processing request it waits behind is still owned.
    assert_eq!(submitted.lock().await.len(), 1);
    assert_eq!(reactions.stop(Duration::from_millis(100)).await, 1);
    server.abort();
    let trace = trace.lock().await;
    assert!(
        trace[0].starts_with("message <@200> asked to run `/compact keep decisions`"),
        "{trace:?}"
    );
    assert!(
        trace.iter().any(|request| request.contains("/reactions/")),
        "{trace:?}"
    );
    Ok(())
}
