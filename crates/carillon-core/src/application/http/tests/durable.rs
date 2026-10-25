//! A durable runtime over HTTP, against real storage: core hands it every
//! turn in order and waits for no answer, a turn it did not take is handed
//! over again, also after core restarts, and drains wait for none of them.

use std::{collections::VecDeque, fs, path::Path, sync::Mutex};

use anyhow::Context as _;
use axum::body::Bytes;
use carillon_protocol::turns::{Silence, TurnOpening};
use tempfile::tempdir;

use super::{
    Fixture, configured, installation,
    live::{lock, ok, operator, submit},
};
use crate::{InboxFailure, Stalled, WorkerStep, application::http::*};
use carillon_protocol::model::RuntimeTurnRequest;

/// A durable runtime that keeps every turn core hands it, answering each
/// hand-over with the next scripted answer (202 once they run out), and
/// each control with `control`.
#[derive(Clone)]
pub(super) struct DurableRuntime {
    pub(super) url: String,
    handed: Arc<Mutex<Vec<RuntimeTurnRequest>>>,
    script: Arc<Mutex<VecDeque<(StatusCode, Value)>>>,
    controlled: Arc<Mutex<Vec<(String, Value)>>>,
    pub(super) control: Arc<Mutex<(StatusCode, Value)>>,
}

impl DurableRuntime {
    pub(super) async fn start() -> Result<Self> {
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let runtime = Self {
            url: format!("http://{}", listener.local_addr()?),
            handed: Arc::default(),
            script: Arc::default(),
            controlled: Arc::default(),
            control: Arc::new(Mutex::new((StatusCode::NO_CONTENT, Value::Null))),
        };
        let (handed, script) = (runtime.handed.clone(), runtime.script.clone());
        let (controlled, control) = (runtime.controlled.clone(), runtime.control.clone());
        let router = Router::new()
            .route("/v1/restart", get(async || Json(json!({"state": "ready"}))))
            .route(
                "/v1/inbox",
                post(move |Json(request): Json<RuntimeTurnRequest>| {
                    lock(&handed).push(request);
                    let answer = lock(&script)
                        .pop_front()
                        .unwrap_or_else(|| (StatusCode::ACCEPTED, json!({})));
                    async move { (answer.0, Json(answer.1)) }
                }),
            )
            .route(
                "/v1/turns/{id}/control",
                post(
                    move |AxumPath(id): AxumPath<String>, Json(body): Json<Value>| {
                        lock(&controlled).push((id, body));
                        let (status, body) = lock(&control).clone();
                        async move {
                            if body.is_null() {
                                status.into_response()
                            } else {
                                (status, Json(body)).into_response()
                            }
                        }
                    },
                ),
            );
        tokio::spawn(async move { axum::serve(listener, router).await });
        Ok(runtime)
    }

    /// Answers the next hand-overs as given, in order.
    pub(super) fn answer(&self, answers: impl IntoIterator<Item = (StatusCode, Value)>) {
        lock(&self.script).extend(answers);
    }

    /// The inputs of the turns handed over, in order.
    pub(super) fn handed(&self) -> Vec<String> {
        lock(&self.handed)
            .iter()
            .map(|request| request.input.clone())
            .collect()
    }

    pub(super) fn request(&self, index: usize) -> Result<RuntimeTurnRequest> {
        lock(&self.handed)
            .get(index)
            .cloned()
            .context("no such hand-over")
    }

    pub(super) fn controlled(&self) -> Vec<(String, Value)> {
        lock(&self.controlled).clone()
    }
}

/// An installation whose runtime is `runtime`, durable, served by core.
pub(super) fn durable(directory: &Path, runtime: &DurableRuntime) -> Result<(Fixture, AppState)> {
    let mut fixture = installation(directory)?;
    let provider = &mut fixture.config.value.providers[1];
    provider.url.clone_from(&runtime.url);
    fixture.config.value.validate()?;
    let state = configured(&fixture);
    Ok((fixture, state))
}

/// Registers another runtime, `id`, with its own credential `token`.
pub(super) fn add_runtime(fixture: &mut Fixture, id: &str, token: &str) -> Result<()> {
    let mut other = fixture.config.value.providers[1].clone();
    id.clone_into(&mut other.id);
    other.url = format!(
        "http://127.0.0.1:{}",
        40_000 + fixture.config.value.providers.len()
    );
    other.token_hash = carillon_installation::config::hash_token(token);
    fixture.config.value.providers.push(other);
    fs::write(
        fixture.config.root.join(format!("credentials/{id}.token")),
        token,
    )?;
    fixture.config.value.validate()
}

pub(super) async fn dispatch(state: &AppState) -> Result<WorkerStep, Stalled> {
    runtime::dispatch_next(state, "test-runtime", &CancellationToken::new()).await
}

/// How core answers `runtime`, presenting `token`, settling `turn_id` as
/// `outcome` says.
pub(super) async fn settle(
    state: &AppState,
    (runtime, token): (&str, &str),
    turn_id: &str,
    outcome: &Value,
) -> Result<StatusCode> {
    let mut headers = HeaderMap::new();
    headers.insert(PROVIDER_HEADER, runtime.parse()?);
    headers.insert(
        http::header::AUTHORIZATION,
        format!("Bearer {token}").parse()?,
    );
    let body = Bytes::from(serde_json::to_vec(outcome)?);
    Ok(
        match runtime::outcome(
            State(state.clone()),
            headers,
            AxumPath(turn_id.to_owned()),
            body,
        )
        .await
        {
            Ok(status) => status,
            Err(refusal) => refusal.status,
        },
    )
}

pub(super) const RUNTIME: (&str, &str) = ("test-runtime", "test-only");

pub(super) fn state_of(state: &AppState, turn_id: &str) -> Result<TurnState> {
    Ok(state
        .core
        .inspect_turn(turn_id)?
        .context("no such turn")?
        .state)
}

/// Every delivery waiting for the interface, claimed and acknowledged.
pub(super) fn delivered(state: &AppState) -> Result<Vec<Delivery>> {
    let mut delivered = Vec::new();
    while let Some(delivery) = state.core.claim_delivery("test-interface", "instance")? {
        let ack = DeliveryAck {
            native_ref: None,
            outcome: carillon_protocol::model::DeliveryOutcome::Delivered,
        };
        state
            .core
            .acknowledge_delivery("test-interface", "instance", delivery.id, &ack)
            .map_err(|error| anyhow!("{error:?}"))?;
        delivered.push(delivery);
    }
    Ok(delivered)
}

#[tokio::test]
async fn a_conversations_messages_all_reach_the_inbox_in_order_without_waiting_for_answers()
-> Result<()> {
    let directory = tempdir()?;
    let runtime = DurableRuntime::start().await?;
    let (_fixture, state) = durable(directory.path(), &runtime)?;
    let first = ok(submit(&state, "one", "conversation").await)?;
    let second = ok(submit(&state, "two", "conversation").await)?;
    ok(submit(&state, "three", "elsewhere").await)?;
    for _ in 0..3 {
        assert_eq!(dispatch(&state).await?, WorkerStep::Advanced);
    }
    assert_eq!(dispatch(&state).await?, WorkerStep::Idle);
    assert_eq!(runtime.handed(), ["one", "two", "three"]);
    let request = runtime.request(0)?;
    assert_eq!(request.correlation_id, first.turn_id);
    for turn in [&first.turn_id, &second.turn_id] {
        assert_eq!(state_of(&state, turn)?, TurnState::Running);
    }
    // The second message's answer may come first: the runtime orders them.
    let answer = json!({"outcome": "answered", "output": "Second first."});
    assert_eq!(
        settle(&state, RUNTIME, &second.turn_id, &answer).await?,
        StatusCode::NO_CONTENT
    );
    assert_eq!(state_of(&state, &first.turn_id)?, TurnState::Running);
    assert_eq!(
        delivered(&state)?
            .iter()
            .map(|delivery| delivery.content.as_str())
            .collect::<Vec<_>>(),
        ["Second first."]
    );
    Ok(())
}

#[tokio::test]
async fn a_turn_the_runtime_did_not_take_stays_queued_and_is_handed_over_again() -> Result<()> {
    let directory = tempdir()?;
    let runtime = DurableRuntime::start().await?;
    let (mut fixture, state) = durable(directory.path(), &runtime)?;
    let first = ok(submit(&state, "one", "conversation").await)?;
    ok(submit(&state, "two", "elsewhere").await)?;
    let later = [
        StatusCode::INTERNAL_SERVER_ERROR,
        StatusCode::SERVICE_UNAVAILABLE,
        StatusCode::REQUEST_TIMEOUT,
        StatusCode::TOO_MANY_REQUESTS,
    ];
    // An inbox that is not installed as core expects is no fault of the turn.
    let misinstalled = [
        StatusCode::UNAUTHORIZED,
        StatusCode::FORBIDDEN,
        StatusCode::NOT_FOUND,
        StatusCode::METHOD_NOT_ALLOWED,
        StatusCode::OK,
    ];
    runtime.answer(
        later
            .into_iter()
            .chain(misinstalled)
            .map(|status| (status, json!({}))),
    );
    for _ in later {
        assert!(matches!(
            dispatch(&state).await,
            Err(Stalled::Inbox(InboxFailure::Later(_)))
        ));
        assert_eq!(state_of(&state, &first.turn_id)?, TurnState::Queued);
    }
    for _ in misinstalled {
        assert!(matches!(
            dispatch(&state).await,
            Err(Stalled::Inbox(InboxFailure::Misinstalled(_)))
        ));
        assert_eq!(state_of(&state, &first.turn_id)?, TurnState::Queued);
    }
    // Nothing behind it overtakes it.
    assert_eq!(runtime.handed(), ["one"; 9]);
    assert_eq!(dispatch(&state).await?, WorkerStep::Advanced);
    assert_eq!(state_of(&state, &first.turn_id)?, TurnState::Running);
    assert_eq!(runtime.request(9)?.correlation_id, first.turn_id);
    // A runtime core cannot reach has not taken the next one either.
    fixture.config.value.providers[1].url = "http://127.0.0.1:9".to_owned();
    let unreachable = configured(&fixture);
    assert!(matches!(
        dispatch(&unreachable).await,
        Err(Stalled::Inbox(InboxFailure::Later(_)))
    ));
    assert_eq!(unreachable.core.turn_counts()?.pending, 1);
    Ok(())
}

#[tokio::test]
async fn a_refused_turn_is_interrupted_with_the_runtimes_explanation() -> Result<()> {
    let directory = tempdir()?;
    let runtime = DurableRuntime::start().await?;
    let (_fixture, state) = durable(directory.path(), &runtime)?;
    let refused = ok(submit(&state, "one", "conversation").await)?;
    let unexplained = ok(submit(&state, "two", "conversation").await)?;
    let next = ok(submit(&state, "three", "conversation").await)?;
    runtime.answer([
        (
            StatusCode::UNPROCESSABLE_ENTITY,
            json!({"error": "That agent is not set up here."}),
        ),
        (StatusCode::CONFLICT, json!({})),
    ]);
    for _ in 0..3 {
        assert_eq!(dispatch(&state).await?, WorkerStep::Advanced);
    }
    assert_eq!(state_of(&state, &refused.turn_id)?, TurnState::Interrupted);
    assert_eq!(
        state_of(&state, &unexplained.turn_id)?,
        TurnState::Interrupted
    );
    assert_eq!(state_of(&state, &next.turn_id)?, TurnState::Running);
    let notices = delivered(&state)?;
    assert_eq!(
        notices
            .iter()
            .map(|notice| (notice.turn_id.as_str(), notice.turn_state))
            .collect::<Vec<_>>(),
        [
            (refused.turn_id.as_str(), TurnState::Interrupted),
            (unexplained.turn_id.as_str(), TurnState::Interrupted),
        ]
    );
    assert!(
        notices[0]
            .content
            .starts_with("That turn was interrupted: That agent is not set up here.\n"),
        "{}",
        notices[0].content
    );
    assert!(
        notices[1]
            .content
            .starts_with("That turn was interrupted. ")
    );
    // Its refusal is the outcome: a late answer cannot replace it.
    let answer = json!({"outcome": "answered", "output": "Late."});
    assert_eq!(
        settle(&state, RUNTIME, &refused.turn_id, &answer).await?,
        StatusCode::CONFLICT
    );
    Ok(())
}

#[tokio::test]
async fn a_restart_hands_held_turns_over_again_and_a_late_outcome_is_delivered_once() -> Result<()>
{
    let directory = tempdir()?;
    let runtime = DurableRuntime::start().await?;
    let (fixture, state) = durable(directory.path(), &runtime)?;
    let held = ok(submit(&state, "one", "conversation").await)?;
    assert_eq!(dispatch(&state).await?, WorkerStep::Advanced);
    // Handed over just before core stopped, before it recorded the 202.
    let unrecorded = ok(submit(&state, "two", "elsewhere").await)?;
    let opening = TurnOpening {
        interface_id: "test-interface".to_owned(),
        agent_id: "test-agent".to_owned(),
        audience_id: "conversation".to_owned(),
        key: "nightly".to_owned(),
        label: None,
    };
    let opened = state
        .core
        .open_turn("test-runtime", &opening)
        .map_err(|error| anyhow!("{error:?}"))?;
    let restarted = configured(&fixture);
    assert_eq!(restarted.core.recover_interrupted()?, 0);
    // Its 202 may have been lost, so it is handed over again, in order.
    assert_eq!(state_of(&restarted, &held.turn_id)?, TurnState::Queued);
    assert_eq!(state_of(&restarted, &opened.turn_id)?, TurnState::Running);
    for _ in 0..2 {
        assert_eq!(dispatch(&restarted).await?, WorkerStep::Advanced);
    }
    assert_eq!(runtime.handed(), ["one", "one", "two"]);
    assert_eq!(runtime.request(1)?.correlation_id, held.turn_id);
    assert!(delivered(&restarted)?.is_empty());
    let answer = json!({"outcome": "answered", "output": "Done after the restart."});
    for turn in [&held.turn_id, &unrecorded.turn_id, &opened.turn_id] {
        for _ in 0..2 {
            assert_eq!(
                settle(&restarted, RUNTIME, turn, &answer).await?,
                StatusCode::NO_CONTENT
            );
        }
    }
    assert_eq!(
        delivered(&restarted)?
            .iter()
            .map(|delivery| delivery.turn_id.as_str())
            .collect::<Vec<_>>(),
        [
            held.turn_id.as_str(),
            unrecorded.turn_id.as_str(),
            opened.turn_id.as_str()
        ]
    );
    assert_eq!(dispatch(&restarted).await?, WorkerStep::Idle);
    Ok(())
}

#[tokio::test]
async fn a_drain_ends_while_turns_are_held_and_outcomes_are_recorded_during_it() -> Result<()> {
    let directory = tempdir()?;
    let runtime = DurableRuntime::start().await?;
    let (_fixture, state) = durable(directory.path(), &runtime)?;
    let held = ok(submit(&state, "one", "conversation").await)?;
    let stopping = CancellationToken::new();
    let supervisor = tokio::spawn(workers::supervise(
        state.clone(),
        state.workers.clone(),
        stopping.clone(),
    ));
    tokio::time::timeout(Duration::from_secs(10), async {
        while state_of(&state, &held.turn_id)? != TurnState::Running {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        Ok::<_, anyhow::Error>(())
    })
    .await??;
    let lease = Lease { lease_ms: 30_000 };
    let holder = ok(maintenance::begin(State(state.clone()), operator()?, Json(lease)).await)?
        .0
        .holder;
    let drain = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let Json(drain) =
                ok(maintenance::renew(State(state.clone()), operator()?, AxumPath(holder)).await)?;
            if drain.workers == Some(0) {
                return Ok::<_, anyhow::Error>(drain);
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await??;
    assert_eq!(drain.runtimes.len(), 1);
    assert_eq!(drain.runtimes[0].unready, None);
    assert_eq!(state_of(&state, &held.turn_id)?, TurnState::Running);
    // Paused, core still records what the runtime holds.
    let stopped = json!({"outcome": "stopped"});
    assert_eq!(
        settle(&state, RUNTIME, &held.turn_id, &stopped).await?,
        StatusCode::NO_CONTENT
    );
    let delivery = delivered(&state)?;
    assert_eq!(delivery.len(), 1);
    assert_eq!(delivery[0].silence, Some(Silence::Stopped));
    // Queued meanwhile, a message waits for the pause to end.
    ok(submit(&state, "two", "conversation").await)?;
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(runtime.handed(), ["one"]);
    stopping.cancel();
    supervisor.await?;
    Ok(())
}

#[test]
fn a_dispatcher_backs_off_up_to_a_minute_and_logs_each_new_failure_once() {
    let unavailable = || Stalled::Inbox(InboxFailure::Later(anyhow!("unreachable")));
    let mut failing = runtime::Failing::default();
    let waits = (0..8)
        .map(|_| failing.after(&unavailable()))
        .collect::<Vec<_>>();
    assert_eq!(
        waits
            .iter()
            .map(|(wait, _)| wait.as_secs())
            .collect::<Vec<_>>(),
        [1, 2, 4, 8, 16, 32, 60, 60]
    );
    assert_eq!(waits.iter().filter(|(_, new)| *new).count(), 1);
    // Another kind of failure is logged again, without starting over.
    let misinstalled = Stalled::Inbox(InboxFailure::Misinstalled(anyhow!("401")));
    assert_eq!(
        failing.after(&misinstalled),
        (Duration::from_secs(60), true)
    );
    let storage = Stalled::Core(anyhow!("database is locked"));
    assert_eq!(failing.after(&storage), (Duration::from_secs(60), true));
    assert!(failing.recovered());
    assert!(!failing.recovered());
    assert_eq!(
        failing.after(&unavailable()),
        (Duration::from_secs(1), true)
    );
}
