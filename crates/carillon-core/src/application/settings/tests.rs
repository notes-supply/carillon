use std::sync::{Arc, Mutex, PoisonError};

use anyhow::{Context as _, Result, anyhow};
use axum::{
    Json, Router,
    extract::{Path as AxumPath, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use carillon_installation::config::{LoadedConfig, ProviderRole};
use carillon_protocol::settings::{
    Change, Choice, Page, Revision, RuntimePart, RuntimeSettings, Setting, SettingsRequest,
    Subject, Value,
};
use serde_json::json;

use super::{Edit, change, edits, render, settings, summary};
use crate::application::admin::{Runtime, assistant, initialize};

fn revision(number: usize) -> Revision {
    Revision::try_from(format!("{number:064x}")).unwrap_or_else(|_| unreachable!())
}

fn setting(
    id: &str,
    name: &str,
    choices: &[(&str, &str)],
    value: Option<&str>,
    inherited: bool,
) -> Setting {
    Setting {
        id: id.to_owned(),
        name: name.to_owned(),
        choices: choices
            .iter()
            .map(|&(id, name)| Choice {
                id: id.to_owned(),
                name: name.to_owned(),
                group: None,
            })
            .collect(),
        value: value.map(|id| Value::One { id: id.to_owned() }),
        inherited,
        revision: revision(0),
    }
}

const MODELS: [(&str, &str); 2] = [
    ("openai-codex/gpt-6.1-sol", "GPT-6.1 Sol"),
    ("openai-codex/gpt-6.1-mini", "GPT-6.1 Mini"),
];

fn home(generation: usize) -> Page {
    let mut page = Page {
        title: "#home".to_owned(),
        subject: Subject::Channel {
            interface_id: "discord-private".to_owned(),
            channel_id: "300".to_owned(),
            agent_id: "assistant".to_owned(),
        },
        interface: vec![
            setting(
                "agent",
                "Agent",
                &[("assistant", "assistant")],
                Some("assistant"),
                false,
            ),
            setting(
                "listens",
                "Listens",
                &[("always", "always")],
                Some("always"),
                false,
            ),
            setting(
                "replies",
                "Replies",
                &[("thread", "threads")],
                Some("thread"),
                false,
            ),
        ],
        runtime: RuntimePart::Settings(RuntimeSettings {
            settings: vec![
                setting("model", "Model", &MODELS, Some(MODELS[0].0), true),
                setting(
                    "model.thinking",
                    "Thinking",
                    &[("low", "Low"), ("medium", "Medium"), ("high", "High")],
                    Some("medium"),
                    true,
                ),
            ],
        }),
    };
    renumber(&mut page, generation);
    page
}

fn lines_mut(page: &mut Page) -> impl Iterator<Item = &mut Setting> {
    let runtime = match &mut page.runtime {
        RuntimePart::Settings(part) => part.settings.as_mut_slice(),
        RuntimePart::Unavailable { .. } => &mut [],
    };
    page.interface.iter_mut().chain(runtime)
}

fn renumber(page: &mut Page, generation: usize) {
    for (index, line) in lines_mut(page).enumerate() {
        line.revision = revision(generation * 100 + index);
    }
}

#[test]
fn a_page_is_drawn_as_its_owners_return_it() {
    assert_eq!(
        render(&home(0)),
        "#home\n\
         Agent     assistant\n\
         Listens   always\n\
         Replies   threads\n\
         Model     GPT-6.1 Sol · default\n\
         Thinking  Medium · default\n"
    );
    let page = Page {
        title: "assistant".to_owned(),
        subject: Subject::Agent {
            agent_id: "assistant".to_owned(),
        },
        interface: Vec::new(),
        runtime: RuntimePart::Settings(RuntimeSettings {
            settings: vec![
                setting("model", "Model", &MODELS, Some(MODELS[1].0), false),
                setting("speed", "Speed", &[], Some("fast"), true),
                Setting {
                    value: Some(Value::Many {
                        ids: vec!["web".to_owned(), "files".to_owned()],
                    }),
                    ..setting("extensions", "Extensions", &[("web", "Web")], None, false)
                },
                setting("busy", "Busy", &[("queue", "Queue")], None, true),
            ],
        }),
    };
    assert_eq!(
        render(&page),
        "assistant\n\
         Model       GPT-6.1 Mini\n\
         Speed       fast · default\n\
         Extensions  Web, files\n\
         Busy\n"
    );
    let page = Page {
        runtime: RuntimePart::Unavailable {
            reason: "The runtime is not reachable right now.".to_owned(),
        },
        ..home(0)
    };
    assert_eq!(
        render(&page),
        "⚠ The runtime is not reachable right now.\n\
         #home\n\
         Agent    assistant\n\
         Listens  always\n\
         Replies  threads\n"
    );
}

#[test]
fn routed_channels_are_listed_one_line_each() {
    let other = Page {
        title: "#carillon-fresh-install".to_owned(),
        subject: Subject::Channel {
            interface_id: "discord-private".to_owned(),
            channel_id: "3010".to_owned(),
            agent_id: "t3-code".to_owned(),
        },
        runtime: RuntimePart::Unavailable {
            reason: "The runtime did not answer in time.".to_owned(),
        },
        ..home(0)
    };
    assert_eq!(
        summary(&[home(0), other]),
        "300   #home                    assistant, always, threads, GPT-6.1 Sol, Medium\n\
         3010  #carillon-fresh-install  assistant, always, threads  ⚠ The runtime did not answer in time.\n"
    );
}

fn words(words: &[&str]) -> Vec<String> {
    words.iter().map(|&word| word.to_owned()).collect()
}

fn set(key: &str, value: &str) -> Edit {
    Edit::Set {
        key: key.to_owned(),
        value: value.to_owned(),
    }
}

#[test]
fn changes_are_key_value_pairs_or_resets() -> Result<()> {
    assert_eq!(
        edits(&words(&["thinking=high", "reset", "model", "model=a=b"]))?,
        [
            set("thinking", "high"),
            Edit::Reset {
                key: "model".to_owned()
            },
            set("model", "a=b"),
        ]
    );
    crate::assert_refused(edits(&words(&["reset"])), "reset <key>")?;
    crate::assert_refused(
        edits(&words(&["thinking"])),
        "thinking is neither key=value nor reset <key>",
    )?;
    Ok(())
}

#[test]
fn a_key_names_a_line_and_a_value_one_of_its_choices() -> Result<()> {
    let page = home(1);
    let thinking = Change {
        setting: "model.thinking".to_owned(),
        value: Some(Value::One {
            id: "high".to_owned(),
        }),
        expected: revision(104),
    };
    for edit in [set("thinking", "high"), set("model.thinking", "HIGH")] {
        assert_eq!(change(&page, &edit)?, thinking, "{edit:?}");
    }
    for value in ["openai-codex/gpt-6.1-mini", "gpt-6.1 mini", "gpt-6.1-mini"] {
        let changed = change(&page, &set("model", value))?;
        assert_eq!(
            (changed.setting.as_str(), changed.value, changed.expected),
            (
                "model",
                Some(Value::One {
                    id: MODELS[1].0.to_owned()
                }),
                revision(103)
            )
        );
    }
    let reset = change(
        &page,
        &Edit::Reset {
            key: "thinking".to_owned(),
        },
    )?;
    assert_eq!((reset.value, reset.expected), (None, revision(104)));
    Ok(())
}

fn refusal(result: Result<Change>) -> String {
    result
        .err()
        .map(|error| error.to_string())
        .unwrap_or_default()
}

#[test]
fn an_unknown_key_or_value_lists_what_the_line_accepts() {
    let page = home(0);
    assert_eq!(
        refusal(change(&page, &set("temperature", "2"))),
        "temperature is not a setting of #home. Its settings are:\n\
         agent           Agent\n\
         listens         Listens\n\
         replies         Replies\n\
         model           Model\n\
         model.thinking  Thinking"
    );
    let choices = "openai-codex/gpt-6.1-sol   GPT-6.1 Sol\n\
                   openai-codex/gpt-6.1-mini  GPT-6.1 Mini";
    assert_eq!(
        refusal(change(&page, &set("model", "x"))),
        format!("x is not a choice of Model. Its choices are:\n{choices}")
    );
    assert_eq!(
        refusal(change(&page, &set("model", ""))),
        format!("Model needs a value. Its choices are:\n{choices}")
    );
    let mut shared = home(0);
    lines_mut(&mut shared)
        .filter(|line| line.id == "model")
        .flat_map(|line| &mut line.choices)
        .for_each(|choice| "Sonnet".clone_into(&mut choice.name));
    assert!(
        refusal(change(&shared, &set("model", "sonnet")))
            .starts_with("sonnet is not a choice of Model."),
        "a name two choices share names neither"
    );
}

struct Stub {
    token: String,
    home: Page,
    generation: usize,
    agent: Option<String>,
    calls: Vec<serde_json::Value>,
}

type Shared = Arc<Mutex<Stub>>;

fn lock(stub: &Shared) -> std::sync::MutexGuard<'_, Stub> {
    stub.lock().unwrap_or_else(PoisonError::into_inner)
}

fn refused(status: StatusCode, error: &str) -> Response {
    (status, Json(json!({ "error": error }))).into_response()
}

impl Stub {
    fn authorized(&self, headers: &HeaderMap) -> bool {
        headers
            .get(http::header::AUTHORIZATION)
            .and_then(|value| value.to_str().ok())
            == Some(format!("Bearer {}", self.token).as_str())
    }

    fn change(&mut self, change: Change) -> Response {
        let Some(line) = lines_mut(&mut self.home).find(|line| line.id == change.setting) else {
            return refused(StatusCode::UNPROCESSABLE_ENTITY, "No such setting.");
        };
        if line.revision != change.expected {
            return refused(
                StatusCode::CONFLICT,
                &format!("{} changed meanwhile.", line.name),
            );
        }
        if !line.id.starts_with("model") {
            return refused(
                StatusCode::UNPROCESSABLE_ENTITY,
                &format!("{} is read-only.", line.name),
            );
        }
        line.inherited = change.value.is_none();
        if change.value.is_some() {
            line.value = change.value;
        }
        self.generation += 1;
        renumber(&mut self.home, self.generation);
        Json(self.home.clone()).into_response()
    }

    fn agent_settings(&self) -> RuntimeSettings {
        let mut model = setting("model", "Model", &MODELS, self.agent.as_deref(), false);
        model.revision = revision(self.generation);
        RuntimeSettings {
            settings: vec![model],
        }
    }
}

fn stub_router(stub: Shared) -> Router {
    Router::new()
        .route(
            "/v1/channels",
            get(
                |State(stub): State<Shared>, headers: HeaderMap| async move {
                    let stub = lock(&stub);
                    if !stub.authorized(&headers) {
                        return StatusCode::FORBIDDEN.into_response();
                    }
                    Json(vec![stub.home.clone()]).into_response()
                },
            ),
        )
        .route(
            "/v1/channels/{channel}/settings",
            get(
                |State(stub): State<Shared>,
                 headers: HeaderMap,
                 AxumPath(channel): AxumPath<String>| async move {
                    let stub = lock(&stub);
                    match (stub.authorized(&headers), channel.as_str()) {
                        (false, _) => StatusCode::FORBIDDEN.into_response(),
                        (true, "300") => Json(stub.home.clone()).into_response(),
                        (true, _) => refused(StatusCode::NOT_FOUND, "not a routed channel"),
                    }
                },
            )
            .put(
                |State(stub): State<Shared>,
                 headers: HeaderMap,
                 AxumPath(channel): AxumPath<String>,
                 Json(change): Json<Change>| async move {
                    let mut stub = lock(&stub);
                    if !stub.authorized(&headers) || channel != "300" {
                        return StatusCode::FORBIDDEN.into_response();
                    }
                    stub.calls.push(json!({ "put": channel, "change": change }));
                    stub.change(change)
                },
            ),
        )
        .route(
            "/v1/settings",
            post(
                |State(stub): State<Shared>,
                 headers: HeaderMap,
                 Json(request): Json<SettingsRequest>| async move {
                    let mut stub = lock(&stub);
                    if !stub.authorized(&headers) {
                        return StatusCode::FORBIDDEN.into_response();
                    }
                    stub.calls.push(json!({ "post": request }));
                    if let Some(change) = request.change {
                        stub.agent = match change.value {
                            Some(Value::One { id }) => Some(id),
                            _ => None,
                        };
                        stub.generation += 1;
                    }
                    Json(stub.agent_settings()).into_response()
                },
            ),
        )
        .with_state(stub)
}

async fn installation(
    directory: &std::path::Path,
) -> Result<(
    LoadedConfig,
    Shared,
    tokio::task::JoinHandle<std::io::Result<()>>,
)> {
    let path = initialize(
        &directory.join("home"),
        44_100,
        Runtime::Pi,
        vec![assistant(directory)],
    )?;
    let mut config = LoadedConfig::load(&path)?;
    let stub = Arc::new(Mutex::new(Stub {
        token: config.credential("operator")?,
        home: home(0),
        generation: 0,
        agent: None,
        calls: Vec::new(),
    }));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let address = listener.local_addr()?;
    config.value.core.listen = address;
    config
        .value
        .providers
        .iter_mut()
        .find(|provider| provider.role == ProviderRole::Interface)
        .context("no interface provider")?
        .url = format!("http://{address}");
    let router = stub_router(Arc::clone(&stub));
    let served = tokio::spawn(async move { axum::serve(listener, router).await });
    Ok((config, stub, served))
}

async fn run(config: &LoadedConfig, agent: Option<&str>, arguments: &[&str]) -> Result<String> {
    settings(config, agent.map(str::to_owned), &words(arguments)).await
}

fn calls(stub: &Shared) -> Vec<serde_json::Value> {
    std::mem::take(&mut lock(stub).calls)
}

#[tokio::test]
async fn channels_are_listed_shown_and_changed_left_to_right() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let (config, stub, served) = installation(directory.path()).await?;
    assert_eq!(
        run(&config, None, &[]).await?,
        "300  #home  assistant, always, threads, GPT-6.1 Sol, Medium\n"
    );
    assert_eq!(run(&config, None, &["300"]).await?, render(&home(0)));
    assert_eq!(
        run(
            &config,
            None,
            &["300", "thinking=High", "model=gpt-6.1 mini"]
        )
        .await?,
        "#home\n\
         Agent     assistant\n\
         Listens   always\n\
         Replies   threads\n\
         Model     GPT-6.1 Mini\n\
         Thinking  High\n"
    );
    run(&config, None, &["300", "reset", "thinking"]).await?;
    let change = |setting: &str, value: Option<&str>, expected: usize| {
        json!({
            "put": "300",
            "change": {
                "setting": setting,
                "value": value.map(|id| json!({"kind": "one", "id": id})),
                "expected": revision(expected),
            },
        })
    };
    assert_eq!(
        calls(&stub),
        [
            change("model.thinking", Some("high"), 4),
            change("model", Some(MODELS[1].0), 103),
            change("model.thinking", None, 204),
        ],
        "each change names the revision of the page read before it"
    );
    crate::assert_refused(
        run(&config, None, &["999"]).await,
        "no interface routes channel 999",
    )?;
    served.abort();
    Ok(())
}

#[tokio::test]
async fn the_first_refusal_stops_the_rest_and_an_unknown_change_is_never_sent() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let (config, stub, served) = installation(directory.path()).await?;
    let failure = |result: Result<String>| {
        result
            .err()
            .map(|error| format!("{error:#}"))
            .unwrap_or_default()
    };
    assert_eq!(
        failure(run(&config, None, &["300", "agent=assistant", "thinking=low"]).await),
        "agent: Agent is read-only."
    );
    assert_eq!(calls(&stub).len(), 1, "a change after a refusal was sent");
    assert_eq!(
        failure(
            run(
                &config,
                None,
                &["300", "thinking=low", "agent=assistant", "model=echo"]
            )
            .await
        ),
        "agent: Agent is read-only.\nAlready applied: thinking."
    );
    assert_eq!(calls(&stub).len(), 2, "a change after a refusal was sent");
    for (change, refusal) in [
        ("thinking=extreme", "extreme is not a choice of Thinking."),
        ("temperature=2", "temperature is not a setting of #home."),
    ] {
        assert!(
            failure(run(&config, None, &["300", change]).await).starts_with(refusal),
            "{change}"
        );
    }
    assert_eq!(calls(&stub), Vec::<serde_json::Value>::new());
    lock(&stub).token = "rotated".to_owned();
    crate::assert_refused(run(&config, None, &["300"]).await, "refused with 403")?;
    served.abort();
    Ok(())
}

#[tokio::test]
async fn an_agents_page_is_read_and_changed_through_core() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let (config, stub, served) = installation(directory.path()).await?;
    assert_eq!(
        run(&config, Some("assistant"), &[]).await?,
        "assistant\nModel\n"
    );
    crate::assert_refused(
        run(&config, Some("assistant"), &["model=x"]).await,
        "openai-codex/gpt-6.1-mini  GPT-6.1 Mini",
    )?;
    assert_eq!(
        run(
            &config,
            Some("assistant"),
            &["model=openai-codex/gpt-6.1-sol"]
        )
        .await?,
        "assistant\nModel  GPT-6.1 Sol\n"
    );
    let subject = json!({"kind": "agent", "agent_id": "assistant"});
    assert_eq!(
        calls(&stub),
        [
            json!({"post": {"subject": subject}}),
            json!({"post": {"subject": subject}}),
            json!({"post": {"subject": subject}}),
            json!({"post": {
                "subject": subject,
                "change": {
                    "setting": "model",
                    "value": {"kind": "one", "id": MODELS[0].0},
                    "expected": revision(0),
                },
            }}),
        ]
    );
    served.abort();
    Ok(())
}

#[tokio::test]
async fn an_unreachable_owner_is_named() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let (config, _stub, served) = installation(directory.path()).await?;
    served.abort();
    let _ = served.await;
    let failure = run(&config, None, &["300"])
        .await
        .err()
        .ok_or_else(|| anyhow!("an unreachable provider answered"))?;
    assert!(
        format!("{failure:#}").contains("/v1/channels/300/settings did not answer"),
        "{failure:#}"
    );
    Ok(())
}

#[test]
fn a_change_while_the_runtime_is_out_names_the_outage() {
    let mut page = home(0);
    page.runtime = RuntimePart::Unavailable {
        reason: "The runtime is not reachable right now.".to_owned(),
    };
    assert_eq!(
        refusal(change(&page, &set("thinking", "high"))),
        "thinking cannot be changed now. The runtime is not reachable right now."
    );
}
