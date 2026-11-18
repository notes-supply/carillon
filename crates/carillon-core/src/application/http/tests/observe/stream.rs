//! `GET /v1/observe` through core's router. A reader here takes the
//! response body frame by frame, so each case decides when the stream is
//! read. Cases that ask no runtime run on a paused clock, where a wait for
//! silence costs no time and ends only once core has nothing left to do.

use std::{
    collections::HashMap,
    future,
    sync::atomic::{AtomicU32, Ordering},
};

use anyhow::{Context as _, bail};
use axum::{
    body::{Body, BodyDataStream},
    http::Request,
};
use carillon_protocol::{
    model::JOB_READ_LIMIT,
    observation::{Ending, Opened, Progress, Snapshot},
};
use futures_util::StreamExt as _;
use tempfile::tempdir;
use tower::ServiceExt as _;

use super::{
    super::{Fixture, accept, answer, configured, hand_next, installation},
    audiences, key, live, recorded, say, say_elsewhere, second_interface, texts,
};
use crate::application::http::*;

const OWN: Option<&str> = Some("test-only");
const OPEN: &str = "agent_id=test-agent&audience_id=conversation";

/// How core answers `provider`, presenting `token`, asking to observe `query`.
async fn asked(
    state: &AppState,
    provider: &str,
    token: Option<&str>,
    query: &str,
) -> Result<Response> {
    let mut request = Request::builder()
        .uri(format!("/v1/observe?{query}"))
        .header(PROVIDER_HEADER, provider);
    if let Some(token) = token {
        request = request.header(http::header::AUTHORIZATION, format!("Bearer {token}"));
    }
    Ok(router(state.clone())
        .oneshot(request.body(Body::empty())?)
        .await?)
}

/// The refusal core answers `query` with: its status and what it says.
async fn refused(
    state: &AppState,
    provider: &str,
    token: Option<&str>,
    query: &str,
) -> Result<(StatusCode, String)> {
    let response = asked(state, provider, token, query).await?;
    let status = response.status();
    let body = axum::body::to_bytes(response.into_body(), 1 << 20).await?;
    let body: Value = serde_json::from_slice(&body)?;
    let said = body["error"].as_str().context("no refusal")?.to_owned();
    anyhow::ensure!(body == json!({ "error": said }), "{body}");
    Ok((status, said))
}

/// One reader of an observation. Core composes its next snapshot only
/// while the reader waits for one.
struct Reader(BodyDataStream);

/// Opens `query` as `test-interface`.
async fn open(state: &AppState, query: &str) -> Result<Reader> {
    let response = asked(state, "test-interface", OWN, query).await?;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.headers()[http::header::CONTENT_TYPE],
        "text/event-stream"
    );
    Ok(Reader(response.into_body().into_data_stream()))
}

impl Reader {
    /// The next snapshot sent, or `None` once the stream has ended.
    async fn next(&mut self) -> Result<Option<Snapshot>> {
        let sent = async {
            loop {
                let Some(frame) = self.0.next().await.transpose()? else {
                    return Ok(None);
                };
                let frame = std::str::from_utf8(&frame)?;
                if frame == ":\n\n" {
                    continue;
                }
                let snapshot = frame
                    .strip_prefix("event: snapshot\ndata: ")
                    .and_then(|rest| rest.strip_suffix("\n\n"))
                    .with_context(|| format!("not one snapshot event: {frame:.120}"))?;
                return Ok(Some(serde_json::from_str(snapshot)?));
            }
        };
        tokio::time::timeout(Duration::from_secs(30), sent)
            .await
            .context("the stream neither sent a snapshot nor ended")?
    }

    async fn snapshot(&mut self) -> Result<Snapshot> {
        self.next().await?.context("the stream ended")
    }

    /// The first snapshot sent that `shown` holds of.
    async fn until(&mut self, shown: impl Fn(&Snapshot) -> bool) -> Result<Snapshot> {
        loop {
            let snapshot = self.snapshot().await?;
            if shown(&snapshot) {
                return Ok(snapshot);
            }
        }
    }

    /// Fails if the stream sends a snapshot or ends. On a paused clock the
    /// wait ends once core has nothing left to do for this reader.
    async fn silent(&mut self) -> Result<()> {
        match tokio::time::timeout(Duration::from_secs(5), self.next()).await {
            Err(_) => Ok(()),
            Ok(sent) => bail!("the stream was not silent: {:.120}", format!("{:?}", sent?)),
        }
    }
}

/// How far each turn the open conversation shows has come.
fn progress(snapshot: &Snapshot) -> Result<Vec<Progress>> {
    let (_, turns, _) = recorded(snapshot)?;
    Ok(turns.iter().map(|turn| turn.progress.clone()).collect())
}

fn answered(text: &str, finished_at: i64) -> Progress {
    Progress::Ended {
        finished_at,
        ending: Ending::Answered {
            text: text.to_owned(),
            files: Vec::new(),
        },
        report: None,
    }
}

#[tokio::test(start_paused = true)]
async fn an_open_stream_is_sent_every_change_of_what_its_view_shows() -> Result<()> {
    let directory = tempdir()?;
    let fixture = installation(directory.path())?;
    let state = configured(&fixture);
    let mut reader = open(&state, OPEN).await?;
    assert_eq!(
        reader.snapshot().await?,
        Snapshot {
            conversations: Vec::new(),
            more: false,
            open: Some(Opened::Unrecorded {
                key: key("conversation")?
            }),
        }
    );

    // Its first message records the conversation, on the stream already open.
    let receipt = accept(&fixture, "one", "conversation").await?;
    let accepted = reader.snapshot().await?;
    let (conversation, turns, _) = recorded(&accepted)?;
    assert_eq!(accepted.conversations, std::slice::from_ref(conversation));
    assert_eq!(
        (turns[0].turn_id.as_str(), turns.len()),
        (receipt.turn_id.as_str(), 1)
    );
    assert_eq!(progress(&accepted)?, [Progress::Queued]);

    // No runtime listens here, so the running turn shows no activity.
    let held = hand_next(&state.core, "test-runtime")
        .await?
        .context("queued")?;
    let running = Progress::Running { activity: None };
    assert_eq!(
        progress(&reader.snapshot().await?)?,
        std::slice::from_ref(&running)
    );

    let opening = carillon_protocol::turns::TurnOpening {
        interface_id: "test-interface".to_owned(),
        agent_id: "test-agent".to_owned(),
        audience_id: "conversation".to_owned(),
        key: "nightly".to_owned(),
        label: None,
    };
    let opened = state.core.open_turn("test-runtime", &opening)?;
    let both = [running.clone(), running.clone()];
    assert_eq!(progress(&reader.snapshot().await?)?, both);

    answer(&state.core, "test-runtime", &held, None)?;
    let ended = reader.snapshot().await?;
    let at = recorded(&ended)?.0.active_at;
    assert_eq!(progress(&ended)?, [answered("reply to one", at), running]);

    let stopped = carillon_protocol::turns::TurnOutcome::Stopped {};
    state
        .core
        .settle("test-runtime", &opened.receipt.turn_id, stopped)?;
    let last = reader.snapshot().await?;
    let (conversation, turns, _) = recorded(&last)?;
    assert_eq!(conversation.at_work, 0);
    assert!(matches!(
        &turns[1].progress,
        Progress::Ended {
            ending: Ending::Stopped,
            ..
        }
    ));

    // A message its runtime refuses ends interrupted, with nothing run.
    accept(&fixture, "two", "conversation").await?;
    assert_eq!(progress(&reader.snapshot().await?)?[2], Progress::Queued);
    let refuse = async |_| {
        Err(crate::InboxFailure::Refused {
            status: 422,
            explanation: None,
        })
    };
    let stopping = CancellationToken::new();
    state
        .core
        .dispatch_next("test-runtime", &stopping, refuse)
        .await?;
    let refused = reader.snapshot().await?;
    assert!(matches!(
        &recorded(&refused)?.1[2].progress,
        Progress::Ended {
            ending: Ending::Interrupted { .. },
            ..
        }
    ));
    reader.silent().await
}

#[tokio::test(start_paused = true)]
async fn a_reader_that_falls_behind_is_sent_the_latest_snapshot_and_no_backlog() -> Result<()> {
    let directory = tempdir()?;
    let fixture = installation(directory.path())?;
    let state = configured(&fixture);
    let mut reader = open(&state, OPEN).await?;
    reader.snapshot().await?;
    let burst = (0..12).map(|index| format!("message {index}"));
    for message in burst.clone() {
        accept(&fixture, &message, "conversation").await?;
    }
    // Twelve changes while it was not reading are one snapshot when it reads.
    let latest = reader.snapshot().await?;
    assert_eq!(texts(recorded(&latest)?.1), burst.collect::<Vec<_>>());
    reader.silent().await?;
    accept(&fixture, "one more", "conversation").await?;
    assert_eq!(texts(recorded(&reader.snapshot().await?)?.1).len(), 13);
    reader.silent().await
}

#[tokio::test(start_paused = true)]
async fn a_change_right_after_a_snapshot_is_composed_half_a_second_after_it() -> Result<()> {
    let directory = tempdir()?;
    let fixture = installation(directory.path())?;
    let state = configured(&fixture);
    let mut reader = open(&state, OPEN).await?;
    reader.snapshot().await?;
    for message in ["one", "two", "three"] {
        let sent = tokio::time::Instant::now();
        accept(&fixture, message, "conversation").await?;
        reader.snapshot().await?;
        assert!(sent.elapsed() >= Duration::from_millis(500), "{message}");
    }
    // A change after a quiet half second is composed at once.
    tokio::time::advance(Duration::from_millis(500)).await;
    let quiet = tokio::time::Instant::now();
    accept(&fixture, "four", "conversation").await?;
    reader.snapshot().await?;
    assert!(quiet.elapsed() < Duration::from_millis(500));
    Ok(())
}

#[tokio::test(start_paused = true)]
async fn a_stream_is_sent_nothing_for_a_change_its_view_does_not_show() -> Result<()> {
    let directory = tempdir()?;
    let mut fixture = installation(directory.path())?;
    second_interface(&mut fixture, directory.path())?;
    accept(&fixture, "mine", "conversation").await?;
    let state = configured(&fixture);
    let held = hand_next(&state.core, "test-runtime")
        .await?
        .context("queued")?;
    answer(&state.core, "test-runtime", &held, None)?;
    let mut reader = open(&state, OPEN).await?;
    let first = reader.snapshot().await?;

    // Another interface's turn in a conversation of the same name.
    say_elsewhere(&fixture, "theirs", "conversation").await?;
    reader.silent().await?;
    // The interface's own change of what no view shows: its delivery.
    let delivery = state.core.claim_delivery("test-interface", "instance")?;
    assert_eq!(
        delivery.context("nothing to deliver")?.content,
        "reply to mine"
    );
    reader.silent().await?;

    accept(&fixture, "mine again", "conversation").await?;
    let next = reader.snapshot().await?;
    assert_eq!(texts(recorded(&next)?.1), ["mine", "mine again"]);
    assert_eq!(next.conversations.len(), first.conversations.len());
    Ok(())
}

#[tokio::test(start_paused = true)]
async fn a_conversation_is_shown_however_long_the_audience_admission_accepted() -> Result<()> {
    let directory = tempdir()?;
    let fixture = installation(directory.path())?;
    let state = configured(&fixture);
    let long = "a".repeat(4096);
    accept(&fixture, "in the long one", &long).await?;
    accept(&fixture, "in another", "conversation").await?;

    let another = open(&state, OPEN).await?.snapshot().await?;
    assert_eq!(audiences(&another.conversations), ["conversation", &long]);
    assert_eq!(texts(recorded(&another)?.1), ["in another"]);
    let named = format!("agent_id=test-agent&audience_id={long}");
    let opened = open(&state, &named).await?.snapshot().await?;
    assert_eq!(texts(recorded(&opened)?.1), ["in the long one"]);
    Ok(())
}

/// A runtime that says every turn it is asked about has made `calls` calls,
/// and counts how often it was asked.
#[derive(Clone, Default)]
struct Describing {
    calls: Arc<AtomicU32>,
    asked: watch::Sender<u32>,
}

impl Describing {
    /// Starts it as the runtime of `fixture`, saying one call was made.
    async fn start(fixture: &mut Fixture) -> Result<Self> {
        let runtime = Self::default();
        runtime.calls.store(1, Ordering::SeqCst);
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        fixture.config.value.providers[1].url = format!("http://{}", listener.local_addr()?);
        let described = runtime.clone();
        let activity = get(move || {
            described.asked.send_modify(|asked| *asked += 1);
            let said = live(described.calls.load(Ordering::SeqCst), &[]);
            async move {
                said.map(Json)
                    .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)
            }
        });
        let router = Router::new().route("/v1/turns/{id}/activity", activity);
        tokio::spawn(async move { axum::serve(listener, router).await });
        Ok(runtime)
    }

    fn asked(&self) -> u32 {
        *self.asked.borrow()
    }

    /// Returns once it has been asked `times` times.
    async fn asked_times(&self, times: u32) -> Result<()> {
        let mut asked = self.asked.subscribe();
        asked.wait_for(|&asked| asked >= times).await?;
        Ok(())
    }
}

/// The calls the running turn `snapshot` shows has made, as its runtime said.
fn calls(snapshot: &Snapshot) -> Result<Option<u32>> {
    match progress(snapshot)?.as_slice() {
        [Progress::Running { activity }] => Ok(activity.as_ref().map(|said| said.tool_calls)),
        other => bail!("not one running turn: {other:?}"),
    }
}

#[tokio::test]
async fn a_running_turn_is_sent_again_when_its_runtime_says_something_new() -> Result<()> {
    let directory = tempdir()?;
    let mut fixture = installation(directory.path())?;
    let runtime = Describing::start(&mut fixture).await?;
    accept(&fixture, "one", "conversation").await?;
    let state = configured(&fixture);
    let held = hand_next(&state.core, "test-runtime")
        .await?
        .context("queued")?;
    let mut reader = open(&state, OPEN).await?;
    assert_eq!(calls(&reader.snapshot().await?)?, Some(1));

    // Nothing is committed: the runtime alone says something new.
    runtime.calls.store(2, Ordering::SeqCst);
    assert_eq!(calls(&reader.snapshot().await?)?, Some(2));
    assert_eq!(runtime.asked(), 2);

    // Asked twice more, the runtime says the same, and nothing is sent.
    // Core asks a fifth time only once it has taken the fourth answer.
    tokio::select! {
        sent = reader.next() => bail!("sent what the reader already had: {sent:?}"),
        asked = runtime.asked_times(5) => asked?,
    }
    answer(&state.core, "test-runtime", &held, None)?;
    let ended = reader.snapshot().await?;
    let at = recorded(&ended)?.0.active_at;
    assert_eq!(progress(&ended)?, [answered("reply to one", at)]);
    Ok(())
}

#[tokio::test]
async fn core_asks_a_runtime_once_for_each_snapshot_a_reader_takes_and_never_for_one_gone()
-> Result<()> {
    let directory = tempdir()?;
    let mut fixture = installation(directory.path())?;
    let runtime = Describing::start(&mut fixture).await?;
    accept(&fixture, "one", "conversation").await?;
    let state = configured(&fixture);
    hand_next(&state.core, "test-runtime").await?;
    let mut gone = open(&state, OPEN).await?;
    assert_eq!(calls(&gone.snapshot().await?)?, Some(1));
    drop(gone);

    let mut reader = open(&state, OPEN).await?;
    assert_eq!(calls(&reader.snapshot().await?)?, Some(1));
    for said in 2..4 {
        runtime.calls.store(said, Ordering::SeqCst);
        assert_eq!(calls(&reader.snapshot().await?)?, Some(said));
    }
    // One for the reader that left, and one for each of the three
    // snapshots this one took, though a second passed between them.
    assert_eq!(runtime.asked(), 4);
    Ok(())
}

#[tokio::test(start_paused = true)]
async fn many_running_turns_take_no_longer_to_show_than_one_when_their_runtime_never_answers()
-> Result<()> {
    let directory = tempdir()?;
    let mut fixture = installation(directory.path())?;
    // Nothing takes a connection from this listener, so the system accepts
    // each one and no request on it is ever answered.
    let silent = TcpListener::bind("127.0.0.1:0").await?;
    fixture.config.value.providers[1].url = format!("http://{}", silent.local_addr()?);
    accept(&fixture, "one", "conversation").await?;
    let state = configured(&fixture);
    hand_next(&state.core, "test-runtime").await?;
    // One more than core asks about at once.
    let many = JOB_READ_LIMIT + 1;
    for key in 1..many {
        let opening = carillon_protocol::turns::TurnOpening {
            interface_id: "test-interface".to_owned(),
            agent_id: "test-agent".to_owned(),
            audience_id: "conversation".to_owned(),
            key: key.to_string(),
            label: None,
        };
        state.core.open_turn("test-runtime", &opening)?;
    }

    let running = Progress::Running { activity: None };
    let first_of = async |turns: usize| -> Result<Duration> {
        let asked = tokio::time::Instant::now();
        let mut reader = open(&state, &format!("{OPEN}&turns={turns}")).await?;
        let first = reader.snapshot().await?;
        assert_eq!(progress(&first)?, vec![running.clone(); turns]);
        Ok(asked.elapsed())
    };
    assert_eq!(first_of(many).await?, first_of(1).await?);
    Ok(())
}

#[tokio::test]
async fn a_turn_shows_what_its_runtime_said_though_an_older_turn_is_never_described() -> Result<()>
{
    let directory = tempdir()?;
    let mut fixture = installation(directory.path())?;
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    fixture.config.value.providers[1].url = format!("http://{}", listener.local_addr()?);
    let state = configured(&fixture);
    let mut turns = Vec::new();
    for message in ["one", "two", "three"] {
        turns.push(accept(&fixture, message, "conversation").await?.turn_id);
        hand_next(&state.core, "test-runtime").await?;
    }
    // The runtime never answers about the oldest turn.
    let made = HashMap::from([(turns[1].clone(), 2), (turns[2].clone(), 3)]);
    let activity = get(move |AxumPath(turn): AxumPath<String>| {
        let calls = made.get(&turn).copied();
        async move {
            match calls {
                Some(calls) => live(calls, &[])
                    .map(Json)
                    .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR),
                None => future::pending().await,
            }
        }
    });
    let router = Router::new().route("/v1/turns/{id}/activity", activity);
    tokio::spawn(async move { axum::serve(listener, router).await });

    let first = open(&state, OPEN).await?.snapshot().await?;
    let described = |calls| -> Result<Progress> {
        let activity = Some(live(calls, &[])?);
        Ok(Progress::Running { activity })
    };
    let silent = Progress::Running { activity: None };
    assert_eq!(progress(&first)?, [silent, described(2)?, described(3)?]);
    Ok(())
}

#[tokio::test]
async fn a_view_is_refused_only_once_its_caller_is_authenticated() -> Result<()> {
    let directory = tempdir()?;
    let mut fixture = installation(directory.path())?;
    second_interface(&mut fixture, directory.path())?;
    fixture.config.value.providers[2].enabled = false;
    let state = configured(&fixture);
    let window = "a window holds 1 to 500 items";
    let named = "a conversation is named by its agent and its audience";
    for (query, reason) in [
        ("conversations=0", window),
        ("conversations=501", window),
        ("conversations=many", "invalid digit"),
        ("agent_id=test-agent&audience_id=someone&turns=0", window),
        ("agent_id=test-agent&audience_id=someone&turns=501", window),
        ("agent_id=test-agent", named),
        ("audience_id=someone", named),
        ("agent_id=&audience_id=someone", named),
        ("turns=5", "a window of turns needs the conversation"),
        (
            "interface_id=other-interface",
            "unknown field `interface_id`",
        ),
        ("conversations=1&conversations=2", "duplicate field"),
    ] {
        let (status, said) = refused(&state, "test-interface", OWN, query).await?;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{query}: {said}");
        assert!(said.contains(reason), "{query}: {said}");
        // A caller core does not know learns nothing of the view.
        for (provider, token, refusal) in [
            ("test-interface", None, "component authentication required"),
            (
                "test-interface",
                Some("wrong"),
                "component authentication failed",
            ),
            ("test-runtime", OWN, "unknown interface provider"),
            ("unknown", OWN, "unknown interface provider"),
            (
                "other-interface",
                Some("other-only"),
                "interface provider is disabled",
            ),
        ] {
            assert_eq!(
                refused(&state, provider, token, query).await?,
                (StatusCode::FORBIDDEN, refusal.to_owned()),
                "{provider} with {token:?} asked for {query}"
            );
        }
    }
    Ok(())
}

#[tokio::test(start_paused = true)]
async fn a_snapshot_of_16_mib_is_sent_and_one_a_byte_over_is_refused_and_ends_its_stream()
-> Result<()> {
    let directory = tempdir()?;
    let fixture = installation(directory.path())?;
    let state = configured(&fixture);
    let limit = 16 << 20;
    let size = |snapshot: &Snapshot| anyhow::Ok(serde_json::to_string(snapshot)?.len());
    let of = |audience: &str| format!("agent_id=test-agent&audience_id={audience}");
    let said = async |audience: &str, turn: u8, text: String| {
        let event = format!("{audience} {turn}");
        say(&fixture, "test-interface", &event, audience, text).await
    };
    for audience in ["exact", "above"] {
        said(audience, 1, "x".to_owned()).await?;
    }
    // A reader of the conversation with `audience`, once more turns have
    // brought its snapshot to `over` bytes more than the limit.
    let grown = async |audience: &str, over: usize| -> Result<Reader> {
        let mut reader = open(&state, &of(audience)).await?;
        reader.snapshot().await?;
        // An event takes at most 2 MiB, so turns of 1 MiB bring the
        // snapshot near the limit, and one more makes up the rest.
        let megabyte = 1 << 20;
        for turn in 2..=16 {
            said(audience, turn, "x".repeat(megabyte)).await?;
        }
        let near = open(&state, &of(audience)).await?.snapshot().await?;
        // The last turn takes what the one before it takes but for its
        // text, and a comma.
        let before = recorded(&near)?.1.last().context("no turn")?;
        let rest = serde_json::to_string(before)?.len() - megabyte + 1;
        let text = "x".repeat(limit + over - size(&near)? - rest);
        said(audience, 17, text).await?;
        Ok(reader)
    };

    let mut exact = grown("exact", 0).await?;
    assert_eq!(size(&exact.snapshot().await?)?, limit);
    let first = open(&state, &of("exact")).await?.snapshot().await?;
    assert_eq!(size(&first)?, limit);

    let mut above = grown("above", 1).await?;
    assert!(above.next().await?.is_none(), "a snapshot over the limit");
    let (status, said) = refused(&state, "test-interface", OWN, &of("above")).await?;
    assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE);
    assert!(said.contains(&format!("{} bytes", limit + 1)), "{said}");
    assert!(said.ends_with("ask for fewer turns"), "{said}");
    let mut fewer = open(&state, &format!("{}&turns=1", of("above"))).await?;
    let newest = fewer.snapshot().await?;
    let (_, turns, earlier) = recorded(&newest)?;
    assert_eq!((turns.len(), earlier), (1, true));
    Ok(())
}

#[tokio::test(start_paused = true)]
async fn open_streams_end_when_core_stops() -> Result<()> {
    let directory = tempdir()?;
    let fixture = installation(directory.path())?;
    accept(&fixture, "one", "conversation").await?;
    let state = configured(&fixture);
    let mut readers = [open(&state, OPEN).await?, open(&state, "").await?];
    for reader in &mut readers {
        reader.snapshot().await?;
    }
    state.stopping.cancel();
    for reader in &mut readers {
        assert_eq!(reader.next().await?, None);
    }
    Ok(())
}

#[tokio::test(start_paused = true)]
async fn a_stream_ends_at_once_when_its_snapshot_cannot_be_composed() -> Result<()> {
    let directory = tempdir()?;
    let fixture = installation(directory.path())?;
    accept(&fixture, "one", "conversation").await?;
    let state = configured(&fixture);
    let mut reader = open(&state, OPEN).await?;
    reader.snapshot().await?;
    let storage =
        rusqlite::Connection::open(fixture.config.value.data_dir.join("carillon.sqlite3"))?;
    // A turn stored as no turn can be, which the next composition meets.
    storage.execute("UPDATE turns SET state = 'completed'", [])?;
    let before = tokio::time::Instant::now();
    accept(&fixture, "elsewhere", "another").await?;
    assert_eq!(reader.next().await?, None);
    // It ended with the composition that failed: nothing waited to try again.
    assert!(
        before.elapsed() < Duration::from_secs(1),
        "{:?}",
        before.elapsed()
    );

    let (status, _) = refused(&state, "test-interface", OWN, OPEN).await?;
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
    storage.execute("UPDATE turns SET state = 'queued'", [])?;
    let again = open(&state, OPEN).await?.snapshot().await?;
    assert_eq!(again.conversations.len(), 2);
    assert_eq!(progress(&again)?, [Progress::Queued]);
    Ok(())
}

#[tokio::test]
async fn an_observation_asked_for_once_a_stop_closed_storage_is_refused_as_unavailable()
-> Result<()> {
    let directory = tempdir()?;
    let fixture = installation(directory.path())?;
    let state = configured(&fixture);
    state.core.storage_settled().await;
    let (status, said) = refused(&state, "test-interface", OWN, OPEN).await?;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{said}");
    Ok(())
}

#[tokio::test(start_paused = true)]
async fn a_core_started_again_on_its_storage_sends_a_complete_first_snapshot() -> Result<()> {
    let directory = tempdir()?;
    let fixture = installation(directory.path())?;
    accept(&fixture, "held", "conversation").await?;
    hand_next(&fixture.core, "test-runtime").await?;
    let opening = carillon_protocol::turns::TurnOpening {
        interface_id: "test-interface".to_owned(),
        agent_id: "test-agent".to_owned(),
        audience_id: "conversation".to_owned(),
        key: "nightly".to_owned(),
        label: Some("opened".to_owned()),
    };
    fixture.core.open_turn("test-runtime", &opening)?;
    accept(&fixture, "queued", "conversation").await?;

    let core = Core::open(core_options(&fixture.config))?;
    core.recover_interrupted()?;
    let state = AppState {
        core,
        ..configured(&fixture)
    };
    let first = open(&state, OPEN).await?.snapshot().await?;
    let (conversation, turns, _) = recorded(&first)?;
    assert_eq!(conversation.at_work, 3);
    assert_eq!(texts(turns), ["held", "opened", "queued"]);
    // The turn core handed over waits to be handed over again; the one its
    // runtime opened still runs.
    let running = Progress::Running { activity: None };
    assert_eq!(
        progress(&first)?,
        [Progress::Queued, running, Progress::Queued]
    );
    Ok(())
}

/// A conversation whose snapshot takes long enough to compose that a
/// change committed meanwhile lands while it is composed.
async fn slow_to_compose(fixture: &Fixture) -> Result<()> {
    for index in 0..12 {
        let text = "long ".repeat((1 << 20) / 5);
        say(
            fixture,
            "test-interface",
            &format!("long {index}"),
            "conversation",
            text,
        )
        .await?;
    }
    Ok(())
}

fn shows(text: &str) -> impl Fn(&Snapshot) -> bool {
    move |snapshot| recorded(snapshot).is_ok_and(|(_, turns, _)| texts(turns).contains(&text))
}

/// Timing, not proof of the order: with the subscription made after the
/// first read, this still passes if no round's write lands inside the
/// composition.
#[tokio::test(start_paused = true)]
async fn by_timing_a_change_committed_while_the_first_snapshot_is_composed_reaches_the_stream()
-> Result<()> {
    let directory = tempdir()?;
    let fixture = installation(directory.path())?;
    slow_to_compose(&fixture).await?;
    let state = configured(&fixture);
    for round in 0..4 {
        let message = format!("round {round}");
        let (writer, said) = (fixture.clone(), message.clone());
        let written = tokio::spawn(async move { accept(&writer, &said, "conversation").await });
        let mut reader = open(&state, OPEN).await?;
        written.await??;
        reader.until(shows(&message)).await?;
    }
    Ok(())
}

/// Timing, not proof of the order: with the mark made after the composition,
/// this still passes if no round's second write lands inside one.
#[tokio::test(start_paused = true)]
async fn by_timing_a_change_committed_while_a_snapshot_is_composed_is_sent_next() -> Result<()> {
    let directory = tempdir()?;
    let fixture = installation(directory.path())?;
    slow_to_compose(&fixture).await?;
    let state = configured(&fixture);
    let mut reader = open(&state, OPEN).await?;
    reader.snapshot().await?;
    for round in 0..6 {
        // Past the least time between compositions, so the first change
        // is composed at once and the second lands while it is.
        tokio::time::advance(Duration::from_secs(1)).await;
        let (first, second) = (format!("first of {round}"), format!("second of {round}"));
        let (writer, both) = (fixture.clone(), [first, second.clone()]);
        let written = tokio::spawn(async move {
            for message in both {
                accept(&writer, &message, "conversation").await?;
            }
            anyhow::Ok(())
        });
        reader.until(shows(&second)).await?;
        written.await??;
    }
    Ok(())
}
