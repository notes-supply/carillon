use crate::conversations::tests::{discord_config, interaction};
use std::{
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicU16, AtomicUsize, Ordering},
    },
    time::Duration,
};

use anyhow::{Context as _, anyhow};
use axum::{
    Json, Router,
    body::Body,
    http::{Method, Request, StatusCode, Uri},
    response::IntoResponse as _,
    routing::{get, post},
};
use carillon_discord_commands::contract::{InteractionKind, Reply};
use carillon_installation::config::{LoadedConfig, hash_token};
use carillon_protocol::model::{
    DeliveryOutcome, DeliveryReconciliation, EventReceipt, ExternalEvent, Health, TurnState,
};
use serde_json::{Value, json};
use tower::ServiceExt as _;
use twilight_http::response::StatusCode as DiscordStatusCode;

use super::{
    DiscordState, IncomingMessage, InteractionSource, MessagePlan, Participation, QueuedMessage,
    ServeOptions, discord_chunks, discord_router, discord_status_is_terminal, interaction_source,
    queue_message, send_discord_message, submit_event_with_retry,
};
use crate::{
    CoreClient,
    client::test_client as core_client,
    commands::{
        Commands,
        tests::{app, definition, describing, registration_server, replying, write_module},
    },
    config::{DiscordActivation, DiscordConversation},
};

/// A command module in `directory` that defines `/model` and answers
/// every interaction with `reply`.
async fn stub_commands(directory: &Path, reply: &str) -> anyhow::Result<Arc<Commands>> {
    let module = directory.join("commands.wasm");
    write_module(&module, &describing(&[definition("model")]), reply)?;
    Ok(Arc::new(
        Commands::load(module, directory.join("commands.json")).await?,
    ))
}

async fn discord_state(core: CoreClient, directory: &Path) -> anyhow::Result<DiscordState> {
    Ok(DiscordState {
        discord: discord_config(DiscordActivation::Always, DiscordConversation::Channel),
        health: Health {
            id: "discord".to_owned(),
            role: "interface".to_owned(),
            version: "test".to_owned(),
            status: "ready".to_owned(),
        },
        core,
        participation: Arc::new(Participation::load(directory.join("participation.json"))?),
        commands: stub_commands(directory, &replying("stub reply", &[])).await?,
        app: None,
        operator_token_hash: hash_token("operator"),
    })
}

/// An installation whose interface points at `provider_address` and whose
/// core is the fake at `core_address`, plus the token file and command
/// module `serve` needs.
fn served_installation(
    root: &Path,
    core_address: std::net::SocketAddr,
    provider_address: std::net::SocketAddr,
) -> anyhow::Result<(LoadedConfig, PathBuf, PathBuf)> {
    let mut config = crate::config::tests::installation(root, "discord-private", 45_200)?;
    config.value.core.listen = core_address;
    config
        .value
        .providers
        .iter_mut()
        .find(|provider| provider.id == "discord-private")
        .context("missing interface")?
        .url = format!("http://{provider_address}");
    let module = root.join("commands.wasm");
    write_module(
        &module,
        &describing(&[definition("model")]),
        &replying("unused", &[]),
    )?;
    let token_file = root.join("credentials").join("discord-private.token");
    Ok((config, token_file, module))
}

fn deterministic_options<'a>(token_file: &'a Path, module: &'a Path) -> ServeOptions<'a> {
    ServeOptions {
        provider_id: "discord-private",
        token_file,
        discord_token_file: None,
        commands: module,
        deterministic: true,
    }
}

#[tokio::test]
async fn startup_reconciliation_refuses_permanent_errors_and_recovers_transient_errors()
-> anyhow::Result<()> {
    for status in [
        StatusCode::FORBIDDEN,
        StatusCode::NOT_FOUND,
        StatusCode::BAD_GATEWAY,
        StatusCode::SERVICE_UNAVAILABLE,
    ] {
        let directory = tempfile::tempdir()?;
        let core_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let provider_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let provider_address = provider_listener.local_addr()?;
        drop(provider_listener);
        let (config, token_file, module) = served_installation(
            &directory.path().join("installation"),
            core_listener.local_addr()?,
            provider_address,
        )?;
        let attempts = Arc::new(AtomicUsize::new(0));
        let request_attempts = attempts.clone();
        let app = Router::new().route(
            "/v1/deliveries/reconcile",
            post(move || {
                let attempt = request_attempts.fetch_add(1, Ordering::SeqCst);
                async move {
                    if attempt == 0 {
                        status.into_response()
                    } else {
                        Json(DeliveryReconciliation {
                            feedback_target_ids: Vec::new(),
                        })
                        .into_response()
                    }
                }
            }),
        );
        let mut services = tokio::task::JoinSet::new();
        services.spawn(async move { axum::serve(core_listener, app).await.map_err(Into::into) });
        if status != StatusCode::SERVICE_UNAVAILABLE {
            let result = tokio::time::timeout(
                Duration::from_secs(2),
                super::serve(config.clone(), deterministic_options(&token_file, &module)),
            )
            .await?;
            assert!(
                result
                    .err()
                    .context("permanent reconciliation failure accepted")?
                    .to_string()
                    .contains("delivery reconciliation rejected")
            );
            assert_eq!(attempts.load(Ordering::SeqCst), 1);
            // Failed startup releases the socket; repairing core permits the same config.
            let probe = tokio::net::TcpListener::bind(provider_address).await?;
            drop(probe);
        }
        services.spawn(async move {
            super::serve(config, deterministic_options(&token_file, &module)).await
        });
        // A connection can be refused before serve binds. Yield only on that
        // boundary; a connected request waits for reconciliation and health.
        let health = tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                match reqwest::get(format!("http://{provider_address}/health")).await {
                    Ok(response) => break response.json::<Health>().await,
                    Err(error) if error.is_connect() => tokio::task::yield_now().await,
                    Err(error) => break Err(error),
                }
            }
        })
        .await??;
        assert_eq!(health.status, "ready");
        assert_eq!(attempts.load(Ordering::SeqCst), 2);
    }
    Ok(())
}

#[tokio::test]
async fn occupied_listener_fails_before_touching_delivery_claims() -> anyhow::Result<()> {
    for deterministic in [true, false] {
        let directory = tempfile::tempdir()?;
        let core_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let occupant = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let (config, token_file, module) = served_installation(
            &directory.path().join("installation"),
            core_listener.local_addr()?,
            occupant.local_addr()?,
        )?;
        let reconciles = Arc::new(AtomicUsize::new(0));
        let counted = reconciles.clone();
        let app = Router::new().route(
            "/v1/deliveries/reconcile",
            post(move || {
                counted.fetch_add(1, Ordering::SeqCst);
                async {
                    Json(DeliveryReconciliation {
                        feedback_target_ids: Vec::new(),
                    })
                }
            }),
        );
        let core = tokio::spawn(async move { axum::serve(core_listener, app).await });
        let discord_token_file = directory.path().join("missing-discord.token");
        let failure = tokio::time::timeout(
            Duration::from_secs(2),
            super::serve(
                config,
                ServeOptions {
                    provider_id: "discord-private",
                    token_file: &token_file,
                    discord_token_file: Some(&discord_token_file),
                    commands: &module,
                    deterministic,
                },
            ),
        )
        .await?
        .err()
        .context("occupied listener was accepted")?;
        core.abort();
        assert!(
            failure.chain().any(|cause| cause
                .to_string()
                .contains("bind Discord interface provider")),
            "{failure:#}"
        );
        assert_eq!(reconciles.load(Ordering::SeqCst), 0);
    }
    Ok(())
}

#[tokio::test]
async fn core_requests_timeout_on_stalled_headers_and_bodies() -> anyhow::Result<()> {
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

    let mut cases = tokio::task::JoinSet::new();
    for operation in 0..4 {
        for partial_body in [false, true] {
            // Acknowledgements need only a success status, not a response body.
            if operation == 3 && partial_body {
                continue;
            }
            cases.spawn(async move {
                let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
                let client = core_client(format!("http://{}", listener.local_addr()?));
                let mut services = tokio::task::JoinSet::new();
                services.spawn(async move {
                    let (mut connection, _) = listener.accept().await?;
                    let mut request = Vec::new();
                    let mut buffer = [0; 1024];
                    while !request.windows(4).any(|bytes| bytes == b"\r\n\r\n") {
                        let length = connection.read(&mut buffer).await?;
                        anyhow::ensure!(length > 0, "request disconnected early");
                        request.extend_from_slice(&buffer[..length]);
                    }
                    if partial_body {
                        connection
                            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 100\r\n\r\n{")
                            .await?;
                    }
                    std::future::pending::<()>().await;
                    Ok::<_, anyhow::Error>(())
                });
                let started = tokio::time::Instant::now();
                let result = tokio::time::timeout(Duration::from_secs(15), async {
                    match operation {
                        0 => client
                            .submit_event(&ExternalEvent {
                                event_id: "timeout".to_owned(),
                                agent_id: "agent".to_owned(),
                                user_id: "user".to_owned(),
                                audience_id: "conversation".to_owned(),
                                delivery_target_id: "target".to_owned(),
                                feedback_target_id: "feedback".to_owned(),
                                content: "hello".to_owned(),
                                native: Value::Null,
                            })
                            .await
                            .map(|_| ()),
                        1 => client.claim_delivery().await.map(|_| ()),
                        2 => client.reconcile().await.map(|_| ()),
                        _ => {
                            client
                                .acknowledge_delivery(1, DeliveryOutcome::Delivered, None)
                                .await
                        }
                    }
                })
                .await?;
                let failure = result.err().context("stalled response was accepted")?;
                assert!(failure.is_transient());
                assert!(started.elapsed() >= Duration::from_secs(10));
                assert!(
                    services.try_join_next().is_none(),
                    "server failed instead of stalling"
                );
                Ok::<_, anyhow::Error>(())
            });
        }
    }
    let mut count = 0;
    while let Some(result) = cases.join_next().await {
        result??;
        count += 1;
    }
    assert_eq!(count, 7);
    Ok(())
}

#[tokio::test]
async fn model_calls_relay_rejections_in_the_runtime_words() -> anyhow::Result<()> {
    let app = Router::new().route(
        "/v1/model",
        get(|| async {
            (
                StatusCode::CONFLICT,
                Json(json!({"error": "The runtime provider is disabled."})),
            )
        })
        .put(|| async { (StatusCode::OK, "{not json") }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let address = listener.local_addr()?;
    let server = tokio::spawn(async move { axum::serve(listener, app).await });
    let core = core_client(format!("http://{address}"));
    let query = carillon_protocol::model::ModelQuery {
        agent_id: "default".to_owned(),
        audience_id: None,
    };
    assert_eq!(
        core.model_state(&query).await.err().as_deref(),
        Some("The runtime provider is disabled.")
    );
    let update = carillon_protocol::model::ModelUpdate {
        agent_id: "default".to_owned(),
        audience_id: None,
        scope: carillon_protocol::model::ModelScope::Agent,
        selection: None,
        expected: None,
    };
    assert_eq!(
        core.set_model(&update).await.err().as_deref(),
        Some("The runtime reported an unusable model state.")
    );
    server.abort();
    let unreachable = core_client("http://127.0.0.1:1".to_owned());
    assert_eq!(
        unreachable.model_state(&query).await.err().as_deref(),
        Some("The runtime is not reachable right now.")
    );
    Ok(())
}

#[tokio::test]
async fn ambiguous_thread_creation_recovers_native_thread_before_retrying() -> anyhow::Result<()> {
    let directory = tempfile::tempdir()?;
    let mut state = discord_state(
        core_client("http://127.0.0.1:1".to_owned()),
        directory.path(),
    )
    .await?;
    state.participation = Arc::new(Participation::load(
        directory
            .path()
            .join("components/custom-discord/participation.json"),
    )?);
    let plan = MessagePlan {
        message_id: "200".to_owned(),
        guild_id: "1".to_owned(),
        user_id: "2".to_owned(),
        agent_id: "agent".to_owned(),
        channel_id: "100".to_owned(),
        route_channel_id: "100".to_owned(),
        content: "hello".to_owned(),
        create_thread: true,
        adopt_thread: false,
        native: Value::Null,
    };
    let creates = Arc::new(AtomicUsize::new(0));
    let get_creates = creates.clone();
    let post_creates = creates.clone();
    let app = Router::new()
            .route("/api/v10/channels/200", get(move || {
                let exists = get_creates.load(Ordering::SeqCst) > 0;
                async move {
                    if exists {
                        Json(serde_json::json!({"id":"200", "type":11, "parent_id":"100"})).into_response()
                    } else {
                        (StatusCode::NOT_FOUND, Json(serde_json::json!({"code":10003, "message":"Unknown Channel"}))).into_response()
                    }
                }
            }))
            .route("/api/v10/channels/100/messages/200/threads", post(move || {
                let attempt = post_creates.fetch_add(1, Ordering::SeqCst);
                async move {
                    if attempt == 0 {
                        // Discord committed the thread, but its successful response is undecodable.
                        (StatusCode::CREATED, "{broken response").into_response()
                    } else {
                        (StatusCode::BAD_REQUEST, Json(serde_json::json!({"code":160_004, "message":"A thread has already been created for this message"}))).into_response()
                    }
                }
            }));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let http = twilight_http::Client::builder()
        .token("test-only".to_owned())
        .proxy(listener.local_addr()?.to_string(), true)
        .ratelimiter(None)
        .build();
    let mut services = tokio::task::JoinSet::new();
    services.spawn(async move { axum::serve(listener, app).await });
    tokio::time::timeout(Duration::from_secs(3), async {
        let failure =
            super::prepare_discord_conversation(&state.participation, &http, plan.clone())
                .await
                .err()
                .context("undecodable create should fail")?;
        assert!(!super::discord_failure_is_terminal(&failure));
        assert_eq!(state.participation.created_thread("200"), None);
        let recovered =
            super::prepare_discord_conversation(&state.participation, &http, plan.clone()).await?;
        assert_eq!(recovered.event.event_id, "200");
        assert_eq!(recovered.event.delivery_target_id, "200");
        assert_eq!(recovered.event.audience_id, "discord-1-200");
        state.participation = Arc::new(Participation::load(
            directory
                .path()
                .join("components/custom-discord/participation.json"),
        )?);
        assert_eq!(
            state.participation.created_thread("200").as_deref(),
            Some("200")
        );
        assert_eq!(
            state.participation.parent_route("200").as_deref(),
            Some("100")
        );
        super::prepare_discord_conversation(&state.participation, &http, plan).await?;
        assert_eq!(creates.load(Ordering::SeqCst), 1);
        Ok::<_, anyhow::Error>(())
    })
    .await??;
    services.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn thread_lookup_failures_do_not_speculatively_create() -> anyhow::Result<()> {
    for status in [StatusCode::FORBIDDEN, StatusCode::SERVICE_UNAVAILABLE] {
        let creates = Arc::new(AtomicUsize::new(0));
        let seen = creates.clone();
        let app = Router::new().fallback(move |method: Method| {
            if method == Method::POST {
                seen.fetch_add(1, Ordering::SeqCst);
            }
            async move {
                (
                    status,
                    Json(serde_json::json!({"code":50001, "message":"Unavailable"})),
                )
            }
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let http = twilight_http::Client::builder()
            .token("test-only".to_owned())
            .proxy(listener.local_addr()?.to_string(), true)
            .ratelimiter(None)
            .build();
        let mut services = tokio::task::JoinSet::new();
        services.spawn(async move { axum::serve(listener, app).await });
        let plan = MessagePlan {
            message_id: "200".to_owned(),
            guild_id: "1".to_owned(),
            user_id: "2".to_owned(),
            agent_id: "agent".to_owned(),
            channel_id: "100".to_owned(),
            route_channel_id: "100".to_owned(),
            content: "hello".to_owned(),
            create_thread: true,
            adopt_thread: false,
            native: Value::Null,
        };
        let failure = tokio::time::timeout(
            Duration::from_secs(3),
            crate::conversations::create_conversation_thread(&http, &plan),
        )
        .await?
        .err()
        .context("lookup should fail")?;
        assert_eq!(
            super::discord_failure_is_terminal(&failure),
            status == StatusCode::FORBIDDEN
        );
        assert_eq!(creates.load(Ordering::SeqCst), 0);
        services.shutdown().await;
    }
    Ok(())
}

#[tokio::test]
async fn production_discord_does_not_expose_deterministic_endpoints() -> anyhow::Result<()> {
    let directory = tempfile::tempdir()?;
    let state = discord_state(
        core_client("http://127.0.0.1:1".to_owned()),
        directory.path(),
    )
    .await?;
    for path in ["/v1/simulate", "/v1/interactions"] {
        let mut request = Request::new(Body::empty());
        *request.method_mut() = Method::POST;
        *request.uri_mut() = Uri::from_static(path);
        let response = discord_router(state.clone(), false)
            .oneshot(request)
            .await?;
        assert_eq!(response.status(), StatusCode::NOT_FOUND, "{path}");
    }
    Ok(())
}

#[test]
fn gateway_interactions_map_to_their_source() -> anyhow::Result<()> {
    let base = json!({
        "id": "1",
        "application_id": "2",
        "authorizing_integration_owners": {},
        "token": "t",
        "version": 1,
        "guild_id": "100",
        "member": {
            "user": {"id": "200", "username": "u", "discriminator": "0", "avatar": null},
            "roles": [],
            "joined_at": null,
            "deaf": false,
            "mute": false,
            "flags": 0
        }
    });
    let mut command = base.clone();
    command["type"] = json!(2);
    command["channel"] = json!({"id": "400", "type": 11, "parent_id": "300"});
    command["data"] = json!({
        "id": "3",
        "name": "model",
        "type": 1,
        "options": [{"name": "scope", "type": 3, "value": "agent"}]
    });
    let source =
        interaction_source(&serde_json::from_value(command)?).context("command was not mapped")?;
    assert_eq!(source.kind, InteractionKind::Command);
    assert_eq!(source.name, "model");
    assert_eq!(source.guild_id.as_deref(), Some("100"));
    assert_eq!(source.user_id, "200");
    assert_eq!(source.channel_id, "400");
    assert_eq!(source.parent_channel_id.as_deref(), Some("300"));
    assert_eq!(source.options[0]["name"], "scope");

    let mut component = base.clone();
    component["type"] = json!(3);
    // A text channel's parent is a category, not a route.
    component["channel"] = json!({"id": "300", "type": 0, "parent_id": "50"});
    component["data"] = json!({
        "custom_id": "model:pick",
        "component_type": 3,
        "values": ["provider/model"]
    });
    let source = interaction_source(&serde_json::from_value(component)?)
        .context("component was not mapped")?;
    assert_eq!(source.kind, InteractionKind::Component);
    assert_eq!(source.name, "model:pick");
    assert_eq!(source.values, vec!["provider/model"]);
    assert_eq!(source.parent_channel_id, None);

    let mut ping = base;
    ping["type"] = json!(1);
    assert!(interaction_source(&serde_json::from_value(ping)?).is_none());
    Ok(())
}

async fn interact(
    state: &DiscordState,
    bearer: Option<&str>,
    source: &InteractionSource,
) -> anyhow::Result<(StatusCode, Value)> {
    let mut request = Request::builder()
        .method(Method::POST)
        .uri("/v1/interactions")
        .header("content-type", "application/json");
    if let Some(bearer) = bearer {
        request = request.header("authorization", format!("Bearer {bearer}"));
    }
    let response = discord_router(state.clone(), true)
        .oneshot(request.body(Body::from(serde_json::to_vec(source)?))?)
        .await?;
    let status = response.status();
    let body = axum::body::to_bytes(response.into_body(), usize::MAX).await?;
    Ok((status, serde_json::from_slice(&body)?))
}

#[test]
fn oversized_reply_content_is_rejected_before_the_discord_edit() {
    let mut reply = carillon_discord_commands::contract::Reply {
        content: "🦀".repeat(2000),
        components: vec![],
    };
    assert!(super::validate_reply(&reply).is_ok());
    reply.content.push('x');
    assert!(super::validate_reply(&reply).is_err());
}

#[test]
fn native_selects_need_no_string_options() -> anyhow::Result<()> {
    for kind in [5, 6, 7, 8] {
        let reply = carillon_discord_commands::contract::Reply {
            content: "Choose".to_owned(),
            components: vec![json!({
                "type": 1,
                "components": [{"type": kind, "custom_id": "native", "min_values": 1, "max_values": 1}]
            })],
        };
        assert_eq!(super::validate_reply(&reply)?.len(), 1, "type {kind}");
    }
    for options in [Value::Null, json!([])] {
        let reply = carillon_discord_commands::contract::Reply {
            content: "Choose".to_owned(),
            components: vec![json!({
                "type": 1, "components": [{"type": 3, "custom_id": "string", "options": options}]
            })],
        };
        assert!(super::validate_reply(&reply).is_err());
    }
    Ok(())
}

#[tokio::test]
async fn deterministic_interactions_run_the_module_and_check_replies() -> anyhow::Result<()> {
    let directory = tempfile::tempdir()?;
    let mut state = discord_state(
        core_client("http://127.0.0.1:1".to_owned()),
        directory.path(),
    )
    .await?;
    let button = json!({"type": 1, "components": [{"type": 2, "style": 2, "custom_id": "model:refresh", "label": "Refresh"}]});
    state.commands = stub_commands(
        directory.path(),
        &replying("Current model", std::slice::from_ref(&button)),
    )
    .await?;

    let (status, body) = interact(&state, Some("token"), &interaction("300", None)).await?;
    assert_eq!(status, StatusCode::OK, "{body}");
    let reply: Reply = serde_json::from_value(body)?;
    assert_eq!(reply.content, "Current model");
    assert_eq!(reply.components, vec![button.clone()]);

    let (status, _) = interact(&state, None, &interaction("300", None)).await?;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (status, body) = interact(&state, Some("token"), &interaction("301", None)).await?;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(
        body["error"],
        "channel is not configured as a Discord route"
    );

    // Replies Discord would refuse are reported instead of sent.
    let option =
        |index: usize| json!({"label": format!("Option {index}"), "value": index.to_string()});
    let select = |options: Vec<Value>| json!({"type": 1, "components": [{"type": 3, "custom_id": "model:model", "options": options}]});
    for (components, complaint) in [
        (vec![json!("not a component")], "invalid components"),
        (vec![button.clone(); 6], "action rows"),
        (
            vec![select((0..26).map(option).collect())],
            "unusable component",
        ),
        (
            vec![
                json!({"type": 1, "components": [{"type": 2, "style": 2, "custom_id": "x".repeat(101), "label": "X"}]}),
            ],
            "unusable component",
        ),
        (vec![select(Vec::new())], "without options"),
        (
            vec![json!({"type": 1, "components": []})],
            "empty action row",
        ),
    ] {
        state.commands = stub_commands(directory.path(), &replying("bad", &components)).await?;
        let (status, body) = interact(&state, Some("token"), &interaction("300", None)).await?;
        assert_eq!(status, StatusCode::BAD_GATEWAY, "{body}");
        assert!(
            body["error"]
                .as_str()
                .is_some_and(|error| error.contains(complaint)),
            "{body}"
        );
    }
    state.commands = stub_commands(
        directory.path(),
        &replying("full", &[select((0..25).map(option).collect())]),
    )
    .await?;
    let (status, body) = interact(&state, Some("token"), &interaction("300", None)).await?;
    assert_eq!(status, StatusCode::OK, "{body}");

    state.commands = stub_commands(directory.path(), r#"{"Failure":"no such command"}"#).await?;
    let (status, body) = interact(&state, Some("token"), &interaction("300", None)).await?;
    assert_eq!(status, StatusCode::BAD_GATEWAY);
    assert!(
        body["error"]
            .as_str()
            .is_some_and(|error| error.contains("no such command"))
    );
    Ok(())
}

async fn reload(state: &DiscordState, bearer: Option<&str>) -> anyhow::Result<(StatusCode, Value)> {
    let mut request = Request::builder()
        .method(Method::POST)
        .uri("/v1/commands/reload");
    if let Some(bearer) = bearer {
        request = request.header("authorization", format!("Bearer {bearer}"));
    }
    let response = discord_router(state.clone(), false)
        .oneshot(request.body(Body::empty())?)
        .await?;
    let status = response.status();
    let body = axum::body::to_bytes(response.into_body(), usize::MAX).await?;
    Ok((status, serde_json::from_slice(&body)?))
}

#[tokio::test]
async fn command_reload_requires_the_operator_credential() -> anyhow::Result<()> {
    let directory = tempfile::tempdir()?;
    let state = discord_state(
        core_client("http://127.0.0.1:1".to_owned()),
        directory.path(),
    )
    .await?;
    let module = directory.path().join("commands.wasm");
    for bearer in [None, Some("token"), Some("wrong")] {
        let (status, _) = reload(&state, bearer).await?;
        assert_eq!(status, StatusCode::FORBIDDEN, "{bearer:?}");
    }
    let (status, body) = reload(&state, Some("operator")).await?;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        body,
        json!({"commands": 1, "changed": false, "registered": false})
    );

    write_module(
        &module,
        &describing(&[definition("model"), definition("other")]),
        &replying("unused", &[]),
    )?;
    let (status, body) = reload(&state, Some("operator")).await?;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        body,
        json!({"commands": 2, "changed": true, "registered": false})
    );

    std::fs::write(&module, b"\0asm broken")?;
    let (status, body) = reload(&state, Some("operator")).await?;
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
    assert!(
        body["error"]
            .as_str()
            .is_some_and(|error| error.contains("load command module"))
    );
    assert_eq!(state.commands.definitions().len(), 2);
    Ok(())
}

#[tokio::test]
async fn reloads_serve_the_replacement_only_once_discord_confirmed_it() -> anyhow::Result<()> {
    let directory = tempfile::tempdir()?;
    let status = Arc::new(AtomicU16::new(200));
    let (address, server) = registration_server(status.clone()).await?;
    let mut state = discord_state(
        core_client("http://127.0.0.1:1".to_owned()),
        directory.path(),
    )
    .await?;
    state.app = Some(Arc::new(app(&address.to_string(), 1, 100)));
    let module = directory.path().join("commands.wasm");

    // Nothing is recorded yet, so even unchanged definitions register.
    let (code, body) = reload(&state, Some("operator")).await?;
    assert_eq!(code, StatusCode::OK, "{body}");
    assert_eq!(
        body,
        json!({"commands": 1, "changed": false, "registered": true})
    );
    let (_, body) = reload(&state, Some("operator")).await?;
    assert_eq!(
        body,
        json!({"commands": 1, "changed": false, "registered": false})
    );

    write_module(
        &module,
        &describing(&[definition("model"), definition("other")]),
        &replying("replacement", &[]),
    )?;
    for (code, expected) in [
        (400, "Discord rejected the commands"),
        (500, "Discord did not confirm the commands"),
    ] {
        status.store(code, Ordering::SeqCst);
        let (status, body) = reload(&state, Some("operator")).await?;
        assert_eq!(status, StatusCode::BAD_GATEWAY, "{body}");
        let error = body["error"].as_str().unwrap_or_default();
        assert!(error.starts_with(expected), "{error}");
        assert!(
            error.ends_with("the previous command module keeps serving"),
            "{error}"
        );
        assert_eq!(state.commands.definitions().len(), 1);
        let (_, reply) = interact(&state, Some("token"), &interaction("300", None)).await?;
        assert_eq!(reply["content"], "stub reply");
    }

    status.store(200, Ordering::SeqCst);
    let (code, body) = reload(&state, Some("operator")).await?;
    assert_eq!(code, StatusCode::OK, "{body}");
    assert_eq!(
        body,
        json!({"commands": 2, "changed": true, "registered": true})
    );
    let (_, reply) = interact(&state, Some("token"), &interaction("300", None)).await?;
    assert_eq!(reply["content"], "replacement");
    server.abort();
    Ok(())
}

#[tokio::test]
async fn a_full_message_queue_refuses_instead_of_holding_the_gateway() -> anyhow::Result<()> {
    let (sender, mut receiver) = tokio::sync::mpsc::channel(1);
    let inbound = QueuedMessage {
        request: IncomingMessage {
            event_id: "1".to_owned(),
            guild_id: Some("100".to_owned()),
            user_id: "200".to_owned(),
            channel_id: "300".to_owned(),
            parent_channel_id: None,
            content: "hello".to_owned(),
        },
        native: Value::Null,
    };
    assert!(queue_message(&sender, inbound.clone())?.is_ok());
    let refused = queue_message(&sender, inbound.clone())?
        .err()
        .context("a second message should be refused while the first waits")?;
    assert_eq!(refused.request.event_id, "1");
    receiver.recv().await.context("queued message")?;
    assert!(queue_message(&sender, inbound.clone())?.is_ok());
    drop(receiver);
    assert!(queue_message(&sender, inbound).is_err());
    Ok(())
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
async fn configuration_rejection_does_not_block_the_next_discord_message() -> anyhow::Result<()> {
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
    let mut state =
        discord_state(core_client(format!("http://{address}")), directory.path()).await?;
    state.discord.typing = false;
    let mut other = state.discord.routes[0].clone();
    other.channel_id = "301".to_owned();
    state.discord.routes.push(other);
    state.discord.failed_emoji = "❌".to_owned();
    let mut services = tokio::task::JoinSet::new();
    services.spawn(async move { axum::serve(listener, app).await });
    let (send, receive) = tokio::sync::mpsc::channel(2);
    for (event, channel) in [("400", "300"), ("401", "301")] {
        send.send(super::QueuedMessage {
            request: IncomingMessage {
                event_id: event.to_owned(),
                guild_id: Some("100".to_owned()),
                user_id: "200".to_owned(),
                channel_id: channel.to_owned(),
                parent_channel_id: None,
                content: "hello".to_owned(),
            },
            native: Value::Null,
        })
        .await?;
    }
    drop(send);
    tokio::time::timeout(
        Duration::from_secs(3),
        super::message_worker(state, http, receive),
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
    // Observe actual failure feedback before the unrelated event's admission.
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
        .context("unrelated message not submitted")?;
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

/// A core whose first delivery poll fails ambiguously, whose
/// reconciliation reports one abandoned claim, and whose second poll
/// ends the loop; plus a Discord that records reaction requests.
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
    let directory = tempfile::tempdir()?;
    let mut state =
        discord_state(core_client(format!("http://{address}")), directory.path()).await?;
    state.discord.reactions = true;
    let expected_uncertain = state.discord.uncertain_emoji.clone();
    let http = Arc::new(
        twilight_http::Client::builder()
            .proxy(address.to_string(), true)
            .ratelimiter(None)
            .build(),
    );
    let outcome = tokio::time::timeout(Duration::from_secs(5), delivery_loop(state, http)).await;
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

#[test]
fn discord_route_errors_distinguish_terminal_responses() {
    assert!(discord_status_is_terminal(DiscordStatusCode::FORBIDDEN));
    assert!(discord_status_is_terminal(DiscordStatusCode::NOT_FOUND));
    assert!(!discord_status_is_terminal(
        DiscordStatusCode::TOO_MANY_REQUESTS
    ));
    assert!(!discord_status_is_terminal(
        DiscordStatusCode::GATEWAY_UNAVAILABLE
    ));
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

async fn delivery_loop(
    state: DiscordState,
    http: Arc<twilight_http::Client>,
) -> anyhow::Result<()> {
    super::delivery_loop(state.core, state.discord, http).await
}
