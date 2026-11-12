//! A provider over a temporary installation and an in-process fake of
//! core's interface routes, for the tests of every module.

use std::{
    collections::HashMap,
    path::PathBuf,
    sync::{Arc, Mutex, MutexGuard, PoisonError},
};

use anyhow::{Context as _, Result};
use axum::{
    Json, Router,
    body::{Body, to_bytes},
    extract::{Path, Query, State},
    http::{
        Method, Request, StatusCode,
        header::{AUTHORIZATION, CONTENT_SECURITY_POLICY, CONTENT_TYPE, HOST},
        request::Builder,
        response::Parts,
    },
    response::{IntoResponse as _, Response},
    routing::{get, post},
};
use carillon_installation::config::{LoadedConfig, attachment_staging, hash_token};
use carillon_interface::{CoreClient, Staging};
use carillon_protocol::{
    conversations::{ConversationPage, ConversationSummary, Transcript},
    model::{
        CommandCatalog, EventReceipt, EventStatus, ExternalEvent, Health, Job, JobPage,
        RuntimeCommand, TurnState,
    },
    settings::{RuntimeSettings, SettingsRequest},
    turns::TurnControl,
};
use serde_json::{Value, json};
use tempfile::TempDir;
use tokio::{net::TcpListener, sync::Notify};
use tokio_util::sync::CancellationToken;
use tower::ServiceExt as _;

use crate::{
    browser::{self, Browser},
    serve::{Web, router},
    site::Site,
};

pub(crate) const AUTHORITY: &str = "127.0.0.1:45404";
pub(crate) const INTERFACE_TOKEN: &str = "interface-token";
pub(crate) const OPERATOR_TOKEN: &str = "operator-token";

/// What the fake of core holds and what it was asked.
#[derive(Default)]
pub(crate) struct Core {
    pub(crate) conversations: Vec<ConversationSummary>,
    /// By `agent/audience`.
    pub(crate) transcripts: HashMap<String, Transcript>,
    /// Every event submitted, with the bytes of each file it staged as core
    /// would read them then.
    pub(crate) events: Vec<(ExternalEvent, Vec<Vec<u8>>)>,
    /// What the next submissions, controls and settings calls are refused
    /// with.
    pub(crate) refusal: Option<(StatusCode, &'static str)>,
    pub(crate) jobs: Vec<Job>,
    pub(crate) controls: Vec<(String, TurnControl)>,
    pub(crate) settings: Vec<SettingsRequest>,
    /// Each request's path and query.
    pub(crate) asked: Vec<String>,
    /// A submission waits here before core reads its files, as a capture
    /// that takes a while does.
    pub(crate) hold: Option<Arc<Notify>>,
    staging: PathBuf,
}

type Fake = Arc<Mutex<Core>>;

fn locked(core: &Fake) -> MutexGuard<'_, Core> {
    core.lock().unwrap_or_else(PoisonError::into_inner)
}

fn refused(core: &Core) -> Option<Response> {
    core.refusal
        .map(|(status, error)| (status, Json(json!({ "error": error }))).into_response())
}

async fn list(
    State(core): State<Fake>,
    Query(query): Query<HashMap<String, String>>,
) -> Json<ConversationPage> {
    let core = locked(&core);
    let before = query.get("before").and_then(|before| before.parse().ok());
    Json(ConversationPage {
        conversations: core
            .conversations
            .iter()
            .filter(|listed| {
                query
                    .get("audience_id")
                    .is_none_or(|audience| *audience == listed.audience_id)
                    && before.is_none_or(|before: i64| listed.activity < before)
            })
            .cloned()
            .collect(),
        next: None,
    })
}

async fn transcript(
    State(core): State<Fake>,
    Path((agent, audience)): Path<(String, String)>,
) -> Json<Transcript> {
    let found = locked(&core)
        .transcripts
        .get(&format!("{agent}/{audience}"))
        .cloned();
    Json(found.unwrap_or_default())
}

async fn submit(State(core): State<Fake>, Json(event): Json<ExternalEvent>) -> Response {
    let hold = locked(&core).hold.clone();
    if let Some(hold) = hold {
        hold.notified().await;
    }
    let mut core = locked(&core);
    let staged = event.attachments.iter().flat_map(|attachments| {
        (0..attachments.files.len()).map(|ordinal| {
            let file = core
                .staging
                .join(&attachments.batch)
                .join(ordinal.to_string());
            std::fs::read(file).unwrap_or_default()
        })
    });
    let staged = staged.collect();
    let duplicate = core
        .events
        .iter()
        .any(|(held, _)| held.event_id == event.event_id);
    let receipt = EventReceipt {
        event_id: event.event_id.clone(),
        turn_id: format!("turn-{}", event.event_id),
        state: TurnState::Queued,
        duplicate,
    };
    core.events.push((event, staged));
    refused(&core).unwrap_or_else(|| Json(receipt).into_response())
}

async fn jobs(
    State(core): State<Fake>,
    Query(query): Query<HashMap<String, String>>,
) -> Json<JobPage> {
    let tracked: Vec<&str> = query
        .get("tracked")
        .map(|tracked| tracked.split(',').collect())
        .unwrap_or_default();
    let jobs = locked(&core).jobs.clone();
    Json(JobPage {
        jobs: jobs
            .into_iter()
            .filter(|job| tracked.contains(&job.origin.turn_id.as_str()))
            .collect(),
        next: None,
    })
}

async fn control(
    State(core): State<Fake>,
    Path(event_id): Path<String>,
    Json(control): Json<TurnControl>,
) -> Response {
    let mut core = locked(&core);
    core.controls.push((event_id, control));
    refused(&core).unwrap_or_else(|| StatusCode::NO_CONTENT.into_response())
}

async fn commands(
    State(core): State<Fake>,
    Query(query): Query<HashMap<String, String>>,
) -> Response {
    refused(&locked(&core)).unwrap_or_else(|| {
        Json(CommandCatalog {
            commands: vec![RuntimeCommand {
                name: format!("skill:{}", query["agent_id"]),
                description: Some(format!("in {}", query["channel_id"])),
                hint: query.get("audience_id").cloned(),
            }],
        })
        .into_response()
    })
}

async fn settings(State(core): State<Fake>, Json(request): Json<SettingsRequest>) -> Response {
    let mut core = locked(&core);
    core.settings.push(request);
    refused(&core).unwrap_or_else(|| Json(RuntimeSettings::default()).into_response())
}

/// Core's interface routes, noting each request's path and query.
fn fake(core: &Fake) -> Router {
    async fn noted(
        State(core): State<Fake>,
        request: Request<Body>,
        next: axum::middleware::Next,
    ) -> Response {
        locked(&core).asked.push(request.uri().to_string());
        next.run(request).await
    }
    Router::new()
        .route("/v1/conversations", get(list))
        .route("/v1/conversations/{agent}/{audience}", get(transcript))
        .route("/v1/events", post(submit))
        .route(
            "/v1/events/{event_id}",
            get(|| async { Json(EventStatus::Unknown) }),
        )
        .route("/v1/events/{event_id}/control", post(control))
        .route("/v1/jobs", get(jobs))
        .route("/v1/commands", get(commands))
        .route("/v1/settings", post(settings))
        .layer(axum::middleware::from_fn_with_state(
            Arc::clone(core),
            noted,
        ))
        .with_state(Arc::clone(core))
}

pub(crate) struct Fixture {
    pub(crate) directory: TempDir,
    pub(crate) web: Arc<Web>,
    core: Fake,
}

/// An answer, read whole.
pub(crate) struct Answer {
    pub(crate) status: StatusCode,
    pub(crate) parts: Parts,
    pub(crate) body: Vec<u8>,
}

impl Answer {
    pub(crate) fn json(&self) -> Result<Value> {
        serde_json::from_slice(&self.body)
            .with_context(|| format!("{}: {}", self.status, self.text()))
    }

    pub(crate) fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }

    pub(crate) fn header(&self, name: &str) -> &str {
        self.parts
            .headers
            .get(name)
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default()
    }
}

impl Fixture {
    pub(crate) async fn new() -> Result<Self> {
        let directory = tempfile::tempdir()?;
        let root = directory.path();
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let address = listener.local_addr()?;
        let registry = format!(
            r#"data_dir = "state"
[core]
listen = "{address}"
operator_token_hash = "{operator}"
[core.attachments]
max_files = 3
max_file_mib = 1
max_event_mib = 2
storage_mib = 2
[[providers]]
id = "web"
role = "interface"
program = "web"
version = "0.0.0"
url = "http://{AUTHORITY}"
token_hash = "{token}"
enabled = true
max_upload_mib = 1024
delivery = "retained"
[[providers]]
id = "runtime-other"
role = "runtime"
program = "runtime-other"
version = "0.0.0"
url = "http://127.0.0.1:45402"
token_hash = "{token}"
enabled = true
[[agents]]
id = "assistant"
name = "Assistant"
runtime = "runtime-other"
workspace = "work"
[[agents]]
id = "second"
name = "Second"
runtime = "runtime-other"
workspace = "work"
"#,
            operator = hash_token(OPERATOR_TOKEN),
            token = hash_token(INTERFACE_TOKEN),
        );
        std::fs::write(root.join("carillon.toml"), registry)?;
        let release = root.join("release");
        std::fs::create_dir_all(&release)?;
        std::fs::write(release.join("index.html"), "<main>release page</main>")?;
        std::fs::write(release.join("app.js"), "// release script")?;
        let registry = LoadedConfig::load(&root.join("carillon.toml"))?;
        let data_dir = registry.value.data_dir.clone();
        let staging = attachment_staging(&data_dir, "web");
        let core = Arc::new(Mutex::new(Core {
            staging: staging.clone(),
            ..Core::default()
        }));
        let fake = fake(&core);
        tokio::spawn(async move { axum::serve(listener, fake).await });
        let web = Arc::new(Web {
            core: CoreClient::new(
                format!("http://{address}"),
                "web".to_owned(),
                INTERFACE_TOKEN.to_owned(),
            )?,
            health: Health {
                id: "web".to_owned(),
                role: "interface".to_owned(),
                version: "0.0.0".to_owned(),
                status: "ready".to_owned(),
            },
            browser: Browser::open(browser::key_file(&data_dir, "web"), AUTHORITY.to_owned())?,
            staging: Staging::open(staging)?,
            site: Site {
                custom: data_dir.join("components/web/site"),
                release,
            },
            user: "owner".to_owned(),
            stopping: CancellationToken::new(),
            registry,
        });
        Ok(Self {
            directory,
            web,
            core,
        })
    }

    pub(crate) fn core(&self) -> MutexGuard<'_, Core> {
        locked(&self.core)
    }

    /// The answer to `request`. No answer of this provider may carry a CORS
    /// header or a cookie, and each has the headers that keep it on its
    /// origin.
    pub(crate) async fn send(&self, request: Request<Body>) -> Result<Answer> {
        let response = router(Arc::clone(&self.web)).oneshot(request).await?;
        let (parts, body) = response.into_parts();
        for name in parts.headers.keys() {
            assert!(
                !name.as_str().starts_with("access-control-"),
                "{name} was sent"
            );
        }
        assert_eq!(parts.headers["x-content-type-options"], "nosniff");
        assert_eq!(parts.headers["referrer-policy"], "no-referrer");
        assert_eq!(parts.headers["cross-origin-resource-policy"], "same-origin");
        assert!(parts.headers.contains_key(CONTENT_SECURITY_POLICY));
        assert!(!parts.headers.contains_key("set-cookie"));
        Ok(Answer {
            status: parts.status,
            parts,
            body: to_bytes(body, usize::MAX).await?.to_vec(),
        })
    }

    /// A token as a browser gets one: a code from the operator's route,
    /// traded at the page's.
    pub(crate) async fn sign_in(&self) -> Result<String> {
        let issued = self
            .send(operator(Method::POST, "/v1/sign-in").body(Body::empty())?)
            .await?;
        assert_eq!(issued.status, StatusCode::OK);
        let code = issued.json()?["code"]
            .as_str()
            .context("no code")?
            .to_owned();
        let traded = self.send(trade(&code)?).await?;
        assert_eq!(traded.status, StatusCode::OK);
        Ok(traded.json()?["token"]
            .as_str()
            .context("no token")?
            .to_owned())
    }

    /// The answer to `GET path` with `token`.
    pub(crate) async fn get(&self, path: &str, token: &str) -> Result<Answer> {
        self.send(bearer(Method::GET, path, token).body(Body::empty())?)
            .await
    }
}

/// A conversation of `agent` as core lists it.
pub(crate) fn conversation(agent: &str, audience: &str, activity: i64) -> ConversationSummary {
    ConversationSummary {
        agent_id: agent.to_owned(),
        audience_id: audience.to_owned(),
        channel_id: audience.to_owned(),
        title: format!("title {activity}"),
        started_at: 100,
        active_at: 100 + activity,
        activity,
        at_work: 0,
    }
}

/// A request that names this provider, as a program sends one.
pub(crate) fn request(method: Method, path: &str) -> Builder {
    Request::builder()
        .method(method)
        .uri(path)
        .header(HOST, AUTHORITY)
}

pub(crate) fn bearer(method: Method, path: &str, token: &str) -> Builder {
    request(method, path).header(AUTHORIZATION, format!("Bearer {token}"))
}

pub(crate) fn operator(method: Method, path: &str) -> Builder {
    bearer(method, path, OPERATOR_TOKEN)
}

pub(crate) fn trade(code: &str) -> Result<Request<Body>> {
    Ok(request(Method::POST, "/api/sign-in")
        .header(CONTENT_TYPE, "application/json")
        .body(Body::from(json!({ "code": code }).to_string()))?)
}
