//! The browser API, under `/api`: what the page reads and sends. Every
//! route but `sign_in` is behind the browser's token. Core's wire types are
//! parsed here into the page's own view types. Only a runtime's own words
//! for the page, its commands and its activity snapshots, go on as they are.

use std::sync::Arc;

use axum::{
    Extension, Json, Router,
    extract::DefaultBodyLimit,
    extract::{Path, Query, State},
    http::StatusCode,
    routing::{get, post, put},
};
use carillon_interface::InterfaceError;
use carillon_protocol::{
    conversations::{ConversationSummary, TranscriptTurn, TurnSource},
    model::{ActivityPhase, CommandCatalog, ConversationQuery, TurnActivity, TurnState},
    turns::{ControlAction, Silence},
};
use serde::{Deserialize, Serialize};
use tracing::warn;

use crate::{
    browser::{Key, refusal},
    files::{self, FileView},
    identity::{ConversationId, EventId, agent_of, canonical},
    markdown::render,
    send,
    serve::Web,
    site::has_page,
};

/// What the page is told while Carillon cannot answer it: core is away, as
/// it is for a moment at every restart, or the provider is stopping.
pub(crate) const AWAY: &str = "Carillon is not answering right now. Try again in a moment.";

/// What the page is told when core did not do what was asked of it. A
/// refusal core wrote for an interface keeps its words. Anything else is
/// one status a page tries again after, and what core said goes to the
/// log and never to the page: it can name a file of this host.
pub(crate) fn from_core(failure: InterfaceError) -> InterfaceError {
    if failure.status.is_client_error() || failure.status == StatusCode::INSUFFICIENT_STORAGE {
        return failure;
    }
    warn!(status = %failure.status, error = %failure.message, "core did not answer for the page");
    refusal(StatusCode::SERVICE_UNAVAILABLE, AWAY)
}

pub(crate) fn routes() -> Router<Arc<Web>> {
    Router::new()
        .route("/session", get(session))
        .route("/conversations", get(conversations))
        .route("/conversations/{id}/turns", get(turns))
        // A message's files are bounded part by part as they are staged.
        .route(
            "/conversations/{id}/messages/{event_id}",
            put(send::send).layer(DefaultBodyLimit::disable()),
        )
        .route("/conversations/{id}/commands", get(commands))
        .route("/turns", get(live))
        .route("/messages/{event_id}/control", post(control))
}

#[derive(Deserialize)]
pub(crate) struct SignIn {
    code: String,
}

#[derive(Serialize)]
pub(crate) struct SignedIn {
    token: String,
}

/// `POST /api/sign-in`: trades the code of `carillon web open`'s link for
/// the browser's token. The code is the request's whole authority.
pub(crate) async fn sign_in(
    State(web): State<Arc<Web>>,
    Json(SignIn { code }): Json<SignIn>,
) -> Result<Json<SignedIn>, InterfaceError> {
    let token = web.browser.redeem(&code).ok_or_else(|| {
        refusal(
            StatusCode::UNAUTHORIZED,
            "This sign-in link was used already or is older than two minutes. \
             Run `carillon web open` again.",
        )
    })?;
    Ok(Json(SignedIn { token }))
}

/// What the page needs before it shows anything.
#[derive(Serialize)]
struct Session {
    /// The configured agents, in registry order, for a new conversation to
    /// choose from.
    agents: Vec<Agent>,
    limits: Limits,
    /// The page served is the owner's copy, not the release's.
    custom_site: bool,
}

#[derive(Serialize)]
struct Agent {
    id: String,
    name: String,
}

/// Core's bounds on one message's files, for the page to check before it
/// uploads.
#[derive(Serialize)]
struct Limits {
    files: u32,
    file_bytes: u64,
    message_bytes: u64,
}

async fn session(State(web): State<Arc<Web>>) -> Json<Session> {
    let registry = &web.registry.value;
    let attachments = &registry.core.attachments;
    Json(Session {
        agents: registry
            .agents
            .iter()
            .map(|agent| Agent {
                id: agent.id.clone(),
                name: agent.label(&registry.agents),
            })
            .collect(),
        limits: Limits {
            files: attachments.max_files,
            file_bytes: attachments.max_file_bytes(),
            message_bytes: attachments.max_event_bytes(),
        },
        custom_site: has_page(&web.site.custom),
    })
}

/// Where a page of a list continues from.
#[derive(Deserialize)]
struct Before<T> {
    before: Option<T>,
}

#[derive(Serialize)]
struct Conversations {
    conversations: Vec<Conversation>,
    /// More follow: read them with `before` set to this.
    next: Option<i64>,
}

#[derive(Serialize)]
struct Conversation {
    id: String,
    agent_id: String,
    /// The first line of the first message, empty when it had only files.
    title: String,
    /// Unix seconds.
    started_at: i64,
    active_at: i64,
    /// Rises with every message, every turn the runtime opens and every
    /// turn's end. While it and `at_work` are as they were, the
    /// conversation has no new turn and no new answer.
    activity: i64,
    /// How many of its turns are queued or running.
    at_work: u32,
}

impl Conversation {
    /// `None` for a conversation the page did not make, whose audience is
    /// no UUID.
    fn of(summary: ConversationSummary) -> Option<Self> {
        canonical(&summary.audience_id).then_some(Self {
            id: summary.audience_id,
            agent_id: summary.agent_id,
            title: summary.title,
            started_at: summary.started_at,
            active_at: summary.active_at,
            activity: summary.activity,
            at_work: summary.at_work,
        })
    }
}

/// `GET /api/conversations[?before=]`: a page of conversations, the latest
/// active first.
async fn conversations(
    State(web): State<Arc<Web>>,
    Query(Before { before }): Query<Before<i64>>,
) -> Result<Json<Conversations>, InterfaceError> {
    let page = web.core.conversations(before).await.map_err(from_core)?;
    Ok(Json(Conversations {
        conversations: page
            .conversations
            .into_iter()
            .filter_map(Conversation::of)
            .collect(),
        next: page.next,
    }))
}

#[derive(Serialize)]
struct Turns {
    /// Oldest first.
    turns: Vec<Turn>,
    /// Older turns exist: read them with `before` set to this.
    before: Option<String>,
}

/// One turn, ready to draw: who began it, then how it stands.
#[derive(Serialize)]
struct Turn {
    id: String,
    from: From,
    /// Unix seconds.
    accepted_at: i64,
    finished_at: Option<i64>,
    #[serde(flatten)]
    progress: Progress,
    /// An ended turn's final report, as its runtime drew it. A turn still
    /// at work has its snapshot in a live read.
    report: Option<TurnActivity>,
}

#[derive(Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum From {
    Person {
        /// What a control of the turn names.
        event_id: String,
        /// As written: text, never markup.
        text: String,
        /// It invoked one of the runtime's commands.
        command: bool,
        files: Vec<FileView>,
    },
    /// The runtime opened the turn itself, for a schedule or a subagent's
    /// report, and says what for.
    Runtime { label: Option<String> },
}

/// How a turn stands, as a person reads it.
#[derive(Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
enum Progress {
    Queued,
    Running,
    Answered {
        html: String,
        files: Vec<FileView>,
    },
    /// It did not complete. `html` is core's or the runtime's explanation.
    Interrupted {
        html: String,
    },
    /// The message went into a turn already running. Its answer is on the
    /// newest message that took part, which may not be the one above.
    Steered,
    /// Someone stopped it.
    Stopped,
}

impl Turn {
    async fn of(web: &Web, key: &Key, turn: TranscriptTurn) -> Self {
        let from = match turn.source {
            TurnSource::Message {
                event_id,
                text,
                command,
                files,
            } => {
                let mut views = Vec::with_capacity(files.len());
                for file in &files {
                    views.push(files::view(web, key, &file.path, &file.name, file.size).await);
                }
                From::Person {
                    event_id,
                    text,
                    command,
                    files: views,
                }
            }
            TurnSource::Opened { label } => From::Runtime { label },
        };
        let (content, silence, files) = turn.answer.map_or_else(Default::default, |answer| {
            (answer.content, answer.silence, answer.files)
        });
        let progress = match (turn.state, silence) {
            (TurnState::Queued, _) => Progress::Queued,
            (TurnState::Running, _) => Progress::Running,
            (_, Some(Silence::Steered)) => Progress::Steered,
            (_, Some(Silence::Stopped)) => Progress::Stopped,
            (TurnState::Interrupted, None) => Progress::Interrupted {
                html: render(&content),
            },
            (TurnState::Completed, None) => {
                let mut views = Vec::with_capacity(files.len());
                for file in &files {
                    views.push(files::view(web, key, &file.path, &file.name, file.size).await);
                }
                Progress::Answered {
                    html: render(&content),
                    files: views,
                }
            }
        };
        Self {
            id: turn.turn_id,
            from,
            accepted_at: turn.accepted_at,
            finished_at: turn.finished_at,
            progress,
            report: turn.report,
        }
    }
}

/// `GET /api/conversations/{id}/turns[?before=]`: a page of a
/// conversation's turns, its newest first.
async fn turns(
    State(web): State<Arc<Web>>,
    Extension(key): Extension<Key>,
    Path(id): Path<ConversationId>,
    Query(Before { before }): Query<Before<String>>,
) -> Result<Json<Turns>, InterfaceError> {
    let found = web.core.conversations_of(id.as_str()).await;
    let agent_id = agent_of(&web, found.map_err(from_core)?, None)?;
    let page = web
        .core
        .transcript(&agent_id, id.as_str(), before.as_deref())
        .await
        .map_err(from_core)?;
    let before = page
        .turns
        .first()
        .filter(|_| page.earlier)
        .map(|first| first.turn_id.clone());
    let mut turns = Vec::with_capacity(page.turns.len());
    for turn in page.turns {
        turns.push(Turn::of(&web, &key, turn).await);
    }
    Ok(Json(Turns { turns, before }))
}

/// The turns a live read names, by ID, with commas between.
#[derive(Deserialize)]
struct Named {
    ids: String,
}

#[derive(Serialize)]
struct Live {
    /// The named turns core has of this interface, in no order.
    turns: Vec<LiveTurn>,
}

#[derive(Serialize)]
struct LiveTurn {
    id: String,
    state: LiveState,
    /// The runtime's latest snapshot, drawn as it is given. Its `controls`
    /// are what a person may do with the turn now.
    activity: Option<TurnActivity>,
}

#[derive(Serialize)]
#[serde(rename_all = "snake_case")]
enum LiveState {
    /// It waits: for its runtime, or in it behind another turn.
    Queued,
    Running,
    /// It has ended, and the conversation's turns say how.
    Ended,
}

/// `GET /api/turns?ids=`: how the named unfinished turns stand now. A turn
/// going from queued to running, and its progress, change nothing a
/// conversation's summary shows, so the page asks here for the unfinished
/// turns on screen.
async fn live(
    State(web): State<Arc<Web>>,
    Query(Named { ids }): Query<Named>,
) -> Result<Json<Live>, InterfaceError> {
    let named: Vec<String> = ids
        .split(',')
        .filter(|id| !id.is_empty())
        .map(str::to_owned)
        .collect();
    let jobs = if named.is_empty() {
        Vec::new()
    } else {
        web.core.named_jobs(&named).await.map_err(from_core)?
    };
    let turns = jobs.into_iter().filter_map(|job| {
        let began_by_message = job.feedback_target_id.is_some();
        let turn = job.turns.into_iter().next_back()?;
        let mut activity = turn.activity;
        if let Some(activity) = &mut activity
            && !began_by_message
        {
            // A control names its turn by the message that began it, and a
            // turn the runtime opened has none.
            activity.controls.clear();
        }
        let waits = activity
            .as_ref()
            .is_some_and(|activity| activity.phase == ActivityPhase::Queued);
        Some(LiveTurn {
            id: turn.turn_id,
            state: match turn.state {
                TurnState::Queued => LiveState::Queued,
                TurnState::Running if waits => LiveState::Queued,
                TurnState::Running => LiveState::Running,
                TurnState::Completed | TurnState::Interrupted => LiveState::Ended,
            },
            activity,
        })
    });
    Ok(Json(Live {
        turns: turns.collect(),
    }))
}

#[derive(Deserialize)]
struct Control {
    action: ControlAction,
}

/// `POST /api/messages/{event_id}/control`: what a person does with the
/// turn of a message while its snapshot offers that control. Core relays
/// it to the runtime and keeps nothing of it.
async fn control(
    State(web): State<Arc<Web>>,
    Path(event_id): Path<EventId>,
    Json(Control { action }): Json<Control>,
) -> Result<StatusCode, InterfaceError> {
    web.core
        .control(event_id.as_str(), action)
        .await
        .map_err(from_core)?;
    Ok(StatusCode::NO_CONTENT)
}

/// The agent of a conversation core does not have yet.
#[derive(Deserialize)]
struct Chosen {
    agent: Option<String>,
}

/// `GET /api/conversations/{id}/commands[?agent=]`: the commands the
/// conversation's runtime offers its agent, in the runtime's order. A
/// message invokes one with its name first in the text and `command=true`.
async fn commands(
    State(web): State<Arc<Web>>,
    Path(id): Path<ConversationId>,
    Query(Chosen { agent }): Query<Chosen>,
) -> Result<Json<CommandCatalog>, InterfaceError> {
    let found = web.core.conversations_of(id.as_str()).await;
    let query = ConversationQuery {
        agent_id: agent_of(&web, found.map_err(from_core)?, agent)?,
        channel_id: id.as_str().to_owned(),
        audience_id: Some(id.as_str().to_owned()),
    };
    Ok(Json(web.core.commands(&query).await.map_err(from_core)?))
}

#[cfg(test)]
mod tests;
