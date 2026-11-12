use anyhow::Context as _;
use axum::{
    Json, Router,
    extract::{Path as RoutePath, Request, State},
    http::{HeaderMap, HeaderValue, StatusCode, header::AUTHORIZATION},
    middleware::{self, Next},
    response::{IntoResponse as _, Response},
    routing::{get, post},
};
use carillon_installation::config::{LoadedConfig, hash_token};
use carillon_interface::{
    CoreClient, Staging, authorize_operator, settled, start, submit_event_with_retry,
};
use carillon_protocol::{
    model::{
        DeliveryOutcome, DeliveryReconciliation, EventReceipt, EventStatus, ExternalEvent,
        INSTANCE_HEADER, PROVIDER_HEADER, TurnState,
    },
    turns::ControlAction,
};
use serde_json::{Value, json};
use std::{net::SocketAddr, path::Path, sync::Arc};
use tokio::{net::TcpListener, sync::Mutex};
use tokio_util::sync::CancellationToken;

const TOKEN: &str = "interface-token";

#[derive(Default)]
struct Ledger {
    submitted: Vec<String>,
    held: Vec<String>,
    answers_to_lose: usize,
    capturing: usize,
    refuse: Option<StatusCode>,
    requests: Vec<(String, Option<String>)>,
}

type FakeCore = Arc<Mutex<Ledger>>;

fn receipt(event_id: &str, duplicate: bool) -> EventReceipt {
    EventReceipt {
        event_id: event_id.to_owned(),
        turn_id: format!("turn-{event_id}"),
        state: TurnState::Queued,
        duplicate,
    }
}

async fn submit(State(core): State<FakeCore>, Json(event): Json<ExternalEvent>) -> Response {
    let mut core = core.lock().await;
    core.submitted.push(event.event_id.clone());
    if let Some(status) = core.refuse {
        return (status, Json(json!({"error": "refused"}))).into_response();
    }
    let duplicate = core.held.contains(&event.event_id);
    if !duplicate {
        core.held.push(event.event_id.clone());
    }
    if core.answers_to_lose > 0 {
        core.answers_to_lose -= 1;
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    }
    Json(receipt(&event.event_id, duplicate)).into_response()
}

async fn status(
    State(core): State<FakeCore>,
    RoutePath(event_id): RoutePath<String>,
) -> Json<EventStatus> {
    let mut core = core.lock().await;
    Json(if core.capturing > 0 {
        core.capturing -= 1;
        EventStatus::Capturing
    } else if core.held.contains(&event_id) {
        EventStatus::Accepted(receipt(&event_id, true))
    } else {
        EventStatus::Unknown
    })
}

async fn authenticated(State(core): State<FakeCore>, request: Request, next: Next) -> Response {
    let headers = request.headers().clone();
    let header = |name: &str| Some(headers.get(name)?.to_str().ok()?.to_owned());
    if header(AUTHORIZATION.as_str()).as_deref() != Some(&format!("Bearer {TOKEN}"))
        || header(PROVIDER_HEADER).as_deref() != Some("web")
    {
        return StatusCode::FORBIDDEN.into_response();
    }
    let seen = (request.uri().path().to_owned(), header(INSTANCE_HEADER));
    core.lock().await.requests.push(seen);
    next.run(request).await
}

async fn fake_core() -> anyhow::Result<(FakeCore, CoreClient)> {
    let core = FakeCore::default();
    let reconciled = || async {
        Json(DeliveryReconciliation {
            feedback_target_ids: Vec::new(),
        })
    };
    let app = Router::new()
        .route("/v1/events", post(submit))
        .route("/v1/events/{event_id}", get(status))
        .route(
            "/v1/events/{event_id}/control",
            post(|| async { StatusCode::NO_CONTENT }),
        )
        .route("/v1/deliveries", get(|| async { Json(Value::Null) }))
        .route("/v1/deliveries/reconcile", post(reconciled))
        .route(
            "/v1/deliveries/{id}/ack",
            post(|| async { StatusCode::NO_CONTENT }),
        )
        .layer(middleware::from_fn_with_state(core.clone(), authenticated))
        .with_state(core.clone());
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let client = CoreClient::new(
        format!("http://{}", listener.local_addr()?),
        "web".to_owned(),
        TOKEN.to_owned(),
    )?;
    tokio::spawn(async move { axum::serve(listener, app).await });
    Ok((core, client))
}

fn event(event_id: &str) -> ExternalEvent {
    ExternalEvent {
        event_id: event_id.to_owned(),
        agent_id: "assistant".to_owned(),
        user_id: "owner".to_owned(),
        audience_id: "conversation".to_owned(),
        channel_id: "conversation".to_owned(),
        delivery_target_id: "conversation".to_owned(),
        feedback_target_id: event_id.to_owned(),
        content: "hello".to_owned(),
        native: Value::Null,
        attachments: None,
    }
}

#[tokio::test]
async fn a_lost_answer_settles_by_asking_core_without_a_second_submission() -> anyhow::Result<()> {
    let (fake, core) = fake_core().await?;
    {
        let mut fake = fake.lock().await;
        (fake.answers_to_lose, fake.capturing) = (1, 1);
    }
    let failure = core.submit_event(&event("m-1")).await;
    let failure = failure.err().context("the lost answer arrived")?;
    assert!(failure.is_transient());

    let receipt = settled(&core, "m-1").await?.context("core held nothing")?;
    assert_eq!(receipt.turn_id, "turn-m-1");
    assert_eq!(fake.lock().await.submitted, ["m-1"]);
    assert!(settled(&core, "never-sent").await?.is_none());
    Ok(())
}

#[tokio::test]
async fn a_submission_repeated_after_a_lost_answer_stays_one_turn() -> anyhow::Result<()> {
    let (fake, core) = fake_core().await?;
    fake.lock().await.answers_to_lose = 1;
    let receipt = submit_event_with_retry(&core, &CancellationToken::new(), &event("m-1"))
        .await
        .context("the submission stopped")??;
    assert!(receipt.duplicate);
    assert_eq!(receipt.turn_id, "turn-m-1");
    let fake = fake.lock().await;
    assert_eq!(fake.submitted, ["m-1", "m-1"]);
    assert_eq!(fake.held, ["m-1"]);
    Ok(())
}

#[tokio::test]
async fn failures_worth_repeating_are_told_from_final_ones() -> anyhow::Result<()> {
    let (fake, core) = fake_core().await?;
    for (status, transient) in [
        (StatusCode::INTERNAL_SERVER_ERROR, true),
        (StatusCode::SERVICE_UNAVAILABLE, true),
        (StatusCode::GATEWAY_TIMEOUT, true),
        (StatusCode::BAD_REQUEST, false),
        (StatusCode::FORBIDDEN, false),
        (StatusCode::CONFLICT, false),
        (StatusCode::BAD_GATEWAY, false),
        (StatusCode::INSUFFICIENT_STORAGE, false),
    ] {
        fake.lock().await.refuse = Some(status);
        let failure = core.submit_event(&event("m-1")).await;
        let failure = failure.err().context("a refusal was accepted")?;
        assert_eq!(
            (failure.status, failure.is_transient()),
            (status, transient)
        );
        assert_eq!(
            failure.message,
            "submit event to core: {\"error\":\"refused\"}"
        );
    }
    fake.lock().await.submitted.clear();
    let refused = submit_event_with_retry(&core, &CancellationToken::new(), &event("m-2"))
        .await
        .context("the submission stopped")?;
    let refused = refused.err().context("a refusal was accepted")?;
    assert_eq!(refused.status, StatusCode::INSUFFICIENT_STORAGE);
    assert_eq!(fake.lock().await.submitted, ["m-2"]);

    let unreachable = CoreClient::new(
        "http://127.0.0.1:1".to_owned(),
        "web".to_owned(),
        TOKEN.to_owned(),
    )?;
    let failure = unreachable.event_status("m-1").await;
    assert!(failure.err().context("nothing answered")?.is_transient());
    Ok(())
}

#[tokio::test]
async fn a_control_succeeds_without_a_body() -> anyhow::Result<()> {
    let (_fake, core) = fake_core().await?;
    core.control("m-1", ControlAction::Stop).await?;
    Ok(())
}

#[tokio::test]
async fn only_delivery_claims_carry_an_instance_identity_and_each_opening_its_own()
-> anyhow::Result<()> {
    let (fake, core) = fake_core().await?;
    core.event_status("m-1").await?;
    let claims = core.deliveries();
    assert!(claims.claim().await?.is_none());
    assert!(claims.claim_turn("turn-m-1").await?.is_none());
    claims.reconcile().await?;
    claims
        .acknowledge(7, DeliveryOutcome::Delivered, None)
        .await?;
    core.deliveries().claim().await?;

    let fake = fake.lock().await;
    let (paths, instances): (Vec<_>, Vec<_>) = fake.requests.iter().cloned().unzip();
    assert_eq!(
        paths,
        [
            "/v1/events/m-1",
            "/v1/deliveries",
            "/v1/deliveries",
            "/v1/deliveries/reconcile",
            "/v1/deliveries/7/ack",
            "/v1/deliveries",
        ]
    );
    assert_eq!(instances[0], None);
    let own = instances[1].clone().context("a claim named no instance")?;
    assert!(
        instances[1..5]
            .iter()
            .all(|seen| seen.as_ref() == Some(&own))
    );
    let other = instances[5].clone().context("a claim named no instance")?;
    assert_ne!(own, other);
    Ok(())
}

fn installation(root: &Path, address: SocketAddr, enabled: bool) -> anyhow::Result<LoadedConfig> {
    std::fs::create_dir_all(root.join("credentials"))?;
    std::fs::write(root.join("credentials/web.token"), format!("{TOKEN}\n"))?;
    let registry = format!(
        r#"data_dir = "state"
[core]
listen = "127.0.0.1:45400"
operator_token_hash = "{operator}"
[[providers]]
id = "web"
role = "interface"
program = "web"
version = "0.0.0"
url = "http://{address}"
token_hash = "{token}"
enabled = {enabled}
[[providers]]
id = "runtime-other"
role = "runtime"
program = "runtime-other"
version = "0.0.0"
url = "http://127.0.0.1:45402"
token_hash = "{token}"
enabled = true
"#,
        operator = hash_token("operator-token"),
        token = hash_token(TOKEN),
    );
    std::fs::write(root.join("carillon.toml"), registry)?;
    LoadedConfig::load(&root.join("carillon.toml"))
}

#[tokio::test]
async fn an_interface_the_registry_disables_or_lacks_ends_before_its_credential_and_address()
-> anyhow::Result<()> {
    let directory = tempfile::tempdir()?;
    let taken = TcpListener::bind("127.0.0.1:0").await?;
    let absent = directory.path().join("absent.token");
    let disabled = installation(directory.path(), taken.local_addr()?, false)?;
    assert!(start(&disabled, "web", &absent).await?.is_none());
    let enabled = installation(directory.path(), taken.local_addr()?, true)?;
    assert!(start(&enabled, "slack", &absent).await?.is_none());
    assert!(!directory.path().join("state").exists());

    let failure = start(&enabled, "runtime-other", &absent).await.err();
    let failure = failure.context("a runtime started as an interface")?;
    assert!(format!("{failure:#}").contains("does not have role interface"));
    Ok(())
}

#[tokio::test]
async fn an_interface_checks_its_credential_then_binds_its_address() -> anyhow::Result<()> {
    let directory = tempfile::tempdir()?;
    let occupant = TcpListener::bind("127.0.0.1:0").await?;
    let address = occupant.local_addr()?;
    let config = installation(directory.path(), address, true)?;
    let wrong = directory.path().join("wrong.token");
    std::fs::write(&wrong, "another-token")?;
    let failure = start(&config, "web", &wrong).await.err();
    let failure = failure.context("a wrong credential started")?;
    assert!(format!("{failure:#}").contains("token does not match"));

    let token = directory.path().join("credentials/web.token");
    let failure = start(&config, "web", &token).await.err();
    let failure = failure.context("a taken address started")?;
    assert!(format!("{failure:#}").contains(&format!("bind interface provider web to {address}")));

    drop(occupant);
    let started = start(&config, "web", &token).await?;
    let started = started.context("an enabled interface did not start")?;
    assert_eq!(started.listener.local_addr()?, address);
    assert_eq!(
        (started.health.id.as_str(), started.health.role.as_str()),
        ("web", "interface")
    );
    assert_eq!(started.core.provider_id(), "web");
    assert!(started.core.authenticates(TOKEN));
    Ok(())
}

#[test]
fn operator_endpoints_take_only_the_operator_credential() -> anyhow::Result<()> {
    let directory = tempfile::tempdir()?;
    let config = installation(directory.path(), "127.0.0.1:45401".parse()?, true)?;
    let bearing = |token: &str| -> anyhow::Result<HeaderMap> {
        let mut headers = HeaderMap::new();
        headers.insert(
            AUTHORIZATION,
            HeaderValue::from_str(&format!("Bearer {token}"))?,
        );
        Ok(headers)
    };
    authorize_operator(&config.value, &bearing("operator-token")?)?;
    for headers in [bearing(TOKEN)?, bearing("")?, HeaderMap::new()] {
        let refusal = authorize_operator(&config.value, &headers).err();
        let refusal = refusal.context("another credential was taken")?;
        assert_eq!(refusal.status, StatusCode::FORBIDDEN);
    }
    Ok(())
}

#[test]
fn each_request_stages_under_a_fresh_batch_that_goes_with_it() -> anyhow::Result<()> {
    let directory = tempfile::tempdir()?;
    let incoming = directory.path().join("components/web/incoming");
    std::fs::create_dir_all(incoming.join("left-by-an-earlier-instance"))?;
    let staging = Staging::open(incoming.clone())?;
    assert_eq!(std::fs::read_dir(&incoming)?.count(), 0);

    let (first, second) = (staging.batch()?, staging.batch()?);
    assert_ne!(first.name(), second.name());
    assert_eq!(first.file(0), incoming.join(first.name()).join("0"));
    std::fs::write(first.file(0), b"staged")?;
    std::fs::write(second.file(0), b"staged")?;
    drop(first);
    assert_eq!(std::fs::read_dir(&incoming)?.count(), 1);
    assert!(second.file(0).exists());
    Ok(())
}
