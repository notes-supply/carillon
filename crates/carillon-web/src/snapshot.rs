//! Everything the page shows, made from core's snapshot of the page's
//! view. The page replaces what it had with each one, so nothing here says
//! what changed.
//!
//! These types are the page's contract, and they are closed: the page
//! refuses a snapshot with a field it does not know. Only what a person may
//! see crosses. An answer and an interruption's notice arrive as HTML made
//! here, and a file as a signed link. Where a file is on this host, its
//! hash, and a conversation's channel stay behind. A runtime's own words
//! for a turn go on as it gave them.

use carillon_protocol::{
    conversations::TurnSource,
    model::TurnActivity,
    observation::{self, ObservedTurn},
    turns::ControlAction,
};
use serde::Serialize;

use crate::{
    files::{FileLink, Links},
    identity::ConversationKey,
    markdown::render,
    site::has_page,
};

#[derive(Serialize)]
pub(crate) struct ChatSnapshot {
    /// The configured agents, in registry order. A new conversation takes
    /// the first.
    agents: Vec<Agent>,
    limits: Limits,
    /// The page served is the owner's copy, not the release's.
    custom_site: bool,
    /// The latest active first.
    conversations: Vec<Conversation>,
    /// The interface has more conversations than are listed.
    more: bool,
    /// `None` when the view opened no conversation.
    open: Option<Opened>,
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

#[derive(Serialize)]
struct Conversation {
    key: ConversationKey,
    /// The first line of the first message, empty when it had only files.
    title: String,
    /// Unix seconds.
    started_at: i64,
    active_at: i64,
    /// Rises with every message, every turn the runtime opens and every
    /// turn's end.
    activity: i64,
    /// How many of its turns are queued or running.
    at_work: u32,
}

impl Conversation {
    fn keyed(key: ConversationKey, recorded: observation::Conversation) -> Self {
        Self {
            key,
            title: recorded.title,
            started_at: recorded.started_at,
            active_at: recorded.active_at,
            activity: recorded.activity,
            at_work: recorded.at_work,
        }
    }
}

#[derive(Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum Opened {
    /// Core has no conversation under this key. Its first admitted message
    /// records it.
    Unrecorded { key: ConversationKey },
    Recorded {
        conversation: Conversation,
        /// Its newest turns, oldest first.
        turns: Vec<Turn>,
        /// The conversation has turns older than those listed.
        earlier: bool,
    },
}

#[derive(Serialize)]
struct Turn {
    #[serde(rename = "turn_id")]
    id: String,
    source: Source,
    /// Unix seconds.
    accepted_at: i64,
    progress: Progress,
}

/// Who began a turn.
#[derive(Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum Source {
    Message {
        /// What a control of the turn names.
        event_id: String,
        /// As written: text, never markup.
        text: String,
        /// The provider took it for one of the runtime's commands.
        command: bool,
        files: Vec<FileLink>,
    },
    /// The runtime opened the turn itself, for a schedule or a subagent's
    /// report, and says what for.
    Opened {
        #[serde(skip_serializing_if = "Option::is_none")]
        label: Option<String>,
    },
}

/// How far a turn has come.
#[derive(Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
enum Progress {
    Queued,
    Running {
        /// `None` when the runtime gave no answer.
        activity: Option<Activity>,
        /// What a person may do with the turn now.
        controls: Vec<ControlAction>,
    },
    Ended {
        /// Unix seconds.
        finished_at: i64,
        ending: Ending,
        #[serde(skip_serializing_if = "Option::is_none")]
        report: Option<Activity>,
    },
}

#[derive(Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum Ending {
    Answered {
        html: String,
        files: Vec<FileLink>,
    },
    /// It did not complete. `html` is core's or the runtime's explanation.
    Interrupted {
        html: String,
    },
    /// The message went into a turn already running, whose answer covers it.
    Steered,
    /// Someone stopped it.
    Stopped,
}

/// A runtime's own words for a turn, the same for a running turn and an
/// ended one's report. It holds no controls: those sit on the running turn,
/// and a report can offer none.
#[derive(Serialize)]
#[serde(transparent)]
struct Activity(TurnActivity);

impl Activity {
    /// What the runtime said, apart from what a person may do.
    fn parted(mut said: TurnActivity) -> (Self, Vec<ControlAction>) {
        let controls = std::mem::take(&mut said.controls);
        (Self(said), controls)
    }
}

impl ChatSnapshot {
    /// What the page is shown of `core`'s snapshot of a view that opened
    /// `open`. A conversation the page did not make is not listed.
    pub(crate) async fn of(
        links: &Links<'_>,
        open: Option<&ConversationKey>,
        core: observation::Snapshot,
    ) -> Self {
        let registry = &links.web.registry.value;
        let attachments = &registry.core.attachments;
        let conversations = core.conversations.into_iter().filter_map(|listed| {
            Some(Conversation::keyed(
                ConversationKey::of(&listed.key)?,
                listed,
            ))
        });
        let opened = match open.zip(core.open) {
            None => None,
            Some((key, observation::Opened::Unrecorded { .. })) => {
                Some(Opened::Unrecorded { key: key.clone() })
            }
            Some((
                key,
                observation::Opened::Recorded {
                    conversation,
                    turns: observed,
                    earlier,
                },
            )) => {
                let mut turns = Vec::with_capacity(observed.len());
                for turn in observed {
                    turns.push(Turn::of(links, turn).await);
                }
                Some(Opened::Recorded {
                    conversation: Conversation::keyed(key.clone(), conversation),
                    turns,
                    earlier,
                })
            }
        };
        Self {
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
            custom_site: has_page(&links.web.site.custom),
            conversations: conversations.collect(),
            more: core.more,
            open: opened,
        }
    }
}

impl Turn {
    async fn of(links: &Links<'_>, turn: ObservedTurn) -> Self {
        let source = match turn.source {
            TurnSource::Message {
                event_id,
                text,
                command,
                files,
            } => {
                let mut sent = Vec::with_capacity(files.len());
                for file in &files {
                    sent.push(links.to(&file.path, &file.name, file.size).await);
                }
                Source::Message {
                    event_id,
                    text,
                    command,
                    files: sent,
                }
            }
            TurnSource::Opened { label } => Source::Opened { label },
        };
        let progress = match turn.progress {
            observation::Progress::Queued => Progress::Queued,
            observation::Progress::Running { activity } => {
                let (activity, controls) = activity.map(Activity::parted).unzip();
                Progress::Running {
                    activity,
                    controls: controls.unwrap_or_default(),
                }
            }
            observation::Progress::Ended {
                finished_at,
                ending,
                report,
            } => Progress::Ended {
                finished_at,
                ending: Ending::of(links, ending).await,
                report: report.map(|report| Activity::parted(report).0),
            },
        };
        Self {
            id: turn.turn_id,
            source,
            accepted_at: turn.accepted_at,
            progress,
        }
    }
}

impl Ending {
    async fn of(links: &Links<'_>, ending: observation::Ending) -> Self {
        match ending {
            observation::Ending::Answered { text, files } => {
                let mut attached = Vec::with_capacity(files.len());
                for file in &files {
                    attached.push(links.to(&file.path, &file.name, file.size).await);
                }
                Self::Answered {
                    html: render(&text),
                    files: attached,
                }
            }
            observation::Ending::Interrupted { notice } => Self::Interrupted {
                html: render(&notice),
            },
            observation::Ending::Steered => Self::Steered,
            observation::Ending::Stopped => Self::Stopped,
        }
    }
}
