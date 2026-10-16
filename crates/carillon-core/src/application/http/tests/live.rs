//! The registry applied while core serves: its classification, the apply
//! operation, and the runtime workers following it, against a runtime whose
//! turns wait for the case to release them.

use std::{collections::HashMap, fs, path::Path, sync::Mutex};

use anyhow::Context as _;
use tempfile::tempdir;
use tokio::sync::oneshot;

use super::{Fixture, completed, configured, installation};
use crate::application::http::{registry::Difference, *};

/// A runtime that keeps every turn until the case releases it, answering
/// with or without a follow-up cursor, and records what it was given.
#[derive(Clone)]
pub(super) struct HeldRuntime {
    url: String,
    held: Arc<Mutex<HashMap<String, oneshot::Sender<Option<Value>>>>>,
    /// Every turn it was given, in order: its input and whether it was an
    /// observation.
    given: Arc<Mutex<Vec<(String, bool)>>>,
    /// What it answers when asked whether it can restart.
    pub(super) restart: Arc<Mutex<Value>>,
}

pub(super) fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

impl HeldRuntime {
    pub(super) async fn start() -> Result<Self> {
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let runtime = Self {
            url: format!("http://{}", listener.local_addr()?),
            held: Arc::default(),
            given: Arc::default(),
            restart: Arc::new(Mutex::new(json!({"state": "ready"}))),
        };
        let (held, given, restart) = (
            runtime.held.clone(),
            runtime.given.clone(),
            runtime.restart.clone(),
        );
        let answer = move || {
            let answer = lock(&restart).clone();
            async move { Json(answer) }
        };
        let router = Router::new().route("/v1/restart", get(answer)).route(
            "/v1/turns",
            post(move |Json(request): Json<RuntimeTurnRequest>| {
                let (held, given) = (held.clone(), given.clone());
                async move {
                    let (answer, answered) = oneshot::channel();
                    lock(&given).push((request.input.clone(), request.follow_up.is_some()));
                    lock(&held).insert(request.input.clone(), answer);
                    let follow_up = answered.await.unwrap_or_default();
                    let mut response = completed(request);
                    response.follow_up = follow_up;
                    Json(response)
                }
            }),
        );
        tokio::spawn(async move { axum::serve(listener, router).await });
        Ok(runtime)
    }

    /// Waits until it holds `count` turns, and returns their inputs.
    pub(super) async fn holding(&self, count: usize) -> Result<Vec<String>> {
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                let held = lock(&self.held).keys().cloned().collect::<Vec<_>>();
                if held.len() >= count {
                    return held;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .with_context(|| format!("the runtime never held {count} turns"))
    }

    /// Answers the turn given `input`, optionally saying its work goes on.
    pub(super) fn release(&self, input: &str, follow_up: Option<Value>) -> Result<()> {
        lock(&self.held)
            .remove(input)
            .with_context(|| format!("the runtime holds no turn {input}"))?
            .send(follow_up)
            .map_err(|_| anyhow!("turn {input} was abandoned"))
    }

    pub(super) fn given(&self) -> Vec<(String, bool)> {
        lock(&self.given).clone()
    }

    pub(super) async fn drain(&self, state: &AppState, wanted: (u64, u64, u64)) -> Result<()> {
        tokio::time::timeout(Duration::from_secs(10), async {
            while counts(state)? != wanted {
                let held = lock(&self.held).keys().cloned().collect::<Vec<_>>();
                for input in held {
                    self.release(&input, None)?;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            Ok::<_, anyhow::Error>(())
        })
        .await
        .with_context(|| format!("core never reached {wanted:?}: {:?}", counts(state)))?
    }
}

/// An installation whose runtime is `runtime`, its registry file written
/// out as core would read it, and core serving it.
pub(super) fn serving(
    directory: &Path,
    runtime: &HeldRuntime,
    parallel: u32,
) -> Result<(Fixture, AppState)> {
    let mut fixture = installation(directory)?;
    let provider = &mut fixture.config.value.providers[1];
    provider.url.clone_from(&runtime.url);
    provider.parallel_turns = Some(parallel);
    write(&fixture.config)?;
    let state = configured(&fixture);
    Ok((fixture, state))
}

fn write(config: &LoadedConfig) -> Result<()> {
    fs::write(
        config.root.join("carillon.toml"),
        toml::to_string(&config.value)?,
    )?;
    Ok(())
}

/// Edits the registry file as an operator would.
pub(super) fn edit(fixture: &mut Fixture, change: impl FnOnce(&mut Provider)) -> Result<()> {
    change(&mut fixture.config.value.providers[1]);
    write(&fixture.config)
}

pub(super) fn ok<T>(answer: Result<T, ApiError>) -> Result<T> {
    answer.map_err(|refusal| anyhow!("{}: {}", refusal.status, refusal.message))
}

pub(super) fn operator() -> Result<HeaderMap> {
    let mut headers = HeaderMap::new();
    headers.insert(http::header::AUTHORIZATION, "Bearer test-operator".parse()?);
    Ok(headers)
}

pub(super) async fn apply(state: &AppState) -> Result<Applied, ApiError> {
    apply_registry(State(state.clone()), operator()?)
        .await
        .map(|Json(applied)| applied)
}

pub(super) async fn submit(
    state: &AppState,
    input: &str,
    conversation: &str,
) -> Result<EventReceipt, ApiError> {
    let mut headers = HeaderMap::new();
    headers.insert(
        PROVIDER_HEADER,
        "test-interface".parse().map_err(anyhow::Error::from)?,
    );
    headers.insert(
        http::header::AUTHORIZATION,
        "Bearer test-only".parse().map_err(anyhow::Error::from)?,
    );
    accept_event(
        State(state.clone()),
        headers,
        Json(ExternalEvent {
            event_id: input.to_owned(),
            agent_id: "test-agent".to_owned(),
            user_id: "test-user".to_owned(),
            audience_id: conversation.to_owned(),
            delivery_target_id: "test-destination".to_owned(),
            feedback_target_id: "test-feedback".to_owned(),
            content: input.to_owned(),
            native: Value::Null,
            attachments: None,
        }),
    )
    .await
    .map(|Json(receipt)| receipt)
}

/// Waits until the supervisor reports `wanted` for the test runtime.
pub(super) async fn workers_are(
    state: &AppState,
    wanted: impl Fn(&workers::RuntimeWorkers) -> bool,
) -> Result<()> {
    let mut census = state.workers.subscribe();
    tokio::time::timeout(
        Duration::from_secs(10),
        census.wait_for(|census: &Census| census.runtimes.get("test-runtime").is_some_and(&wanted)),
    )
    .await
    .with_context(|| {
        format!(
            "workers never arranged: {:?}",
            state.workers.borrow().runtimes
        )
    })??;
    Ok(())
}

pub(super) fn counts(state: &AppState) -> Result<(u64, u64, u64)> {
    let turns = state.core.turn_counts()?;
    Ok((turns.pending, turns.active, turns.completed))
}

#[test]
fn only_runtime_availability_and_concurrency_apply_live() -> Result<()> {
    let directory = tempdir()?;
    let fixture = installation(directory.path())?;
    let serving = &fixture.config.value;
    let mut file = serving.clone();
    file.providers[1].enabled = false;
    file.providers[1].parallel_turns = Some(3);
    assert_eq!(
        Difference::between(serving, &file),
        Difference {
            live: vec![
                "providers.test-runtime.enabled".to_owned(),
                "providers.test-runtime.parallel_turns".to_owned(),
            ],
            restart: Vec::new(),
        }
    );
    // An interface's flag decides whether its program serves; addresses,
    // credentials, the agent and the set of providers need a restart.
    file.providers[0].enabled = false;
    file.providers[1].url = "http://127.0.0.1:9".to_owned();
    file.providers[1].token_hash = "0".repeat(64);
    file.core.listen.set_port(9);
    file.agents[0].runtime = "elsewhere".to_owned();
    let mut added = file.agents[0].clone();
    "added-agent".clone_into(&mut added.id);
    file.agents.push(added);
    let mut extra = file.providers[1].clone();
    "another".clone_into(&mut extra.id);
    file.providers.push(extra);
    let difference = Difference::between(serving, &file);
    assert_eq!(
        difference.restart,
        [
            "core.listen",
            "agents",
            "providers.another (added)",
            "providers.test-interface.enabled",
            "providers.test-runtime.url",
            "providers.test-runtime.token_hash",
        ]
    );
    assert_eq!(difference.live.len(), 2);
    Ok(())
}

#[tokio::test]
async fn an_apply_serves_live_changes_whole_or_nothing_for_the_operator_alone() -> Result<()> {
    let directory = tempdir()?;
    let runtime = HeldRuntime::start().await?;
    let (mut fixture, state) = serving(directory.path(), &runtime, 1)?;
    // Only the operator applies.
    for headers in [HeaderMap::new(), {
        let mut wrong = HeaderMap::new();
        wrong.insert(http::header::AUTHORIZATION, "Bearer test-only".parse()?);
        wrong
    }] {
        let refused = apply_registry(State(state.clone()), headers)
            .await
            .err()
            .context("applied without the operator")?;
        assert_eq!(refused.status, StatusCode::FORBIDDEN);
    }
    // A file that also changes what needs a restart is refused whole, naming
    // it, and nothing of it serves.
    edit(&mut fixture, |provider| {
        provider.enabled = false;
        provider.url = "http://127.0.0.1:9".to_owned();
    })?;
    let refused = apply(&state)
        .await
        .err()
        .context("applied a restart-only change")?;
    assert_eq!(refused.status, StatusCode::CONFLICT);
    assert!(
        refused.message.contains("providers.test-runtime.url"),
        "{}",
        refused.message
    );
    assert!(state.config().value.providers[1].enabled);
    ok(submit(&state, "still", "one").await)?;
    // A file that does not read as a registry changes nothing either, and
    // says so without its credentials.
    fs::write(
        fixture.config.root.join("carillon.toml"),
        "data_dir = [\"broken\"]\n",
    )?;
    let refused = apply(&state)
        .await
        .err()
        .context("applied an invalid file")?;
    assert_eq!(refused.status, StatusCode::UNPROCESSABLE_ENTITY);
    assert!(!refused.message.contains("test-only"));
    let Json(status) = ok(registry_status(State(state.clone()), operator()?).await)?;
    assert!(matches!(status.file, FileState::Invalid { .. }));
    // The live change alone applies: core's engine admits nothing more for
    // the runtime, as the registry now says.
    fixture.config.value.providers[1]
        .url
        .clone_from(&runtime.url);
    edit(&mut fixture, |provider| provider.enabled = false)?;
    let Json(status) = ok(registry_status(State(state.clone()), operator()?).await)?;
    let FileState::Read(difference) = status.file else {
        anyhow::bail!("the file did not read");
    };
    assert_eq!(difference.live, ["providers.test-runtime.enabled"]);
    let applied = ok(apply(&state).await)?;
    assert_eq!(applied.changed, ["providers.test-runtime.enabled"]);
    assert!(!applied.runtimes[0].enabled);
    assert!(!state.config().value.providers[1].enabled);
    let refused = submit(&state, "later", "two")
        .await
        .err()
        .context("admitted for a disabled runtime")?;
    assert_eq!(refused.status, StatusCode::CONFLICT);
    // Applying the same file again changes nothing.
    assert!(ok(apply(&state).await)?.changed.is_empty());
    Ok(())
}

#[tokio::test]
async fn a_misspelt_setting_is_refused_rather_than_dropped() -> Result<()> {
    let directory = tempdir()?;
    let runtime = HeldRuntime::start().await?;
    let (fixture, state) = serving(directory.path(), &runtime, 2)?;
    let serving = state.config();
    // The runtime entry names a setting the registry does not define.
    let path = fixture.config.root.join("carillon.toml");
    let file = fs::read_to_string(&path)?;
    let misspelt = file.replace(
        "parallel_turns = 2",
        "parallel_turns = 2\nparallel_turn = 4",
    );
    assert_ne!(misspelt, file);
    fs::write(&path, misspelt)?;
    let refused = apply(&state)
        .await
        .err()
        .context("applied a file with a misspelt setting")?;
    assert_eq!(refused.status, StatusCode::UNPROCESSABLE_ENTITY);
    assert!(
        refused.message.contains("unknown field `parallel_turn`"),
        "{}",
        refused.message
    );
    assert!(Arc::ptr_eq(&state.config(), &serving));
    let Json(status) = ok(registry_status(State(state.clone()), operator()?).await)?;
    let FileState::Invalid { error } = status.file else {
        anyhow::bail!("a misspelt file read as a registry");
    };
    assert!(error.contains("parallel_turn"), "{error}");
    Ok(())
}

#[tokio::test]
async fn fewer_parallel_turns_let_running_turns_finish_and_then_run_fewer_at_once() -> Result<()> {
    let directory = tempdir()?;
    let runtime = HeldRuntime::start().await?;
    let (mut fixture, state) = serving(directory.path(), &runtime, 4)?;
    // Eight conversations, one of them with two messages.
    for conversation in ["a", "b", "c", "d", "e", "f", "g", "h"] {
        ok(submit(&state, &format!("{conversation}1"), conversation).await)?;
    }
    ok(submit(&state, "a2", "a").await)?;
    let stopping = CancellationToken::new();
    let supervisor = tokio::spawn(workers::supervise(
        state.clone(),
        state.workers.clone(),
        stopping.clone(),
    ));
    let first = runtime.holding(4).await?;
    workers_are(&state, |runtime| runtime.messages.claiming == 4).await?;
    edit(&mut fixture, |provider| provider.parallel_turns = Some(2))?;
    assert_eq!(
        ok(apply(&state).await)?.changed,
        ["providers.test-runtime.parallel_turns"]
    );
    // Every worker holds a turn: two are retired and settle theirs.
    workers_are(&state, |runtime| {
        runtime.messages.claiming == 2 && runtime.messages.settling == 2
    })
    .await?;
    assert_eq!(counts(&state)?, (5, 4, 0));
    for input in &first {
        runtime.release(input, None)?;
    }
    // The retired workers end; two run at once from now on.
    workers_are(&state, |runtime| {
        runtime.messages.claiming == 2 && runtime.messages.settling == 0
    })
    .await?;
    let second = runtime.holding(2).await?;
    assert_eq!(counts(&state)?, (3, 2, 4));
    for input in second {
        runtime.release(&input, None)?;
    }
    runtime.drain(&state, (0, 0, 9)).await?;
    // Each message ran once, a conversation's in order, and each answer
    // waits for its interface.
    let given = runtime.given();
    assert_eq!(given.len(), 9);
    let position = |input: &str| given.iter().position(|(given, _)| given == input);
    assert!(position("a1") < position("a2"));
    assert_eq!(state.core.delivery_counts()?.pending, 9);
    stopping.cancel();
    supervisor.await?;
    Ok(())
}

#[tokio::test]
async fn a_disabled_runtime_settles_its_turn_and_resumes_its_queue_once_enabled() -> Result<()> {
    let directory = tempdir()?;
    let runtime = HeldRuntime::start().await?;
    let (mut fixture, state) = serving(directory.path(), &runtime, 1)?;
    ok(submit(&state, "first", "one").await)?;
    ok(submit(&state, "queued", "two").await)?;
    let stopping = CancellationToken::new();
    let supervisor = tokio::spawn(workers::supervise(
        state.clone(),
        state.workers.clone(),
        stopping.clone(),
    ));
    runtime.holding(1).await?;
    assert_eq!(runtime.given(), [("first".to_owned(), false)]);
    // Disabled while its turn runs: the turn's worker settles it and claims
    // no other; idle observation workers end.
    edit(&mut fixture, |provider| provider.enabled = false)?;
    ok(apply(&state).await)?;
    workers_are(&state, |runtime| {
        runtime.messages.claiming == 0
            && runtime.messages.settling == 1
            && runtime.observations.claiming + runtime.observations.settling == 0
    })
    .await?;
    let refused = submit(&state, "refused", "three")
        .await
        .err()
        .context("admitted while disabled")?;
    assert_eq!(refused.status, StatusCode::CONFLICT);
    // Enabled again before that turn ends: no second worker claims beside
    // the one still settling.
    edit(&mut fixture, |provider| provider.enabled = true)?;
    ok(apply(&state).await)?;
    workers_are(&state, |runtime| {
        runtime.messages.claiming == 0
            && runtime.messages.settling == 1
            && runtime.observations.claiming == workers::OBSERVATION_WORKERS
    })
    .await?;
    // Disabled again; the running turn ends saying its work goes on.
    edit(&mut fixture, |provider| provider.enabled = false)?;
    ok(apply(&state).await)?;
    runtime.release("first", Some(json!({"cursor": 1})))?;
    workers_are(&state, |runtime| {
        *runtime == workers::RuntimeWorkers::default()
    })
    .await?;
    // Its answer waits for the interface; the queued message and the
    // observation of the delegated work wait for the runtime.
    assert_eq!(counts(&state)?, (2, 0, 1));
    assert_eq!(state.core.delivery_counts()?.pending, 1);
    edit(&mut fixture, |provider| provider.enabled = true)?;
    ok(apply(&state).await)?;
    runtime.holding(2).await?;
    runtime.drain(&state, (0, 0, 3)).await?;
    // Each turn ran once: the message, the queued one and the observation.
    let mut given = runtime.given();
    given.sort();
    assert_eq!(
        given,
        [
            (String::new(), true),
            ("first".to_owned(), false),
            ("queued".to_owned(), false),
        ]
    );
    workers_are(&state, |runtime| {
        runtime.messages.claiming == 1
            && runtime.observations.claiming == workers::OBSERVATION_WORKERS
            && runtime.messages.settling + runtime.observations.settling == 0
    })
    .await?;
    stopping.cancel();
    supervisor.await?;
    Ok(())
}

#[tokio::test]
async fn core_and_its_workers_follow_one_serving_registry() -> Result<()> {
    let directory = tempdir()?;
    let runtime = HeldRuntime::start().await?;
    let (mut fixture, state) = serving(directory.path(), &runtime, 3)?;
    let stopping = CancellationToken::new();
    let supervisor = tokio::spawn(workers::supervise(
        state.clone(),
        state.workers.clone(),
        stopping.clone(),
    ));
    for (enabled, parallel) in [(true, 3), (false, 3), (true, 1), (true, 2)] {
        edit(&mut fixture, |provider| {
            provider.enabled = enabled;
            provider.parallel_turns = Some(parallel);
        })?;
        ok(apply(&state).await)?;
        let config = state.config();
        let provider = config.provider("test-runtime", ProviderRole::Runtime)?;
        assert_eq!(provider.enabled, enabled);
        // Core's engine has the derived facts, and nothing more.
        let engine = state.core.query_runtime(
            "test-interface",
            &ModelQuery {
                agent_id: "test-agent".to_owned(),
                audience_id: None,
            },
        );
        assert_eq!(
            engine.err().map(|error| error.to_string()),
            (!enabled).then(|| "conversation runtime provider is disabled".to_owned())
        );
        let wanted = if enabled { parallel as usize } else { 0 };
        workers_are(&state, |runtime| runtime.messages.claiming == wanted).await?;
    }
    stopping.cancel();
    supervisor.await?;
    Ok(())
}
