//! A provider over a temporary installation and an in-process fake of
//! core's interface routes, for the tests of every module.

use std::{
    cmp::Reverse,
    collections::HashMap,
    convert::Infallible,
    ops::{Deref, DerefMut},
    path::PathBuf,
    sync::{
        Arc,
        nonpoison::{Mutex, MutexGuard},
    },
};

use anyhow::{Context as _, Result};
use axum::{
    Json, Router,
    body::{Body, BodyDataStream, Bytes, to_bytes},
    extract::{Path, Query, State},
    http::{
        Method, Request, StatusCode,
        header::{AUTHORIZATION, CONTENT_SECURITY_POLICY, CONTENT_TYPE, HOST},
        request::Builder,
        response::Parts,
    },
    response::{
        IntoResponse as _, Response,
        sse::{Event, KeepAlive, Sse},
    },
    routing::{get, post},
};
use carillon_installation::config::{LoadedConfig, attachment_staging, hash_token};
use carillon_interface::{CoreClient, Staging};
use carillon_protocol::{
    conversations::TurnSource,
    model::{
        CommandCatalog, EventReceipt, ExternalEvent, Health, INTENT_COMMAND, NATIVE_INTENT,
        RuntimeCommand, TurnState,
    },
    observation::{
        Conversation, ConversationKey, KEEP_ALIVE, ObservedTurn, Opened, Progress, SNAPSHOT_EVENT,
        Snapshot, View,
    },
    turns::TurnControl,
};
use futures_util::{StreamExt as _, stream};
use serde_json::{Value, json};
use sse_core::{SseDecoder, SseEvent};
use tempfile::TempDir;
use tokio::{
    net::TcpListener,
    sync::{Notify, watch},
    task::JoinHandle,
};
use tokio_util::sync::CancellationToken;
use tower::ServiceExt as _;

use crate::{
    browser::{self, Address, Browser},
    serve::{Web, router},
    site::Site,
};

pub(crate) const AUTHORITY: &str = "127.0.0.1:45404";
pub(crate) const INTERFACE_TOKEN: &str = "interface-token";
pub(crate) const OPERATOR_TOKEN: &str = "operator-token";
pub(crate) const PASSWORD: &str = "correct horse battery";
pub(crate) const TAILNET: &str = "https://chat.example-tailnet.ts.net";

/// What the fake of core holds and what it was asked.
#[derive(Default)]
pub(crate) struct Core {
    /// The interface's conversations, in no order.
    pub(crate) conversations: Vec<Conversation>,
    /// Each conversation's turns, oldest first.
    pub(crate) turns: HashMap<ConversationKey, Vec<ObservedTurn>>,
    /// Every event admitted, once for each event ID, with the bytes of each
    /// file it staged as core would read them then. A turn is named for its
    /// place here. Its files are kept here and not listed with its turn.
    pub(crate) events: Vec<(ExternalEvent, Vec<Vec<u8>>)>,
    /// What the next submissions, controls and command reads are refused
    /// with.
    pub(crate) refusal: Option<(StatusCode, &'static str)>,
    /// How many requests yet, of any kind, core answers as it does while
    /// it shuts down.
    pub(crate) away_for_requests: usize,
    /// Core is stopping: it ends every observation.
    pub(crate) stopping: bool,
    pub(crate) controls: Vec<(String, TurnControl)>,
    /// Each request's path and query.
    pub(crate) asked: Vec<String>,
    /// A submission waits here before core reads its files, as a capture
    /// that takes a while does.
    pub(crate) hold: Option<Arc<Notify>>,
    staging: PathBuf,
}

/// The fake of core, and the signal it gives whenever what it holds may
/// have changed, as core's storage gives one with every commit.
struct Fake {
    core: Mutex<Core>,
    changes: watch::Sender<()>,
}

/// The fake of core, held to read or change it. Letting go of it gives the
/// change signal.
pub(crate) struct Held<'a> {
    core: MutexGuard<'a, Core>,
    changes: &'a watch::Sender<()>,
}

impl Deref for Held<'_> {
    type Target = Core;

    fn deref(&self) -> &Core {
        &self.core
    }
}

impl DerefMut for Held<'_> {
    fn deref_mut(&mut self) -> &mut Core {
        &mut self.core
    }
}

impl Drop for Held<'_> {
    fn drop(&mut self) {
        self.changes.send_replace(());
    }
}

fn locked(fake: &Fake) -> Held<'_> {
    Held {
        core: fake.core.lock(),
        changes: &fake.changes,
    }
}

fn refused(core: &Core) -> Option<Response> {
    core.refusal
        .map(|(status, error)| (status, Json(json!({ "error": error }))).into_response())
}

/// What `view` shows of what `core` holds, as core composes it.
fn snapshot(core: &Core, view: &View) -> Snapshot {
    let mut conversations = core.conversations.clone();
    conversations.sort_by_key(|listed| Reverse(listed.activity));
    let limit = usize::from(view.conversations.get());
    let more = conversations.len() > limit;
    conversations.truncate(limit);
    let open = view.open.as_ref().map(|open| {
        let key = &open.conversation;
        let Some(conversation) = core.conversations.iter().find(|listed| listed.key == *key) else {
            return Opened::Unrecorded { key: key.clone() };
        };
        let turns = core.turns.get(key).map_or_default(Vec::as_slice);
        let shown = turns.len().min(open.turns.get().into());
        Opened::Recorded {
            conversation: conversation.clone(),
            turns: turns[turns.len() - shown..].to_vec(),
            earlier: turns.len() > shown,
        }
    });
    Snapshot {
        conversations,
        more,
        open,
    }
}

/// `GET /v1/observe` as core answers it: the view's snapshot at once, then
/// another after a change that made it differ from the one sent last, and
/// a keep-alive while there is none. The stream ends when core stops.
async fn observe(State(fake): State<Arc<Fake>>, Query(view): Query<View>) -> Response {
    let mut changes = fake.changes.subscribe();
    changes.mark_unchanged();
    let composed = |fake: &Fake| serde_json::to_string(&snapshot(&locked(fake).core, &view));
    let Ok(first) = composed(&fake) else {
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    };
    let event =
        |snapshot: &str| Ok::<_, Infallible>(Event::default().event(SNAPSHOT_EVENT).data(snapshot));
    let later = stream::unfold(
        (fake, view, changes, first.clone()),
        move |(fake, view, mut changes, sent)| async move {
            loop {
                changes.changed().await.ok()?;
                let core = fake.core.lock();
                if core.stopping {
                    return None;
                }
                let next = serde_json::to_string(&snapshot(&core, &view)).ok()?;
                drop(core);
                if next != sent {
                    let sending = Ok(Event::default().event(SNAPSHOT_EVENT).data(&next));
                    return Some((sending, (fake, view, changes, next)));
                }
            }
        },
    );
    Sse::new(stream::once(async move { event(&first) }).chain(later))
        .keep_alive(KeepAlive::new().interval(KEEP_ALIVE))
        .into_response()
}

async fn submit(State(fake): State<Arc<Fake>>, Json(event): Json<ExternalEvent>) -> Response {
    let hold = fake.core.lock().hold.clone();
    if let Some(hold) = hold {
        hold.notified().await;
    }
    let mut core = locked(&fake);
    if let Some(refused) = refused(&core) {
        return refused;
    }
    let admitted = core
        .events
        .iter()
        .position(|(held, _)| held.event_id == event.event_id);
    let receipt = EventReceipt {
        event_id: event.event_id.clone(),
        turn_id: format!("turn-{}", admitted.unwrap_or(core.events.len()) + 1),
        state: TurnState::Queued,
        duplicate: admitted.is_some(),
    };
    if admitted.is_some() {
        return Json(receipt).into_response();
    }
    let files = event.attachments.iter().flat_map(|attachments| {
        (0..attachments.files.len()).map(|ordinal| {
            core.staging
                .join(&attachments.batch)
                .join(ordinal.to_string())
        })
    });
    let staged = match files.map(std::fs::read).collect::<std::io::Result<_>>() {
        Ok(staged) => staged,
        Err(error) => {
            let refused =
                json!({ "error": format!("a staged attachment cannot be read: {error}") });
            return (StatusCode::BAD_REQUEST, Json(refused)).into_response();
        }
    };
    let Ok(key) = ConversationKey::new(event.agent_id.clone(), event.audience_id.clone()) else {
        let refused = json!({ "error": "a conversation is named by its agent and its audience" });
        return (StatusCode::BAD_REQUEST, Json(refused)).into_response();
    };
    let latest = core.conversations.iter().map(|listed| listed.activity);
    let activity = latest.max().unwrap_or_default() + 1;
    if let Some(recorded) = core
        .conversations
        .iter_mut()
        .find(|listed| listed.key == key)
    {
        recorded.activity = activity;
        recorded.at_work += 1;
    } else {
        core.conversations.push(Conversation {
            key: key.clone(),
            channel_id: event.channel_id.clone(),
            title: event.content.clone(),
            started_at: 100 + activity,
            active_at: 100 + activity,
            activity,
            at_work: 1,
        });
    }
    core.turns.entry(key).or_default().push(ObservedTurn {
        turn_id: receipt.turn_id.clone(),
        source: TurnSource::Message {
            event_id: event.event_id.clone(),
            text: event.content.clone(),
            command: event.native[NATIVE_INTENT] == INTENT_COMMAND,
            files: Vec::new(),
        },
        accepted_at: 100 + activity,
        progress: Progress::Queued,
    });
    core.events.push((event, staged));
    Json(receipt).into_response()
}

async fn control(
    State(fake): State<Arc<Fake>>,
    Path(event_id): Path<String>,
    Json(control): Json<TurnControl>,
) -> Response {
    let mut core = locked(&fake);
    core.controls.push((event_id, control));
    refused(&core).unwrap_or_else(|| StatusCode::NO_CONTENT.into_response())
}

/// The runtime's one command, named for the agent it was asked about.
async fn commands(
    State(fake): State<Arc<Fake>>,
    Query(query): Query<HashMap<String, String>>,
) -> Response {
    refused(&locked(&fake)).unwrap_or_else(|| {
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

/// Core's interface routes, noting each request's path and query.
fn fake(fake: &Arc<Fake>) -> Router {
    async fn noted(
        State(fake): State<Arc<Fake>>,
        request: Request<Body>,
        next: axum::middleware::Next,
    ) -> Response {
        {
            let mut core = fake.core.lock();
            core.asked.push(request.uri().to_string());
            if core.away_for_requests > 0 {
                core.away_for_requests -= 1;
                let shutting_down = json!({ "error": "Carillon is shutting down" });
                return (StatusCode::SERVICE_UNAVAILABLE, Json(shutting_down)).into_response();
            }
        }
        next.run(request).await
    }
    Router::new()
        .route("/v1/observe", get(observe))
        .route("/v1/events", post(submit))
        .route("/v1/events/{event_id}/control", post(control))
        .route("/v1/commands", get(commands))
        .layer(axum::middleware::from_fn_with_state(
            Arc::clone(fake),
            noted,
        ))
        .with_state(Arc::clone(fake))
}

pub(crate) struct Fixture {
    pub(crate) directory: TempDir,
    pub(crate) web: Arc<Web>,
    core: Arc<Fake>,
    pub(crate) serving: JoinHandle<()>,
}

/// One page's stream, read message by message. The provider asks core
/// only while the page waits for one.
pub(crate) struct Page {
    body: BodyDataStream,
    events: SseDecoder,
    unread: Bytes,
}

impl Page {
    /// The next message, or `None` once the stream has ended. Keep-alives
    /// are passed over.
    pub(crate) async fn next(&mut self) -> Result<Option<Value>> {
        loop {
            match self.events.next(&mut self.unread) {
                Some(Ok(SseEvent::Message(event))) => {
                    assert_eq!(event.event, "message", "a message has no name");
                    return Ok(Some(serde_json::from_str(&event.data)?));
                }
                Some(Ok(SseEvent::Retry(_))) => anyhow::bail!("the stream set a retry time"),
                Some(Err(error)) => return Err(error.into()),
                None => {}
            }
            let Some(chunk) = self.body.next().await else {
                return Ok(None);
            };
            self.unread = chunk?;
        }
    }

    /// The snapshot the next message carries.
    pub(crate) async fn snapshot(&mut self) -> Result<Value> {
        let mut message = self.next().await?.context("the stream ended")?;
        assert_eq!(message["kind"], "snapshot", "{message}");
        Ok(message["snapshot"].take())
    }
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

/// The registry of the fixture's installation, with core at `address`.
fn registry(address: std::net::SocketAddr) -> String {
    format!(
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
    )
}

impl Fixture {
    pub(crate) async fn new() -> Result<Self> {
        let directory = tempfile::tempdir()?;
        let root = directory.path();
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let address = listener.local_addr()?;
        let registry = registry(address);
        std::fs::write(root.join("carillon.toml"), registry)?;
        let release = root.join("release");
        std::fs::create_dir_all(&release)?;
        std::fs::write(release.join("index.html"), "<main>release page</main>")?;
        // Named as Vite names the built page's script.
        std::fs::write(release.join("index-Bq3nR7xK.js"), "// release script")?;
        let registry = LoadedConfig::load(&root.join("carillon.toml"))?;
        let data_dir = registry.value.data_dir.clone();
        let staging = attachment_staging(&data_dir, "web");
        let core = Arc::new(Fake {
            core: Mutex::new(Core {
                staging: staging.clone(),
                ..Core::default()
            }),
            changes: watch::Sender::new(()),
        });
        let fake = fake(&core);
        let serving = tokio::spawn(async move {
            let _ = axum::serve(listener, fake).await;
        });
        let staging = Staging::open(staging)?;
        browser::set_password(&browser::password_file(&data_dir, "web"), PASSWORD)?;
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
            staging,
            browser: Browser::new(
                &data_dir,
                "web".to_owned(),
                AUTHORITY.to_owned(),
                vec![Address::parse(TAILNET)?],
            ),
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
            serving,
        })
    }

    pub(crate) fn password_file(&self) -> PathBuf {
        browser::password_file(&self.web.registry.value.data_dir, "web")
    }

    /// The fake of core, to read or change. Letting go of it tells every
    /// observation that what core holds may have changed.
    pub(crate) fn core(&self) -> Held<'_> {
        locked(&self.core)
    }

    /// The answer to `request`, with its body still to read. No answer of
    /// this provider may carry a CORS header or a cookie, and each has the
    /// headers that keep it on its origin.
    async fn answered(&self, request: Request<Body>) -> Result<(Parts, Body)> {
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
        Ok((parts, body))
    }

    /// The answer to `request`, read whole.
    pub(crate) async fn send(&self, request: Request<Body>) -> Result<Answer> {
        let (parts, body) = self.answered(request).await?;
        Ok(Answer {
            status: parts.status,
            parts,
            body: to_bytes(body, usize::MAX).await?.to_vec(),
        })
    }

    /// The status `request` is answered with, of an answer whose body is
    /// not read: a stream's.
    pub(crate) async fn status(&self, request: Request<Body>) -> Result<StatusCode> {
        Ok(self.answered(request).await?.0.status)
    }

    /// The page's stream of the view `query` asks for, opened with `token`.
    pub(crate) async fn stream(&self, query: &str, token: &str) -> Result<Page> {
        let path = format!("/api/stream?{query}");
        let request = bearer(Method::GET, &path, token).body(Body::empty())?;
        let (parts, body) = self.answered(request).await?;
        assert_eq!(parts.status, StatusCode::OK);
        assert_eq!(parts.headers[CONTENT_TYPE], "text/event-stream");
        Ok(Page {
            body: body.into_data_stream(),
            events: SseDecoder::new(),
            unread: Bytes::new(),
        })
    }

    /// A token as a browser gets one: the password, traded at the page's
    /// route.
    pub(crate) async fn sign_in(&self) -> Result<String> {
        let traded = self.send(guess(PASSWORD)?).await?;
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

/// A conversation of `agent` as core records it.
pub(crate) fn conversation(agent: &str, audience: &str, activity: i64) -> Result<Conversation> {
    Ok(Conversation {
        key: key(agent, audience)?,
        channel_id: audience.to_owned(),
        title: format!("title {activity}"),
        started_at: 100,
        active_at: 100 + activity,
        activity,
        at_work: 0,
    })
}

pub(crate) fn key(agent: &str, audience: &str) -> Result<ConversationKey> {
    Ok(ConversationKey::new(agent.to_owned(), audience.to_owned())?)
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

pub(crate) fn guess(password: &str) -> Result<Request<Body>> {
    Ok(request(Method::POST, "/api/sign-in")
        .header(CONTENT_TYPE, "application/json")
        .body(Body::from(json!({ "password": password }).to_string()))?)
}
