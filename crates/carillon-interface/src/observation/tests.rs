//! An observation against a core that answers with exactly the bytes a
//! case feeds it, when the case feeds them.

use std::{
    io,
    sync::{Arc, Mutex},
    time::Duration,
};

use anyhow::{Context as _, Result, ensure};
use axum::{
    Json, Router,
    body::{Body, Bytes},
    http::{HeaderMap, StatusCode, Uri, header::AUTHORIZATION},
    response::IntoResponse as _,
    routing::get,
};
use carillon_protocol::{
    model::PROVIDER_HEADER,
    observation::{
        Conversation, ConversationKey, KEEP_ALIVE, Limit, OpenView, SNAPSHOT_LIMIT, Snapshot, View,
    },
};
use futures_util::stream;
use serde_json::json;
use tokio::sync::mpsc;

use super::Observation;
use crate::{CoreClient, InterfaceError};

/// What a case feeds the stream core answers with: bytes, or the loss of
/// the connection.
type Feed = mpsc::UnboundedSender<io::Result<Bytes>>;

/// A core that answers an observation with `status` and then whatever is
/// fed, and reports the query and headers it was asked with. Dropping the
/// feed ends the stream as core ends it.
async fn core(
    status: StatusCode,
) -> Result<(
    CoreClient,
    Feed,
    mpsc::UnboundedReceiver<(String, HeaderMap)>,
)> {
    let (feed, stream_of) = mpsc::unbounded_channel();
    let stream_of = Arc::new(Mutex::new(Some(stream_of)));
    let (seen, asked) = mpsc::unbounded_channel();
    let observe = get(move |uri: Uri, headers: HeaderMap| {
        let _ = seen.send((uri.query().unwrap_or_default().to_owned(), headers));
        let bytes = stream_of
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
        let body = stream::unfold(bytes, |mut bytes| async {
            let next = bytes.as_mut()?.recv().await?;
            Some((next, bytes))
        });
        async move { (status, Body::from_stream(body)) }
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let url = format!("http://{}", listener.local_addr()?);
    let router = Router::new().route("/v1/observe", observe);
    tokio::spawn(async move { axum::serve(listener, router).await });
    let client = CoreClient::new(url, "web".to_owned(), "token".to_owned())?;
    Ok((client, feed, asked))
}

/// A snapshot told from another by its conversation's `activity`.
fn snapshot(activity: i64) -> Result<Snapshot> {
    Ok(Snapshot {
        conversations: vec![Conversation {
            key: ConversationKey::new("agent".to_owned(), "someone".to_owned())?,
            channel_id: "channel".to_owned(),
            title: "Größe prüfen".to_owned(),
            started_at: 1,
            active_at: 2,
            activity,
            at_work: 0,
        }],
        more: false,
        open: None,
    })
}

/// A snapshot as core sends it.
fn event(snapshot: &Snapshot) -> Result<String> {
    Ok(format!(
        "event: snapshot\ndata: {}\n\n",
        serde_json::to_string(snapshot)?
    ))
}

fn send(feed: &Feed, bytes: impl Into<Bytes>) -> Result<()> {
    feed.send(Ok(bytes.into()))
        .ok()
        .context("core's stream was closed")
}

/// An open observation that has read its first snapshot.
async fn started() -> Result<(Observation, Feed)> {
    let (client, feed, _) = core(StatusCode::OK).await?;
    send(&feed, event(&snapshot(1)?)?)?;
    let mut observation = client.observe(&View::default()).await?;
    assert_eq!(observation.next().await?, Some(snapshot(1)?));
    Ok((observation, feed))
}

/// Fails unless `outcome` is a failure that is core's fault, not worth
/// repeating, and says `reason`.
fn cores_fault<T>(outcome: Result<T, InterfaceError>, reason: &str) -> Result<()> {
    let error = outcome.err().context("accepted")?;
    ensure!(error.status == StatusCode::BAD_GATEWAY, "{error:?}");
    ensure!(!error.is_transient(), "{error:?}");
    ensure!(error.message.contains(reason), "{error:?}");
    Ok(())
}

#[tokio::test]
async fn an_observation_asks_for_its_view_and_reads_snapshots_however_they_arrive() -> Result<()> {
    let (client, feed, mut asked) = core(StatusCode::OK).await?;
    let view = View {
        conversations: Limit::new(20)?,
        open: Some(OpenView {
            conversation: ConversationKey::new("agent".to_owned(), "web chat/1".to_owned())?,
            turns: Limit::new(120)?,
        }),
    };
    let mut observation = client.observe(&view).await?;
    let (query, headers) = asked.recv().await.context("core was not asked")?;
    assert_eq!(
        query,
        "conversations=20&agent_id=agent&audience_id=web+chat%2F1&turns=120"
    );
    assert_eq!(headers[AUTHORIZATION], "Bearer token");
    assert_eq!(headers[PROVIDER_HEADER], "web");

    // A keep-alive, then one snapshot in three pieces, the second cut
    // inside a character.
    send(&feed, ":\n\n")?;
    let first = event(&snapshot(1)?)?.into_bytes();
    let inside = first
        .iter()
        .position(|byte| *byte == 0xc3)
        .context("no character of two bytes")?;
    for piece in [&first[..9], &first[9..=inside], &first[inside + 1..]] {
        send(&feed, piece.to_vec())?;
    }
    assert_eq!(observation.next().await?, Some(snapshot(1)?));

    // Two snapshots at once, their lines ended as a proxy might end them.
    let both = [event(&snapshot(2)?)?, event(&snapshot(3)?)?].concat();
    send(&feed, both.replace('\n', "\r\n"))?;
    assert_eq!(observation.next().await?, Some(snapshot(2)?));
    assert_eq!(observation.next().await?, Some(snapshot(3)?));

    // Half a snapshot when core ends the stream is no snapshot.
    send(&feed, "event: snapshot\ndata: {\"conversations\"")?;
    drop(feed);
    assert_eq!(observation.next().await?, None);
    assert_eq!(observation.next().await?, None);
    Ok(())
}

#[tokio::test]
async fn what_is_not_a_snapshot_ends_an_observation_as_cores_fault() -> Result<()> {
    for (sent, reason) in [
        ("event: turn\ndata: {}\n\n", "an event named \"turn\""),
        ("data: {}\n\n", "an event named \"message\""),
        ("event: snapshot\ndata: not json\n\n", "decode a snapshot"),
        (
            "event: snapshot\ndata: {\"more\":false}\n\n",
            "missing field",
        ),
    ] {
        let (mut observation, feed) = started().await?;
        send(&feed, sent)?;
        cores_fault(observation.next().await, reason)?;
        // It is over: what core sends after is not read.
        send(&feed, event(&snapshot(2)?)?)?;
        assert_eq!(observation.next().await?, None, "{sent}");
    }
    Ok(())
}

#[tokio::test]
async fn a_snapshot_over_its_limit_is_refused_before_it_is_whole() -> Result<()> {
    let (mut observation, feed) = started().await?;
    send(&feed, "event: snapshot\ndata: ")?;
    let megabyte = Bytes::from(vec![b'a'; 1 << 20]);
    for _ in 0..=SNAPSHOT_LIMIT >> 20 {
        send(&feed, megabyte.clone())?;
    }
    // Core never ends the event, and the observation does not wait for it.
    let refused = tokio::time::timeout(Duration::from_secs(60), observation.next()).await?;
    cores_fault(refused, &format!("over {SNAPSHOT_LIMIT} bytes"))?;
    drop(feed);
    Ok(())
}

#[tokio::test]
async fn a_lost_connection_is_a_failure_worth_repeating_that_names_its_cause() -> Result<()> {
    let (mut observation, feed) = started().await?;
    feed.send(Err(io::Error::other("core was killed")))
        .ok()
        .context("core's stream was closed")?;
    let error = observation.next().await.err().context("read on")?;
    assert!(error.is_transient(), "{error:?}");
    assert!(
        error.message.starts_with("read an observation: ")
            && error
                .message
                .ends_with(": unexpected EOF during chunk size line"),
        "{error:?}"
    );
    assert_eq!(observation.next().await?, None);
    Ok(())
}

#[tokio::test]
async fn core_refuses_an_observation_with_its_own_status_and_words() -> Result<()> {
    for status in [
        StatusCode::BAD_REQUEST,
        StatusCode::FORBIDDEN,
        StatusCode::PAYLOAD_TOO_LARGE,
        StatusCode::INTERNAL_SERVER_ERROR,
    ] {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let url = format!("http://{}", listener.local_addr()?);
        let refusal = get(move || async move {
            (
                status,
                Json(json!({ "error": format!("refused with {status}") })),
            )
                .into_response()
        });
        let router = Router::new().route("/v1/observe", refusal);
        tokio::spawn(async move { axum::serve(listener, router).await });
        let client = CoreClient::new(url, "web".to_owned(), "token".to_owned())?;
        let error = client
            .observe(&View::default())
            .await
            .err()
            .context("observed")?;
        assert_eq!(
            (error.status, error.message),
            (status, format!("refused with {status}"))
        );
    }
    let nowhere = CoreClient::new(
        "http://127.0.0.1:1".to_owned(),
        "web".to_owned(),
        "token".to_owned(),
    )?;
    let error = nowhere
        .observe(&View::default())
        .await
        .err()
        .context("observed")?;
    assert!(error.is_transient(), "{error:?}");
    assert!(
        error.message.starts_with("observe core: ")
            && error
                .message
                .ends_with(": Connection refused (os error 111)"),
        "{error:?}"
    );
    Ok(())
}

#[tokio::test]
async fn core_has_ten_seconds_to_answer_and_to_send_the_first_snapshot() -> Result<()> {
    // Core takes the request and never answers it.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let url = format!("http://{}", listener.local_addr()?);
    let (seen, mut asked) = mpsc::unbounded_channel();
    let stalled = get(move || {
        let _ = seen.send(());
        std::future::pending::<StatusCode>()
    });
    let router = Router::new().route("/v1/observe", stalled);
    tokio::spawn(async move { axum::serve(listener, router).await });
    let client = CoreClient::new(url, "web".to_owned(), "token".to_owned())?;
    let view = View::default();
    let (bound, slack) = (Duration::from_secs(10), Duration::from_secs(1));
    let asked_at = tokio::time::Instant::now();
    let observing = client.observe(&view);
    tokio::pin!(observing);
    tokio::select! {
        answered = &mut observing => anyhow::bail!("core answered: {:?}", answered.err()),
        seen = asked.recv() => seen.context("core was not asked")?,
    }
    // Nothing is in flight now, so the paused clock runs to the bound.
    tokio::time::pause();
    let error = observing.await.err().context("core answered")?;
    assert!(error.is_transient(), "{error:?}");
    let waited = asked_at.elapsed();
    assert!(waited >= bound && waited < bound + slack, "{waited:?}");
    tokio::time::resume();

    // Core answers and keeps the stream alive, but sends no snapshot: the
    // bound is counted from the request, and keep-alives do not extend it.
    let (client, feed, _) = core(StatusCode::OK).await?;
    let asked_at = tokio::time::Instant::now();
    let mut observation = client.observe(&view).await?;
    send(&feed, ":\n\n")?;
    tokio::time::pause();
    let error = observation.next().await.err().context("a snapshot came")?;
    assert!(error.is_transient(), "{error:?}");
    assert!(error.message.contains("no first snapshot"), "{error:?}");
    let waited = asked_at.elapsed();
    assert!(waited >= bound && waited < bound + slack, "{waited:?}");
    Ok(())
}

#[tokio::test]
async fn an_observation_gives_up_on_a_stream_silent_for_three_keep_alives() -> Result<()> {
    let (mut observation, _feed) = started().await?;
    // Nothing is in flight, so the paused clock runs to the bound.
    tokio::time::pause();
    let silent = tokio::time::Instant::now();
    let error = observation.next().await.err().context("core spoke")?;
    assert!(error.is_transient(), "{error:?}");
    assert!(error.message.contains("sent nothing"), "{error:?}");
    let waited = silent.elapsed();
    assert!(
        waited >= KEEP_ALIVE * 3 && waited < KEEP_ALIVE * 4,
        "{waited:?}"
    );
    Ok(())
}

#[tokio::test]
async fn keep_alives_alone_hold_an_observation_open() -> Result<()> {
    let (mut observation, feed) = started().await?;
    tokio::time::pause();
    let quiet = tokio::time::Instant::now();
    let kept_alive = tokio::spawn(async move {
        // On the paused clock this is core's cadence and takes no time.
        for _ in 0..6 {
            tokio::time::sleep(KEEP_ALIVE).await;
            send(&feed, ":\n\n")?;
        }
        send(&feed, event(&snapshot(2)?)?)?;
        anyhow::Ok(feed)
    });
    assert_eq!(observation.next().await?, Some(snapshot(2)?));
    assert!(quiet.elapsed() >= KEEP_ALIVE * 6, "{:?}", quiet.elapsed());
    kept_alive.await??;
    Ok(())
}
