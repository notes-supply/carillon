use crate::conversations::tests::{discord_config, interaction};
use std::{
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
use carillon_discord_module::contract::{InteractionKind, Reply};
use carillon_installation::config::LoadedConfig;
use carillon_protocol::model::{DeliveryReconciliation, Health};
use serde_json::{Value, json};
use tower::ServiceExt as _;
use twilight_http::response::StatusCode as DiscordStatusCode;

use super::{
    Connection, DiscordApp, DiscordState, IncomingMessage, InteractionSource, Lanes, MessagePlan,
    Participation, QueuedMessage, Reactions, ServeOptions, discord_router,
    discord_status_is_terminal, interaction_source, queue_message,
};
use crate::{
    attachments::Downloads,
    client::CoreClient,
    client::test_client as core_client,
    config::{DiscordActivation, DiscordConfig, DiscordConversation, Settings},
    module::{
        Module, Source,
        tests::{
            app, definition, describing, files, gated_registration, module, registration_server,
            replying, showing, variant, write_module,
        },
    },
};

/// A Discord module in `directory` that defines `/model` and answers
/// every interaction with `reply`.
async fn stub_module(directory: &Path, reply: &str) -> anyhow::Result<Arc<Module>> {
    write_module(
        &directory.join("release.wasm"),
        &describing(&[definition("model")]),
        reply,
    )?;
    Ok(Arc::new(
        Module::load(files(directory), Source::Bundled).await?,
    ))
}

/// The provider `discord`, in deterministic mode, serving the settings its
/// file under `directory` holds.
async fn discord_state(core: CoreClient, directory: &Path) -> anyhow::Result<DiscordState> {
    let discord = discord_config(DiscordActivation::Always, DiscordConversation::Channel);
    let installation =
        crate::config::tests::installation(&directory.join("installation"), "discord", 45_300)?;
    discord.write(&installation, "discord")?;
    Ok(DiscordState {
        downloads: Arc::new(Downloads::open(
            directory.join("incoming"),
            discord.attachments,
        )?),
        settings: Settings::new(discord),
        installation: Arc::new(installation),
        health: Health {
            id: "discord".to_owned(),
            role: "interface".to_owned(),
            version: "test".to_owned(),
            status: "ready".to_owned(),
        },
        core,
        participation: Arc::new(Participation::load(directory.join("participation.json"))?),
        module: stub_module(directory, &replying("stub reply", &[])).await?,
        connection: None,
    })
}

/// Connects `state` to Discord as `app`, with its lanes stopped.
fn connect(state: &mut DiscordState, app: DiscordApp) {
    state.connection = Some(Connection {
        reactions: Reactions::new(app.http.clone(), state.settings.clone()),
        lanes: Lanes::new(tokio::sync::mpsc::channel(1).0),
        app: Arc::new(app),
    });
}

/// Has `state` serve its settings changed by `change`, as a restart would.
fn restarted(state: &mut DiscordState, change: impl FnOnce(&mut DiscordConfig)) {
    let mut settings = DiscordConfig::clone(&state.settings.current());
    change(&mut settings);
    state.settings = Settings::new(settings);
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
    let module = root.join("module.wasm");
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
        module,
        deterministic: true,
    }
}

#[tokio::test]
async fn a_disabled_provider_ends_before_credentials_state_or_core() -> anyhow::Result<()> {
    let directory = tempfile::tempdir()?;
    // Its address is taken, core does not answer and its credential is
    // missing: a disabled provider needs none of them.
    let occupant = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let (mut config, _, module) = served_installation(
        &directory.path().join("installation"),
        "127.0.0.1:1".parse()?,
        occupant.local_addr()?,
    )?;
    config
        .value
        .providers
        .iter_mut()
        .find(|provider| provider.id == "discord-private")
        .context("missing interface")?
        .enabled = false;
    let state = crate::config::state_directory(&config, "discord-private");
    let missing = directory.path().join("missing.token");
    tokio::time::timeout(
        Duration::from_secs(2),
        super::serve(config, deterministic_options(&missing, &module)),
    )
    .await??;
    assert!(!state.exists(), "a disabled provider created its state");
    Ok(())
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
        let failure = tokio::time::timeout(
            Duration::from_secs(2),
            super::serve(
                config,
                ServeOptions {
                    provider_id: "discord-private",
                    token_file: &token_file,
                    module: &module,
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
        core_client("http://127.0.0.1:1".to_owned())?,
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
        core_client("http://127.0.0.1:1".to_owned())?,
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
fn oversized_reply_content_is_rejected_before_the_discord_edit() -> anyhow::Result<()> {
    let mut reply = Reply {
        content: "🦀".repeat(2000),
        components: vec![],
    };
    super::validate_reply(&reply)?;
    reply.content.push('x');
    assert_eq!(refusal(&reply), "Discord module reply has unusable content");
    Ok(())
}

fn refusal(reply: &Reply) -> String {
    super::validate_reply(reply).map_or_else(|error| error.to_string(), |_| "accepted".to_owned())
}

#[test]
fn native_selects_need_no_string_options() -> anyhow::Result<()> {
    for kind in [5, 6, 7, 8] {
        let reply = Reply {
            content: "Choose".to_owned(),
            components: vec![json!({
                "type": 1,
                "components": [{"type": kind, "custom_id": "native", "min_values": 1, "max_values": 1}]
            })],
        };
        assert_eq!(super::validate_reply(&reply)?.len(), 1, "type {kind}");
    }
    for (options, refused) in [
        (Value::Null, "Discord module reply has invalid components"),
        (
            json!([]),
            "Discord module reply has a select menu without options",
        ),
    ] {
        let reply = Reply {
            content: "Choose".to_owned(),
            components: vec![json!({
                "type": 1, "components": [{"type": 3, "custom_id": "string", "options": options}]
            })],
        };
        assert_eq!(refusal(&reply), refused, "options {options}");
    }
    Ok(())
}

#[tokio::test]
async fn deterministic_interactions_run_the_module_and_check_replies() -> anyhow::Result<()> {
    let directory = tempfile::tempdir()?;
    let mut state = discord_state(
        core_client("http://127.0.0.1:1".to_owned())?,
        directory.path(),
    )
    .await?;
    let button = json!({"type": 1, "components": [{"type": 2, "style": 2, "custom_id": "model:refresh", "label": "Refresh"}]});
    state.module = stub_module(
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
        (
            vec![json!({"type": 1, "components": []})],
            "empty action row",
        ),
    ] {
        state.module = stub_module(directory.path(), &replying("bad", &components)).await?;
        let (status, body) = interact(&state, Some("token"), &interaction("300", None)).await?;
        assert_eq!(status, StatusCode::BAD_GATEWAY, "{body}");
        assert!(
            body["error"]
                .as_str()
                .is_some_and(|error| error.contains(complaint)),
            "{body}"
        );
    }
    state.module = stub_module(
        directory.path(),
        &replying("full", &[select((0..25).map(option).collect())]),
    )
    .await?;
    let (status, body) = interact(&state, Some("token"), &interaction("300", None)).await?;
    assert_eq!(status, StatusCode::OK, "{body}");

    state.module = stub_module(directory.path(), r#"{"Failure":"no such command"}"#).await?;
    let (status, body) = interact(&state, Some("token"), &interaction("300", None)).await?;
    assert_eq!(status, StatusCode::BAD_GATEWAY);
    assert!(
        body["error"]
            .as_str()
            .is_some_and(|error| error.contains("no such command"))
    );
    Ok(())
}

/// The provider's module status, asked as `bearer`.
async fn module_status(
    state: &DiscordState,
    bearer: Option<&str>,
) -> anyhow::Result<(StatusCode, Value)> {
    let mut request = Request::builder().method(Method::GET).uri("/v1/module");
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
async fn agent_operations_authenticate_before_parsing_and_refuse_simulation() -> anyhow::Result<()>
{
    let directory = tempfile::tempdir()?;
    let state = discord_state(
        core_client("http://127.0.0.1:1".to_owned())?,
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

/// Saves the settings `state` serves with the custom module `custom.wasm`
/// in `directory` enabled, after `change`.
fn declare_custom(
    state: &DiscordState,
    directory: &Path,
    change: impl FnOnce(&mut DiscordConfig),
) -> anyhow::Result<()> {
    edit_settings(state, |file| {
        file.module.enabled = true;
        file.module.path = Some(directory.join("custom.wasm"));
        change(file);
    })
}

#[tokio::test]
async fn a_declared_module_serves_only_through_an_operator_apply() -> anyhow::Result<()> {
    let directory = tempfile::tempdir()?;
    let state = discord_state(
        core_client("http://127.0.0.1:1".to_owned())?,
        directory.path(),
    )
    .await?;
    let custom = directory.path().join("custom.wasm");
    for bearer in [None, Some("token"), Some("wrong")] {
        let (status, _) = module_status(&state, bearer).await?;
        assert_eq!(status, StatusCode::FORBIDDEN, "{bearer:?}");
    }
    let (status, body) = module_status(&state, Some("operator")).await?;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["serving"]["source"], "bundled");
    assert_eq!(names(&body["serving"]), ["model"]);
    assert_eq!(body["declared"], json!({"outcome": "serving"}));
    assert_eq!(body["registered"], Value::Null);

    // The file declares a custom module; only an operator's apply serves it.
    std::fs::write(&custom, variant(&["model", "other"], "custom")?)?;
    declare_custom(&state, directory.path(), |_| ())?;
    let (_, body) = settings(&state, Method::GET, "operator").await?;
    assert_eq!(
        body["file"],
        json!({"state": "read", "live": ["module"], "restart": []})
    );
    let (_, body) = module_status(&state, Some("operator")).await?;
    assert_eq!(
        body["declared"],
        json!({"outcome": "other", "source": "custom", "path": custom})
    );
    for bearer in ["", "token", "wrong"] {
        let (status, _) = settings(&state, Method::POST, bearer).await?;
        assert_eq!(status, StatusCode::FORBIDDEN);
    }
    assert_eq!(state.module.serving().source, Source::Bundled);
    let (status, body) = settings(&state, Method::POST, "operator").await?;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["changed"], json!(["module"]));
    assert_eq!(body["module"]["serving"]["source"], "custom");
    assert_eq!(names(&body["module"]["serving"]), ["model", "other"]);
    assert_eq!(body["module"]["discord"], "not_connected");
    let (_, reply) = interact(&state, Some("token"), &interaction("300", None)).await?;
    assert_eq!(reply["content"], "custom");

    // A rebuild at the same path serves when applied.
    std::fs::write(&custom, variant(&["model", "other"], "rebuilt")?)?;
    let (_, body) = settings(&state, Method::GET, "operator").await?;
    assert_eq!(body["file"]["live"], json!(["module"]));
    let (status, body) = settings(&state, Method::POST, "operator").await?;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["changed"], json!(["module"]));
    let (_, reply) = interact(&state, Some("token"), &interaction("300", None)).await?;
    assert_eq!(reply["content"], "rebuilt");

    // A broken rebuild is refused with the rest of its file: the serving
    // module and settings stay.
    std::fs::write(&custom, b"\0asm broken")?;
    declare_custom(&state, directory.path(), |file| {
        file.allowed_user_ids.push("250".to_owned());
    })?;
    let (status, body) = settings(&state, Method::POST, "operator").await?;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    let error = body["error"].as_str().unwrap_or_default();
    assert!(
        error.starts_with("not a usable Discord module") && error.ends_with("nothing was applied"),
        "{body}"
    );
    assert_eq!(body["serving"]["source"], "custom");
    assert_eq!(state.settings.current().allowed_user_ids, ["200"]);
    let (_, reply) = interact(&state, Some("token"), &interaction("300", None)).await?;
    assert_eq!(reply["content"], "rebuilt");

    // Another maintenance operation refuses a module change, not status.
    std::fs::write(&custom, variant(&["model"], "locked out")?)?;
    let held = carillon_installation::maintenance::MaintenanceLock::acquire(directory.path())?;
    let (status, body) = settings(&state, Method::POST, "operator").await?;
    assert_eq!(status, StatusCode::CONFLICT);
    assert!(
        body["error"]
            .as_str()
            .is_some_and(|error| error.contains("another Carillon maintenance operation")),
        "{body}"
    );
    let (status, _) = module_status(&state, Some("operator")).await?;
    assert_eq!(status, StatusCode::OK);
    drop(held);

    // Disabled, the bundled module serves again.
    edit_settings(&state, |file| {
        file.module.enabled = false;
        file.allowed_user_ids.push("250".to_owned());
    })?;
    let (status, body) = settings(&state, Method::POST, "operator").await?;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["changed"], json!(["allowed_user_ids", "module"]));
    assert_eq!(body["module"]["serving"]["source"], "bundled");
    let (_, reply) = interact(&state, Some("token"), &interaction("300", None)).await?;
    assert_eq!(reply["content"], "stub reply");
    let (_, body) = module_status(&state, Some("operator")).await?;
    assert_eq!(body["declared"], json!({"outcome": "serving"}));
    Ok(())
}

#[tokio::test]
async fn applies_serve_a_module_only_once_discord_confirmed_it() -> anyhow::Result<()> {
    let directory = tempfile::tempdir()?;
    let status = Arc::new(AtomicU16::new(200));
    let (address, server) = registration_server(status.clone()).await?;
    let mut state = discord_state(
        core_client("http://127.0.0.1:1".to_owned())?,
        directory.path(),
    )
    .await?;
    connect(&mut state, app(&address.to_string(), 1, 100));
    let (_, body) = module_status(&state, Some("operator")).await?;
    assert_eq!(body["registered"], false);

    // Nothing is recorded yet, so even the serving module's definitions
    // register; a repeat finds them recorded.
    for expected in ["registered", "unchanged"] {
        let (code, body) = settings(&state, Method::POST, "operator").await?;
        assert_eq!(code, StatusCode::OK, "{body}");
        assert_eq!(body["module"]["discord"], expected);
    }
    let (_, body) = module_status(&state, Some("operator")).await?;
    assert_eq!(body["registered"], true);

    std::fs::write(
        directory.path().join("custom.wasm"),
        module(
            &describing(&[definition("model"), definition("other")]),
            &replying("replacement", &[]),
        )?,
    )?;
    declare_custom(&state, directory.path(), |_| ())?;
    for (code, expected, returned) in [
        (
            400,
            "Discord rejected the module's commands",
            StatusCode::BAD_GATEWAY,
        ),
        (
            500,
            "Discord did not confirm the module's commands",
            StatusCode::GATEWAY_TIMEOUT,
        ),
    ] {
        status.store(code, Ordering::SeqCst);
        let (status, body) = settings(&state, Method::POST, "operator").await?;
        assert_eq!(status, returned, "{body}");
        let error = body["error"].as_str().unwrap_or_default();
        assert!(error.starts_with(expected), "{error}");
        assert_eq!(names(&body["serving"]), ["model"]);
        assert!(!state.settings.current().module.enabled);
        let (_, reply) = interact(&state, Some("token"), &interaction("300", None)).await?;
        assert_eq!(reply["content"], "stub reply");
    }

    status.store(200, Ordering::SeqCst);
    let (code, body) = settings(&state, Method::POST, "operator").await?;
    assert_eq!(code, StatusCode::OK, "{body}");
    assert_eq!(body["module"]["discord"], "registered");
    let (_, reply) = interact(&state, Some("token"), &interaction("300", None)).await?;
    assert_eq!(reply["content"], "replacement");
    server.abort();
    Ok(())
}

/// The operator's request only starts an apply: abandoning it, as a CLI
/// that times out does, must neither cancel the change between Discord's
/// confirmation and activation nor release the maintenance lock early.
#[tokio::test]
async fn an_abandoned_apply_completes_while_holding_the_maintenance_lock() -> anyhow::Result<()> {
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
        core_client("http://127.0.0.1:1".to_owned())?,
        directory.path(),
    )
    .await?;
    connect(&mut state, target);
    std::fs::write(
        directory.path().join("custom.wasm"),
        variant(&["model", "other"], "replacement")?,
    )?;
    declare_custom(&state, directory.path(), |_| ())?;

    // Drive the request until Discord has the registration, then drop it.
    let request = settings(&state, Method::POST, "operator");
    let registration = tokio::select! {
        _ = request => anyhow::bail!("the apply answered before Discord confirmed"),
        registration = received.recv() => registration.context("registration closed")?,
    };
    assert_eq!(registration.len(), 2);
    assert!(
        MaintenanceLock::try_acquire(directory.path())?.is_none(),
        "the change must hold the maintenance lock while Discord is registering"
    );
    assert_eq!(state.module.serving().source, Source::Bundled);
    let (_, reply) = interact(&state, Some("token"), &interaction("300", None)).await?;
    assert_eq!(reply["content"], "stub reply");

    gate.add_permits(1);
    // The lock is released only when the change has finished.
    tokio::time::timeout(Duration::from_secs(10), async {
        while MaintenanceLock::try_acquire(directory.path())?.is_none() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        anyhow::Ok(())
    })
    .await
    .context("the abandoned apply did not finish")??;
    assert!(matches!(
        state.module.serving().source,
        Source::Custom { .. }
    ));
    assert!(state.settings.current().module.enabled);
    let (_, reply) = interact(&state, Some("token"), &interaction("300", None)).await?;
    assert_eq!(reply["content"], "replacement");
    let (_, status) = module_status(&state, Some("operator")).await?;
    assert_eq!(status["registered"], true);
    assert_eq!(status["declared"], json!({"outcome": "serving"}));
    server.abort();
    Ok(())
}

/// A Discord module that asks core for the model state through its one
/// native call, then replies `answered` when core gave the state, or
/// `failed` when it did not.
fn calling_model(answered: &str, failed: &str) -> anyhow::Result<Vec<u8>> {
    let description = describing(&[definition("model")]);
    let describe_length =
        serde_json::to_vec(&carillon_discord_module::contract::Invocation::Describe)?.len();
    let request =
        serde_json::to_string(&carillon_discord_module::contract::HostRequest::ModelState)?;
    let (answered, failed) = (replying(answered, &[]), replying(failed, &[]));
    let card = showing("A test card");
    let escape = |text: &str| text.replace('\\', "\\\\").replace('"', "\\\"");
    let text = format!(
        r#"(module
  (import "carillon" "call" (func $call (param i32 i32) (result i32)))
  (import "carillon" "take" (func $take (param i32 i32)))
  (memory (export "memory") 1)
  (global $length (mut i32) (i32.const 0))
  (global $output (mut i32) (i32.const 0))
  (data (i32.const 1024) "{}")
  (data (i32.const 4096) "{}")
  (data (i32.const 5120) "{}")
  (data (i32.const 6144) "{}")
  (data (i32.const 7168) "{}")
  (func (export "input") (param $length i32) (result i32)
    (global.set $length (local.get $length))
    (i32.const 8192))
  (func (export "invoke") (result i32)
    (local $reply i32)
    (if (result i32) (i32.eq (global.get $length) (i32.const {describe_length}))
      (then (global.set $output (i32.const 1024)) (i32.const {}))
      (else
        ;; A card request begins {{"Card"
        (if (result i32) (i32.eq (i32.load8_u (i32.const 8194)) (i32.const 67))
          (then (global.set $output (i32.const 7168)) (i32.const {}))
          (else
            (local.set $reply (call $call (i32.const 4096) (i32.const {})))
            (call $take (i32.const 32768) (local.get $reply))
            ;; {{"ModelState": ...}} or {{"Error": ...}}
            (if (result i32) (i32.eq (i32.load8_u (i32.const 32770)) (i32.const 77))
              (then (global.set $output (i32.const 5120)) (i32.const {}))
              (else (global.set $output (i32.const 6144)) (i32.const {}))))))))
  (func (export "output") (result i32) (global.get $output)))"#,
        escape(&description),
        escape(&request),
        escape(&answered),
        escape(&failed),
        escape(&card),
        description.len(),
        card.len(),
        request.len(),
        answered.len(),
        failed.len(),
    );
    Ok(wat::parse_str(text)?)
}

/// A gateway `/model` command from an allowed user in route channel 300.
fn model_command() -> anyhow::Result<twilight_gateway::Event> {
    use twilight_model::gateway::payload::incoming::InteractionCreate;

    let command = json!({
        "id": "1",
        "application_id": "2",
        "authorizing_integration_owners": {},
        "token": "t",
        "version": 1,
        "type": 2,
        "guild_id": "100",
        "channel": {"id": "300", "type": 0},
        "member": {
            "user": {"id": "200", "username": "u", "discriminator": "0", "avatar": null},
            "roles": [],
            "joined_at": null,
            "deaf": false,
            "mute": false,
            "flags": 0
        },
        "data": {"id": "3", "name": "model", "type": 1}
    });
    Ok(twilight_gateway::Event::InteractionCreate(Box::new(
        InteractionCreate(serde_json::from_value(command)?),
    )))
}

/// The gateway goes, as when the provider stops, after taking an
/// interaction that Discord has acknowledged and whose native call to core
/// is still running: the call's result is still part of the reply sent
/// before the provider returns.
#[tokio::test]
async fn an_acknowledged_interaction_is_answered_before_the_provider_returns() -> anyhow::Result<()>
{
    use axum::routing::patch;
    use tokio::sync::Notify;

    let directory = tempfile::tempdir()?;
    // Discord takes the acknowledgement and the reply at once.
    let acknowledged = Arc::new(Notify::new());
    let replies = Arc::new(std::sync::Mutex::new(Vec::<Value>::new()));
    let discord = Router::new()
        .route(
            "/api/v10/interactions/{id}/{token}/callback",
            post({
                let acknowledged = acknowledged.clone();
                move || async move {
                    acknowledged.notify_one();
                    StatusCode::NO_CONTENT
                }
            }),
        )
        .route(
            "/api/v10/webhooks/{application}/{token}/messages/@original",
            patch({
                let replies = replies.clone();
                move |Json(reply): Json<Value>| async move {
                    if let Ok(mut replies) = replies.lock() {
                        replies.push(reply);
                    }
                    Json(json!({}))
                }
            }),
        );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let discord_address = listener.local_addr()?;
    let discord_server = tokio::spawn(async move { axum::serve(listener, discord).await });
    // Core answers the module's native call only when the test releases it.
    let (asked, release) = (Arc::new(Notify::new()), Arc::new(Notify::new()));
    let core = Router::new().route(
        "/v1/model",
        get({
            let (asked, release) = (asked.clone(), release.clone());
            move || async move {
                asked.notify_one();
                release.notified().await;
                Json(json!({
                    "available": [],
                    "agent": {"selection": null, "overridden": false}
                }))
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let core_address = listener.local_addr()?;
    let core_server = tokio::spawn(async move { axum::serve(listener, core).await });
    let mut state = discord_state(
        core_client(format!("http://{core_address}"))?,
        directory.path(),
    )
    .await?;
    std::fs::write(
        directory.path().join("release.wasm"),
        calling_model("model state received", "the native call failed")?,
    )?;
    state.module = Arc::new(Module::load(files(directory.path()), Source::Bundled).await?);
    connect(&mut state, app(&discord_address.to_string(), 2, 100));
    let event = model_command()?;
    let mut interactions = tokio::task::JoinSet::new();
    assert!(super::take_event(&state, event, None, &mut interactions).is_none());
    // No event follows: the gateway is gone. Discord has the acknowledgement,
    // and the module's native call is waiting for core.
    tokio::time::timeout(Duration::from_secs(5), acknowledged.notified()).await?;
    tokio::time::timeout(Duration::from_secs(5), asked.notified()).await?;
    let answering = super::answered(&mut interactions);
    tokio::pin!(answering);
    tokio::time::timeout(Duration::from_millis(200), &mut answering)
        .await
        .err()
        .context("the provider returned while an acknowledged interaction was still running")?;
    release.notify_one();
    tokio::time::timeout(Duration::from_secs(10), answering).await?;
    let replies = replies
        .lock()
        .map_err(|_| anyhow::anyhow!("poisoned"))?
        .clone();
    assert_eq!(replies.len(), 1, "{replies:?}");
    assert_eq!(replies[0]["content"], "model state received");
    discord_server.abort();
    core_server.abort();
    Ok(())
}

/// A resumed gateway session replays an interaction this provider already
/// answered: Discord takes no second acknowledgement, so the module never
/// runs it again and core is never asked.
#[tokio::test]
async fn a_replayed_interaction_discord_does_not_acknowledge_again_runs_nothing()
-> anyhow::Result<()> {
    let directory = tempfile::tempdir()?;
    let replies = Arc::new(AtomicUsize::new(0));
    let discord = Router::new()
        .route(
            "/api/v10/interactions/{id}/{token}/callback",
            post(|| async {
                (
                    StatusCode::BAD_REQUEST,
                    Json(json!({"code": 40060, "message": "Interaction has already been acknowledged."})),
                )
            }),
        )
        .fallback({
            let replies = replies.clone();
            move || async move {
                replies.fetch_add(1, Ordering::SeqCst);
                StatusCode::NO_CONTENT
            }
        });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let discord_address = listener.local_addr()?;
    let discord_server = tokio::spawn(async move { axum::serve(listener, discord).await });
    let asked = Arc::new(AtomicUsize::new(0));
    let core = Router::new().fallback({
        let asked = asked.clone();
        move || async move {
            asked.fetch_add(1, Ordering::SeqCst);
            StatusCode::NO_CONTENT
        }
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let core_address = listener.local_addr()?;
    let core_server = tokio::spawn(async move { axum::serve(listener, core).await });
    let mut state = discord_state(
        core_client(format!("http://{core_address}"))?,
        directory.path(),
    )
    .await?;
    std::fs::write(
        directory.path().join("release.wasm"),
        calling_model("model state received", "the native call failed")?,
    )?;
    state.module = Arc::new(Module::load(files(directory.path()), Source::Bundled).await?);
    connect(&mut state, app(&discord_address.to_string(), 2, 100));
    let mut interactions = tokio::task::JoinSet::new();
    let taken = super::take_event(&state, model_command()?, None, &mut interactions);
    assert!(
        taken.is_none(),
        "an interaction is not a message awaiting settlement"
    );
    tokio::time::timeout(Duration::from_secs(10), super::answered(&mut interactions)).await?;
    assert_eq!(asked.load(Ordering::SeqCst), 0, "the module ran the replay");
    assert_eq!(replies.load(Ordering::SeqCst), 0, "the replay was answered");
    discord_server.abort();
    core_server.abort();
    Ok(())
}

/// Discord's REST API for a start: the bot and its application answer,
/// and registering the module's commands is refused.
async fn refusing_registration() -> anyhow::Result<std::net::SocketAddr> {
    let discord = Router::new()
        .route(
            "/api/v10/users/@me",
            get(|| async {
                Json(
                    json!({"id": "201", "username": "carillon", "discriminator": "0",
                            "avatar": null, "bot": true, "mfa_enabled": false}),
                )
            }),
        )
        .route(
            "/api/v10/applications/@me",
            get(|| async {
                Json(json!({"id": "2", "name": "carillon", "description": "",
                            "bot_public": false, "bot_require_code_grant": false, "flags": 0,
                            "icon": null, "team": null, "verify_key": "key"}))
            }),
        )
        .fallback(|| async {
            (
                StatusCode::BAD_REQUEST,
                Json(json!({"code": 50035, "message": "Invalid Form Body"})),
            )
        });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let address = listener.local_addr()?;
    tokio::spawn(async move { axum::serve(listener, discord).await });
    Ok(address)
}

/// A start that fails before its gateway serves, here because Discord
/// refuses the module's commands, leaves the recorded session to the next.
#[tokio::test]
async fn a_start_that_fails_before_its_gateway_keeps_the_recorded_session() -> anyhow::Result<()> {
    let directory = tempfile::tempdir()?;
    let discord = refusing_registration().await?;
    let state = discord_state(
        core_client("http://127.0.0.1:1".to_owned())?,
        directory.path(),
    )
    .await?;
    let record = crate::gateway::record_path(&crate::config::state_directory(
        &state.installation,
        &state.health.id,
    ));
    std::fs::create_dir_all(record.parent().context("no state directory")?)?;
    std::fs::write(&record, b"recorded")?;
    let http = Arc::new(
        twilight_http::Client::builder()
            .token("test-only".to_owned())
            .proxy(discord.to_string(), true)
            .ratelimiter(None)
            .build(),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let reconciled = DeliveryReconciliation {
        feedback_target_ids: Vec::new(),
    };
    let failure = super::connect(
        state,
        listener,
        http,
        "test-only".to_owned(),
        reconciled,
        None,
    )
    .await
    .err()
    .context("the start served despite the refused commands")?;
    assert!(
        format!("{failure:#}").contains("register Discord guild commands"),
        "{failure:#}"
    );
    assert_eq!(std::fs::read(&record)?, b"recorded");
    Ok(())
}

/// The lanes have stopped, so a message the gateway admits can no longer
/// be handed to them: the gateway fails, and the session it records keeps
/// the replay floor below that message.
#[tokio::test]
async fn a_message_the_stopped_lanes_never_took_keeps_the_replay_floor_below_it()
-> anyhow::Result<()> {
    use crate::gateway::{
        Gap, Start,
        tests::{LocalGateway, identity, message, ready, recorded},
    };

    let directory = tempfile::tempdir()?;
    let local = LocalGateway::start(vec![vec![ready(1, "session1"), message(2, "900")]]).await?;
    let (mut gateway, _settled) = local.gateway(Start::Identify(Gap::NoRecord));
    let state = discord_state(
        core_client("http://127.0.0.1:1".to_owned())?,
        directory.path(),
    )
    .await?;
    let ingress = crate::inbound::tests::ingress(
        state.core.clone(),
        DiscordConfig::clone(&state.settings.current()),
        state.participation.clone(),
        Arc::new(twilight_http::Client::new(String::new())),
        &directory.path().join("lanes"),
    )?;
    // The dispatcher ends at the stop and drops its end of the queue.
    let (messages, receiver) = tokio::sync::mpsc::channel(crate::inbound::lanes::MAX_PENDING);
    ingress.stopping.cancel();
    tokio::time::timeout(
        Duration::from_secs(5),
        crate::inbound::lanes::dispatch(ingress.clone(), receiver),
    )
    .await?;
    let mut interactions = tokio::task::JoinSet::new();
    let failure = tokio::time::timeout(
        Duration::from_secs(15),
        super::gateway_loop(&state, &mut gateway, &messages, &ingress, &mut interactions),
    )
    .await?
    .err()
    .context("the gateway went on without its lanes")?;
    assert!(
        format!("{failure:#}").contains("Discord message worker stopped"),
        "{failure:#}"
    );
    assert!(gateway.close().await);
    let path = crate::gateway::record_path(directory.path());
    gateway.record(&path, &identity());
    assert_eq!(
        recorded(&path)?["sequence"],
        1,
        "the floor passed message 900"
    );
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
        settings: Arc::new(discord_config(
            DiscordActivation::Always,
            DiscordConversation::Channel,
        )),
        dispatch: None,
    };
    queue_message(&sender, inbound.clone())?
        .ok()
        .context("the first message should be queued")?;
    queue_message(&sender, inbound.clone())?
        .err()
        .context("a second message should be refused while the first waits")?;
    receiver.recv().await.context("queued message")?;
    queue_message(&sender, inbound.clone())?
        .ok()
        .context("a message should be queued once the first was taken")?;
    drop(receiver);
    let stopped = queue_message(&sender, inbound)
        .err()
        .context("a stopped worker should fail the message")?;
    assert_eq!(stopped.to_string(), "Discord message worker stopped");
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

/// Asks the provider's settings endpoint, as `bearer`, for its status or
/// to apply its settings file.
async fn settings(
    state: &DiscordState,
    method: Method,
    bearer: &str,
) -> anyhow::Result<(StatusCode, Value)> {
    let request = Request::builder()
        .method(method)
        .uri("/v1/settings")
        .header("authorization", format!("Bearer {bearer}"))
        .body(Body::empty())?;
    let response = discord_router(state.clone(), false)
        .oneshot(request)
        .await?;
    let status = response.status();
    let body = axum::body::to_bytes(response.into_body(), usize::MAX).await?;
    Ok((status, serde_json::from_slice(&body)?))
}

/// Saves the settings `state` serves, changed by `change`, as its file.
fn edit_settings(
    state: &DiscordState,
    change: impl FnOnce(&mut DiscordConfig),
) -> anyhow::Result<()> {
    let mut file = DiscordConfig::clone(&state.settings.current());
    change(&mut file);
    file.write(&state.installation, "discord")
}

mod simulation;
