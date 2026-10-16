use std::collections::HashMap;

use crate::attachments::tests::{Serve, attachment, serve_cdn};

use super::*;

/// A core for simulation: it accepts events with whatever their staging
/// holds when it captures them, offers each new turn as a delivery, and
/// reports what it holds.
#[derive(Default)]
struct SimulatedCore {
    events: Vec<carillon_protocol::model::ExternalEvent>,
    staged: Vec<Vec<Vec<u8>>>,
    /// Unacknowledged deliveries, oldest first, each with whether this
    /// provider instance holds its claim.
    deliveries: Vec<(carillon_protocol::model::Delivery, bool)>,
    /// Events core is still capturing, for this many more status lookups.
    capturing: HashMap<String, (usize, carillon_protocol::model::ExternalEvent)>,
    /// An event whose next submission core receives but whose answer is lost.
    lose: Option<String>,
}

type SharedCore = Arc<std::sync::Mutex<SimulatedCore>>;

impl SimulatedCore {
    /// Capture `event` from staging as it is now, and keep it.
    fn capture(&mut self, staging: &Path, event: carillon_protocol::model::ExternalEvent) {
        let staged = event.attachments.as_ref().map_or_else(Vec::new, |batch| {
            (0..batch.files.len())
                .map(|ordinal| {
                    std::fs::read(staging.join(&batch.batch).join(ordinal.to_string()))
                        .unwrap_or_default()
                })
                .collect()
        });
        self.staged.push(staged);
        self.events.push(event);
    }

    /// Claim as core does: this instance's own claim first, then the oldest
    /// pending delivery, of `turn` if one is named.
    fn claim(&mut self, turn: Option<&String>) -> Option<carillon_protocol::model::Delivery> {
        let wanted = |held: bool| {
            move |(delivery, claimed): &(carillon_protocol::model::Delivery, bool)| {
                *claimed == held && turn.is_none_or(|turn| *turn == delivery.turn_id)
            }
        };
        let next = self
            .deliveries
            .iter()
            .position(wanted(true))
            .or_else(|| self.deliveries.iter().position(wanted(false)))?;
        self.deliveries[next].1 = true;
        Some(self.deliveries[next].0.clone())
    }

    /// Offer the answer to `event`'s turn for delivery.
    fn answer(&mut self, event: &carillon_protocol::model::ExternalEvent) {
        self.deliveries.push((
            carillon_protocol::model::Delivery {
                turn_state: carillon_protocol::model::TurnState::Completed,
                id: i64::try_from(self.events.len()).unwrap_or(i64::MAX),
                turn_id: format!("turn-{}", event.event_id),
                target_id: event.delivery_target_id.clone(),
                feedback_target_id: event.feedback_target_id.clone(),
                content: "answer".to_owned(),
                origin: carillon_protocol::model::Origin {
                    turn_id: format!("turn-{}", event.event_id),
                    accepted_at: 1_790_000_000,
                },
            },
            false,
        ));
    }
}

async fn simulated_core(staging: PathBuf) -> anyhow::Result<(std::net::SocketAddr, SharedCore)> {
    use axum::extract::{Path as AxumPath, Query, State};
    use carillon_protocol::model::{EventReceipt, EventStatus, ExternalEvent, TurnState};
    let shared = SharedCore::default();
    // A turn core already held has finished.
    let receipt = |event_id: &str, duplicate| EventReceipt {
        event_id: event_id.to_owned(),
        turn_id: format!("turn-{event_id}"),
        state: if duplicate {
            TurnState::Completed
        } else {
            TurnState::Queued
        },
        duplicate,
    };
    let (submitted, looked_up) = (staging.clone(), staging);
    let app = Router::new()
        .route(
            "/v1/events",
            post(
                move |State(core): State<SharedCore>, Json(event): Json<ExternalEvent>| async move {
                    let Ok(mut core) = core.lock() else {
                        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
                    };
                    if core.lose.as_deref() == Some(event.event_id.as_str()) {
                        core.lose = None;
                        core.capturing.insert(event.event_id.clone(), (1, event));
                        return StatusCode::SERVICE_UNAVAILABLE.into_response();
                    }
                    let duplicate = core.events.iter().any(|seen| seen.event_id == event.event_id);
                    if !duplicate {
                        core.capture(&submitted, event.clone());
                        core.answer(&event);
                    }
                    Json(receipt(&event.event_id, duplicate)).into_response()
                },
            ),
        )
        .route(
            "/v1/events/{event_id}",
            get(
                move |State(core): State<SharedCore>, AxumPath(event_id): AxumPath<String>| async move {
                    let Ok(mut core) = core.lock() else {
                        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
                    };
                    if let Some((lookups, _)) = core.capturing.get_mut(&event_id)
                        && *lookups > 0
                    {
                        *lookups -= 1;
                        return Json(EventStatus::Capturing).into_response();
                    }
                    if let Some((_, event)) = core.capturing.remove(&event_id) {
                        core.capture(&looked_up, event);
                    }
                    let known = core.events.iter().any(|event| event.event_id == event_id);
                    Json(if known {
                        EventStatus::Accepted(receipt(&event_id, true))
                    } else {
                        EventStatus::Unknown
                    })
                    .into_response()
                },
            ),
        )
        .route(
            "/v1/deliveries",
            get(
                |State(core): State<SharedCore>,
                 Query(claim): Query<HashMap<String, String>>| async move {
                    let Ok(mut core) = core.lock() else {
                        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
                    };
                    Json(core.claim(claim.get("turn_id"))).into_response()
                },
            ),
        )
        .route(
            "/v1/deliveries/{id}/ack",
            post(
                |State(core): State<SharedCore>, AxumPath(id): AxumPath<i64>| async move {
                    if let Ok(mut core) = core.lock() {
                        core.deliveries.retain(|(delivery, _)| delivery.id != id);
                    }
                    StatusCode::NO_CONTENT
                },
            ),
        )
        // This core records no turn's report: no job earns a card.
        .route("/v1/jobs", get(|| async { Json(json!({"jobs": []})) }))
        .with_state(shared.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let address = listener.local_addr()?;
    tokio::spawn(async move { axum::serve(listener, app).await });
    Ok((address, shared))
}

#[tokio::test]
async fn the_operator_alone_applies_a_settings_file_whole_or_not_at_all() -> anyhow::Result<()> {
    let directory = tempfile::tempdir()?;
    let state = discord_state(
        core_client("http://127.0.0.1:1".to_owned())?,
        directory.path(),
    )
    .await?;
    let serving = state.settings.current();
    let (status, body) = settings(&state, Method::GET, "operator").await?;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        body["file"],
        json!({"state": "read", "live": [], "restart": []})
    );
    assert_eq!(body["serving"]["allowed_user_ids"], json!(["200"]));
    // Neither anyone else nor the interface's own credential reads or
    // applies settings.
    edit_settings(&state, |file| file.allowed_user_ids.push("250".to_owned()))?;
    for bearer in ["", "wrong", "token"] {
        for method in [Method::GET, Method::POST] {
            let (status, _) = settings(&state, method, bearer).await?;
            assert_eq!(status, StatusCode::FORBIDDEN);
        }
    }
    assert!(Arc::ptr_eq(&state.settings.current(), &serving));
    // A file that also changes the provider's identity is refused whole,
    // naming what only a restart applies.
    edit_settings(&state, |file| {
        file.allowed_user_ids.push("250".to_owned());
        "999".clone_into(&mut file.guild_id);
    })?;
    let (status, body) = settings(&state, Method::POST, "operator").await?;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(
        body["error"],
        "the settings file changes what only a restart applies (guild_id); nothing was applied"
    );
    assert_eq!(body["restart"], json!(["guild_id"]));
    assert!(Arc::ptr_eq(&state.settings.current(), &serving));
    let (_, body) = settings(&state, Method::GET, "operator").await?;
    assert_eq!(
        body["file"],
        json!({"state": "read", "live": ["allowed_user_ids"], "restart": ["guild_id"]})
    );
    // A file that is not valid settings, or is gone, is refused.
    let path = state.installation.provider_config_path("discord")?;
    std::fs::write(&path, "allowed_user_ids = [")?;
    let (status, body) = settings(&state, Method::POST, "operator").await?;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    let error = body["error"].as_str().context("no error")?;
    assert!(
        error.starts_with("the settings file is not valid: "),
        "{error}"
    );
    assert!(error.ends_with("; nothing was applied"), "{error}");
    // Only a refusal of what a restart applies says so.
    assert!(body.get("restart").is_none(), "{body}");
    let (_, body) = settings(&state, Method::GET, "operator").await?;
    assert_eq!(body["file"]["state"], "invalid");
    std::fs::remove_file(&path)?;
    let (status, _) = settings(&state, Method::POST, "operator").await?;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert!(Arc::ptr_eq(&state.settings.current(), &serving));
    // A file whose changes all apply live serves whole, once.
    edit_settings(&state, |file| {
        file.allowed_user_ids.push("250".to_owned());
        "⏳".clone_into(&mut file.processing_emoji);
    })?;
    let (status, body) = settings(&state, Method::POST, "operator").await?;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        body["changed"],
        json!(["allowed_user_ids", "processing_emoji"])
    );
    let applied = state.settings.current();
    assert_eq!(applied.allowed_user_ids, ["200", "250"]);
    assert_eq!(applied.processing_emoji, "⏳");
    let (_, body) = settings(&state, Method::POST, "operator").await?;
    assert_eq!(body["changed"], json!([]));
    // What the endpoint shows holds no credential.
    let (_, body) = settings(&state, Method::GET, "operator").await?;
    assert!(!body.to_string().contains("token"), "{body}");
    Ok(())
}

#[tokio::test]
async fn a_misspelt_setting_is_refused_rather_than_dropped() -> anyhow::Result<()> {
    let directory = tempfile::tempdir()?;
    let state = discord_state(
        core_client("http://127.0.0.1:1".to_owned())?,
        directory.path(),
    )
    .await?;
    let serving = state.settings.current();
    let path = state.installation.provider_config_path("discord")?;
    let file = std::fs::read_to_string(&path)?;
    // A setting the file does not define, at the top or in a route.
    for (misspelt, field) in [
        (
            format!("processing_emoj = \"⏳\"\n{file}"),
            "processing_emoj",
        ),
        (
            file.replace("[[routes]]\n", "[[routes]]\nchanel_id = \"301\"\n"),
            "chanel_id",
        ),
    ] {
        assert_ne!(misspelt, file);
        std::fs::write(&path, misspelt)?;
        let (status, body) = settings(&state, Method::POST, "operator").await?;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
        let error = body["error"].as_str().context("no error")?;
        assert!(
            error.contains(&format!("unknown field `{field}`")),
            "{error}"
        );
        assert!(Arc::ptr_eq(&state.settings.current(), &serving));
        let (_, body) = settings(&state, Method::GET, "operator").await?;
        assert_eq!(body["file"]["state"], "invalid");
    }
    Ok(())
}

#[tokio::test]
async fn each_message_is_admitted_by_the_settings_serving_when_it_arrives() -> anyhow::Result<()> {
    let directory = tempfile::tempdir()?;
    let (core_address, _core) = simulated_core(directory.path().join("staging")).await?;
    let state = discord_state(
        core_client(format!("http://{core_address}"))?,
        directory.path(),
    )
    .await?;
    let from = |id: &str, user: &str, channel: &str| {
        json!({
            "event_id": id, "guild_id": "100", "user_id": user, "channel_id": channel,
            "content": "hello", "attachments": [],
        })
    };
    // Neither the person nor the channel is admitted yet.
    let (status, _) = simulate_message(&state, &from("1", "250", "300")).await?;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (status, _) = simulate_message(&state, &from("2", "200", "301")).await?;
    assert_eq!(status, StatusCode::FORBIDDEN);
    edit_settings(&state, |file| {
        file.allowed_user_ids.push("250".to_owned());
        let mut route = file.routes[0].clone();
        "301".clone_into(&mut route.channel_id);
        file.routes.push(route);
        file.reactions = false;
    })?;
    let (status, _) = settings(&state, Method::POST, "operator").await?;
    assert_eq!(status, StatusCode::OK);
    // The next messages are, and are shown as the applied settings say.
    let (status, body) = simulate_message(&state, &from("3", "250", "300")).await?;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["feedback"], json!([]));
    let (status, body) = simulate_message(&state, &from("4", "200", "301")).await?;
    assert_eq!(status, StatusCode::OK, "{body}");
    Ok(())
}

/// Send `files` from `user` through deterministic simulation.
async fn simulate(
    state: &DiscordState,
    id: &str,
    user: &str,
    files: &[crate::conversations::SourceAttachment],
) -> anyhow::Result<(StatusCode, Value)> {
    simulate_message(
        state,
        &json!({
            "event_id": id, "guild_id": "100", "user_id": user, "channel_id": "300",
            "content": "", "attachments": files,
        }),
    )
    .await
}

async fn simulate_message(
    state: &DiscordState,
    message: &Value,
) -> anyhow::Result<(StatusCode, Value)> {
    let request = Request::builder()
        .method(Method::POST)
        .uri("/v1/simulate")
        .header("authorization", "Bearer token")
        .header("content-type", "application/json")
        .body(Body::from(serde_json::to_vec(message)?))?;
    let response = discord_router(state.clone(), true).oneshot(request).await?;
    let status = response.status();
    let body = axum::body::to_bytes(response.into_body(), usize::MAX).await?;
    let body = if body.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&body)?
    };
    Ok((status, body))
}

#[tokio::test]
async fn simulation_admits_then_downloads_files_under_the_provider_policy() -> anyhow::Result<()> {
    let directory = tempfile::tempdir()?;
    let staging = directory.path().join("staging");
    let (cdn, log) = serve_cdn(HashMap::from([
        (
            "/attachments/300/601/a.png".to_owned(),
            Serve::Bytes(vec![7; 32]),
        ),
        (
            "/attachments/300/602/gone.png".to_owned(),
            Serve::Status(404),
        ),
    ]))
    .await?;
    let (core_address, core) = simulated_core(staging.clone()).await?;
    let mut state = discord_state(
        core_client(format!("http://{core_address}"))?,
        directory.path(),
    )
    .await?;
    state.downloads = Arc::new(
        Downloads::open(staging.clone(), state.settings.current().attachments)?
            .with_fixture_cdn(cdn),
    );
    let requests = || log.lock().map_or(usize::MAX, |log| log.requests.len());
    let file = attachment(cdn, "601", "a.png", 32);

    // Admission comes first: a sender who is not allowed, or a message that
    // would not activate, causes no fetch.
    let (status, _) = simulate(&state, "1", "999", std::slice::from_ref(&file)).await?;
    assert_eq!(status, StatusCode::FORBIDDEN);
    restarted(&mut state, |settings| {
        settings.routes[0].activation = DiscordActivation::Mention;
    });
    let (status, _) = simulate(&state, "1", "200", std::slice::from_ref(&file)).await?;
    assert_eq!(status, StatusCode::NO_CONTENT);
    restarted(&mut state, |settings| {
        settings.routes[0].activation = DiscordActivation::Always;
    });
    assert_eq!(requests(), 0);
    // The provider's address policy refuses before any request.
    let mut elsewhere = file.clone();
    "https://example.com/attachments/300/601/a.png".clone_into(&mut elsewhere.url);
    let (status, body) = simulate(&state, "2", "200", &[elsewhere]).await?;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert!(
        body["error"]
            .as_str()
            .is_some_and(|error| error.contains("not a Discord attachment"))
    );
    assert_eq!(requests(), 0);
    // Without a Discord session an expired address is not refreshed.
    let (status, body) =
        simulate(&state, "3", "200", &[attachment(cdn, "602", "gone.png", 4)]).await?;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert!(
        body["error"]
            .as_str()
            .is_some_and(|error| error.contains("no longer serves"))
    );

    // An admitted message's files are staged for core, which takes them whole.
    let (status, body) = simulate(&state, "4", "200", std::slice::from_ref(&file)).await?;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["receipt"]["duplicate"], false);
    {
        let core = core.lock().map_err(|_| anyhow::anyhow!("poisoned"))?;
        let [event] = core.events.as_slice() else {
            anyhow::bail!("expected one accepted event");
        };
        let batch = event.attachments.as_ref().context("no manifest")?;
        assert_eq!(batch.files[0].name, "a.png");
        assert_eq!(core.staged[0], vec![vec![7; 32]]);
    }
    for _ in 0..200 {
        if std::fs::read_dir(&staging)?.count() == 0 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    assert_eq!(std::fs::read_dir(&staging)?.count(), 0);
    let fetched = requests();
    // A message core already holds is not fetched again.
    let (status, body) = simulate(&state, "4", "200", std::slice::from_ref(&file)).await?;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["receipt"]["duplicate"], true);
    assert_eq!(requests(), fetched);
    // Nor is one core is still capturing: simulation waits for it.
    let mut replay = core.lock().map_err(|_| anyhow::anyhow!("poisoned"))?.events[0].clone();
    "5".clone_into(&mut replay.event_id);
    core.lock()
        .map_err(|_| anyhow::anyhow!("poisoned"))?
        .capturing
        .insert("5".to_owned(), (1, replay));
    let (status, body) = simulate(&state, "5", "200", std::slice::from_ref(&file)).await?;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["receipt"]["event_id"], "5");
    assert_eq!(requests(), fetched);
    // When core's answer is lost while it captures, the staged files stay
    // until core settles the submission.
    core.lock().map_err(|_| anyhow::anyhow!("poisoned"))?.lose = Some("6".to_owned());
    let (status, body) = simulate(&state, "6", "200", &[file]).await?;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["receipt"]["event_id"], "6");
    let core = core.lock().map_err(|_| anyhow::anyhow!("poisoned"))?;
    assert_eq!(
        core.events.last().map(|event| event.event_id.as_str()),
        Some("6")
    );
    assert_eq!(core.staged.last(), Some(&vec![vec![7; 32]]));
    Ok(())
}

#[tokio::test]
async fn simulation_settles_its_own_turns_delivery_and_no_other() -> anyhow::Result<()> {
    let directory = tempfile::tempdir()?;
    let (core_address, core) = simulated_core(directory.path().join("staging")).await?;
    let state = discord_state(
        core_client(format!("http://{core_address}"))?,
        directory.path(),
    )
    .await?;
    // Earlier simulations of these were accepted, but their turns finished
    // only after the requests stopped waiting: both answers are pending, the
    // abandoned one oldest.
    {
        let mut core = core.lock().map_err(|_| anyhow::anyhow!("poisoned"))?;
        for id in ["abandoned", "late"] {
            let event = carillon_protocol::model::ExternalEvent {
                event_id: id.to_owned(),
                agent_id: "default".to_owned(),
                user_id: "200".to_owned(),
                audience_id: "discord:channel:300".to_owned(),
                delivery_target_id: "300".to_owned(),
                feedback_target_id: format!("300:{id}"),
                content: String::new(),
                native: Value::Null,
                attachments: None,
            };
            core.events.push(event.clone());
            core.answer(&event);
        }
    }
    let text = |id: &str| {
        json!({
            "event_id": id, "guild_id": "100", "user_id": "200", "channel_id": "300",
            "content": "hello", "attachments": [],
        })
    };
    // Sending one again delivers its answer, once.
    let (status, body) = simulate_message(&state, &text("late")).await?;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["receipt"]["duplicate"], true);
    assert_eq!(body["delivery"]["turn_id"], "turn-late");
    let (status, body) = simulate_message(&state, &text("late")).await?;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["delivery"], Value::Null);
    // A new message gets its own answer, not a stale one.
    let (status, body) = simulate_message(&state, &text("next")).await?;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["delivery"]["turn_id"], "turn-next");
    // Nobody claimed the abandoned turn's answer.
    let core = core.lock().map_err(|_| anyhow::anyhow!("poisoned"))?;
    assert!(
        matches!(
            core.deliveries.as_slice(),
            [(delivery, false)] if delivery.turn_id == "turn-abandoned"
        ),
        "{:?}",
        core.deliveries
    );
    Ok(())
}
