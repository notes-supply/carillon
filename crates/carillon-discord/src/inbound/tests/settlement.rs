//! Which gateway messages a stop leaves unsettled, so a resumed session
//! replays them, and how a replay meets the ones core already accepted.

use std::{
    collections::{BTreeSet, HashMap},
    sync::{Arc, Mutex},
    time::Duration,
};

use anyhow::{Context as _, anyhow};
use axum::{
    Json, Router,
    extract::{Path, State},
    http::StatusCode,
    response::{IntoResponse as _, Response},
    routing::{get, post},
};
use carillon_protocol::model::{EventReceipt, EventStatus, ExternalEvent, TurnState};
use serde_json::Value;
use tokio::sync::mpsc;

use super::{discord_client, ingress};
use crate::{
    attachments::{
        Downloads,
        tests::{Serve, attachment, serve_cdn},
    },
    client::test_client as core_client,
    config::{DiscordActivation, DiscordConversation, DiscordRoute},
    conversations::{IncomingMessage, Participation, tests::discord_config},
    gateway::Dispatch,
    inbound::{QueuedMessage, Settlements, lanes},
};

/// What the fake core does with each event, and what it saw.
#[derive(Default)]
struct Core {
    /// Events it holds.
    accepted: BTreeSet<String>,
    /// Events whose submission it refuses.
    refused: BTreeSet<String>,
    /// Events whose submission never gets an answer it confirms.
    lost: BTreeSet<String>,
    /// Events whose first status lookup never answers.
    stalled: BTreeSet<String>,
    /// Status lookups and submissions, by event.
    lookups: HashMap<String, usize>,
    submissions: HashMap<String, usize>,
}

type Shared = Arc<Mutex<Core>>;

fn lock(core: &Shared) -> std::sync::MutexGuard<'_, Core> {
    core.lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn receipt(event_id: &str, duplicate: bool) -> EventReceipt {
    EventReceipt {
        event_id: event_id.to_owned(),
        turn_id: format!("turn-{event_id}"),
        state: TurnState::Queued,
        duplicate,
    }
}

async fn submit(State(core): State<Shared>, Json(event): Json<ExternalEvent>) -> Response {
    let id = event.event_id;
    let mut core = lock(&core);
    *core.submissions.entry(id.clone()).or_default() += 1;
    if core.refused.contains(&id) {
        return (StatusCode::CONFLICT, "refused").into_response();
    }
    if core.lost.contains(&id) {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    }
    let duplicate = !core.accepted.insert(id.clone());
    Json(receipt(&id, duplicate)).into_response()
}

async fn status(State(core): State<Shared>, Path(id): Path<String>) -> Response {
    let (lookup, stalled, held, lost) = {
        let mut core = lock(&core);
        let lookup = core.lookups.entry(id.clone()).or_default();
        *lookup += 1;
        let lookup = *lookup;
        (
            lookup,
            core.stalled.contains(&id),
            core.accepted.contains(&id),
            core.lost.contains(&id),
        )
    };
    if stalled && lookup == 1 {
        std::future::pending::<()>().await;
    }
    // After a stalled lookup, or a submission whose answer was lost, core
    // will not say what became of the event.
    if (stalled || lost) && lookup > 1 {
        return (StatusCode::BAD_REQUEST, "no").into_response();
    }
    Json(if held {
        EventStatus::Accepted(receipt(&id, true))
    } else {
        EventStatus::Unknown
    })
    .into_response()
}

/// Message `id` from `user` in `channel`, with a file when `cdn` serves one.
fn message(
    id: &str,
    (channel, user): (&str, &str),
    cdn: Option<u16>,
    settings: &Arc<crate::config::DiscordConfig>,
) -> QueuedMessage {
    QueuedMessage {
        request: IncomingMessage {
            event_id: id.to_owned(),
            guild_id: Some("100".to_owned()),
            user_id: user.to_owned(),
            channel_id: channel.to_owned(),
            parent_channel_id: None,
            content: "hello".to_owned(),
            attachments: cdn
                .map(|cdn| vec![attachment(cdn, "601", "a.bin", 8)])
                .unwrap_or_default(),
        },
        native: Value::Null,
        settings: settings.clone(),
        dispatch: Some(Dispatch::new(1, id.parse().unwrap_or_default())),
    }
}

/// One process's lanes over `core`, reporting settlements on the returned
/// receiver.
async fn lanes_over(
    core: &Shared,
    directory: &std::path::Path,
    cdn: u16,
) -> anyhow::Result<(
    Arc<crate::inbound::Ingress>,
    Arc<crate::config::DiscordConfig>,
    mpsc::UnboundedReceiver<Dispatch>,
    tokio::task::JoinHandle<std::io::Result<()>>,
)> {
    let app = Router::new()
        .route("/v1/events", post(submit))
        .route("/v1/events/{event_id}", get(status))
        .with_state(core.clone())
        .fallback(|| async { StatusCode::NO_CONTENT });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let address = listener.local_addr()?;
    let server = tokio::spawn(async move { axum::serve(listener, app).await });
    let mut config = discord_config(DiscordActivation::Always, DiscordConversation::Channel);
    config.typing = false;
    for channel in ["302", "304", "306"] {
        config.routes.push(DiscordRoute {
            channel_id: channel.to_owned(),
            ..config.routes[0].clone()
        });
    }
    let settings = Arc::new(config.clone());
    let mut ingress = ingress(
        core_client(format!("http://{address}"))?,
        config,
        Arc::new(Participation::load(directory.join("participation.json"))?),
        discord_client(address),
        directory,
    )?;
    let (settled, settlements) = mpsc::unbounded_channel();
    let owned = Arc::get_mut(&mut ingress).context("shared ingress")?;
    owned.settlements = Settlements::new(settled);
    owned.downloads = Arc::new(
        Downloads::open(directory.join("incoming"), settings.attachments)?.with_fixture_cdn(cdn),
    );
    Ok((ingress, settings, settlements, server))
}

fn reported(settlements: &mut mpsc::UnboundedReceiver<Dispatch>) -> BTreeSet<u64> {
    std::iter::from_fn(|| settlements.try_recv().ok())
        .map(Dispatch::sequence)
        .collect()
}

#[tokio::test]
async fn a_stop_leaves_unsettled_exactly_what_core_may_still_need_replayed() -> anyhow::Result<()> {
    let directory = tempfile::tempdir()?;
    let (cdn, _) = serve_cdn(HashMap::from([(
        "/attachments/300/601/a.bin".to_owned(),
        Serve::Bytes(vec![1; 8]),
    )]))
    .await?;
    let core = Shared::default();
    {
        let mut core = lock(&core);
        core.refused.insert("11".to_owned());
        core.stalled.insert("12".to_owned());
        core.lost.insert("13".to_owned());
        core.lost.insert("16".to_owned());
    }
    let (ingress, settings, mut settlements, server) =
        lanes_over(&core, directory.path(), cdn).await?;
    let (send, receive) = mpsc::channel(lanes::MAX_PENDING);
    let dispatcher = tokio::spawn(lanes::dispatch(ingress.clone(), receive));
    // One conversation: accepted, refused by core, a lookup core never
    // answers, and one waiting behind it. Another: a submission whose answer
    // is lost. A third: a person who is not allowlisted.
    for (id, from, cdn) in [
        ("10", ("300", "200"), None),
        ("11", ("300", "200"), None),
        ("12", ("300", "200"), None),
        ("14", ("300", "200"), None),
        ("13", ("302", "200"), None),
        ("15", ("304", "999"), None),
        ("16", ("306", "200"), Some(cdn)),
    ] {
        send.send(message(id, from, cdn, &settings).into()).await?;
    }
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            {
                let core = lock(&core);
                if core.lookups.contains_key("12")
                    && core.submissions.contains_key("13")
                    && core.submissions.contains_key("16")
                {
                    break;
                }
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .map_err(|_| anyhow!("the preparations did not get under way"))?;
    // The provider stops.
    ingress.stopping.cancel();
    drop(send);
    tokio::time::timeout(Duration::from_secs(10), dispatcher).await??;
    ingress.reactions.stop(Duration::from_secs(5)).await;
    // Settled: accepted, refused by core, ignored. Not settled: the stalled
    // lookup and the lost answers, for text and for files, which core would
    // not resolve, and the message still waiting.
    assert_eq!(reported(&mut settlements), BTreeSet::from([10, 11, 15]));
    assert_eq!(lock(&core).submissions.get("14"), None);
    server.abort();

    // The next process: Discord replays from the floor. Core's identity
    // resolves the message it accepted, and the others are admitted.
    {
        let mut core = lock(&core);
        core.stalled.clear();
        core.lost.clear();
        core.lookups.clear();
    }
    let (ingress, settings, mut settlements, server) =
        lanes_over(&core, directory.path(), cdn).await?;
    let (send, receive) = mpsc::channel(lanes::MAX_PENDING);
    let dispatcher = tokio::spawn(lanes::dispatch(ingress.clone(), receive));
    for (id, channel, cdn) in [
        ("10", "300", None),
        ("12", "300", None),
        ("14", "300", None),
        ("13", "302", None),
        ("16", "306", Some(cdn)),
    ] {
        send.send(message(id, (channel, "200"), cdn, &settings).into())
            .await?;
    }
    drop(send);
    tokio::time::timeout(Duration::from_secs(10), dispatcher).await??;
    ingress.reactions.stop(Duration::from_secs(5)).await;
    assert_eq!(
        reported(&mut settlements),
        BTreeSet::from([10, 12, 13, 14, 16])
    );
    let core = lock(&core);
    assert_eq!(
        core.submissions.get("10"),
        Some(&1),
        "an accepted message was submitted again"
    );
    for id in ["12", "13", "14", "16"] {
        assert!(
            core.accepted.contains(id),
            "{id} was not accepted on replay"
        );
    }
    server.abort();
    Ok(())
}
