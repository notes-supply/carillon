//! The session record, and the pinned Twilight shard resuming and closing
//! against a local gateway that speaks Discord's gateway protocol.

use std::{os::unix::fs::PermissionsExt as _, path::Path, time::Duration};

use anyhow::{Context as _, anyhow, bail};
use futures_util::{SinkExt as _, StreamExt as _};
use serde_json::{Value, json};
use tokio::{
    net::TcpListener,
    sync::{mpsc, oneshot},
    task::JoinHandle,
};
use tokio_websockets::{Message, ServerBuilder};
use twilight_gateway::{ConfigBuilder, Event, Session};

use super::{
    Dispatch, Gap, Gateway, INTENTS, Identity, RECORD_AGE, Record, ResumeAddress, Start, now,
    record_path, take_record,
};

const TOKEN: &str = "test-only-token";

/// Which kind of gap an unusable record must give.
type Expected = fn(&Gap) -> bool;

fn identity() -> Identity {
    Identity::new("discord-private", "201", "100", TOKEN)
}

fn record(identity: Identity) -> Record {
    Record {
        identity,
        session_id: "0123abcd".to_owned(),
        sequence: 41,
        resume_url: "wss://gateway-us-east1-b.discord.gg".to_owned(),
        recorded_at: now(),
    }
}

fn write(path: &Path, record: &Record) -> anyhow::Result<()> {
    std::fs::write(path, serde_json::to_vec(record)?)?;
    Ok(())
}

#[test]
fn a_recorded_session_is_resumed_once_by_its_own_identity() -> anyhow::Result<()> {
    let directory = tempfile::tempdir()?;
    let path = record_path(directory.path());
    write(&path, &record(identity()))?;
    let Start::Resume { session, address } = take_record(&path, &identity()) else {
        bail!("a valid record was not resumed");
    };
    assert_eq!(session, Session::new(41, "0123abcd".to_owned()));
    assert_eq!(
        address,
        ResumeAddress("wss://gateway-us-east1-b.discord.gg".to_owned())
    );
    // Consumed before use: the next start has nothing to resume.
    assert!(!path.exists());
    assert!(matches!(
        take_record(&path, &identity()),
        Start::Identify(Gap::NoRecord)
    ));
    Ok(())
}

#[test]
fn an_unusable_record_starts_a_new_session_and_is_never_used_again() -> anyhow::Result<()> {
    let directory = tempfile::tempdir()?;
    let path = record_path(directory.path());
    let mut cases: Vec<(&str, Vec<u8>, Expected)> = Vec::new();
    for other in [
        Identity::new("discord-other", "201", "100", TOKEN),
        Identity::new("discord-private", "202", "100", TOKEN),
        Identity::new("discord-private", "201", "101", TOKEN),
        Identity::new("discord-private", "201", "100", "another-token"),
    ] {
        cases.push((
            "another identity",
            serde_json::to_vec(&record(other))?,
            |gap| matches!(gap, Gap::OtherIdentity),
        ));
    }
    let stale = Record {
        recorded_at: now() - RECORD_AGE.as_secs() - 1,
        ..record(identity())
    };
    let future = Record {
        recorded_at: now() + 120,
        ..record(identity())
    };
    for aged in [stale, future] {
        cases.push(("stale", serde_json::to_vec(&aged)?, |gap| {
            matches!(gap, Gap::Stale)
        }));
    }
    for address in [
        "ws://gateway-us-east1-b.discord.gg",
        "https://gateway.discord.gg",
        "wss://gateway.discord.gg.example.com",
        "wss://example.com",
        "wss://user@gateway.discord.gg",
        "wss://gateway.discord.gg:443",
        "wss://gateway.discord.gg/path",
        "wss://gateway.discord.gg?query",
        "wss://.discord.gg",
        "wss://GATEWAY.discord.gg",
    ] {
        let unsafe_address = Record {
            resume_url: address.to_owned(),
            ..record(identity())
        };
        cases.push((
            "unsafe address",
            serde_json::to_vec(&unsafe_address)?,
            |gap| matches!(gap, Gap::UnsafeAddress),
        ));
    }
    for session_id in ["", "not-alphanumeric", &"a".repeat(129)] {
        let invalid = Record {
            session_id: session_id.to_owned(),
            ..record(identity())
        };
        cases.push(("invalid session", serde_json::to_vec(&invalid)?, |gap| {
            matches!(gap, Gap::Corrupt)
        }));
    }
    let mut unknown = serde_json::to_value(record(identity()))?;
    unknown["token"] = json!(TOKEN);
    cases.push(("unknown field", serde_json::to_vec(&unknown)?, |gap| {
        matches!(gap, Gap::Corrupt)
    }));
    cases.push(("not JSON", b"{".to_vec(), |gap| matches!(gap, Gap::Corrupt)));
    cases.push(("oversized", vec![b' '; 5000], |gap| {
        matches!(gap, Gap::Corrupt)
    }));
    for (case, bytes, expected) in cases {
        std::fs::write(&path, bytes)?;
        let Start::Identify(gap) = take_record(&path, &identity()) else {
            bail!("{case}: an unusable record was resumed");
        };
        assert!(expected(&gap), "{case}: {gap}");
        // The reason names no secret.
        assert!(!gap.to_string().contains(TOKEN), "{case}");
        assert!(
            !path.exists(),
            "{case}: the record was left for another start"
        );
    }
    Ok(())
}

#[test]
fn a_record_that_cannot_be_removed_is_not_used() -> anyhow::Result<()> {
    let directory = tempfile::tempdir()?;
    let state = directory.path().join("state");
    std::fs::create_dir(&state)?;
    let path = record_path(&state);
    write(&path, &record(identity()))?;
    std::fs::set_permissions(&state, std::fs::Permissions::from_mode(0o500))?;
    let start = take_record(&path, &identity());
    std::fs::set_permissions(&state, std::fs::Permissions::from_mode(0o700))?;
    assert!(
        matches!(start, Start::Identify(Gap::NotConsumed(_))),
        "{start:?}"
    );
    Ok(())
}

/// What the shard sent the local gateway.
#[derive(Debug, PartialEq)]
enum Received {
    Identify {
        token: String,
    },
    Resume {
        session_id: String,
        sequence: u64,
        token: String,
    },
    Close(Option<u16>),
}

/// A local gateway: on each connection it says hello, and once the shard
/// identifies or resumes it sends that connection's scripted frames, then
/// any frame the test sends. It reports what the shard sent, and answers a
/// close with a close.
struct LocalGateway {
    url: String,
    frames: mpsc::UnboundedSender<(String, oneshot::Sender<()>)>,
    received: mpsc::UnboundedReceiver<Received>,
    server: JoinHandle<()>,
}

impl Drop for LocalGateway {
    fn drop(&mut self) {
        self.server.abort();
    }
}

impl LocalGateway {
    async fn start(connections: Vec<Vec<Value>>) -> anyhow::Result<Self> {
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let url = format!("ws://{}", listener.local_addr()?);
        let (frames, mut outgoing) = mpsc::unbounded_channel::<(String, oneshot::Sender<()>)>();
        let (report, received) = mpsc::unbounded_channel();
        let server = tokio::spawn(async move {
            for scripted in connections {
                let Ok((stream, _)) = listener.accept().await else {
                    return;
                };
                let Ok((_, mut socket)) = ServerBuilder::new().accept(stream).await else {
                    return;
                };
                let hello = json!({"op": 10, "d": {"heartbeat_interval": 45_000}});
                if socket.send(Message::text(hello.to_string())).await.is_err() {
                    return;
                }
                let mut scripted = Some(scripted);
                loop {
                    tokio::select! {
                        frame = socket.next() => {
                            let Some(Ok(frame)) = frame else { break };
                            if let Some((code, _)) = frame.as_close() {
                                let _ = report.send(Received::Close(Some(code.into())));
                                let _ = socket.close().await;
                                break;
                            }
                            let Some(text) = frame.as_text() else { continue };
                            let Ok(payload) = serde_json::from_str::<Value>(text) else { continue };
                            let d = &payload["d"];
                            let token = d["token"].as_str().unwrap_or_default().to_owned();
                            let heard = match payload["op"].as_u64() {
                                Some(2) => Received::Identify { token },
                                Some(6) => Received::Resume {
                                    session_id: d["session_id"].as_str().unwrap_or_default().to_owned(),
                                    sequence: d["seq"].as_u64().unwrap_or(u64::MAX),
                                    token,
                                },
                                // Heartbeats and anything else need no answer.
                                _ => continue,
                            };
                            let _ = report.send(heard);
                            for frame in scripted.take().unwrap_or_default() {
                                if socket.send(Message::text(frame.to_string())).await.is_err() {
                                    return;
                                }
                            }
                        }
                        Some((frame, sent)) = outgoing.recv() => {
                            if socket.send(Message::text(frame)).await.is_err() {
                                return;
                            }
                            let _ = sent.send(());
                        }
                    }
                }
            }
        });
        Ok(Self {
            url,
            frames,
            received,
            server,
        })
    }

    /// Sends `frame` on the current connection, returning once it is sent.
    async fn send(&self, frame: &Value) -> anyhow::Result<()> {
        let (sent, confirmed) = oneshot::channel();
        self.frames
            .send((frame.to_string(), sent))
            .map_err(|_| anyhow!("the local gateway stopped"))?;
        tokio::time::timeout(Duration::from_secs(10), confirmed).await??;
        Ok(())
    }

    async fn heard(&mut self) -> anyhow::Result<Received> {
        tokio::time::timeout(Duration::from_secs(10), self.received.recv())
            .await?
            .context("the local gateway stopped")
    }

    /// A gateway that connects here, starting as `start` says, with
    /// settlements reported on the returned sender.
    fn gateway(&self, start: Start) -> (Gateway, mpsc::UnboundedSender<Dispatch>) {
        let (settled, settlements) = mpsc::unbounded_channel();
        let config = ConfigBuilder::new(TOKEN.to_owned(), INTENTS).proxy_url(self.url.clone());
        (Gateway::configured(config, start, settlements), settled)
    }
}

fn dispatch(kind: &str, sequence: u64, data: &Value) -> Value {
    json!({"op": 0, "t": kind, "s": sequence, "d": data})
}

fn ready(sequence: u64, session_id: &str) -> Value {
    dispatch(
        "READY",
        sequence,
        &json!({
            "v": 10,
            "user": {"id": "201", "username": "carillon", "discriminator": "0", "avatar": null,
                     "bot": true, "mfa_enabled": false},
            "guilds": [{"id": "100", "unavailable": true}],
            "session_id": session_id,
            "resume_gateway_url": "wss://gateway-us-east1-b.discord.gg",
            "application": {"id": "301", "flags": 0},
        }),
    )
}

fn message(sequence: u64, id: &str) -> Value {
    dispatch(
        "MESSAGE_CREATE",
        sequence,
        &json!({
            "id": id, "channel_id": "300", "guild_id": "100",
            "author": {"id": "200", "username": "person", "discriminator": "0", "avatar": null},
            "content": "hello", "attachments": [], "embeds": [], "mention_everyone": false,
            "mention_roles": [], "mentions": [], "pinned": false,
            "timestamp": "2026-10-09T00:00:00.000000+00:00", "tts": false, "type": 0,
        }),
    )
}

/// The next message the gateway yields, with its dispatch; closes on the way
/// are the gateway loop's to ignore.
async fn next_message(gateway: &mut Gateway) -> anyhow::Result<(String, Dispatch)> {
    loop {
        let (event, dispatch) = tokio::time::timeout(Duration::from_secs(15), gateway.next_event())
            .await?
            .context("the gateway stream ended")??;
        match event {
            Event::MessageCreate(message) => {
                return Ok((
                    message.id.to_string(),
                    dispatch.context("a message without its dispatch")?,
                ));
            }
            Event::GatewayClose(_) => {}
            other => bail!("unexpected event {:?}", other.kind()),
        }
    }
}

fn recorded(path: &Path) -> anyhow::Result<Value> {
    assert_eq!(std::fs::metadata(path)?.permissions().mode() & 0o777, 0o600);
    Ok(serde_json::from_slice(&std::fs::read(path)?)?)
}

#[tokio::test]
async fn a_stop_closes_for_a_resume_and_records_a_floor_below_what_did_not_settle()
-> anyhow::Result<()> {
    let mut local = LocalGateway::start(vec![vec![
        ready(1, "session1"),
        message(2, "900"),
        message(3, "901"),
    ]])
    .await?;
    let (mut gateway, settled) = local.gateway(Start::Identify(Gap::NoRecord));
    let (id, first) = next_message(&mut gateway).await?;
    assert_eq!((id.as_str(), first), ("900", Dispatch::new(1, 2)));
    assert_eq!(
        local.heard().await?,
        Received::Identify {
            token: format!("Bot {TOKEN}")
        }
    );
    gateway.taken(Some(first));
    let (id, second) = next_message(&mut gateway).await?;
    assert_eq!((id.as_str(), second), ("901", Dispatch::new(1, 3)));
    gateway.taken(Some(second));
    // The later message settles first; the earlier one is still preparing.
    settled.send(second)?;
    // A message reaches the shard as it closes: it is not taken.
    local.send(&message(4, "902")).await?;
    assert!(gateway.close().await);
    assert_eq!(local.heard().await?, Received::Close(Some(4000)));
    let directory = tempfile::tempdir()?;
    let path = record_path(directory.path());
    gateway.record(&path, &identity());
    let record = recorded(&path)?;
    assert_eq!(record["session_id"], "session1");
    assert_eq!(
        record["sequence"], 1,
        "the floor passed an unsettled message"
    );
    assert_eq!(record["resume_url"], "wss://gateway-us-east1-b.discord.gg");
    assert!(!String::from_utf8(std::fs::read(&path)?)?.contains(TOKEN));
    // Once the earlier message settles, the floor still stays below the one
    // seen while closing.
    settled.send(first)?;
    gateway.record(&path, &identity());
    assert_eq!(recorded(&path)?["sequence"], 3);
    Ok(())
}

#[tokio::test]
async fn the_shard_resumes_from_the_recorded_floor_and_reports_the_replay_done()
-> anyhow::Result<()> {
    let mut local = LocalGateway::start(vec![vec![
        message(42, "942"),
        dispatch("RESUMED", 43, &json!({})),
    ]])
    .await?;
    let (mut gateway, settled) = local.gateway(Start::Resume {
        session: Session::new(41, "session1".to_owned()),
        address: ResumeAddress::unchecked(local.url.clone()),
    });
    let (id, replayed) = next_message(&mut gateway).await?;
    assert_eq!((id.as_str(), replayed), ("942", Dispatch::new(0, 42)));
    assert_eq!(
        local.heard().await?,
        Received::Resume {
            session_id: "session1".to_owned(),
            sequence: 41,
            token: format!("Bot {TOKEN}")
        }
    );
    gateway.taken(Some(replayed));
    local.send(&message(44, "944")).await?;
    let (_, later) = next_message(&mut gateway).await?;
    assert_eq!(later, Dispatch::new(0, 44));
    assert!(!gateway.resuming, "RESUMED was not observed");
    gateway.taken(Some(later));
    settled.send(replayed)?;
    assert!(gateway.close().await);
    assert_eq!(local.heard().await?, Received::Close(Some(4000)));
    // The floor is below the later message; the resume address is the
    // one the session was resumed at (local here, so it is not recorded).
    let (session, address) = gateway.floor().context("no floor")?;
    assert_eq!(session, Session::new(43, "session1".to_owned()));
    assert_eq!(address, local.url);
    Ok(())
}

#[tokio::test]
async fn a_new_session_in_process_never_takes_the_old_sessions_settlements() -> anyhow::Result<()> {
    // Discord replays one message, then refuses to go on with the session:
    // the shard identifies anew, and the new session's sequences restart.
    let mut local = LocalGateway::start(vec![
        vec![message(2, "902"), json!({"op": 9, "d": false})],
        vec![ready(1, "session2"), message(2, "912")],
    ])
    .await?;
    let (mut gateway, settled) = local.gateway(Start::Resume {
        session: Session::new(1, "session1".to_owned()),
        address: ResumeAddress::unchecked(local.url.clone()),
    });
    let (_, old) = next_message(&mut gateway).await?;
    assert_eq!(old, Dispatch::new(0, 2));
    gateway.taken(Some(old));
    let (id, new) = next_message(&mut gateway).await?;
    assert_eq!((id.as_str(), new), ("912", Dispatch::new(1, 2)));
    gateway.taken(Some(new));
    assert!(matches!(local.heard().await?, Received::Resume { .. }));
    assert_eq!(local.heard().await?, Received::Close(Some(1000)));
    assert!(matches!(local.heard().await?, Received::Identify { .. }));
    // The old session's preparation settles late: the new session's message
    // with the same sequence is still unsettled.
    settled.send(old)?;
    assert!(gateway.close().await);
    let directory = tempfile::tempdir()?;
    let path = record_path(directory.path());
    gateway.record(&path, &identity());
    let record = recorded(&path)?;
    assert_eq!(record["session_id"], "session2");
    assert_eq!(record["sequence"], 1);
    Ok(())
}

#[tokio::test]
async fn more_unsettled_messages_than_a_record_covers_record_nothing() -> anyhow::Result<()> {
    let local = LocalGateway::start(vec![vec![ready(1, "session1"), message(2, "900")]]).await?;
    let (mut gateway, _settled) = local.gateway(Start::Identify(Gap::NoRecord));
    let (_, first) = next_message(&mut gateway).await?;
    for sequence in 2..=2 + super::UNSETTLED_LIMIT as u64 {
        gateway.taken(Some(Dispatch::new(first.session, sequence)));
    }
    assert!(gateway.close().await);
    let directory = tempfile::tempdir()?;
    let path = record_path(directory.path());
    gateway.record(&path, &identity());
    assert!(!path.exists());
    Ok(())
}
