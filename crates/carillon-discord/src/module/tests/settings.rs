//! `/settings` on the release module, run through the provider's Wasm host
//! against a fake core.

use std::sync::Mutex;

use axum::{
    Json,
    extract::State,
    response::{IntoResponse, Response},
    routing::post,
};
use carillon_discord_module::contract::Reply;
use carillon_protocol::settings::{
    Choice, Revision, RuntimeSettings, Setting, SettingsRequest, Subject, Value as SettingValue,
};

use super::*;

/// The runtime behind core's `/v1/settings`: a model line and a thinking
/// line, both inherited until changed. It records every request, and
/// refuses changes with `refusal` while one is set.
struct Runtime {
    settings: RuntimeSettings,
    requests: Vec<SettingsRequest>,
    refusal: Option<(StatusCode, &'static str)>,
    revisions: u64,
}

type Shared = Arc<Mutex<Runtime>>;

impl Runtime {
    fn new() -> anyhow::Result<Self> {
        let choice = |id: &str, name: &str, group: Option<&str>| Choice {
            id: id.to_owned(),
            name: name.to_owned(),
            group: group.map(str::to_owned),
        };
        let line = |id: &str, name: &str, choices, value: &str, seed: u64| {
            Ok::<_, anyhow::Error>(Setting {
                id: id.to_owned(),
                name: name.to_owned(),
                choices,
                value: Some(SettingValue::One {
                    id: value.to_owned(),
                }),
                inherited: true,
                revision: revision(seed)?,
            })
        };
        Ok(Self {
            settings: RuntimeSettings {
                settings: vec![
                    line(
                        "model",
                        "Model",
                        vec![
                            choice(
                                "openai-codex/gpt-6.1-sol",
                                "GPT-6.1 Sol",
                                Some("OpenAI Codex"),
                            ),
                            choice(
                                "openai-codex/gpt-6.1-mini",
                                "GPT-6.1 Mini",
                                Some("OpenAI Codex"),
                            ),
                        ],
                        "openai-codex/gpt-6.1-sol",
                        1,
                    )?,
                    line(
                        "model.thinking",
                        "Thinking",
                        vec![
                            choice("low", "Low", None),
                            choice("medium", "Medium", None),
                            choice("high", "High", None),
                        ],
                        "medium",
                        2,
                    )?,
                ],
            },
            requests: Vec::new(),
            refusal: None,
            revisions: 100,
        })
    }

    /// Sets a line on the channel, as a change from anywhere does.
    fn set(&mut self, setting: &str, value: Option<SettingValue>) -> anyhow::Result<()> {
        self.revisions += 1;
        let revision = revision(self.revisions)?;
        let line = self
            .settings
            .settings
            .iter_mut()
            .find(|line| line.id == setting)
            .context("no such line")?;
        line.inherited = value.is_none();
        if let Some(value) = value {
            line.value = Some(value);
        }
        line.revision = revision;
        Ok(())
    }

    fn changes(&self) -> usize {
        self.requests
            .iter()
            .filter(|request| request.change.is_some())
            .count()
    }
}

fn revision(seed: u64) -> anyhow::Result<Revision> {
    Revision::try_from(format!("{seed:064x}")).map_err(anyhow::Error::msg)
}

async fn answer(State(runtime): State<Shared>, Json(request): Json<SettingsRequest>) -> Response {
    let Ok(mut runtime) = runtime.lock() else {
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    };
    runtime.requests.push(request.clone());
    if let Some(change) = request.change {
        if let Some((status, error)) = runtime.refusal {
            return (status, Json(json!({ "error": error }))).into_response();
        }
        let line = runtime
            .settings
            .settings
            .iter()
            .find(|line| line.id == change.setting);
        if line.is_none_or(|line| line.revision != change.expected) {
            return (
                StatusCode::CONFLICT,
                Json(json!({"error": "This setting changed meanwhile."})),
            )
                .into_response();
        }
        if runtime.set(&change.setting, change.value).is_err() {
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    }
    Json(runtime.settings.clone()).into_response()
}

/// Core serving `runtime` at its address.
async fn core(
    runtime: &Shared,
) -> anyhow::Result<(
    crate::client::CoreClient,
    tokio::task::JoinHandle<std::io::Result<()>>,
)> {
    let router = Router::new()
        .route("/v1/settings", post(answer))
        .with_state(runtime.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let address = listener.local_addr()?;
    let server = tokio::spawn(async move { axum::serve(listener, router).await });
    Ok((
        crate::client::test_client(format!("http://{address}"))?,
        server,
    ))
}

/// The release module's reply to `interaction`, admitted from `source` in
/// a thread route 300, checked as the provider checks it for Discord.
async fn invoke(
    core: &crate::client::CoreClient,
    source: crate::conversations::InteractionSource,
    markers: &ActivityMarkers,
) -> anyhow::Result<Reply> {
    let mut config = crate::conversations::tests::discord_config(
        crate::config::DiscordActivation::Always,
        crate::config::DiscordConversation::Thread,
    );
    config.activity_markers = markers.clone();
    let admitted = crate::conversations::admit_interaction(&config, source)
        .map_err(|refusal| anyhow::anyhow!(refusal.message))?;
    let calls =
        super::super::InteractionCalls::new(core.clone(), admitted.route, admitted.audience_id);
    let reply = release_module()
        .await?
        .invoke(calls, admitted.interaction)
        .await?;
    super::super::answer::answer(&reply)?;
    Ok(reply)
}

/// `/settings`, or a click on `custom_id` choosing `values`, in `channel`.
fn source(
    channel: &str,
    custom_id: Option<(&str, &[&str])>,
) -> crate::conversations::InteractionSource {
    let parent = (channel != "300").then(|| "300".to_owned());
    let (kind, name, values) = match custom_id {
        None => (InteractionKind::Command, "settings", Vec::new()),
        Some((id, values)) => (
            InteractionKind::Component,
            id,
            values.iter().map(|value| (*value).to_owned()).collect(),
        ),
    };
    crate::conversations::InteractionSource {
        parent_channel_id: parent,
        kind,
        name: name.to_owned(),
        values,
        ..crate::conversations::tests::interaction(channel, None)
    }
}

fn controls(reply: &Reply) -> impl Iterator<Item = &Value> {
    reply
        .components
        .iter()
        .filter_map(|row| row["components"].as_array())
        .flatten()
}

fn control(reply: &Reply, control: &str) -> anyhow::Result<String> {
    let prefix = format!("settings:{control}:");
    controls(reply)
        .filter_map(|component| component["custom_id"].as_str())
        .find(|id| id.starts_with(&prefix))
        .map(str::to_owned)
        .with_context(|| format!("no {control} control"))
}

/// The value of the option labelled `label` in the control `custom_id`.
fn option(reply: &Reply, custom_id: &str, label: &str) -> anyhow::Result<String> {
    controls(reply)
        .filter(|component| component["custom_id"] == custom_id)
        .filter_map(|component| component["options"].as_array())
        .flatten()
        .find(|option| option["label"] == label)
        .and_then(|option| option["value"].as_str())
        .map(str::to_owned)
        .with_context(|| format!("no option {label}"))
}

/// Clicks the option labelled `label` in the line picker, or the picked
/// line's choices.
async fn click(
    core: &crate::client::CoreClient,
    reply: &Reply,
    picker: &str,
    label: &str,
) -> anyhow::Result<Reply> {
    let id = control(reply, picker)?;
    let value = option(reply, &id, label)?;
    invoke(
        core,
        source("401", Some((&id, &[&value]))),
        &ActivityMarkers::default(),
    )
    .await
}

const HOME: &str = "<#300>\n```\n\
    Agent     default\n\
    Listens   always\n\
    Replies   threads\n\
    Model     GPT-6.1 Sol · default\n\
    Thinking  Medium · default\n\
    ```";

#[tokio::test]
async fn a_thread_draws_its_channels_page_through_the_host() -> anyhow::Result<()> {
    let runtime = Arc::new(Mutex::new(Runtime::new()?));
    let (core, server) = core(&runtime).await?;
    let markers = ActivityMarkers::default();
    let in_channel = invoke(&core, source("300", None), &markers).await?;
    let in_thread = invoke(&core, source("401", None), &markers).await?;
    assert_eq!(in_thread.content, HOME);
    assert_eq!(in_thread, in_channel);
    let requests = runtime
        .lock()
        .map_err(|_| anyhow::anyhow!("poisoned"))?
        .requests
        .clone();
    let channel = SettingsRequest {
        subject: Subject::Channel {
            interface_id: "discord".to_owned(),
            channel_id: "300".to_owned(),
            agent_id: "default".to_owned(),
        },
        change: None,
    };
    assert_eq!(requests, [channel.clone(), channel]);
    server.abort();
    Ok(())
}

#[tokio::test]
async fn a_change_applies_through_core_and_a_stale_card_sends_nothing() -> anyhow::Result<()> {
    let runtime = Arc::new(Mutex::new(Runtime::new()?));
    let (core, server) = core(&runtime).await?;
    let drawn = invoke(&core, source("401", None), &ActivityMarkers::default()).await?;
    let thinking = click(&core, &drawn, "pick", "Thinking").await?;
    let high = click(&core, &thinking, "set", "High").await?;
    assert!(
        high.content.contains("\nThinking  High\n"),
        "{}",
        high.content
    );
    assert!(high.content.starts_with("<#300>\n"));
    control(&high, "reset")?;
    let lock = || runtime.lock().map_err(|_| anyhow::anyhow!("poisoned"));
    assert_eq!(lock()?.changes(), 1);

    let stale = click(&core, &thinking, "set", "Low").await?;
    assert_eq!(lock()?.changes(), 1, "a stale card sent a change");
    assert!(
        stale.content.starts_with("⚠ Changed meanwhile\n<#300>\n"),
        "{}",
        stale.content
    );
    assert!(stale.content.contains("\nThinking  High\n"));

    lock()?.set("model.thinking", None)?;
    let raced = click(&core, &stale, "set", "Low").await?;
    assert_eq!(
        lock()?.changes(),
        1,
        "a card raced by another writer sent a change"
    );
    assert!(raced.content.starts_with("⚠ Changed meanwhile\n"));
    let fresh = click(&core, &raced, "set", "Low").await?;
    lock()?.refusal = Some((StatusCode::CONFLICT, "This setting changed meanwhile."));
    let refused = click(&core, &fresh, "set", "High").await?;
    assert_eq!(lock()?.changes(), 3);
    assert!(
        refused.content.starts_with("⚠ Changed meanwhile\n"),
        "a change core finds stale is read again: {}",
        refused.content
    );
    server.abort();
    Ok(())
}

#[tokio::test]
async fn a_change_core_could_not_confirm_is_read_again_and_not_retried() -> anyhow::Result<()> {
    let runtime = Arc::new(Mutex::new(Runtime::new()?));
    let (core, server) = core(&runtime).await?;
    let lock = || runtime.lock().map_err(|_| anyhow::anyhow!("poisoned"));
    let drawn = invoke(&core, source("401", None), &ActivityMarkers::default()).await?;
    lock()?.refusal = Some((
        StatusCode::GATEWAY_TIMEOUT,
        "The runtime did not answer in time.",
    ));
    let unconfirmed = click(&core, &drawn, "set", "GPT-6.1 Mini").await?;
    assert!(
        unconfirmed.content.starts_with("⚠ Not confirmed\n<#300>\n"),
        "{}",
        unconfirmed.content
    );
    let requests = lock()?.requests.clone();
    assert_eq!(
        requests
            .iter()
            .map(|request| request.change.is_some())
            .collect::<Vec<_>>(),
        [false, false, true, false],
        "the click reads, changes once, and reads again"
    );

    lock()?.refusal = Some((
        StatusCode::UNPROCESSABLE_ENTITY,
        "That model is not offered here.",
    ));
    let refused = click(&core, &unconfirmed, "set", "GPT-6.1 Mini").await?;
    assert!(
        refused
            .content
            .starts_with("⚠ That model is not offered here.\n<#300>\n"),
        "{}",
        refused.content
    );
    server.abort();
    Ok(())
}

#[tokio::test]
async fn route_lines_draw_while_core_is_unreachable() -> anyhow::Result<()> {
    let core = crate::client::test_client("http://127.0.0.1:1".to_owned())?;
    let reply = invoke(&core, source("401", None), &ActivityMarkers::default()).await?;
    assert_eq!(
        reply.content,
        "⚠ The runtime is not reachable right now.\n<#300>\n```\n\
         Agent    default\n\
         Listens  always\n\
         Replies  threads\n\
         ```"
    );
    assert!(reply.components.is_empty());
    Ok(())
}

/// Discord may refuse a badge as an option's emoji; the provider then sends
/// the card again without them.
#[tokio::test]
async fn model_badges_can_be_dropped_for_discord() -> anyhow::Result<()> {
    let runtime = Arc::new(Mutex::new(Runtime::new()?));
    let (core, server) = core(&runtime).await?;
    let markers = ActivityMarkers {
        models: std::collections::BTreeMap::from([(
            "gpt-".to_owned(),
            "<:openai:1300000000000000002>".to_owned(),
        )]),
        ..ActivityMarkers::default()
    };
    let reply = invoke(&core, source("300", None), &markers).await?;
    assert_eq!(reply.content, HOME);
    let models = control(&reply, "set")?;
    let badges = controls(&reply)
        .filter(|component| component["custom_id"] == models.as_str())
        .filter_map(|component| component["options"].as_array())
        .flatten()
        .filter(|option| option["emoji"]["name"] == "openai")
        .count();
    assert_eq!(badges, 2);
    let mut answer = super::super::answer::answer(&reply)?;
    assert!(answer.remove_option_emojis());
    assert!(!answer.remove_option_emojis(), "a badge was left");
    server.abort();
    Ok(())
}

#[tokio::test]
async fn a_change_that_outlasts_its_time_still_reads_the_page_again() -> anyhow::Result<()> {
    let runtime = Arc::new(Mutex::new(Runtime::new()?));
    let router = Router::new()
        .route(
            "/v1/settings",
            post(
                |State(runtime): State<Shared>, Json(request): Json<SettingsRequest>| async move {
                    let changing = request.change.is_some();
                    let response = answer(State(runtime), Json(request)).await;
                    let late = if changing { 10_000 } else { 500 };
                    tokio::time::sleep(Duration::from_millis(late)).await;
                    response
                },
            ),
        )
        .with_state(runtime.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let client = crate::client::test_client(format!("http://{}", listener.local_addr()?))?;
    let server = tokio::spawn(async move { axum::serve(listener, router).await });
    let drawn = invoke(&client, source("401", None), &ActivityMarkers::default()).await?;
    let reply = click(&client, &drawn, "set", "GPT-6.1 Mini").await?;
    let writes: Vec<bool> = runtime
        .lock()
        .map_err(|_| anyhow::anyhow!("poisoned"))?
        .requests
        .iter()
        .map(|request| request.change.is_some())
        .collect();
    server.abort();
    assert_eq!(
        writes,
        [false, false, true, false],
        "the click reads, changes once, then reads again"
    );
    assert!(
        reply.content.starts_with("⚠ Not confirmed\n"),
        "{}",
        reply.content
    );
    assert!(reply.content.contains("GPT-6.1 Mini"), "{}", reply.content);
    Ok(())
}

#[tokio::test]
async fn a_runtime_that_goes_out_on_the_change_is_named_once() -> anyhow::Result<()> {
    let runtime = Arc::new(Mutex::new(Runtime::new()?));
    let down = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let router = Router::new()
        .route(
            "/v1/settings",
            post(
                move |State(runtime): State<Shared>, Json(request): Json<SettingsRequest>| {
                    let down = down.clone();
                    async move {
                        use std::sync::atomic::Ordering;
                        if let Ok(mut held) = runtime.lock() {
                            held.requests.push(request.clone());
                        }
                        if request.change.is_some() {
                            down.store(true, Ordering::SeqCst);
                        }
                        if down.load(Ordering::SeqCst) {
                            return (
                                StatusCode::SERVICE_UNAVAILABLE,
                                Json(json!({"error": "The runtime is not reachable right now."})),
                            )
                                .into_response();
                        }
                        let settings = runtime.lock().map(|held| held.settings.clone());
                        match settings {
                            Ok(settings) => Json(settings).into_response(),
                            Err(_) => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
                        }
                    }
                },
            ),
        )
        .with_state(runtime.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let client = crate::client::test_client(format!("http://{}", listener.local_addr()?))?;
    let server = tokio::spawn(async move { axum::serve(listener, router).await });
    let drawn = invoke(&client, source("401", None), &ActivityMarkers::default()).await?;
    let reply = click(&client, &drawn, "set", "GPT-6.1 Mini").await?;
    server.abort();
    assert_eq!(
        reply
            .content
            .matches("The runtime is not reachable right now.")
            .count(),
        1,
        "{}",
        reply.content
    );
    Ok(())
}
