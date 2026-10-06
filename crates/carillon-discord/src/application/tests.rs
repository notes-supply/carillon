use crate::conversations::tests::{discord_config, interaction};
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicU16, AtomicUsize, Ordering},
    },
    time::Duration,
};

use anyhow::Context as _;
use axum::{
    Json, Router,
    body::Body,
    http::{Method, Request, StatusCode, Uri},
    response::IntoResponse as _,
    routing::{get, post},
};
use carillon_discord_commands::contract::{InteractionKind, Reply};
use carillon_installation::config::{LoadedConfig, hash_token};
use carillon_protocol::model::{DeliveryReconciliation, Health};
use serde_json::{Value, json};
use tower::ServiceExt as _;
use twilight_http::response::StatusCode as DiscordStatusCode;

use super::{
    DiscordState, IncomingMessage, InteractionSource, MessagePlan, Participation, QueuedMessage,
    ServeOptions, discord_router, discord_status_is_terminal, interaction_source, queue_message,
};
use crate::{
    CoreClient,
    attachments::{
        Downloads,
        tests::{Serve, attachment, serve_cdn},
    },
    client::test_client as core_client,
    commands::{
        Commands, MODULE_LIMIT, Source,
        tests::{
            app, definition, describing, files, gated_registration, module, padded,
            registration_server, replying, variant, write_module,
        },
    },
    config::{DiscordActivation, DiscordConversation},
};

/// A command module in `directory` that defines `/model` and answers
/// every interaction with `reply`.
async fn stub_commands(directory: &Path, reply: &str) -> anyhow::Result<Arc<Commands>> {
    write_module(
        &directory.join("release.wasm"),
        &describing(&[definition("model")]),
        reply,
    )?;
    Ok(Arc::new(Commands::load(files(directory)).await?))
}

async fn discord_state(core: CoreClient, directory: &Path) -> anyhow::Result<DiscordState> {
    let discord = discord_config(DiscordActivation::Always, DiscordConversation::Channel);
    Ok(DiscordState {
        downloads: Arc::new(Downloads::open(
            directory.join("incoming"),
            discord.attachments,
        )?),
        discord,
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
        reactions: None,
        lanes: None,
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
        attachments: Vec::new(),
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
        let failure = super::prepare_conversation(&state.participation, &http, plan.clone())
            .await
            .err()
            .context("undecodable create should fail")?;
        assert!(!super::discord_failure_is_terminal(&failure));
        assert_eq!(state.participation.created_thread("200"), None);
        let recovered =
            super::prepare_conversation(&state.participation, &http, plan.clone()).await?;
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
        super::prepare_conversation(&state.participation, &http, plan).await?;
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
            attachments: Vec::new(),
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

async fn operate(
    state: &DiscordState,
    method: Method,
    bearer: Option<&str>,
    body: Vec<u8>,
) -> anyhow::Result<(StatusCode, Value)> {
    let mut request = Request::builder().method(method).uri("/v1/commands");
    if let Some(bearer) = bearer {
        request = request.header("authorization", format!("Bearer {bearer}"));
    }
    let response = discord_router(state.clone(), false)
        .oneshot(request.body(Body::from(body))?)
        .await?;
    let status = response.status();
    let body = axum::body::to_bytes(response.into_body(), usize::MAX).await?;
    Ok((status, serde_json::from_slice(&body)?))
}

#[tokio::test]
async fn agent_operations_authenticate_before_parsing_and_refuse_simulation() -> anyhow::Result<()>
{
    let directory = tempfile::tempdir()?;
    let state = discord_state(
        core_client("http://127.0.0.1:1".to_owned()),
        directory.path(),
    )
    .await?;
    for (token, body, expected) in [
        ("wrong", vec![0; 6 * 1024 * 1024 + 1], StatusCode::FORBIDDEN),
        ("token", b"not json".to_vec(), StatusCode::FORBIDDEN),
        (
            "operator",
            vec![0; 6 * 1024 * 1024 + 1],
            StatusCode::PAYLOAD_TOO_LARGE,
        ),
        ("operator", b"not json".to_vec(), StatusCode::BAD_REQUEST),
        (
            "operator",
            br#"{"action":"server.info"}"#.to_vec(),
            StatusCode::SERVICE_UNAVAILABLE,
        ),
    ] {
        let request = Request::builder()
            .method(Method::POST)
            .uri("/v1/operations")
            .header("authorization", format!("Bearer {token}"))
            .body(Body::from(body))?;
        let response = discord_router(state.clone(), true).oneshot(request).await?;
        assert_eq!(response.status(), expected);
    }
    Ok(())
}

fn names(module: &Value) -> Vec<&str> {
    module["commands"]
        .as_array()
        .map(|names| names.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default()
}

#[tokio::test]
async fn command_module_operations_require_the_operator_credential() -> anyhow::Result<()> {
    let directory = tempfile::tempdir()?;
    let state = discord_state(
        core_client("http://127.0.0.1:1".to_owned()),
        directory.path(),
    )
    .await?;
    let wider = module(
        &describing(&[definition("model"), definition("other")]),
        &replying("wider", &[]),
    )?;
    for method in [Method::GET, Method::PUT, Method::DELETE] {
        for bearer in [None, Some("token"), Some("wrong")] {
            let (status, _) = operate(&state, method.clone(), bearer, wider.clone()).await?;
            assert_eq!(status, StatusCode::FORBIDDEN, "{method} {bearer:?}");
        }
    }
    assert_eq!(state.commands.serving().source, Source::Release);

    let (status, body) = operate(&state, Method::GET, Some("operator"), Vec::new()).await?;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["serving"]["source"], "release");
    assert_eq!(names(&body["serving"]), ["model"]);
    assert_eq!(body["next_start"], json!({"outcome": "same"}));
    assert_eq!(body["registered"], Value::Null);

    let (status, body) = operate(&state, Method::PUT, Some("operator"), wider.clone()).await?;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["serving"]["source"], "installed");
    assert_eq!(names(&body["serving"]), ["model", "other"]);
    assert_eq!(names(&body["previous"]), ["model"]);
    assert_eq!(body["discord"], "not_connected");
    assert_eq!(body["saved"], json!({"outcome": "durable"}));

    // A refusal says why and what still serves.
    let (status, body) = operate(
        &state,
        Method::PUT,
        Some("operator"),
        b"\0asm broken".to_vec(),
    )
    .await?;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert!(
        body["error"]
            .as_str()
            .is_some_and(|error| error.starts_with("not a usable command module")
                && error.ends_with("nothing was changed")),
        "{body}"
    );
    assert_eq!(body["serving"]["source"], "installed");
    assert_eq!(names(&body["serving"]), ["model", "other"]);

    // The body limit refuses before reading more; a module at the limit installs.
    let (status, body) = operate(
        &state,
        Method::PUT,
        Some("operator"),
        vec![0; MODULE_LIMIT + 1],
    )
    .await?;
    assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE);
    assert_eq!(
        body["error"],
        format!(
            "a command module may have at most {MODULE_LIMIT} bytes (16 MiB); nothing was changed"
        )
    );
    let at_limit = padded(variant(&["model", "other"], "at limit")?, MODULE_LIMIT)?;
    let (status, body) = operate(&state, Method::PUT, Some("operator"), at_limit).await?;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["saved"], json!({"outcome": "durable"}));

    // Another maintenance operation refuses changes, not status.
    let held = carillon_installation::maintenance::MaintenanceLock::acquire(directory.path())?;
    let (status, body) = operate(&state, Method::DELETE, Some("operator"), Vec::new()).await?;
    assert_eq!(status, StatusCode::CONFLICT);
    assert!(
        body["error"]
            .as_str()
            .is_some_and(|error| error.contains("another Carillon maintenance operation")),
        "{body}"
    );
    let (status, _) = operate(&state, Method::GET, Some("operator"), Vec::new()).await?;
    assert_eq!(status, StatusCode::OK);
    drop(held);

    let (status, body) = operate(&state, Method::DELETE, Some("operator"), Vec::new()).await?;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["serving"]["source"], "release");
    assert_eq!(names(&body["serving"]), ["model"]);
    assert!(!files(directory.path()).installed.exists());
    Ok(())
}

#[tokio::test]
async fn installs_serve_the_replacement_only_once_discord_confirmed_it() -> anyhow::Result<()> {
    let directory = tempfile::tempdir()?;
    let status = Arc::new(AtomicU16::new(200));
    let (address, server) = registration_server(status.clone()).await?;
    let mut state = discord_state(
        core_client("http://127.0.0.1:1".to_owned()),
        directory.path(),
    )
    .await?;
    state.app = Some(Arc::new(app(&address.to_string(), 1, 100)));
    let (_, body) = operate(&state, Method::GET, Some("operator"), Vec::new()).await?;
    assert_eq!(body["registered"], false);

    // Nothing is recorded yet, so even the serving module's definitions
    // register; a repeat finds them recorded.
    let release = std::fs::read(files(directory.path()).release)?;
    for expected in ["registered", "unchanged"] {
        let (code, body) = operate(&state, Method::PUT, Some("operator"), release.clone()).await?;
        assert_eq!(code, StatusCode::OK, "{body}");
        assert_eq!(body["discord"], expected);
    }
    let (_, body) = operate(&state, Method::GET, Some("operator"), Vec::new()).await?;
    assert_eq!(body["registered"], true);

    let replacement = module(
        &describing(&[definition("model"), definition("other")]),
        &replying("replacement", &[]),
    )?;
    for (code, expected, returned) in [
        (
            400,
            "Discord rejected the commands",
            StatusCode::BAD_GATEWAY,
        ),
        (
            500,
            "Discord did not confirm the commands",
            StatusCode::GATEWAY_TIMEOUT,
        ),
    ] {
        status.store(code, Ordering::SeqCst);
        let (status, body) =
            operate(&state, Method::PUT, Some("operator"), replacement.clone()).await?;
        assert_eq!(status, returned, "{body}");
        let error = body["error"].as_str().unwrap_or_default();
        assert!(error.starts_with(expected), "{error}");
        assert_eq!(names(&body["serving"]), ["model"]);
        let (_, reply) = interact(&state, Some("token"), &interaction("300", None)).await?;
        assert_eq!(reply["content"], "stub reply");
    }

    status.store(200, Ordering::SeqCst);
    let (code, body) = operate(&state, Method::PUT, Some("operator"), replacement).await?;
    assert_eq!(code, StatusCode::OK, "{body}");
    assert_eq!(body["discord"], "registered");
    let (_, reply) = interact(&state, Some("token"), &interaction("300", None)).await?;
    assert_eq!(reply["content"], "replacement");
    server.abort();
    Ok(())
}

/// The operator's request only starts a change: abandoning it, as a CLI
/// that times out does, must neither cancel the change between Discord's
/// confirmation and activation nor release the maintenance lock early.
#[tokio::test]
async fn an_abandoned_install_completes_while_holding_the_maintenance_lock() -> anyhow::Result<()> {
    use carillon_installation::maintenance::MaintenanceLock;
    use tokio::sync::{Semaphore, mpsc};

    let directory = tempfile::tempdir()?;
    let (sent, mut received) = mpsc::unbounded_channel();
    let gate = Arc::new(Semaphore::new(0));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let target = app(&listener.local_addr()?.to_string(), 1, 100);
    let router = gated_registration(sent, gate.clone());
    let server = tokio::spawn(async move { axum::serve(listener, router).await });
    let mut state = discord_state(
        core_client("http://127.0.0.1:1".to_owned()),
        directory.path(),
    )
    .await?;
    state.app = Some(Arc::new(target));
    let replacement = variant(&["model", "other"], "replacement")?;

    // Drive the request until Discord has the registration, then drop it.
    let request = operate(&state, Method::PUT, Some("operator"), replacement.clone());
    let registration = tokio::select! {
        _ = request => anyhow::bail!("install answered before Discord confirmed"),
        registration = received.recv() => registration.context("registration closed")?,
    };
    assert_eq!(registration.len(), 2);
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(
        MaintenanceLock::try_acquire(directory.path())?.is_none(),
        "the change must hold the maintenance lock while Discord is registering"
    );
    assert_eq!(state.commands.serving().source, Source::Release);
    let (_, reply) = interact(&state, Some("token"), &interaction("300", None)).await?;
    assert_eq!(reply["content"], "stub reply");
    assert!(!files(directory.path()).installed.exists());

    gate.add_permits(1);
    // The lock is released only when the change has finished, save included.
    tokio::time::timeout(Duration::from_secs(10), async {
        while MaintenanceLock::try_acquire(directory.path())?.is_none() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        anyhow::Ok(())
    })
    .await
    .context("the abandoned install did not finish")??;
    assert_eq!(
        std::fs::read(files(directory.path()).installed)?,
        replacement
    );
    assert_eq!(state.commands.serving().source, Source::Installed);
    let (_, reply) = interact(&state, Some("token"), &interaction("300", None)).await?;
    assert_eq!(reply["content"], "replacement");
    let (_, status) = operate(&state, Method::GET, Some("operator"), Vec::new()).await?;
    assert_eq!(status["registered"], true);
    assert_eq!(status["next_start"], json!({"outcome": "same"}));
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
            attachments: Vec::new(),
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

#[test]
fn native_messages_keep_attachment_metadata_without_signed_addresses() -> anyhow::Result<()> {
    let message: twilight_model::channel::Message = serde_json::from_value(json!({
        "id": "500", "channel_id": "300",
        "author": {"id": "200", "username": "sender", "discriminator": "0"},
        "content": "", "mention_everyone": false, "mention_roles": [],
        "mentions": [], "pinned": false, "tts": false, "type": 0,
        "timestamp": "2026-01-01T00:00:00.000000+00:00",
        "attachments": [{
            "id": "600", "filename": "photo.png", "size": 10, "content_type": "image/png",
            "url": "https://cdn.discordapp.com/attachments/300/600/photo.png?ex=1&hm=signed",
            "proxy_url": "https://media.discordapp.net/attachments/300/600/photo.png?ex=1&hm=signed"
        }],
        "embeds": [{
            "type": "image",
            "url": "https://cdn.discordapp.com/attachments/300/602/shown.png?ex=1&hm=embedded",
            "image": {"url": "https://media.discordapp.net/attachments/300/602/shown.png?ex=1&hm=embedded"}
        }],
        // A reply carries the replied-to message, with its own attachments.
        "referenced_message": {
            "id": "499", "channel_id": "300",
            "author": {"id": "201", "username": "other", "discriminator": "0"},
            "content": "", "embeds": [], "mention_everyone": false, "mention_roles": [],
            "mentions": [], "pinned": false, "tts": false, "type": 0,
            "timestamp": "2026-01-01T00:00:00.000000+00:00",
            "attachments": [{
                "id": "601", "filename": "earlier.pdf", "size": 20,
                "url": "https://cdn.discordapp.com/attachments/300/601/earlier.pdf?ex=1&hm=nested",
                "proxy_url": "https://media.discordapp.net/attachments/300/601/earlier.pdf?ex=1&hm=nested"
            }]
        }
    }))?;
    let native = super::native_message(&message);
    assert_eq!(native["attachments"][0]["filename"], "photo.png");
    assert_eq!(native["attachments"][0]["size"], 10);
    assert!(!native.to_string().contains("hm=signed"));
    assert!(!native.to_string().contains("hm=nested"));
    assert!(!native.to_string().contains("hm=embedded"));
    assert_eq!(
        native["embeds"][0]["url"],
        "https://cdn.discordapp.com/attachments/300/602/shown.png"
    );
    assert_eq!(
        native["referenced_message"]["attachments"][0]["filename"],
        "earlier.pdf"
    );
    assert!(native["attachments"][0].get("url").is_none());
    Ok(())
}

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
        .with_state(shared.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let address = listener.local_addr()?;
    tokio::spawn(async move { axum::serve(listener, app).await });
    Ok((address, shared))
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

/// Send `message` through deterministic simulation.
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
        core_client(format!("http://{core_address}")),
        directory.path(),
    )
    .await?;
    state.downloads = Arc::new(
        Downloads::open(staging.clone(), state.discord.attachments)?.with_fixture_cdn(cdn),
    );
    let requests = || log.lock().map_or(usize::MAX, |log| log.requests.len());
    let file = attachment(cdn, "601", "a.png", 32);

    // Admission comes first: a sender who is not allowed, or a message that
    // would not activate, causes no fetch.
    let (status, _) = simulate(&state, "1", "999", std::slice::from_ref(&file)).await?;
    assert_eq!(status, StatusCode::FORBIDDEN);
    state.discord.routes[0].activation = DiscordActivation::Mention;
    let (status, _) = simulate(&state, "1", "200", std::slice::from_ref(&file)).await?;
    assert_eq!(status, StatusCode::NO_CONTENT);
    state.discord.routes[0].activation = DiscordActivation::Always;
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
        core_client(format!("http://{core_address}")),
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
