//! The registry applied while core serves: its classification, the apply
//! operation, and the dispatchers following it, against a runtime whose
//! hand-overs wait for the case to release them.

use std::{collections::HashMap, fs, path::Path, sync::Mutex};

use anyhow::Context as _;
use tempfile::tempdir;
use tokio::sync::oneshot;

use super::{Fixture, configured, installation};
use crate::application::http::{registry::Difference, *};
use carillon_protocol::{model::RuntimeTurnRequest, turns::TurnOutcome};

/// A runtime whose inbox keeps each hand-over waiting until the case
/// releases it, then takes the turn, and records what it was handed.
#[derive(Clone)]
pub(super) struct HeldRuntime {
    url: String,
    held: Arc<Mutex<HashMap<String, oneshot::Sender<()>>>>,
    /// The input of every turn it was handed, in order.
    given: Arc<Mutex<Vec<String>>>,
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
            "/v1/inbox",
            post(move |Json(request): Json<RuntimeTurnRequest>| {
                let (held, given) = (held.clone(), given.clone());
                async move {
                    let (release, released) = oneshot::channel();
                    lock(&given).push(request.input.clone());
                    lock(&held).insert(request.input, release);
                    let _ = released.await;
                    (StatusCode::ACCEPTED, Json(json!({})))
                }
            }),
        );
        tokio::spawn(async move { axum::serve(listener, router).await });
        Ok(runtime)
    }

    /// Waits until it holds `count` hand-overs, and returns their inputs.
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
        .with_context(|| format!("the runtime never held {count} hand-overs"))
    }

    /// Takes the turn handed over with `input`.
    pub(super) fn release(&self, input: &str) -> Result<()> {
        lock(&self.held)
            .remove(input)
            .with_context(|| format!("the runtime holds no hand-over {input}"))?
            .send(())
            .map_err(|()| anyhow!("the hand-over of {input} was abandoned"))
    }

    pub(super) fn given(&self) -> Vec<String> {
        lock(&self.given).clone()
    }

    /// Takes every turn handed over and answers every turn it holds until
    /// core's counts are `wanted`.
    pub(super) async fn drain(&self, state: &AppState, wanted: (u64, u64, u64)) -> Result<()> {
        tokio::time::timeout(Duration::from_secs(10), async {
            while counts(state)? != wanted {
                let held = lock(&self.held).keys().cloned().collect::<Vec<_>>();
                for input in held {
                    self.release(&input)?;
                }
                answer_held(state)?;
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            Ok::<_, anyhow::Error>(())
        })
        .await
        .with_context(|| format!("core never reached {wanted:?}: {:?}", counts(state)))?
    }
}

/// Answers every turn the test runtime holds, as it settles them through core.
pub(super) fn answer_held(state: &AppState) -> Result<()> {
    for running in state.core.running_turns("test-runtime")? {
        let answer = TurnOutcome::Answered {
            output: format!("reply to {}", running.request.input),
            activity: None,
        };
        state
            .core
            .settle("test-runtime", &running.request.correlation_id, answer)?;
    }
    Ok(())
}

/// An installation whose runtime is `runtime`, its registry file written
/// out as core would read it, and core serving it.
pub(super) fn serving(directory: &Path, runtime: &HeldRuntime) -> Result<(Fixture, AppState)> {
    let mut fixture = installation(directory)?;
    fixture.config.value.providers[1]
        .url
        .clone_from(&runtime.url);
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
            channel_id: "test-channel".to_owned(),
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

/// Waits until the supervisor reports the test runtime's dispatcher `wanted`.
pub(super) async fn workers_are(state: &AppState, wanted: Dispatcher) -> Result<()> {
    let mut census = state.workers.subscribe();
    tokio::time::timeout(
        Duration::from_secs(10),
        census.wait_for(|census: &Census| census.runtimes.get("test-runtime") == Some(&wanted)),
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
fn only_runtime_availability_applies_live() -> Result<()> {
    let directory = tempdir()?;
    let fixture = installation(directory.path())?;
    let serving = &fixture.config.value;
    let mut file = serving.clone();
    file.providers[1].enabled = false;
    assert_eq!(
        Difference::between(serving, &file),
        Difference {
            live: vec!["providers.test-runtime.enabled".to_owned()],
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
    assert_eq!(difference.live.len(), 1);
    Ok(())
}

#[tokio::test]
async fn an_apply_serves_live_changes_whole_or_nothing_for_the_operator_alone() -> Result<()> {
    let directory = tempdir()?;
    let runtime = HeldRuntime::start().await?;
    let (mut fixture, state) = serving(directory.path(), &runtime)?;
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
    let (fixture, state) = serving(directory.path(), &runtime)?;
    let serving = state.config();
    // The runtime entry names a setting the registry does not define.
    let path = fixture.config.root.join("carillon.toml");
    let file = fs::read_to_string(&path)?;
    let misspelt = file.replace("role = \"runtime\"", "role = \"runtime\"\nenabeld = false");
    assert_ne!(misspelt, file);
    fs::write(&path, misspelt)?;
    let refused = apply(&state)
        .await
        .err()
        .context("applied a file with a misspelt setting")?;
    assert_eq!(refused.status, StatusCode::UNPROCESSABLE_ENTITY);
    assert!(
        refused.message.contains("unknown field `enabeld`"),
        "{}",
        refused.message
    );
    assert!(Arc::ptr_eq(&state.config(), &serving));
    let Json(status) = ok(registry_status(State(state.clone()), operator()?).await)?;
    let FileState::Invalid { error } = status.file else {
        anyhow::bail!("a misspelt file read as a registry");
    };
    assert!(error.contains("enabeld"), "{error}");
    Ok(())
}

#[tokio::test]
async fn a_disabled_runtime_settles_what_it_holds_and_is_handed_its_queue_once_enabled()
-> Result<()> {
    let directory = tempdir()?;
    let runtime = HeldRuntime::start().await?;
    let (mut fixture, state) = serving(directory.path(), &runtime)?;
    ok(submit(&state, "first", "one").await)?;
    ok(submit(&state, "queued", "two").await)?;
    let stopping = CancellationToken::new();
    let supervisor = tokio::spawn(workers::supervise(
        state.clone(),
        state.workers.clone(),
        stopping.clone(),
    ));
    runtime.holding(1).await?;
    assert_eq!(runtime.given(), ["first"]);
    // Disabled while it hands a turn over: its dispatcher retires, and ends
    // once that hand-over has.
    edit(&mut fixture, |provider| provider.enabled = false)?;
    ok(apply(&state).await)?;
    workers_are(&state, Dispatcher::Retiring).await?;
    let refused = submit(&state, "refused", "three")
        .await
        .err()
        .context("admitted while disabled")?;
    assert_eq!(refused.status, StatusCode::CONFLICT);
    // Enabled again before that hand-over ends: no second dispatcher starts
    // beside the one retiring.
    edit(&mut fixture, |provider| provider.enabled = true)?;
    ok(apply(&state).await)?;
    workers_are(&state, Dispatcher::Retiring).await?;
    // Disabled again; the hand-over ends, and the runtime still settles the
    // turn it took.
    edit(&mut fixture, |provider| provider.enabled = false)?;
    ok(apply(&state).await)?;
    runtime.release("first")?;
    workers_are(&state, Dispatcher::Stopped).await?;
    assert_eq!(counts(&state)?, (1, 1, 0));
    answer_held(&state)?;
    assert_eq!(counts(&state)?, (1, 0, 1));
    assert_eq!(state.core.delivery_counts()?.pending, 1);
    // Enabled, its queued turn is handed over, once.
    edit(&mut fixture, |provider| provider.enabled = true)?;
    ok(apply(&state).await)?;
    runtime.drain(&state, (0, 0, 2)).await?;
    assert_eq!(runtime.given(), ["first", "queued"]);
    workers_are(&state, Dispatcher::Dispatching).await?;
    stopping.cancel();
    supervisor.await?;
    Ok(())
}

#[tokio::test]
async fn core_and_its_workers_follow_one_serving_registry() -> Result<()> {
    let directory = tempdir()?;
    let runtime = HeldRuntime::start().await?;
    let (mut fixture, state) = serving(directory.path(), &runtime)?;
    let stopping = CancellationToken::new();
    let supervisor = tokio::spawn(workers::supervise(
        state.clone(),
        state.workers.clone(),
        stopping.clone(),
    ));
    for enabled in [true, false, true] {
        edit(&mut fixture, |provider| provider.enabled = enabled)?;
        ok(apply(&state).await)?;
        let config = state.config();
        let provider = config.provider("test-runtime", ProviderRole::Runtime)?;
        assert_eq!(provider.enabled, enabled);
        // Core's engine has the derived facts, and nothing more.
        let engine = state.core.query_runtime(
            "test-interface",
            &ConversationQuery {
                agent_id: "test-agent".to_owned(),
                audience_id: None,
            },
        );
        assert_eq!(
            engine.err().map(|error| error.to_string()),
            (!enabled).then(|| "conversation runtime provider is disabled".to_owned())
        );
        let wanted = if enabled {
            Dispatcher::Dispatching
        } else {
            Dispatcher::Stopped
        };
        workers_are(&state, wanted).await?;
    }
    stopping.cancel();
    supervisor.await?;
    Ok(())
}
