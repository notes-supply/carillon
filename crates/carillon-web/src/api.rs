//! The browser API, under `/api`: what the page reads and sends. Every
//! route but `sign_in` is behind the browser's token. Core's wire types are
//! parsed here into the page's own view types and never forwarded.

use std::sync::Arc;

use axum::{
    Json, Router,
    extract::DefaultBodyLimit,
    extract::{Path, Query, State},
    http::StatusCode,
    routing::{get, put},
};
use carillon_interface::InterfaceError;
use carillon_protocol::{
    conversations::{ConversationSummary, TranscriptTurn, TurnSource},
    model::{TurnActivity, TurnState},
    turns::Silence,
};
use serde::{Deserialize, Serialize};

use crate::{
    browser::refusal,
    files::{self, FileView},
    identity::{ConversationId, known},
    markdown::render,
    send,
    serve::Web,
};

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
        custom_site: web.site.is_custom(),
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
    id: ConversationId,
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
        Some(Self {
            id: ConversationId::parse(&summary.audience_id)?,
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
    let page = web.core.conversations(before).await?;
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
    async fn of(web: &Web, turn: TranscriptTurn) -> Self {
        let from = match turn.source {
            TurnSource::Message {
                event_id,
                text,
                command,
                files,
            } => {
                let mut views = Vec::with_capacity(files.len());
                for file in &files {
                    views.push(files::view(web, &file.path, &file.name, file.size).await);
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
                    views.push(files::view(web, &file.path, &file.name, file.size).await);
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
    Path(id): Path<ConversationId>,
    Query(Before { before }): Query<Before<String>>,
) -> Result<Json<Turns>, InterfaceError> {
    let conversation = known(&web.core, id).await?;
    let page = web
        .core
        .transcript(
            &conversation.agent_id,
            &conversation.audience_id,
            before.as_deref(),
        )
        .await?;
    let before = page
        .turns
        .first()
        .filter(|_| page.earlier)
        .map(|first| first.turn_id.clone());
    let mut turns = Vec::with_capacity(page.turns.len());
    for turn in page.turns {
        turns.push(Turn::of(&web, turn).await);
    }
    Ok(Json(Turns { turns, before }))
}

#[cfg(test)]
mod tests;
